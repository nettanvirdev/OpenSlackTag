//! Context compaction: when a conversation outgrows its budget, the older part
//! is replaced by a handoff note written by the same model, and the recent
//! part is kept word for word.

use std::collections::HashSet;

use openslacktag_core::Provider;
use openslacktag_core::message::{Entry, UserEntry};
use openslacktag_core::provider::ChatRequest;

/// Entries kept verbatim at the end when they fit in half the budget.
const KEEP_RECENT: usize = 8;
/// How much of any one old entry goes into the digest the summariser reads.
const MAX_DIGEST_CHARS_PER_ENTRY: usize = 1500;
const SUMMARY_PREFIX: &str =
    "Summary of the earlier conversation (older messages were compacted):\n\n";

pub const SUMMARY_INSTRUCTION: &str = "\
You are writing the handoff note for the earlier part of a working conversation, \
so the work can continue in a smaller context. Write for the teammate picking it up, \
who has the recent messages but not these. Dense, factual, no preamble, no narrative. \
Use exactly these headings, and write \"none\" under a heading rather than leaving it out:

## Goal
What was asked for, in the asker's terms, and what would count as done.

## People and preferences
Who is involved, rules they gave, how they want things done.

## Done
What has been completed and verified, with links, pages and paths.

## Decisions
Choices already made and why, so they are not reopened.

## Open
What remains, what was tried and failed, the next concrete step.

## Worth keeping
Facts that were expensive to find: commands that work, errors and their causes, values, names. \
Not tool output; what the output taught.";

/// Rough token count: four characters per token is close enough to decide when to compact.
pub fn estimate_tokens(history: &[Entry]) -> usize {
    let chars: usize = history
        .iter()
        .map(|entry| {
            let calls = match entry {
                Entry::Assistant(a) => a
                    .tool_calls
                    .iter()
                    .map(|c| c.name.len() + c.arguments.len())
                    .sum(),
                _ => 0,
            };
            entry.text().len() + calls
        })
        .sum();
    chars / 4
}

/// Indexes where the history can be cut cleanly: the entry there is a
/// message from a person or a teammate step, and every tool call before it
/// already has its result before it. Strict providers reject a tool result
/// whose call was cut away, or a call left without its result.
fn clean_cuts(history: &[Entry]) -> Vec<usize> {
    let mut unanswered: HashSet<&str> = HashSet::new();
    let mut cuts = Vec::new();
    for (i, entry) in history.iter().enumerate() {
        if i > 0 && unanswered.is_empty() && !matches!(entry, Entry::Tool(_)) {
            cuts.push(i);
        }
        match entry {
            Entry::Assistant(a) => unanswered.extend(a.tool_calls.iter().map(|c| c.id.as_ref())),
            Entry::Tool(t) => {
                unanswered.remove(t.tool_call_id.as_ref());
            }
            Entry::User(_) => {}
        }
    }
    cuts
}

/// Where the verbatim tail starts. Prefers keeping at least `KEEP_RECENT`
/// entries, but never a tail bigger than half the budget: a single task that
/// ran long can still be cut between its own steps. `None` when no clean cut
/// exists.
fn cut_point(history: &[Entry], budget_tokens: usize) -> Option<usize> {
    let cuts = clean_cuts(history);
    let mut suffix = vec![0; history.len() + 1];
    for i in (0..history.len()).rev() {
        suffix[i] = suffix[i + 1] + estimate_tokens(&history[i..=i]);
    }
    let fits = |i: usize| suffix[i] <= budget_tokens / 2;
    let keep_from = history.len().saturating_sub(KEEP_RECENT);
    cuts.iter()
        .copied()
        .find(|&i| i <= keep_from && fits(i))
        .or_else(|| cuts.iter().copied().find(|&i| fits(i)))
        .or_else(|| cuts.last().copied())
}

