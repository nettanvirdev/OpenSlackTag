//! The engine turns "messages arrived in a conversation" into "a reply",
//! whatever the surface: Slack or the local chat.
//!
//! It keeps each conversation's full transcript (tool calls included) on disk,
//! compacts it when it outgrows the context budget, and folds messages that
//! arrive mid-task into the running task instead of starting a second one.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use openslacktag_agent::{Agent, AgentConfig, AgentEvent, DOOM_LOOP_KEY, Steer, StopReason, Turn};
use openslacktag_core::message::{Entry, ToolCall};
use openslacktag_core::permission::Action;
use openslacktag_core::tool::{Decision, PermissionGate, PermissionRequest, Tool, ToolRegistry};
use openslacktag_core::{Provider, SessionId};
use openslacktag_mcp::provider::McpProvider;
use openslacktag_tools::Registry;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::compact;
use crate::tools::{Sandbox, Wiki, builtin_tools};

/// A message for the engine, with the surface's position marker (Slack's
/// `ts`). Entries at or before the conversation's cursor were already seen and
/// are dropped, which makes duplicate deliveries harmless.
#[derive(Debug, Clone)]
pub struct Incoming {
    pub cursor: Option<String>,
    pub entry: Entry,
}

#[derive(Debug, PartialEq)]
pub enum Reply {
    /// The answer to post.
    Answer(String),
    /// A task was already running here; the messages were handed to it.
    Steered,
    /// Everything in the batch had already been seen.
    Nothing,
}

/// What a deployment plugs into the engine.
pub struct Parts {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub data_dir: PathBuf,
    pub sandbox: Option<Sandbox>,
    pub mcp: Arc<McpProvider>,
    /// Compact a conversation once it is estimated to exceed this many tokens.
    pub compact_at_tokens: usize,
    /// Extra tools, for tests and embedders.
    pub extra_tools: Vec<Arc<dyn Tool>>,
}

impl std::fmt::Debug for Parts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parts")
            .field("model", &self.model)
            .field("data_dir", &self.data_dir)
            .field("extra_tools", &self.extra_tools.len())
            .finish_non_exhaustive()
    }
}

pub struct Engine {
    provider: Arc<dyn Provider>,
    registry: Arc<dyn ToolRegistry>,
    gate: Arc<dyn PermissionGate>,
    config: AgentConfig,
    wiki: Arc<Wiki>,
    mcp: Arc<McpProvider>,
    sandbox: Option<Sandbox>,
    data_dir: PathBuf,
    compact_at_tokens: usize,
    running: Mutex<HashMap<String, Running>>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("model", &self.config.model)
            .field("data_dir", &self.data_dir)
            .finish_non_exhaustive()
    }
}

struct Running {
    steer: Steer,
    cursor: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    cursor: Option<String>,
    history: Vec<Entry>,
}

