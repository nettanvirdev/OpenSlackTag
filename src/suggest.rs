//! Automation suggestions: look back over recent conversations, find the
//! requests people keep making, and propose schedules for them.

use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use openslacktag_core::Provider;
use openslacktag_core::message::Entry;
use openslacktag_core::provider::ChatRequest;
use serde_json::Value;

/// Too little history to see a pattern in.
const MIN_MESSAGES: usize = 5;
const MAX_DIGEST_CHARS: usize = 60_000;
const MAX_CHARS_PER_MESSAGE: usize = 300;

pub const SUGGEST_PROMPT: &str = "\
You review what a team asked their AI teammate recently and spot requests that recur: the same report, check, \
summary or lookup asked for again and again, often at similar times. For each real pattern (at least two \
occurrences), propose one automation.

Reply in Slack formatting, short:
*Automations you might want*
- What recurs, with an example of how it was asked, and who asks.
  Suggested: `when` and the exact prompt, ready for schedule_task, e.g. `every weekday at 09:00`: \"Summarise new \
issues in #support since yesterday\".

At most 5 suggestions, most valuable first. Never invent patterns: if nothing recurs, reply with exactly NONE.";

/// Messages people sent in conversations active within the last `days`.
fn recent_requests(data_dir: &Path, days: u64) -> Vec<String> {
    let since = SystemTime::now() - Duration::from_secs(days * 24 * 3600);
    let Ok(entries) = std::fs::read_dir(data_dir.join("conversations")) else {
        return Vec::new();
    };
    let mut messages = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let fresh = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t >= since);
        if !fresh || path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(saved) = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        else {
            continue;
        };
        let history: Vec<Entry> =
            serde_json::from_value(saved["history"].clone()).unwrap_or_default();
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        for item in history {
            if let Entry::User(user) = &item
                && !user.compacted
            {
                let text: String = item.text().chars().take(MAX_CHARS_PER_MESSAGE).collect();
                if !text.trim().is_empty() {
                    messages.push(format!("[{name}] {}", text.replace('\n', " ")));
                }
            }
        }
    }
    messages.sort();
    messages
}

/// Suggestions to post, or `None` when there is nothing worth suggesting.
pub async fn suggest(
    provider: &dyn Provider,
    model: &str,
    data_dir: &Path,
    days: u64,
) -> Result<Option<String>> {
    let messages = recent_requests(data_dir, days);
    if messages.len() < MIN_MESSAGES {
        return Ok(None);
    }
    let mut digest = messages.join("\n");
    if digest.len() > MAX_DIGEST_CHARS {
        let cut = digest
            .char_indices()
            .map(|(i, _)| i)
            .find(|&i| i >= digest.len() - MAX_DIGEST_CHARS)
            .unwrap_or(0);
        digest = digest[cut..].to_string();
    }
    let request = ChatRequest {
        model: model.to_string(),
        system: SUGGEST_PROMPT.to_string(),
        history: vec![Entry::user(format!(
            "Requests from the last {days} days, one per line, prefixed by conversation:\n\n{digest}"
        ))],
        temperature: Some(0.2),
        ..Default::default()
    };
    let text = crate::complete_text(provider, request)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    let text = text.trim();
    Ok((!text.is_empty() && text != "NONE").then(|| text.to_string()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use openslacktag_mock::MockProvider;
    use serde_json::json;

    fn conversation(dir: &Path, name: &str, texts: &[&str]) {
        let history: Vec<Entry> = texts.iter().map(|t| Entry::user(*t)).collect();
        std::fs::create_dir_all(dir.join("conversations")).unwrap();
        std::fs::write(
            dir.join("conversations").join(format!("{name}.json")),
            serde_json::to_vec(&json!({"history": history})).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn too_little_history_suggests_nothing() {
        let dir = tempfile::tempdir().unwrap();
        conversation(dir.path(), "c1", &["<@U1>: hi"]);
        let provider = MockProvider::new().replying("unused");
        assert!(
            suggest(&provider, "m", dir.path(), 7)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(provider.remaining(), 1, "the model was not asked");
    }

    #[tokio::test]
    async fn recurring_requests_reach_the_model() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..3 {
            conversation(
                dir.path(),
                &format!("C1-{i}"),
                &[
                    "<@U1>: summarise #support since yesterday",
                    "<@U2>: what did we ship?",
                ],
            );
        }
        let provider =
            MockProvider::new().replying("*Automations you might want*\n- daily support summary");
        let text = suggest(&provider, "m", dir.path(), 7)
            .await
            .unwrap()
            .unwrap();
        assert!(text.contains("daily support summary"));
        let asked = provider.last_request().unwrap().history[0]
            .text()
            .to_string();
        assert_eq!(asked.matches("summarise #support").count(), 3);

        let none = MockProvider::new().replying("NONE");
        assert!(suggest(&none, "m", dir.path(), 7).await.unwrap().is_none());
    }
}