/// The part being dropped, written out for the summariser to read.
pub fn digest(entries: &[Entry]) -> String {
    entries
        .iter()
        .map(|entry| {
            let role = match entry {
                Entry::User(_) => "person",
                Entry::Assistant(_) => "teammate",
                Entry::Tool(_) => "tool result",
            };
            // By characters, not bytes: a cut inside a multi-byte character would panic.
            let body: String = entry
                .text()
                .chars()
                .take(MAX_DIGEST_CHARS_PER_ENTRY)
                .collect();
            match entry {
                Entry::Assistant(a) if !a.tool_calls.is_empty() => {
                    let names: Vec<&str> = a.tool_calls.iter().map(|c| c.name.as_str()).collect();
                    format!("{role}: {body}\n[called: {}]", names.join(", "))
                }
                _ => format!("{role}: {body}"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Compact `history` if it is over `budget_tokens`. Returns it unchanged when
/// it fits or cannot be cut; returns an error only if the summary call fails.
pub async fn compact(
    provider: &dyn Provider,
    model: &str,
    history: Vec<Entry>,
    budget_tokens: usize,
) -> Result<Vec<Entry>, String> {
    if estimate_tokens(&history) <= budget_tokens {
        return Ok(history);
    }
    let Some(cut) = cut_point(&history, budget_tokens) else {
        return Ok(history);
    };

    let request = ChatRequest {
        model: model.to_string(),
        system: SUMMARY_INSTRUCTION.to_string(),
        history: vec![Entry::user(cap(digest(&history[..cut]), budget_tokens * 4))],
        // Zero: the same conversation summarised twice should give the same account.
        temperature: Some(0.0),
        ..Default::default()
    };
    let summary = crate::complete_text(provider, request).await?;
    let summary = summary.trim();
    if summary.is_empty() {
        return Err("the model returned an empty summary".into());
    }

    let note = Entry::User(UserEntry {
        content: Some(format!("{SUMMARY_PREFIX}{summary}")),
        pinned: true,
        compacted: true,
        ..Default::default()
    });
    let mut compacted = vec![note];
    // Keep the roles alternating: a tail that starts with a person's message
    // needs a teammate turn between it and the note.
    if matches!(history[cut], Entry::User(_)) {
        compacted.push(Entry::assistant(
            "Understood. Continuing from that summary.",
        ));
    }
    compacted.extend(history.into_iter().skip(cut));
    Ok(compacted)
}

/// Keep a digest under `max_chars`: the start (where the goal was set) and
/// the most recent part, with a marker where the middle was dropped.
fn cap(digest: String, max_chars: usize) -> String {
    let total = digest.chars().count();
    if total <= max_chars {
        return digest;
    }
    let head: String = digest.chars().take(max_chars / 4).collect();
    let tail: String = digest.chars().skip(total - max_chars * 3 / 4).collect();
    format!(
        "{head}

[... middle of the conversation omitted ...]

{tail}"
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use openslacktag_core::message::{AssistantEntry, ToolCall};
    use openslacktag_mock::MockProvider;

    fn long_history(turns: usize) -> Vec<Entry> {
        let mut history = Vec::new();
        for i in 0..turns {
            history.push(Entry::user(format!("question {i} {}", "x".repeat(400))));
            history.push(Entry::Assistant(AssistantEntry {
                content: None,
                tool_calls: vec![ToolCall {
                    id: format!("c{i}").into(),
                    name: "wiki_read".into(),
                    arguments: "{}".into(),
                }],
                ..Default::default()
            }));
            history.push(Entry::tool_result(
                format!("c{i}").into(),
                "page text",
                true,
            ));
            history.push(Entry::assistant(format!("answer {i}")));
        }
        history
    }

    #[tokio::test]
    async fn short_history_is_left_alone() {
        let provider = MockProvider::new().replying("unused");
        let history = long_history(2);
        let out = compact(&provider, "m", history.clone(), 100_000)
            .await
            .unwrap();
        assert_eq!(out, history);
    }

    /// Every tool result's call is earlier in the list, and every call is answered.
    fn assert_well_formed(entries: &[Entry]) {
        let mut open = HashSet::new();
        for entry in entries {
            match entry {
                Entry::Assistant(a) => open.extend(a.tool_calls.iter().map(|c| c.id.to_string())),
                Entry::Tool(t) => {
                    assert!(open.remove(&t.tool_call_id.to_string()), "orphaned result")
                }
                Entry::User(_) => {}
            }
        }
        assert!(open.is_empty(), "unanswered calls: {open:?}");
    }

    #[tokio::test]
    async fn long_history_is_summarised_at_a_clean_cut_within_budget() {
        let provider = MockProvider::new().replying(
            "## Goal
ship it",
        );
        let history = long_history(10);
        let out = compact(&provider, "m", history.clone(), 600).await.unwrap();

        assert!(out[0].is_pinned());
        assert!(out[0].text().contains(
            "## Goal
ship it"
        ));
        assert!(out.len() < history.len());
        assert!(estimate_tokens(&out[1..]) <= 600);
        assert_eq!(out.last(), history.last());
        assert_well_formed(&out);
        // Roles alternate after the note.
        assert!(matches!(out[1], Entry::Assistant(_)));
    }

    #[tokio::test]
    async fn one_long_task_is_cut_between_its_own_steps() {
        // One question, then forty tool steps: no later message from a person to cut at.
        let mut history = vec![Entry::user("<@U1>: audit every repo")];
        for i in 0..40 {
            history.push(Entry::Assistant(AssistantEntry {
                tool_calls: vec![ToolCall {
                    id: format!("c{i}").into(),
                    name: "web_fetch".into(),
                    arguments: "{}".into(),
                }],
                ..Default::default()
            }));
            history.push(Entry::tool_result(
                format!("c{i}").into(),
                "y".repeat(2000),
                true,
            ));
        }
        history.push(Entry::user("<@U1>: so what did you find?"));

        let provider = MockProvider::new().replying(
            "## Goal
audit",
        );
        let out = compact(&provider, "m", history.clone(), 4000)
            .await
            .unwrap();
        assert!(out.len() < 20, "kept {} entries", out.len());
        assert!(estimate_tokens(&out) <= 4000);
        assert_well_formed(&out);
        assert_eq!(out.last(), history.last());
    }

    #[test]
    fn cuts_never_land_inside_a_tool_exchange() {
        let history = long_history(3);
        for cut in clean_cuts(&history) {
            assert!(!matches!(history[cut], Entry::Tool(_)));
            assert_well_formed(&history[cut..]);
        }
        assert!(clean_cuts(&[Entry::user("only")]).is_empty());
    }

    #[test]
    fn oversized_digests_keep_the_start_and_the_end() {
        let capped = cap(format!("GOAL{}END", "m".repeat(10_000)), 1000);
        assert!(capped.starts_with("GOAL") && capped.ends_with("END"));
        assert!(capped.chars().count() < 1100);
        assert_eq!(cap("short".into(), 1000), "short");
    }

    #[tokio::test]
    async fn a_failed_summary_is_an_error() {
        let provider = MockProvider::new().failing("503 upstream", Some(503));
        assert!(
            compact(&provider, "m", long_history(10), 100)
                .await
                .is_err()
        );
    }

    #[test]
    fn digest_names_tool_calls_and_caps_entries() {
        let text = digest(&long_history(1));
        assert!(text.contains("[called: wiki_read]"));
        assert!(
            text.lines()
                .all(|l| l.chars().count() <= MAX_DIGEST_CHARS_PER_ENTRY + 20)
        );
    }
}