impl Engine {
    pub fn new(parts: Parts) -> Result<Self> {
        let wiki = Arc::new(
            Wiki::open(parts.data_dir.join("wiki")).context("could not create the wiki folder")?,
        );
        let gate: Arc<dyn PermissionGate> = Arc::new(AllowAll);
        let registry = Registry::new(gate.clone())
            .with_tools(builtin_tools(&wiki, parts.sandbox.as_ref()))
            .with_tools(parts.extra_tools)
            .with_provider(parts.mcp.clone());
        Ok(Self {
            provider: parts.provider,
            registry: Arc::new(registry),
            gate,
            config: AgentConfig {
                model: parts.model,
                ..AgentConfig::default()
            },
            wiki,
            mcp: parts.mcp,
            sandbox: parts.sandbox,
            data_dir: parts.data_dir,
            compact_at_tokens: parts.compact_at_tokens,
            running: Mutex::new(HashMap::new()),
        })
    }

    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.provider
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    /// Names of the tools the model is offered, MCP tools included.
    pub async fn tool_names(&self) -> Vec<String> {
        self.registry
            .specs()
            .await
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    /// The newest message this conversation has already taken in.
    pub fn cursor(&self, conversation: &str) -> Option<String> {
        if let Some(running) = self.running.lock().get(conversation) {
            return running.cursor.clone();
        }
        self.load(conversation).cursor
    }

    /// Whether this conversation has a saved transcript.
    pub fn knows(&self, conversation: &str) -> bool {
        self.path(conversation).exists()
    }

    /// Take in new messages and answer them. Short progress notes ("Using
    /// web_fetch: https://...") go to `progress` while the work runs.
    pub async fn reply(
        &self,
        conversation: &str,
        incoming: Vec<Incoming>,
        progress: &UnboundedSender<String>,
    ) -> Result<Reply> {
        let mut waited = Duration::ZERO;
        let (mut steer, mut history, mut fresh) = loop {
            match self.admit(conversation, &incoming) {
                Admit::Run(steer, history, fresh) => break (steer, history, fresh),
                Admit::Steered => return Ok(Reply::Steered),
                Admit::Nothing => return Ok(Reply::Nothing),
                // The running task is finishing and no longer takes messages:
                // wait for it, then start a new one from its saved transcript.
                Admit::Wait if waited < MAX_WAIT => {
                    tokio::time::sleep(WAIT_STEP).await;
                    waited += WAIT_STEP;
                }
                Admit::Wait => anyhow::bail!(
                    "the previous task in this conversation is still finishing; try again"
                ),
            }
        };
        // Clears the running entry however this ends, panics and dropped futures included.
        let _guard = RunningGuard {
            engine: self,
            conversation,
        };

        loop {
            let (final_history, answer) = self
                .run(conversation, history, fresh, steer.clone(), progress)
                .await?;

            // Finish under the lock, so no message can slip in between the
            // last look at the steering queue and the transcript being saved.
            let mut running = self.running.lock();
            steer.close();
            let left = steer.take();
            let cursor = running.get(conversation).and_then(|r| r.cursor.clone());
            if left.is_empty() || answer.is_err() {
                // Messages accepted for a task that then failed are kept in the
                // transcript, so they are not lost with it.
                let mut final_history = final_history;
                final_history.extend(left.iter().map(Entry::user));
                self.save(
                    conversation,
                    &Saved {
                        cursor,
                        history: final_history,
                    },
                )?;
                running.remove(conversation);
                return answer.map(Reply::Answer);
            }
            // Messages arrived after the task's last step: answer them too,
            // in a follow-up run that starts from the finished transcript.
            self.save(
                conversation,
                &Saved {
                    cursor,
                    history: final_history.clone(),
                },
            )?;
            steer = Steer::new();
            if let Some(active) = running.get_mut(conversation) {
                active.steer = steer.clone();
            }
            drop(running);
            history = final_history;
            fresh = left.iter().map(Entry::user).collect();
        }
    }

    /// Decide what to do with a batch, atomically with respect to other batches
    /// for the same conversation. The transcript is read under the same lock
    /// it is saved under, so a batch never starts from a stale copy.
    fn admit(&self, conversation: &str, incoming: &[Incoming]) -> Admit {
        let mut running = self.running.lock();
        let saved = self.load(conversation);
        let seen = running
            .get(conversation)
            .map_or(saved.cursor.as_deref(), |r| r.cursor.as_deref());
        let fresh: Vec<&Incoming> = incoming
            .iter()
            .filter(|m| is_newer(m.cursor.as_deref(), seen))
            .collect();
        if fresh.is_empty() {
            return Admit::Nothing;
        }
        let newest = fresh
            .iter()
            .filter_map(|m| m.cursor.clone())
            .max_by(|a, b| compare_cursors(a, b));

        if let Some(active) = running.get_mut(conversation) {
            let text = fresh
                .iter()
                .filter(|m| matches!(m.entry, Entry::User(_)))
                .map(|m| m.entry.text())
                .collect::<Vec<_>>()
                .join("\n\n");
            if text.trim().is_empty() {
                return Admit::Nothing;
            }
            if !active.steer.send(&text) {
                return Admit::Wait;
            }
            if newest.is_some() {
                active.cursor = newest;
            }
            return Admit::Steered;
        }

        let steer = Steer::new();
        running.insert(
            conversation.to_string(),
            Running {
                steer: steer.clone(),
                cursor: newest.or_else(|| saved.cursor.clone()),
            },
        );
        Admit::Run(
            steer,
            saved.history,
            fresh.into_iter().map(|m| m.entry.clone()).collect(),
        )
    }

    /// Run one task. Returns the transcript to save and the answer (or the
    /// failure, after the transcript so far is kept).
    async fn run(
        &self,
        conversation: &str,
        mut history: Vec<Entry>,
        fresh: Vec<Entry>,
        steer: Steer,
        progress: &UnboundedSender<String>,
    ) -> Result<(Vec<Entry>, Result<String>)> {
        history.extend(fresh);
        if compact::estimate_tokens(&history) > self.compact_at_tokens {
            let _ = progress.send("Compacting the conversation".into());
            match compact::compact(
                self.provider.as_ref(),
                &self.config.model,
                history.clone(),
                self.compact_at_tokens,
            )
            .await
            {
                Ok(compacted) => history = compacted,
                // Carry on uncompacted: an oversized request may still fit,
                // and if not, the provider's error says so plainly.
                Err(e) => tracing::warn!("compaction failed: {e}"),
            }
        }

        let agent = Agent::new(
            self.provider.clone(),
            self.registry.clone(),
            self.gate.clone(),
            self.config.clone(),
        )
        .with_steering(steer);
        let turn = Turn {
            session: SessionId::new(),
            system: self.system_prompt().await,
            history: history.clone(),
            root: workspace_dir(&self.data_dir, conversation),
        };

        let mut events = agent.run(turn);
        let mut done = None;
        while let Some(event) = events.next().await {
            match event {
                AgentEvent::ToolStarted { call, title } => {
                    let _ = progress.send(describe_call(&call, title.as_deref()));
                }
                AgentEvent::Notice { message } => tracing::info!("{message}"),
                AgentEvent::Done {
                    stopped, history, ..
                } => done = Some((stopped, history)),
                _ => {}
            }
        }
        let (stopped, final_history) = done.context("the agent stopped without finishing")?;
        let answer = last_answer(&final_history);
        let answer = match stopped {
            StopReason::Complete | StopReason::Refused => Ok(answer),
            StopReason::MaxSteps => Ok(format!(
                "{answer}\n\n_I hit my step limit before finishing. Tell me to continue and I will pick up from here._"
            )),
            StopReason::Cancelled => Ok("_Stopped._".to_string()),
            StopReason::Error { message } => Err(anyhow::anyhow!(message)),
        };
        Ok((
            final_history,
            answer.map(|a| {
                if a.trim().is_empty() {
                    "Done.".into()
                } else {
                    a
                }
            }),
        ))
    }

    async fn system_prompt(&self) -> String {
        let pages = match self.wiki.pages() {
            Ok(p) if !p.is_empty() => p.join(", "),
            _ => "(empty)".into(),
        };
        let servers: Vec<String> = self.mcp.connected().into_iter().map(|r| r.name).collect();
        let computer = match &self.sandbox {
            Some(s) => format!(
                "You have your own sandboxed Linux computer (run_shell, image {}, network {}). Use it to run code, \
                 process data and check your work.",
                s.image, s.network
            ),
            None => "You have no shell in this deployment.".to_string(),
        };
        let integrations = if servers.is_empty() {
            "No MCP servers are connected.".to_string()
        } else {
            format!(
                "Connected MCP servers (their tools are named server_tool): {}.",
                servers.join(", ")
            )
        };
        let now = jiff::Zoned::now();
        format!(
            "{PROMPT}\n\n# This deployment\n\n- Now: {} ({}).\n- {computer}\n- {integrations}\n- Wiki pages: {pages}.",
            now.strftime("%A %Y-%m-%d %H:%M"),
            now.time_zone().iana_name().unwrap_or("local time"),
        )
    }

    fn path(&self, conversation: &str) -> PathBuf {
        self.data_dir
            .join("conversations")
            .join(format!("{}.json", crate::safe_id(conversation)))
    }

    /// The saved transcript, or an empty one. A file that cannot be parsed is
    /// moved aside rather than silently overwritten by the next save.
    fn load(&self, conversation: &str) -> Saved {
        let path = self.path(conversation);
        let Ok(bytes) = std::fs::read(&path) else {
            return Saved::default();
        };
        match serde_json::from_slice(&bytes) {
            Ok(saved) => saved,
            Err(e) => {
                let aside = path.with_extension(format!(
                    "corrupt-{}.json",
                    jiff::Timestamp::now().as_second()
                ));
                tracing::warn!(
                    "could not read {} ({e}); moved it to {} and started fresh",
                    path.display(),
                    aside.display()
                );
                let _ = std::fs::rename(&path, &aside);
                Saved::default()
            }
        }
    }

    /// Written to a temporary file and renamed, so a crash mid-write never
    /// leaves a half transcript behind.
    fn save(&self, conversation: &str, saved: &Saved) -> Result<()> {
        let path = self.path(conversation);
        let dir = path.parent().context("conversation path has no folder")?;
        std::fs::create_dir_all(dir)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(saved)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

enum Admit {
    /// Start a task: its steering queue, the saved transcript, and the new messages.
    Run(Steer, Vec<Entry>, Vec<Entry>),
    Steered,
    Wait,
    Nothing,
}

/// How long a batch waits for a finishing task before giving up.
const MAX_WAIT: Duration = Duration::from_secs(60);
const WAIT_STEP: Duration = Duration::from_millis(200);

/// Removes a conversation's running entry when dropped. Without it, a panic or
/// a dropped future would leave the conversation looking busy forever.
struct RunningGuard<'a> {
    engine: &'a Engine,
    conversation: &'a str,
}

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        if let Some(running) = self.engine.running.lock().remove(self.conversation) {
            running.steer.close();
        }
    }
}

