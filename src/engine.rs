//! The engine turns "messages arrived in a conversation" into "a reply",
//! whatever the surface: Slack, the terminal chat, or a scheduled task.
//!
//! It keeps each conversation's full transcript (tool calls included) on disk,
//! compacts it when it outgrows the context budget, folds messages that arrive
//! mid-task into the running task instead of starting a second one, checks
//! every tool call against the permission rules, and can stop a task on request.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use openslacktag_agent::{Agent, AgentConfig, AgentEvent, Steer, StopReason, Turn};
use openslacktag_core::message::Entry;
use openslacktag_core::permission::Rule;
use openslacktag_core::provider::Usage;
use openslacktag_core::tool::{PermissionGate, Tool, ToolRegistry};
use openslacktag_core::{Provider, SessionId};
use openslacktag_mcp::provider::McpProvider;
use openslacktag_tools::Registry;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

use crate::compact;
use crate::delegate::DelegateTool;
use crate::permissions::{Approver, Gate, NobodyToAsk};
use crate::schedule::{Schedules, schedule_tools};
use crate::skills::Skills;
use crate::tools::{Sandbox, Wiki, builtin_tools};

/// A message for the engine, with the surface's position marker (Slack's
/// `ts`). Entries at or before the conversation's cursor were already seen and
/// are dropped, which makes duplicate deliveries harmless.
#[derive(Debug, Clone)]
pub struct Incoming {
    pub cursor: Option<String>,
    pub entry: Entry,
}

/// Progress while a task runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// What is happening right now, e.g. "Reading #general".
    Doing(String),
    /// A finished step, for the summary under the answer, e.g. "Read #general".
    Done(String),
}

/// Where a conversation's replies go, for tools that post or schedule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub channel: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
}

/// One request to the engine: the new messages plus everything the surface
/// knows about where they came from.
pub struct Request<'a> {
    pub conversation: &'a str,
    pub incoming: Vec<Incoming>,
    /// What is happening ("Reading #general") and what got done, as the work runs.
    pub progress: &'a UnboundedSender<Progress>,
    /// Who answers `ask` permission rules.
    pub approver: Arc<dyn Approver>,
    /// Where replies go; enables scheduling and posting tools.
    pub target: Option<Target>,
    /// Who asked, e.g. a Slack user id.
    pub requester: Option<String>,
    /// Extra lines for the system prompt, e.g. which channel this is.
    pub context: Option<String>,
    /// Tools that only make sense on this surface (reading Slack, sharing files).
    pub tools: Vec<Arc<dyn Tool>>,
}

impl std::fmt::Debug for Request<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("conversation", &self.conversation)
            .field("incoming", &self.incoming.len())
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
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
    /// Permission rules: defaults merged with the operator's file.
    pub rules: Vec<Rule>,
    /// Operator-written skills (folders with a SKILL.md).
    pub skills: Skills,
    /// The operator's standing instructions, added to every system prompt.
    pub instructions: Option<String>,
    /// Extra tools: web search when configured, and tools for tests.
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

impl Parts {
    /// Parts with no sandbox, MCP, skills or instructions, and the default rules.
    pub fn minimal(provider: Arc<dyn Provider>, model: &str, data_dir: &Path) -> Self {
        Self {
            provider,
            model: model.to_string(),
            data_dir: data_dir.to_path_buf(),
            sandbox: None,
            mcp: Arc::new(McpProvider::new()),
            compact_at_tokens: 80_000,
            rules: crate::permissions::default_rules(),
            skills: Skills::default(),
            instructions: None,
            extra_tools: Vec::new(),
        }
    }
}

pub struct Engine {
    provider: Arc<dyn Provider>,
    config: AgentConfig,
    wiki: Arc<Wiki>,
    mcp: Arc<McpProvider>,
    sandbox: Option<Sandbox>,
    data_dir: PathBuf,
    compact_at_tokens: usize,
    rules: Vec<Rule>,
    skills: Skills,
    instructions: Option<String>,
    /// Tools every conversation has: builtins, skills, and the extras.
    tools: Vec<Arc<dyn Tool>>,
    schedules: Arc<Schedules>,
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
    cancel: Arc<Notify>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    cursor: Option<String>,
    history: Vec<Entry>,
    /// Approvals given "always, in this conversation".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    allowed: Vec<Rule>,
}