/// The folder a conversation's sandboxed shell works in.
fn workspace_dir(data_dir: &Path, conversation: &str) -> PathBuf {
    data_dir
        .join("workspaces")
        .join(crate::safe_id(conversation))
}

/// "Using web_fetch: https://…" for the progress note. The detail is the
/// call's title if it has one, else its first string argument (a URL, a page,
/// a query), which reads well for MCP tools too.
fn describe_call(call: &ToolCall, title: Option<&str>) -> String {
    let args = call.parsed_arguments();
    let detail = title.map(str::to_string).or_else(|| {
        args.as_object()?
            .values()
            .find_map(|v| v.as_str().map(|s| crate::one_line(s, 80)))
    });
    match detail {
        Some(detail) if !detail.is_empty() => format!("Using {}: {detail}", call.name),
        _ => format!("Using {}", call.name),
    }
}

/// The text of the model's last reply since the last message from a person.
fn last_answer(history: &[Entry]) -> String {
    history
        .iter()
        .rev()
        .take_while(|e| !matches!(e, Entry::User(_)))
        .find_map(|e| match e {
            Entry::Assistant(a) => a.content.as_deref().filter(|c| !c.trim().is_empty()),
            _ => None,
        })
        .unwrap_or_default()
        .to_string()
}

/// Slack timestamps are `seconds.micros`; compare them numerically.
fn compare_cursors(a: &str, b: &str) -> Ordering {
    let parse = |s: &str| {
        let (secs, frac) = s.split_once('.').unwrap_or((s, "0"));
        (
            secs.parse::<u64>().unwrap_or(0),
            frac.parse::<u64>().unwrap_or(0),
        )
    };
    parse(a).cmp(&parse(b))
}

fn is_newer(cursor: Option<&str>, seen: Option<&str>) -> bool {
    match (cursor, seen) {
        (Some(c), Some(s)) => compare_cursors(c, s) == Ordering::Greater,
        // Messages without a position (the local chat) are always new.
        _ => true,
    }
}

/// Every tool call is allowed. The tools are safe by construction: the shell
/// runs in a sandbox, web fetches cannot reach private networks, and MCP
/// servers are ones the operator chose to connect.
// ponytail: allow-all gate; per-tool approval buttons in Slack are the upgrade path.
#[derive(Debug)]
struct AllowAll;

#[async_trait::async_trait]
impl PermissionGate for AllowAll {
    async fn ask(&self, request: &PermissionRequest) -> openslacktag_core::Result<Decision> {
        // The one thing refused: a fourth identical call in a row. Nobody is
        // watching to approve it, and repeating it again will not help.
        if request.key == DOOM_LOOP_KEY {
            return Ok(Decision::Deny);
        }
        Ok(Decision::Allow)
    }

    async fn verdict(&self, _key: &str, _target: &str) -> Action {
        Action::Allow
    }
}

const PROMPT: &str = "\
You are OpenSlackTag, an AI teammate that lives in the team's Slack. People tag you with tasks; you do them \
end to end with your tools and reply in the thread with the result.