impl Engine {
    pub fn new(parts: Parts) -> Result<Self> {
        let wiki = Arc::new(
            Wiki::open(parts.data_dir.join("wiki")).context("could not create the wiki folder")?,
        );
        let mut tools = builtin_tools(&wiki, parts.sandbox.as_ref());
        tools.extend(crate::files::file_tools());
        tools.extend(parts.skills.tools());
        tools.extend(parts.extra_tools);
        let schedules = Arc::new(Schedules::open(parts.data_dir.join("schedules.json")));
        Ok(Self {
            provider: parts.provider,
            config: AgentConfig {
                model: parts.model,
                ..AgentConfig::default()
            },
            wiki,
            mcp: parts.mcp,
            sandbox: parts.sandbox,
            data_dir: parts.data_dir,
            compact_at_tokens: parts.compact_at_tokens,
            rules: parts.rules,
            skills: parts.skills,
            instructions: parts.instructions,
            tools,
            schedules,
            running: Mutex::new(HashMap::new()),
        })
    }

    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.provider
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    pub fn schedules(&self) -> &Arc<Schedules> {
        &self.schedules
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Names of the tools the model is offered everywhere, MCP tools included
    /// and tools the rules deny outright left out.
    pub async fn tool_names(&self) -> Vec<String> {
        let gate = Arc::new(Gate::new(
            self.rules.clone(),
            Arc::default(),
            Arc::new(NobodyToAsk),
            None,
        ));
        self.registry(gate, Vec::new())
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

    /// Whether a task is running in this conversation.
    pub fn is_running(&self, conversation: &str) -> bool {
        self.running.lock().contains_key(conversation)
    }

    /// Stop the task running in this conversation. Returns false if none was.
    pub fn cancel(&self, conversation: &str) -> bool {
        match self.running.lock().get(conversation) {
            Some(running) => {
                running.cancel.notify_one();
                true
            }
            None => false,
        }
    }

    /// Take in new messages and answer them.
    pub async fn reply(&self, request: Request<'_>) -> Result<Reply> {
        let conversation = request.conversation;
        let mut waited = Duration::ZERO;
        let (mut steer, cancel, saved, mut fresh) = loop {
            match self.admit(conversation, &request.incoming) {
                Admit::Run {
                    steer,
                    cancel,
                    saved,
                    fresh,
                } => break (steer, cancel, saved, fresh),
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
        let session = Arc::new(Mutex::new(saved.allowed));
        let mut history = saved.history;

        loop {
            let outcome = self
                .run(&request, history, fresh, steer.clone(), &cancel, &session)
                .await?;
            if let Some(usage) = &outcome.usage {
                crate::usage::record(&self.data_dir, conversation, &self.config.model, usage);
            }

            // Finish under the lock, so no message can slip in between the
            // last look at the steering queue and the transcript being saved.
            let mut running = self.running.lock();
            steer.close();
            let left = steer.take();
            let cursor = running.get(conversation).and_then(|r| r.cursor.clone());
            let allowed = session.lock().clone();
            if left.is_empty() || outcome.answer.is_err() || outcome.cancelled {
                // Messages accepted for a task that then failed are kept in the
                // transcript, so they are not lost with it.
                let mut final_history = outcome.history;
                final_history.extend(left.iter().map(Entry::user));
                self.save(
                    conversation,
                    &Saved {
                        cursor,
                        history: final_history,
                        allowed,
                    },
                )?;
                running.remove(conversation);
                return outcome.answer.map(Reply::Answer);
            }
            // Messages arrived after the task's last step: answer them too,
            // in a follow-up run that starts from the finished transcript.
            self.save(
                conversation,
                &Saved {
                    cursor,
                    history: outcome.history.clone(),
                    allowed,
                },
            )?;
            steer = Steer::new();
            if let Some(active) = running.get_mut(conversation) {
                active.steer = steer.clone();
            }
            drop(running);
            history = outcome.history;
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
        let cancel = Arc::new(Notify::new());
        running.insert(
            conversation.to_string(),
            Running {
                steer: steer.clone(),
                cursor: newest.or_else(|| saved.cursor.clone()),
                cancel: cancel.clone(),
            },
        );
        Admit::Run {
            steer,
            cancel,
            fresh: fresh.into_iter().map(|m| m.entry.clone()).collect(),
            saved,
        }
    }

    /// The registry for one task: every tool, checked by this task's gate.
    fn registry(
        &self,
        gate: Arc<dyn PermissionGate>,
        extra: Vec<Arc<dyn Tool>>,
    ) -> Arc<dyn ToolRegistry> {
        Arc::new(
            Registry::new(gate)
                .with_tools(self.tools.iter().cloned())
                .with_tools(extra)
                .with_provider(self.mcp.clone()),
        )
    }

    /// Run one task.
    async fn run(
        &self,
        request: &Request<'_>,
        mut history: Vec<Entry>,
        fresh: Vec<Entry>,
        steer: Steer,
        cancel: &Notify,
        session: &Arc<Mutex<Vec<Rule>>>,
    ) -> Result<Outcome> {
        history.extend(fresh);
        if compact::estimate_tokens(&history) > self.compact_at_tokens {
            let _ = request
                .progress
                .send(Progress::Doing("Compacting the conversation".into()));
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

        let gate: Arc<dyn PermissionGate> = Arc::new(Gate::new(
            self.rules.clone(),
            session.clone(),
            request.approver.clone(),
            Some(request.progress.clone()),
        ));
        let mut extra = request.tools.clone();
        if let Some(target) = &request.target {
            extra.extend(schedule_tools(
                &self.schedules,
                target,
                request.requester.as_deref(),
            ));
        }
        // Helpers get every tool except `delegate` itself, so delegation cannot recurse.
        let helpers = self.registry(gate.clone(), extra.clone());
        let system = self.system_prompt(request).await;
        extra.push(Arc::new(DelegateTool::new(
            self.provider.clone(),
            helpers,
            gate.clone(),
            self.config.clone(),
            &system,
        )));
        let registry = self.registry(gate.clone(), extra);

        let agent = Agent::new(self.provider.clone(), registry, gate, self.config.clone())
            .with_steering(steer);
        let turn = Turn {
            session: SessionId::new(),
            system,
            history: history.clone(),
            root: workspace_dir(&self.data_dir, request.conversation),
        };

        let mut events = agent.run(turn);
        let mut done = None;
        // Each call's finished label and arguments, until its result comes back.
        let mut finishing: HashMap<String, (String, String)> = HashMap::new();
        let mut cancelled = false;
        loop {
            tokio::select! {
                event = events.next() => {
                    let Some(event) = event else { break };
                    match event {
                        AgentEvent::ToolStarted { call, .. } => {
                            let step = crate::steps::label(&call.name, &call.parsed_arguments());
                            let _ = request.progress.send(Progress::Doing(step.doing));
                            finishing.insert(call.id.to_string(), (step.done, call.arguments.clone()));
                        }
                        AgentEvent::ToolFinished { result } => {
                            if let Some((done, args)) = finishing.remove(&result.call_id.to_string()) {
                                let outcome = if result.was_denied() {
                                    "denied"
                                } else if result.ok {
                                    "ok"
                                } else {
                                    "failed"
                                };
                                crate::audit::record(&self.data_dir, &crate::audit::Line {
                                    at: jiff::Timestamp::now().to_string(),
                                    conversation: request.conversation.to_string(),
                                    requester: request.requester.clone(),
                                    tool: result.tool.clone(),
                                    args,
                                    outcome: outcome.into(),
                                });
                                let done = match outcome {
                                    "denied" => format!("{done} (not approved)"),
                                    "failed" => format!("{done} (failed)"),
                                    _ => done,
                                };
                                let _ = request.progress.send(Progress::Done(done));
                            }
                        }
                        AgentEvent::Notice { message } => tracing::info!("{message}"),
                        AgentEvent::Done { stopped, history, usage } => done = Some((stopped, history, usage)),
                        _ => {}
                    }
                }
                () = cancel.notified() => {
                    // Dropping the stream stops the model request and any running tool.
                    cancelled = true;
                    break;
                }
            }
        }
        drop(events);

        if cancelled {
            let mut history = history;
            history.push(Entry::assistant(
                "(Stopped at the person's request before finishing.)",
            ));
            return Ok(Outcome {
                history,
                answer: Ok("_Stopped._".into()),
                usage: None,
                cancelled: true,
            });
        }
        let (stopped, final_history, usage) =
            done.context("the agent stopped without finishing")?;
        let answer = last_answer(&final_history);
        let answer = match stopped {
            StopReason::Complete => Ok(answer),
            // Every call in the last step was refused: say so plainly rather
            // than spend another model call. The refusal stays in the
            // transcript, so the next message picks up from it.
            StopReason::Refused if answer.trim().is_empty() => Ok(REFUSED.to_string()),
            StopReason::Refused => Ok(answer),
            StopReason::MaxSteps => Ok(format!(
                "{answer}\n\n_I hit my step limit before finishing. Tell me to continue and I will pick up from here._"
            )),
            StopReason::Cancelled => Ok("_Stopped._".to_string()),
            StopReason::Error { message } => Err(anyhow::anyhow!(message)),
        };
        Ok(Outcome {
            history: final_history,
            answer: answer.map(|a| {
                if a.trim().is_empty() {
                    "Done.".into()
                } else {
                    a
                }
            }),
            usage,
            cancelled: false,
        })
    }

    async fn system_prompt(&self, request: &Request<'_>) -> String {
        let pages = match self.wiki.pages() {
            Ok(p) if !p.is_empty() => p.join(", "),
            _ => "(empty)".into(),
        };
        let servers: Vec<String> = self.mcp.connected().into_iter().map(|r| r.name).collect();
        let computer = match &self.sandbox {
            Some(s) => format!(
                "You have your own sandboxed Linux computer (run_shell, image {}, network {}). Use it to run code, \
                 process data and check your work. Its /work folder is the same folder the file tools use.",
                s.image, s.network
            ),
            None => "You have no shell in this deployment, but the file tools work.".to_string(),
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
        let mut prompt = format!(
            "{PROMPT}\n\n# This deployment\n\n- Now: {} ({}).\n- {computer}\n- {integrations}\n- Wiki pages: {pages}.",
            now.strftime("%A %Y-%m-%d %H:%M"),
            now.time_zone().iana_name().unwrap_or("local time"),
        );
        if let Some(listing) = self.skills.listing() {
            prompt.push_str(&format!("\n\n# Skills\n\nRead a skill with skill_read before doing a task it covers.\n\n{listing}"));
        }
        if let Some(context) = &request.context {
            prompt.push_str(&format!("\n\n# This conversation\n\n{context}"));
        }
        // Standing instructions for this channel, kept in the wiki so the
        // teammate (or anyone) can change them by asking.
        if let Some(target) = &request.target
            && let Ok(page) = self.wiki.read(&format!("channels/{}", target.channel))
        {
            prompt.push_str(&format!(
                "\n\n# Instructions for this channel (wiki page channels/{})\n\n{page}",
                target.channel
            ));
        }
        if let Some(instructions) = &self.instructions {
            prompt.push_str(&format!(
                "\n\n# Instructions from the people who run you\n\n{instructions}"
            ));
        }
        prompt
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
        crate::write_atomic(&path, &serde_json::to_vec(saved)?)
    }
}

struct Outcome {
    history: Vec<Entry>,
    answer: Result<String>,
    usage: Option<Usage>,
    cancelled: bool,
}

enum Admit {
    /// Start a task with the saved transcript and the new messages.
    Run {
        steer: Steer,
        cancel: Arc<Notify>,
        saved: Saved,
        fresh: Vec<Entry>,
    },
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

/// The folder a conversation's files and sandboxed shell work in.
pub fn workspace_dir(data_dir: &Path, conversation: &str) -> PathBuf {
    data_dir
        .join("workspaces")
        .join(crate::safe_id(conversation))
}

/// The text of the model's last reply since the last message from a person.
pub fn last_answer(history: &[Entry]) -> String {
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

const REFUSED: &str = "I stopped because a step I needed wasn't approved. Tell me how you'd like to go on, \
or approve it next time I ask.";

const PROMPT: &str = "\
You are OpenSlackTag, an AI teammate that lives in the team's Slack. People tag you with tasks; you do them \
end to end with your tools and reply in the thread with the result.

# How to work

- Do the work that was asked, not a plan for it. If the request is clear enough to act on, act, then report.
- Check facts with your tools instead of guessing. Search the web when you need current information. If something \
fails, say so plainly with the error; never claim success you did not verify.
- Finish. If a task has four parts, do four. If one is blocked, do the rest and say which one and why.
- Big jobs with independent parts (research several things, review several files) go faster with delegate, which \
runs helpers in parallel.
- If a request is ambiguous and a wrong guess would be costly, ask one short question instead.
- People may add to a task while you work; their messages appear in the conversation. Take them into account.
- Some tools need a person's approval. If one is refused, do not retry it; say what you could not do.

# How to reply

- Lead with the answer. Short; a one-line answer is one line. No restating the question, no offers of more help.
- Cite sources: link every URL you relied on, and name the wiki pages you used.
- Format for Slack: *bold*, _italic_, `code`, ```code blocks```, bullet lists, <https://url|label> links. \
No Markdown headings or tables.
- Messages from people start with who wrote them, like <@U123> (Ana): ... Mention people as <@U123>.
- To give someone a file (a report, a CSV, a chart), write it to your workspace and use share_file.

# Memory

- The team wiki is your long-term memory. Before answering anything about the team, its people, processes, \
decisions or projects, search it.
- When you learn something durable (a decision, a process, who owns what, how a recurring task is done, a \
correction), update the wiki: read the page, then write it back with the new fact merged in. Keep pages short and \
organised by topic, e.g. team/people, processes/deploys, decisions/2026.
- If someone asks you to always behave a certain way in a channel, write it to the wiki page channels/<channel id>; \
it is added to your instructions there.
- Never store secrets, passwords, tokens or sensitive personal data in the wiki.

# Automating

- When a request is clearly recurring (a daily summary, a weekly report, a check every morning), offer to schedule \
it, and use schedule_task when they agree.";

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