# How to work

- Do the work that was asked, not a plan for it. If the request is clear enough to act on, act, then report.
- Check facts with your tools instead of guessing. If something fails, say so plainly with the error; never claim \
success you did not verify.
- Finish. If a task has four parts, do four. If one is blocked, do the rest and say which one and why.
- If a request is ambiguous and a wrong guess would be costly, ask one short question instead.
- People may add to a task while you work; their messages appear in the conversation. Take them into account.

# How to reply

- Lead with the answer. Short; a one-line answer is one line. No restating the question, no offers of more help.
- Cite sources: link every URL you relied on, and name the wiki pages you used.
- Format for Slack: *bold*, _italic_, `code`, ```code blocks```, bullet lists, <https://url|label> links. \
No Markdown headings or tables.
- Messages from people start with their id, like <@U123>: ... Mention people the same way.

# The team wiki is your long-term memory

- Before answering anything about the team, its people, processes, decisions or projects, search the wiki.
- When you learn something durable (a decision, a process, who owns what, how a recurring task is done, a \
correction), update the wiki: read the page, then write it back with the new fact merged in. Keep pages short and \
organised by topic, e.g. team/people, processes/deploys, decisions/2026.
- Never store secrets, passwords, tokens or sensitive personal data in the wiki.";

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn cursors_compare_numerically() {
        assert_eq!(
            compare_cursors("1700000000.000200", "1700000000.000100"),
            Ordering::Greater
        );
        assert_eq!(compare_cursors("999.9", "1000.1"), Ordering::Less);
        assert!(is_newer(Some("2.0"), Some("1.0")));
        assert!(!is_newer(Some("1.0"), Some("1.0")));
        assert!(is_newer(None, Some("5.0")));
        assert!(is_newer(Some("1.0"), None));
    }

    #[test]
    fn progress_notes_name_the_tool_and_its_target() {
        let call = |args: &str| ToolCall {
            id: "c".into(),
            name: "web_fetch".into(),
            arguments: args.into(),
        };
        assert_eq!(
            describe_call(&call(r#"{"url":"https://a.b"}"#), None),
            "Using web_fetch: https://a.b"
        );
        assert_eq!(describe_call(&call("{}"), None), "Using web_fetch");
        assert_eq!(describe_call(&call("not json"), None), "Using web_fetch");
        assert_eq!(
            describe_call(&call("{}"), Some("Title")),
            "Using web_fetch: Title"
        );
    }

    #[test]
    fn last_answer_skips_tool_call_turns() {
        let history = vec![
            Entry::user("q"),
            Entry::assistant("first"),
            Entry::user("q2"),
            Entry::assistant("second"),
            Entry::Assistant(openslacktag_core::message::AssistantEntry::default()),
        ];
        assert_eq!(last_answer(&history), "second");
        // An answer from an earlier exchange is never reused.
        assert_eq!(last_answer(&history[..3]), "");
        assert_eq!(last_answer(&[]), "");
    }
}
