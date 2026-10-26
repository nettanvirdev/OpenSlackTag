//! Slack over Socket Mode: the bot opens an outbound WebSocket, so it needs no
//! public URL and runs fine on a laptop.
//!
//! Besides answering mentions and DMs, this runs scheduled tasks, posts
//! approval cards for tools that need a person's yes, and turns files people
//! share into files (and images) the teammate can work with.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use openslacktag_core::message::{Entry, ImageUrl, Part, UserEntry};
use openslacktag_core::tool::Decision;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::engine::{Engine, Incoming, Progress, Reply, Request, Target, workspace_dir};
use crate::markdown::to_mrkdwn;
use crate::permissions::{Approval, Approver};
use crate::schedule::{Schedule, Watch};
use crate::slack_tools::slack_tools;

const WORKING: &str = "_Working on it…_";
/// What a channel trigger answers when a message needs nothing from it.
const SKIP: &str = "SKIP";
const STEERED: &str = "_Got it. I'm adding that to what I'm working on._";
/// Slack takes longer messages, but splits anything past this badly.
const MAX_MESSAGE_CHARS: usize = 3800;
/// At most one progress edit per interval; only the latest note is shown.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);
/// We ping this often, and treat this much silence as a dead connection
/// (a laptop that slept, a NAT that forgot us).
const PING_INTERVAL: Duration = Duration::from_secs(30);
const SILENCE_LIMIT: Duration = Duration::from_secs(90);
/// Pages of thread history fetched (200 messages each).
const MAX_HISTORY_PAGES: usize = 10;
/// Message subtypes that are still a person talking.
const PERSON_SUBTYPES: &[&str] = &["file_share", "thread_broadcast"];
/// An unanswered approval card is treated as a no after this long.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// How often due scheduled tasks are looked for.
const SCHEDULE_TICK: Duration = Duration::from_secs(30);
/// When automation suggestions are posted, if a channel is set.
const SUGGEST_WHEN: &str = "every monday at 09:00";
/// Files people share: how many per message, and how big, get downloaded.
const MAX_FILES_PER_MESSAGE: usize = 10;
const MAX_FILE_BYTES: u64 = 25_000_000;
/// Images up to this size are also shown to the model directly.
const MAX_IMAGE_BYTES: u64 = 5_000_000;
/// Saying one of these (and only that) to a busy teammate stops its task.
/// Answer text per Block Kit section (Slack's limit is 3000).
const MAX_SECTION_CHARS: usize = 2900;
/// Longer answers go out as plain messages instead (a message holds 50 blocks).
const MAX_ANSWER_SECTIONS: usize = 40;
/// Step lists kept for "Show steps" clicks.
const MAX_REMEMBERED_STEPS: usize = 2000;
const STOP_WORDS: &[&str] = &[
    "stop",
    "cancel",
    "abort",
    "stop it",
    "never mind",
    "nevermind",
];

pub struct Slack {
    http: reqwest::Client,
    api_url: String,
    bot_token: String,
    app_token: String,
    bot_user_id: String,
    team_id: String,
    allowed_users: Vec<String>,
    approvers: Vec<String>,
    seen: Mutex<RecentIds>,
    /// Approval cards waiting for a click, by id.
    pending: Mutex<HashMap<String, PendingApproval>>,
    /// Display names by user id.
    names: Mutex<HashMap<String, String>>,
    /// Channel ids by name, for tools given `#general`.
    channel_ids: Mutex<HashMap<String, String>>,
    /// The steps behind recent answers, for "Show steps".
    steps: Mutex<HashMap<String, Vec<String>>>,
    /// Where 👍 / 👎 clicks are recorded.
    feedback_file: Mutex<Option<std::path::PathBuf>>,
}

/// What a task produced, for the message under the answer.
struct TaskOutcome {
    reply: Result<Reply>,
    steps: Vec<String>,
    seconds: u64,
    model: String,
}

struct PendingApproval {
    answer: oneshot::Sender<(Decision, String)>,
    requester: Option<String>,
    channel: String,
}

impl std::fmt::Debug for Slack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slack")
            .field("api_url", &self.api_url)
            .field("bot_user_id", &self.bot_user_id)
            .field("team_id", &self.team_id)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    envelope_id: Option<String>,
    payload: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    user: Option<String>,
    bot_id: Option<String>,
    subtype: Option<String>,
    #[serde(default)]
    text: String,
    ts: String,
    channel: String,
    thread_ts: Option<String>,
    channel_type: Option<String>,
    /// The sender's own workspace, which differs for Slack Connect guests.
    user_team: Option<String>,
    #[serde(default)]
    files: Vec<Value>,
}

impl Slack {
    /// Check the bot token and learn the bot's own user id and workspace.
    pub async fn connect(api_url: &str, bot_token: &str, app_token: &str) -> Result<Self> {
        let mut slack = Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()?,
            api_url: api_url.trim_end_matches('/').to_string(),
            bot_token: bot_token.to_string(),
            app_token: app_token.to_string(),
            bot_user_id: String::new(),
            team_id: String::new(),
            allowed_users: Vec::new(),
            approvers: Vec::new(),
            seen: Mutex::new(RecentIds::new(2000)),
            pending: Mutex::default(),
            names: Mutex::default(),
            channel_ids: Mutex::default(),
            steps: Mutex::default(),
            feedback_file: Mutex::default(),
        };
        let auth = slack
            .call("auth.test", &slack.bot_token, &[])
            .await
            .context("SLACK_BOT_TOKEN was rejected")?;
        slack.bot_user_id = auth["user_id"]
            .as_str()
            .context("auth.test returned no user_id")?
            .to_string();
        slack.team_id = auth["team_id"].as_str().unwrap_or_default().to_string();
        Ok(slack)
    }

    /// Who may use the teammate (empty: everyone in the workspace) and who may
    /// approve tool calls (empty: whoever asked for the task).
    pub fn with_access(mut self, allowed_users: Vec<String>, approvers: Vec<String>) -> Self {
        self.allowed_users = allowed_users;
        self.approvers = approvers;
        self
    }

    pub fn bot_user_id(&self) -> &str {
        &self.bot_user_id
    }

    /// Ask Slack for a Socket Mode URL. Also used by `doctor` to check the app token.
    pub async fn socket_url(&self) -> Result<String> {
        let resp = self
            .call("apps.connections.open", &self.app_token, &[])
            .await
            .context("SLACK_APP_TOKEN was rejected (it needs the connections:write scope)")?;
        Ok(resp["url"]
            .as_str()
            .context("apps.connections.open returned no url")?
            .to_string())
    }

    /// Serve until the tokens stop working: answer messages, run scheduled
    /// tasks, and (with a channel set) post weekly automation suggestions.
    pub async fn run(
        self: Arc<Self>,
        engine: Arc<Engine>,
        suggestions_channel: Option<String>,
    ) -> Result<()> {
        *self.feedback_file.lock() = Some(engine.data_dir().join("feedback.jsonl"));
        let scheduler = tokio::spawn(self.clone().run_schedules(engine.clone()));
        let suggester = suggestions_channel
            .map(|channel| tokio::spawn(self.clone().run_suggestions(engine.clone(), channel)));
        let result = self.serve(engine).await;
        scheduler.abort();
        if let Some(task) = suggester {
            task.abort();
        }
        result
    }

    async fn serve(self: Arc<Self>, engine: Arc<Engine>) -> Result<()> {
        let mut backoff = 1;
        loop {
            match self.clone().session(engine.clone()).await {
                Ok(()) => backoff = 1,
                Err(e) if is_auth_error(&e) => return Err(e),
                Err(e) => {
                    tracing::warn!("Slack connection lost: {e:#}. Reconnecting in {backoff}s.");
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(60);
                }
            }
        }
    }

    async fn session(self: Arc<Self>, engine: Arc<Engine>) -> Result<()> {
        let url = self.socket_url().await?;
        let (ws, _) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .context("could not open the Socket Mode WebSocket")?;
        let (mut sink, mut stream) = ws.split();
        tracing::info!(
            "Connected to Slack as <@{}>. Mention me in a channel or send me a DM.",
            self.bot_user_id
        );

        let mut ping = tokio::time::interval(PING_INTERVAL);
        let mut last_heard = Instant::now();
        loop {
            tokio::select! {
                frame = stream.next() => {
                    let Some(frame) = frame else { return Ok(()) };
                    last_heard = Instant::now();
                    let text = match frame? {
                        WsMessage::Text(t) => t,
                        WsMessage::Close(_) => return Ok(()),
                        _ => continue,
                    };
                    let envelope: Envelope = match serde_json::from_str(&text) {
                        Ok(e) => e,
                        Err(e) => {
                            tracing::warn!("Ignoring an unreadable Socket Mode frame: {e}");
                            continue;
                        }
                    };
                    // Ack first: Slack retries anything not acknowledged within 3 seconds.
                    if let Some(id) = &envelope.envelope_id {
                        sink.send(WsMessage::text(json!({ "envelope_id": id }).to_string()))
                            .await?;
                    }
                    match envelope.kind.as_str() {
                        "disconnect" => return Ok(()),
                        "events_api" => self.dispatch(envelope.payload.as_ref(), &engine),
                        "interactive" => {
                            if let Some(payload) = &envelope.payload {
                                self.on_interaction(payload);
                            }
                        }
                        _ => {}
                    }
                }
                _ = ping.tick() => {
                    if last_heard.elapsed() > SILENCE_LIMIT {
                        bail!("no data from Slack for {} seconds", SILENCE_LIMIT.as_secs());
                    }
                    sink.send(WsMessage::Ping(Default::default())).await?;
                }
            }
        }
    }

    fn dispatch(self: &Arc<Self>, payload: Option<&Value>, engine: &Arc<Engine>) {
        let Some(payload) = payload else {
            return;
        };
        if let Some(event) = self.accept(payload) {
            let (slack, engine) = (self.clone(), engine.clone());
            tokio::spawn(async move {
                if let Err(e) = slack.handle(&engine, &event).await {
                    tracing::error!("Failed to answer message {}: {e:#}", event.ts);
                }
            });
            return;
        }
        let Some(event) = self.channel_message(payload) else {
            return;
        };
        for watch in engine.schedules().triggered(&event.channel, &event.text) {
            let (slack, engine, event) = (self.clone(), engine.clone(), event.clone());
            tokio::spawn(async move {
                if let Err(e) = slack.run_trigger(&engine, &watch, &event).await {
                    tracing::error!("Trigger {} failed: {e:#}", watch.id);
                }
            });
        }
    }

    /// A new top-level message from a person in a channel, which channel
    /// triggers may want. Messages that mention the bot are left to the
    /// mention handler.
    fn channel_message(&self, payload: &Value) -> Option<Event> {
        let event: Event = serde_json::from_value(payload.get("event")?.clone()).ok()?;
        let user = event.user.as_deref()?;
        let top_level = event.thread_ts.as_deref().is_none_or(|t| t == event.ts);
        let person = event.bot_id.is_none()
            && event
                .subtype
                .as_deref()
                .is_none_or(|s| PERSON_SUBTYPES.contains(&s))
            && user != self.bot_user_id;
        let in_channel = event.kind == "message"
            && matches!(event.channel_type.as_deref(), Some("channel" | "group"));
        let mentions_us = event.text.contains(&format!("<@{}", self.bot_user_id));
        let our_workspace = event
            .user_team
            .as_deref()
            .is_none_or(|team| self.team_id.is_empty() || team == self.team_id);
        if !(top_level
            && person
            && in_channel
            && !mentions_us
            && our_workspace
            && self.is_allowed(user))
        {
            return None;
        }
        let key = payload["event_id"]
            .as_str()
            .map_or_else(|| format!("{}:{}", event.channel, event.ts), str::to_string);
        self.seen.lock().insert(key).then_some(event)
    }

    /// Run a channel trigger on the message that set it off, in that
    /// message's thread. A trigger that decides the message needs nothing
    /// answers SKIP, and nothing is posted.
    async fn run_trigger(
        self: &Arc<Self>,
        engine: &Engine,
        watch: &Watch,
        event: &Event,
    ) -> Result<()> {
        let conversation = format!("{}-{}", event.channel, event.ts);
        let workspace = workspace_dir(engine.data_dir(), &conversation);
        let incoming = self
            .incoming(event, Some(&event.ts), false, None, &workspace)
            .await?;
        let placeholder = self.post(&event.channel, Some(&event.ts), WORKING).await?;
        let by = watch
            .created_by
            .as_deref()
            .map_or("someone".to_string(), |u| format!("<@{u}>"));
        let context = format!(
            "Routine `{id}`, set up by {by}, runs on new messages in <#{channel}>. Its instructions: \"{prompt}\". \
             The message above set it off. Do what the routine says for this message and reply in its thread. If \
             the message needs nothing from you, reply with exactly {SKIP}.",
            id = watch.id,
            channel = event.channel,
            prompt = watch.prompt,
        );
        let target = Target {
            channel: event.channel.clone(),
            thread: Some(event.ts.clone()),
        };
        let outcome = self
            .run_task(
                engine,
                &conversation,
                incoming,
                target.clone(),
                watch.created_by.clone(),
                context,
                &placeholder,
            )
            .await;
        if matches!(&outcome.reply, Ok(Reply::Answer(text)) if text.trim() == SKIP) {
            return self.delete(&event.channel, &placeholder).await;
        }
        self.finish(&target, &placeholder, outcome).await
    }

    /// Keep mentions and DMs from people in this workspace (and on the
    /// allowlist, if there is one). Drop bots, edits, joins, guests from other
    /// organisations, and duplicates.
    fn accept(&self, payload: &Value) -> Option<Event> {
        let event_id = payload["event_id"].as_str().map(str::to_string);
        let event: Event = serde_json::from_value(payload.get("event")?.clone()).ok()?;
        let user = event.user.as_deref()?;
        let from_person = event.bot_id.is_none()
            && event
                .subtype
                .as_deref()
                .is_none_or(|s| PERSON_SUBTYPES.contains(&s))
            && user != self.bot_user_id;
        let wanted = event.kind == "app_mention"
            || (event.kind == "message" && event.channel_type.as_deref() == Some("im"));
        let our_workspace = event
            .user_team
            .as_deref()
            .is_none_or(|team| self.team_id.is_empty() || team == self.team_id);
        if !(from_person && wanted && our_workspace && self.is_allowed(user)) {
            return None;
        }
        let key = event_id.unwrap_or_else(|| format!("{}:{}", event.channel, event.ts));
        self.seen.lock().insert(key).then_some(event)
    }

    fn is_allowed(&self, user: &str) -> bool {
        self.allowed_users.is_empty() || self.allowed_users.iter().any(|u| u == user)
    }

    async fn handle(self: &Arc<Self>, engine: &Engine, event: &Event) -> Result<()> {
        // Channel mentions are answered in a thread; DMs only thread if the person started one.
        let thread = if event.kind == "app_mention" {
            Some(event.thread_ts.clone().unwrap_or_else(|| event.ts.clone()))
        } else {
            event.thread_ts.clone()
        };
        let conversation = match &thread {
            Some(ts) => format!("{}-{ts}", event.channel),
            None => event.channel.clone(),
        };
        let requester = event.user.clone().unwrap_or_default();

        // "stop" while a task runs stops it, rather than becoming part of it.
        let said = strip_mention(&event.text, &self.bot_user_id).to_lowercase();
        if STOP_WORDS.contains(&said.trim_end_matches(['.', '!']).trim())
            && engine.cancel(&conversation)
        {
            let _ = self
                .react(&event.channel, &event.ts, "octagonal_sign")
                .await;
            return Ok(());
        }

        let known = engine.knows(&conversation);
        let since = if known {
            engine.cursor(&conversation)
        } else {
            None
        };
        let workspace = workspace_dir(engine.data_dir(), &conversation);
        let incoming = self
            .incoming(
                event,
                thread.as_deref(),
                known,
                since.as_deref(),
                &workspace,
            )
            .await?;
        let placeholder = self
            .post(&event.channel, thread.as_deref(), WORKING)
            .await?;

        let target = Target {
            channel: event.channel.clone(),
            thread: thread.clone(),
        };
        let name = self.user_name(&requester).await;
        let context = format!(
            "You are answering in Slack, in <#{channel}>{place}. The person asking is <@{requester}> ({name}). \
             Files shared in this conversation are saved in your workspace under inbox/.",
            channel = event.channel,
            place = if thread.is_some() {
                ", in a thread"
            } else {
                ""
            },
        );
        let outcome = self
            .run_task(
                engine,
                &conversation,
                incoming,
                target.clone(),
                Some(requester),
                context,
                &placeholder,
            )
            .await;
        self.finish(&target, &placeholder, outcome).await
    }

    /// Run one task with live progress in `placeholder`.
    #[allow(clippy::too_many_arguments)]
    async fn run_task(
        self: &Arc<Self>,
        engine: &Engine,
        conversation: &str,
        incoming: Vec<Incoming>,
        target: Target,
        requester: Option<String>,
        context: String,
        placeholder: &str,
    ) -> TaskOutcome {
        let started = Instant::now();
        let (progress, mut events) = mpsc::unbounded_channel::<Progress>();
        let steps = Arc::new(Mutex::new(Vec::<String>::new()));
        let updater = {
            let (slack, channel, ts, steps) = (
                self.clone(),
                target.channel.clone(),
                placeholder.to_string(),
                steps.clone(),
            );
            tokio::spawn(async move {
                while let Some(first) = events.recv().await {
                    // Show only the latest activity; older notes are already stale.
                    let mut doing = None;
                    let mut next = Some(first);
                    while let Some(event) = next {
                        match event {
                            Progress::Doing(note) => doing = Some(note),
                            Progress::Done(step) => steps.lock().push(step),
                        }
                        next = events.try_recv().ok();
                    }
                    if let Some(note) = doing {
                        let text = format!("_{}…_", crate::one_line(&note, 150));
                        let _ = slack.update(&channel, &ts, &text).await;
                        tokio::time::sleep(PROGRESS_INTERVAL).await;
                    }
                }
            })
        };
        let approver = Arc::new(SlackApprover {
            slack: self.clone(),
            channel: target.channel.clone(),
            thread: target.thread.clone(),
            requester: requester.clone(),
        });
        let tools = slack_tools(self, &target, requester.as_deref());
        let reply = engine
            .reply(Request {
                conversation,
                incoming,
                progress: &progress,
                approver,
                target: Some(target),
                requester,
                context: Some(context),
                tools,
            })
            .await;
        drop(progress);
        let _ = updater.await;
        let steps = steps.lock().clone();
        TaskOutcome {
            reply,
            steps,
            seconds: started.elapsed().as_secs(),
            model: engine.model().to_string(),
        }
    }

    async fn finish(&self, target: &Target, placeholder: &str, outcome: TaskOutcome) -> Result<()> {
        let (channel, thread) = (target.channel.as_str(), target.thread.as_deref());
        match outcome.reply {
            Ok(Reply::Answer(text)) => {
                let footer = format!("{} · {}s", outcome.model, outcome.seconds.max(1));
                self.deliver_answer(
                    channel,
                    thread,
                    placeholder,
                    &to_mrkdwn(&text),
                    &outcome.steps,
                    &footer,
                )
                .await
            }
            Ok(Reply::Steered) => self.deliver(channel, thread, placeholder, STEERED).await,
            Ok(Reply::Nothing) => self.delete(channel, placeholder).await,
            Err(e) => {
                let text = format!(":warning: Something went wrong: {e:#}");
                self.deliver(channel, thread, placeholder, &to_mrkdwn(&text))
                    .await
            }
        }
    }

    /// The answer, with what was done to get it, which model did it, and
    /// feedback buttons, as one message (Block Kit). Answers too long for one
    /// message fall back to plain text split across several.
    async fn deliver_answer(
        &self,
        channel: &str,
        thread: Option<&str>,
        placeholder: &str,
        text: &str,
        steps: &[String],
        footer: &str,
    ) -> Result<()> {
        let sections = split_message(text, MAX_SECTION_CHARS);
        if sections.len() > MAX_ANSWER_SECTIONS {
            return self.deliver(channel, thread, placeholder, text).await;
        }
        let key = uuid::Uuid::new_v4().simple().to_string();
        let mut blocks: Vec<Value> = sections
            .iter()
            .map(|section| json!({"type": "section", "text": {"type": "mrkdwn", "text": section}}))
            .collect();
        let mut context = Vec::new();
        if !steps.is_empty() {
            let summary = format!(":white_check_mark: {}", crate::steps::summarize(steps));
            context.push(json!({"type": "mrkdwn", "text": short(&summary, 1900)}));
            self.remember_steps(&key, steps);
        }
        context.push(json!({"type": "mrkdwn", "text": crate::markdown::escape(footer)}));
        blocks.push(json!({"type": "context", "elements": context}));
        let button = |label: &str, action: &str| {
            json!({
                "type": "button",
                "text": {"type": "plain_text", "text": label, "emoji": true},
                "action_id": action,
                "value": key,
            })
        };
        let mut buttons = vec![button(":+1:", "ost_good"), button(":-1:", "ost_bad")];
        if steps.len() > 1 {
            buttons.push(button("Show steps", "ost_steps"));
        }
        blocks.push(json!({"type": "actions", "elements": buttons}));
        let blocks = Value::Array(blocks).to_string();
        let fallback = short(text, MAX_SECTION_CHARS);
        let form = [
            ("channel", channel),
            ("ts", placeholder),
            ("text", fallback.as_str()),
            ("blocks", blocks.as_str()),
        ];
        if let Err(e) = self.call("chat.update", &self.bot_token, &form).await {
            tracing::warn!(
                "Could not post the formatted answer ({e:#}); sending it as plain text."
            );
            return self.deliver(channel, thread, placeholder, text).await;
        }
        Ok(())
    }

    fn remember_steps(&self, key: &str, steps: &[String]) {
        let mut store = self.steps.lock();
        if store.len() >= MAX_REMEMBERED_STEPS {
            store.clear();
        }
        store.insert(key.to_string(), steps.to_vec());
    }

    /// Look for due scheduled tasks every so often, forever.
    async fn run_schedules(self: Arc<Self>, engine: Arc<Engine>) {
        let mut tick = tokio::time::interval(SCHEDULE_TICK);
        loop {
            tick.tick().await;
            let (slack, engine) = (self.clone(), engine.clone());
            tokio::spawn(async move { slack.run_due_schedules(&engine).await });
        }
    }

    /// Run every scheduled task that is due, in parallel; returns how many ran.
    pub async fn run_due_schedules(self: &Arc<Self>, engine: &Arc<Engine>) -> usize {
        let due = engine.schedules().take_due();
        let runs = due.iter().map(|schedule| async move {
            if let Err(e) = self.run_scheduled(engine, schedule).await {
                tracing::error!("Scheduled task {} failed: {e:#}", schedule.id);
            }
        });
        futures::future::join_all(runs).await;
        due.len()
    }

    async fn run_scheduled(self: &Arc<Self>, engine: &Engine, schedule: &Schedule) -> Result<()> {
        let target = schedule.target.clone();
        let placeholder = self
            .post(
                &target.channel,
                target.thread.as_deref(),
                &format!("_Running scheduled task `{}`…_", schedule.id),
            )
            .await?;
        let by = schedule
            .created_by
            .as_deref()
            .map_or("someone".to_string(), |u| format!("<@{u}>"));
        let context = format!(
            "This is scheduled task `{id}` ({when}), set up by {by}, posting in <#{channel}>. Nobody is waiting \
             in a conversation: do the task and reply with the result. If it no longer makes sense, say so and \
             suggest cancelling it with schedule_cancel.",
            id = schedule.id,
            when = schedule.when,
            channel = target.channel,
        );
        let incoming = vec![Incoming {
            cursor: None,
            entry: Entry::user(format!("Scheduled task: {}", schedule.prompt)),
        }];
        // Each schedule keeps its own transcript, so a run can compare with the last one.
        let conversation = format!("schedule-{}", schedule.id);
        let outcome = self
            .run_task(
                engine,
                &conversation,
                incoming,
                target.clone(),
                schedule.created_by.clone(),
                context,
                &placeholder,
            )
            .await;
        self.finish(&target, &placeholder, outcome).await
    }

    /// Post automation suggestions on a fixed weekly schedule, forever.
    async fn run_suggestions(self: Arc<Self>, engine: Arc<Engine>, channel: String) {
        loop {
            let wait = crate::schedule::until_next(SUGGEST_WHEN)
                .unwrap_or(Duration::from_secs(7 * 24 * 3600));
            tokio::time::sleep(wait).await;
            match crate::suggest::suggest(
                engine.provider().as_ref(),
                engine.model(),
                engine.data_dir(),
                7,
            )
            .await
            {
                Ok(Some(text)) => {
                    let text = format!("{text}\n\n_Tag me with one of these to set it up._");
                    if let Err(e) = self.post(&channel, None, &to_mrkdwn(&text)).await {
                        tracing::warn!("Could not post suggestions: {e:#}");
                    }
                }
                Ok(None) => tracing::info!("No automation suggestions this week."),
                Err(e) => tracing::warn!("Could not make suggestions: {e:#}"),
            }
        }
    }

    /// A click on a button: an approval card, feedback, or "Show steps".
    fn on_interaction(self: &Arc<Self>, payload: &Value) {
        if payload["type"] != "block_actions" {
            return;
        }
        let user = payload["user"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let channel = payload["channel"]["id"]
            .as_str()
            .or(payload["container"]["channel_id"].as_str())
            .unwrap_or_default()
            .to_string();
        for action in payload["actions"].as_array().into_iter().flatten() {
            let key = action["value"].as_str().unwrap_or_default();
            let decision = match action["action_id"].as_str() {
                Some("ost_allow") => Decision::Allow,
                Some("ost_always") => Decision::AllowAlways,
                Some("ost_deny") => Decision::Deny,
                Some(feedback @ ("ost_good" | "ost_bad")) => {
                    let good = feedback == "ost_good";
                    self.record_feedback(key, &user, good);
                    let reply = if good {
                        "Thanks! Glad that helped."
                    } else {
                        "Thanks for saying so. Reply in the thread with what was wrong and I'll fix it                          (and remember it if it's about how your team works)."
                    };
                    self.whisper(&channel, &user, reply);
                    continue;
                }
                Some("ost_steps") => {
                    let steps = self.steps.lock().get(key).cloned();
                    let text = match steps {
                        Some(steps) => steps
                            .iter()
                            .map(|s| format!("• {s}"))
                            .collect::<Vec<_>>()
                            .join(
                                "
",
                            ),
                        None => "Those steps are no longer available.".into(),
                    };
                    self.whisper(&channel, &user, &text);
                    continue;
                }
                _ => continue,
            };
            let id = action["value"].as_str().unwrap_or_default();
            let mut pending = self.pending.lock();
            let Some(waiting) = pending.get(id) else {
                continue;
            };
            let may_decide = if self.approvers.is_empty() {
                waiting.requester.as_deref().is_none_or(|r| r == user) && self.is_allowed(&user)
            } else {
                self.approvers.contains(&user)
            };
            if !may_decide {
                let who = if self.approvers.is_empty() {
                    waiting
                        .requester
                        .as_deref()
                        .map_or("the person who asked".to_string(), |r| format!("<@{r}>"))
                } else {
                    "an approver".to_string()
                };
                self.whisper(
                    &waiting.channel,
                    &user,
                    &format!("Only {who} can decide this one."),
                );
                continue;
            }
            if let Some(waiting) = pending.remove(id) {
                let _ = waiting.answer.send((decision, user.clone()));
            }
        }
    }

    /// A message only `user` sees.
    fn whisper(self: &Arc<Self>, channel: &str, user: &str, text: &str) {
        let (slack, channel, user, text) = (
            self.clone(),
            channel.to_string(),
            user.to_string(),
            text.to_string(),
        );
        tokio::spawn(async move {
            let form = [
                ("channel", channel.as_str()),
                ("user", user.as_str()),
                ("text", text.as_str()),
            ];
            if let Err(e) = slack
                .call("chat.postEphemeral", &slack.bot_token, &form)
                .await
            {
                tracing::warn!("Could not send a private reply: {e:#}");
            }
        });
    }

    fn record_feedback(&self, answer: &str, user: &str, good: bool) {
        let Some(path) = self.feedback_file.lock().clone() else {
            return;
        };
        let line = json!({
            "at": jiff::Timestamp::now().to_string(),
            "answer": answer,
            "user": user,
            "good": good,
            "steps": self.steps.lock().get(answer).cloned().unwrap_or_default(),
        });
        let write = || -> std::io::Result<()> {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            writeln!(file, "{line}")
        };
        if let Err(e) = write() {
            tracing::warn!("Could not record feedback: {e}");
        }
    }

    /// Put `text` where the placeholder is, splitting long answers into
    /// several messages. If the placeholder cannot be edited (someone deleted
    /// it), the answer is posted instead, so it is never lost.
    async fn deliver(
        &self,
        channel: &str,
        thread: Option<&str>,
        placeholder: &str,
        text: &str,
    ) -> Result<()> {
        let mut chunks = split_message(text, MAX_MESSAGE_CHARS).into_iter();
        if let Some(first) = chunks.next()
            && let Err(e) = self.update(channel, placeholder, &first).await
        {
            tracing::warn!("Could not edit the placeholder ({e:#}); posting instead.");
            self.post(channel, thread, &first).await?;
        }
        for chunk in chunks {
            self.post(channel, thread, &chunk).await?;
        }
        Ok(())
    }

    /// The messages the engine has not seen. For a conversation it already
    /// knows, only messages from people since its cursor are new (its own
    /// replies are in its transcript); the first time, the whole thread is
    /// context. Files on new messages are downloaded into the workspace.
    async fn incoming(
        &self,
        event: &Event,
        thread: Option<&str>,
        known: bool,
        since: Option<&str>,
        workspace: &Path,
    ) -> Result<Vec<Incoming>> {
        let mut raw: Vec<Value> = match thread {
            Some(ts) => self.thread_messages(&event.channel, ts, since).await?,
            None => {
                let form = [("channel", event.channel.as_str()), ("limit", "20")];
                let resp = self
                    .call("conversations.history", &self.bot_token, &form)
                    .await?;
                let mut msgs = resp["messages"].as_array().cloned().unwrap_or_default();
                msgs.reverse(); // newest first -> oldest first
                msgs
            }
        };
        // Slack can lag; make sure the message being answered is included.
        if !raw.iter().any(|m| m["ts"] == event.ts.as_str()) {
            raw.push(json!({
                "user": event.user,
                "text": event.text,
                "ts": event.ts,
                "files": event.files,
            }));
        }
        let mut incoming = Vec::new();
        for m in &raw {
            if let Some(message) = self.to_incoming(m, known, event, workspace).await {
                incoming.push(message);
            }
        }
        Ok(incoming)
    }

    /// A thread's messages, oldest first, following pagination. `since`
    /// skips what was already seen.
    async fn thread_messages(
        &self,
        channel: &str,
        ts: &str,
        since: Option<&str>,
    ) -> Result<Vec<Value>> {
        let mut messages = Vec::new();
        let mut page = String::new();
        for _ in 0..MAX_HISTORY_PAGES {
            let mut form = vec![("channel", channel), ("ts", ts), ("limit", "200")];
            if let Some(since) = since {
                form.push(("oldest", since));
            }
            if !page.is_empty() {
                form.push(("cursor", page.as_str()));
            }
            let resp = self
                .call("conversations.replies", &self.bot_token, &form)
                .await?;
            messages.extend(resp["messages"].as_array().cloned().unwrap_or_default());
            page = resp["response_metadata"]["next_cursor"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if page.is_empty() {
                break;
            }
        }
        Ok(messages)
    }

    async fn to_incoming(
        &self,
        m: &Value,
        known: bool,
        event: &Event,
        workspace: &Path,
    ) -> Option<Incoming> {
        if let Some(subtype) = m["subtype"].as_str()
            && subtype != "bot_message"
            && !PERSON_SUBTYPES.contains(&subtype)
        {
            return None;
        }
        let text = m["text"].as_str().unwrap_or("");
        let user = m["user"].as_str().unwrap_or("");
        let cursor = m["ts"].as_str().map(str::to_string);
        if user == self.bot_user_id {
            // Skip our own placeholders and progress notes, and our replies when
            // the transcript already has them.
            let in_flight = text.starts_with('_') && text.ends_with("…_");
            if known || in_flight {
                return None;
            }
            return Some(Incoming {
                cursor,
                entry: Entry::assistant(text),
            });
        }
        let author = if user.is_empty() {
            "A bot".to_string()
        } else {
            format!("<@{user}> ({})", self.user_name(user).await)
        };
        let mut said = format!("{author}: {}", strip_mention(text, &self.bot_user_id));
        let mut images = Vec::new();
        // Files are fetched for the message being answered (and, the first
        // time a thread is seen, for earlier ones too).
        let fetch = !known || cursor.as_deref() == Some(event.ts.as_str());
        let files: Vec<&Value> = m["files"].as_array().into_iter().flatten().collect();
        if !files.is_empty() {
            let mut notes = Vec::new();
            for file in files.into_iter().take(MAX_FILES_PER_MESSAGE) {
                let name = file["name"].as_str().unwrap_or("file");
                if !fetch {
                    notes.push(format!("{name} (shared earlier)"));
                    continue;
                }
                match self.save_file(file, workspace).await {
                    Ok((path, image)) => {
                        notes.push(format!("{name} -> {path}"));
                        images.extend(image);
                    }
                    Err(e) => notes.push(format!("{name} (could not download: {e:#})")),
                }
            }
            said.push_str(&format!(
                "\n[attached files, saved in your workspace: {}]",
                notes.join("; ")
            ));
        }
        let entry = if images.is_empty() {
            Entry::user(said)
        } else {
            let mut parts = vec![Part::Text { text: said }];
            parts.extend(images.into_iter().map(|url| Part::ImageUrl {
                image_url: ImageUrl::Object { url },
            }));
            Entry::User(UserEntry {
                parts: Some(parts),
                ..Default::default()
            })
        };
        Some(Incoming { cursor, entry })
    }

    /// Download a shared file into `inbox/`. Small images also come back as a
    /// data URL so the model can see them.
    async fn save_file(&self, file: &Value, workspace: &Path) -> Result<(String, Option<String>)> {
        let size = file["size"].as_u64().unwrap_or(0);
        if size > MAX_FILE_BYTES {
            bail!("larger than {} MB", MAX_FILE_BYTES / 1_000_000);
        }
        let url = file["url_private_download"]
            .as_str()
            .or(file["url_private"].as_str())
            .context("no download link (the app may lack the files:read scope)")?;
        let name = crate::safe_file_name(file["name"].as_str().unwrap_or("file"));
        let bytes = self
            .http
            .get(url)
            .bearer_auth(&self.bot_token)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            bail!("larger than {} MB", MAX_FILE_BYTES / 1_000_000);
        }
        let relative = format!("inbox/{name}");
        let path = workspace.join(&relative);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, &bytes)?;
        let mut relative = relative;
        if name.to_lowercase().ends_with(".zip") {
            let folder = format!("inbox/{}", name[..name.len() - 4].trim_end_matches('.'));
            match crate::files::extract_zip(&path, &workspace.join(&folder)) {
                Ok(files) => {
                    relative = format!(
                        "{relative}, unpacked into {folder}/ ({} files)",
                        files.len()
                    )
                }
                Err(e) => relative = format!("{relative} (could not unpack it: {e:#})"),
            }
        }
        let mime = file["mimetype"].as_str().unwrap_or_default();
        let image = (matches!(
            mime,
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        ) && bytes.len() as u64 <= MAX_IMAGE_BYTES)
            .then(|| {
                format!(
                    "data:{mime};base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(&bytes)
                )
            });
        Ok((relative, image))
    }

    // ── Web API helpers, shared with the Slack tools ──────────────────────

    pub(crate) async fn post(
        &self,
        channel: &str,
        thread: Option<&str>,
        text: &str,
    ) -> Result<String> {
        let mut form = vec![("channel", channel), ("text", text)];
        if let Some(ts) = thread {
            form.push(("thread_ts", ts));
        }
        let resp = self
            .call("chat.postMessage", &self.bot_token, &form)
            .await?;
        Ok(resp["ts"]
            .as_str()
            .context("chat.postMessage returned no ts")?
            .to_string())
    }

    async fn post_blocks(
        &self,
        channel: &str,
        thread: Option<&str>,
        text: &str,
        blocks: &Value,
    ) -> Result<String> {
        let blocks = blocks.to_string();
        let mut form = vec![
            ("channel", channel),
            ("text", text),
            ("blocks", blocks.as_str()),
        ];
        if let Some(ts) = thread {
            form.push(("thread_ts", ts));
        }
        let resp = self
            .call("chat.postMessage", &self.bot_token, &form)
            .await?;
        Ok(resp["ts"]
            .as_str()
            .context("chat.postMessage returned no ts")?
            .to_string())
    }

    async fn update(&self, channel: &str, ts: &str, text: &str) -> Result<()> {
        let form = [("channel", channel), ("ts", ts), ("text", text)];
        self.call("chat.update", &self.bot_token, &form)
            .await
            .map(drop)
    }

    /// Replace a message's blocks (and so its buttons) with one text section.
    async fn update_section(&self, channel: &str, ts: &str, text: &str) -> Result<()> {
        let blocks =
            json!([{"type": "section", "text": {"type": "mrkdwn", "text": text}}]).to_string();
        let form = [
            ("channel", channel),
            ("ts", ts),
            ("text", text),
            ("blocks", blocks.as_str()),
        ];
        self.call("chat.update", &self.bot_token, &form)
            .await
            .map(drop)
    }

    async fn delete(&self, channel: &str, ts: &str) -> Result<()> {
        let form = [("channel", channel), ("ts", ts)];
        self.call("chat.delete", &self.bot_token, &form)
            .await
            .map(drop)
    }

    pub(crate) async fn react(&self, channel: &str, ts: &str, emoji: &str) -> Result<()> {
        let form = [("channel", channel), ("timestamp", ts), ("name", emoji)];
        match self.call("reactions.add", &self.bot_token, &form).await {
            Err(e) if format!("{e}").contains("already_reacted") => Ok(()),
            other => other.map(drop),
        }
    }

    /// A person's display name, cached. Falls back to their id.
    pub(crate) async fn user_name(&self, user: &str) -> String {
        if let Some(name) = self.names.lock().get(user) {
            return name.clone();
        }
        let name = match self
            .call("users.info", &self.bot_token, &[("user", user)])
            .await
        {
            Ok(resp) => {
                let profile = &resp["user"]["profile"];
                [
                    &profile["display_name"],
                    &profile["real_name"],
                    &resp["user"]["name"],
                ]
                .iter()
                .filter_map(|v| v.as_str())
                .find(|s| !s.is_empty())
                .unwrap_or(user)
                .to_string()
            }
            Err(_) => user.to_string(),
        };
        self.names.lock().insert(user.to_string(), name.clone());
        name
    }

    pub(crate) async fn user_info(&self, user: &str) -> Result<String> {
        let user = user.trim_start_matches("<@").trim_end_matches('>');
        let resp = self
            .call("users.info", &self.bot_token, &[("user", user)])
            .await?;
        let u = &resp["user"];
        let p = &u["profile"];
        Ok(format!(
            "<@{id}>: {name}{title}. Time zone: {tz}.{bot}",
            id = u["id"].as_str().unwrap_or(user),
            name = p["real_name"].as_str().unwrap_or_default(),
            title = p["title"]
                .as_str()
                .filter(|t| !t.is_empty())
                .map(|t| format!(", {t}"))
                .unwrap_or_default(),
            tz = u["tz"].as_str().unwrap_or("unknown"),
            bot = if u["is_bot"] == true { " (a bot)" } else { "" },
        ))
    }

    /// A channel id from an id, `#name` or `<#C123|name>`.
    pub(crate) async fn resolve_channel(&self, raw: &str) -> Result<String> {
        let raw = raw.trim().trim_start_matches("<#").trim_end_matches('>');
        let raw = raw.split('|').next().unwrap_or(raw);
        if raw.len() > 8
            && raw.starts_with(['C', 'G', 'D'])
            && raw
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        {
            return Ok(raw.to_string());
        }
        let name = raw.trim_start_matches('#').to_lowercase();
        if let Some(id) = self.channel_ids.lock().get(&name) {
            return Ok(id.clone());
        }
        self.channels().await?;
        self.channel_ids
            .lock()
            .get(&name)
            .cloned()
            .with_context(|| format!("I'm not in a channel called #{name}"))
    }

    /// Channels the bot is in, as (id, name, purpose), refreshing the name cache.
    async fn channels(&self) -> Result<Vec<(String, String, String)>> {
        let mut found = Vec::new();
        let mut page = String::new();
        for _ in 0..5 {
            let mut form = vec![
                ("types", "public_channel,private_channel"),
                ("exclude_archived", "true"),
                ("limit", "1000"),
            ];
            if !page.is_empty() {
                form.push(("cursor", page.as_str()));
            }
            let resp = self
                .call("conversations.list", &self.bot_token, &form)
                .await?;
            for c in resp["channels"].as_array().into_iter().flatten() {
                if c["is_member"] != true {
                    continue;
                }
                let (Some(id), Some(name)) = (c["id"].as_str(), c["name"].as_str()) else {
                    continue;
                };
                let purpose = c["purpose"]["value"].as_str().unwrap_or_default();
                found.push((id.to_string(), name.to_string(), purpose.to_string()));
            }
            page = resp["response_metadata"]["next_cursor"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if page.is_empty() {
                break;
            }
        }
        let mut cache = self.channel_ids.lock();
        for (id, name, _) in &found {
            cache.insert(name.to_lowercase(), id.clone());
        }
        Ok(found)
    }

    pub(crate) async fn list_channels(&self) -> Result<String> {
        let channels = self.channels().await?;
        if channels.is_empty() {
            return Ok("I'm not in any channels yet. Invite me with /invite.".into());
        }
        Ok(channels
            .iter()
            .map(|(id, name, purpose)| {
                if purpose.is_empty() {
                    format!("- #{name} ({id})")
                } else {
                    format!("- #{name} ({id}): {}", crate::one_line(purpose, 120))
                }
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// Whether `user` may have the teammate read `channel`: public channels
    /// the bot is in, or private rooms `user` is also in.
    pub(crate) async fn can_read(&self, channel: &str, user: &str) -> Result<bool> {
        let info = self
            .call(
                "conversations.info",
                &self.bot_token,
                &[("channel", channel)],
            )
            .await?;
        let c = &info["channel"];
        let private = c["is_private"] == true || c["is_im"] == true || c["is_mpim"] == true;
        if !private {
            return Ok(c["is_member"] == true);
        }
        let mut page = String::new();
        for _ in 0..10 {
            let mut form = vec![("channel", channel), ("limit", "1000")];
            if !page.is_empty() {
                form.push(("cursor", page.as_str()));
            }
            let resp = self
                .call("conversations.members", &self.bot_token, &form)
                .await?;
            if resp["members"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|m| m == user)
            {
                return Ok(true);
            }
            page = resp["response_metadata"]["next_cursor"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if page.is_empty() {
                break;
            }
        }
        Ok(false)
    }

    pub(crate) async fn read_channel(
        &self,
        channel: &str,
        hours: f64,
        limit: u64,
    ) -> Result<String> {
        let oldest = (jiff::Timestamp::now().as_second() as f64 - hours * 3600.0).max(0.0);
        let (oldest, limit) = (format!("{oldest:.0}"), limit.to_string());
        let form = [
            ("channel", channel),
            ("oldest", oldest.as_str()),
            ("limit", limit.as_str()),
        ];
        let resp = self
            .call("conversations.history", &self.bot_token, &form)
            .await?;
        let mut messages = resp["messages"].as_array().cloned().unwrap_or_default();
        messages.reverse();
        self.render_messages(&messages, "No messages in that time.")
            .await
    }

    pub(crate) async fn read_thread(&self, channel: &str, ts: &str) -> Result<String> {
        let messages = self.thread_messages(channel, ts, None).await?;
        self.render_messages(&messages, "That thread is empty or I can't see it.")
            .await
    }

    async fn render_messages(&self, messages: &[Value], empty: &str) -> Result<String> {
        if messages.is_empty() {
            return Ok(empty.to_string());
        }
        let tz = jiff::tz::TimeZone::system();
        let mut lines = Vec::new();
        for m in messages {
            let user = m["user"].as_str().unwrap_or_default();
            let who = if user.is_empty() {
                "a bot".to_string()
            } else {
                format!("<@{user}> ({})", self.user_name(user).await)
            };
            let when = m["ts"]
                .as_str()
                .and_then(|ts| ts.split('.').next()?.parse::<i64>().ok())
                .and_then(|secs| jiff::Timestamp::from_second(secs).ok())
                .map(|t| t.to_zoned(tz.clone()).strftime("%a %H:%M").to_string())
                .unwrap_or_default();
            let mut line = format!("[{when}] {who}: {}", m["text"].as_str().unwrap_or_default());
            if let Some(replies) = m["reply_count"].as_u64() {
                line.push_str(&format!(
                    "  (thread: {replies} replies, ts {})",
                    m["ts"].as_str().unwrap_or_default()
                ));
            }
            lines.push(line);
        }
        Ok(lines.join("\n"))
    }

    /// Upload a file into a channel or thread (Slack's two-step external upload).
    pub(crate) async fn upload_file(
        &self,
        channel: &str,
        thread: Option<&str>,
        name: &str,
        bytes: Vec<u8>,
        comment: Option<&str>,
    ) -> Result<()> {
        let length = bytes.len().to_string();
        let ticket = self
            .call(
                "files.getUploadURLExternal",
                &self.bot_token,
                &[("filename", name), ("length", length.as_str())],
            )
            .await?;
        let upload_url = ticket["upload_url"]
            .as_str()
            .context("Slack returned no upload_url")?;
        let file_id = ticket["file_id"]
            .as_str()
            .context("Slack returned no file_id")?;
        self.http
            .post(upload_url)
            .body(bytes)
            .send()
            .await?
            .error_for_status()?;
        let files = json!([{ "id": file_id, "title": name }]).to_string();
        let mut form = vec![("files", files.as_str()), ("channel_id", channel)];
        if let Some(ts) = thread {
            form.push(("thread_ts", ts));
        }
        if let Some(comment) = comment {
            form.push(("initial_comment", comment));
        }
        self.call("files.completeUploadExternal", &self.bot_token, &form)
            .await
            .map(drop)
    }

    /// Call a Web API method. Slack reports errors as HTTP 200 with `ok: false`
    /// and rate limits as 429 with Retry-After. Network errors and 5xx are
    /// retried a few times, since both usually clear in seconds.
    pub(crate) async fn call(
        &self,
        method: &str,
        token: &str,
        form: &[(&str, &str)],
    ) -> Result<Value> {
        const ATTEMPTS: u64 = 4;
        let mut last_error = String::new();
        for attempt in 1..=ATTEMPTS {
            let sent = self
                .http
                .post(format!("{}/{method}", self.api_url))
                .bearer_auth(token)
                .form(form)
                .send()
                .await;
            let resp = match sent {
                Ok(resp) => resp,
                Err(e) => {
                    last_error = format!("could not reach Slack: {e}");
                    tokio::time::sleep(Duration::from_secs(attempt)).await;
                    continue;
                }
            };
            let status = resp.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok()?.parse().ok())
                    .unwrap_or(1u64);
                last_error = "rate limited".into();
                tokio::time::sleep(Duration::from_secs(wait.min(60))).await;
                continue;
            }
            if status.is_server_error() {
                last_error = format!("Slack returned {status}");
                tokio::time::sleep(Duration::from_secs(attempt)).await;
                continue;
            }
            let body: Value = resp
                .json()
                .await
                .with_context(|| format!("Slack {method} returned a non-JSON response"))?;
            if body["ok"] != true {
                bail!(
                    "Slack {method} failed: {}",
                    body["error"].as_str().unwrap_or("unknown error")
                );
            }
            return Ok(body);
        }
        bail!("Slack {method} failed after {ATTEMPTS} tries: {last_error}")
    }
}

/// Asks in the conversation's thread with Allow / Always / Deny buttons, and
/// waits for the person who asked (or an approver) to click.
#[derive(Debug)]
struct SlackApprover {
    slack: Arc<Slack>,
    channel: String,
    thread: Option<String>,
    requester: Option<String>,
}

#[async_trait]
impl Approver for SlackApprover {
    async fn approve(&self, approval: &Approval) -> Decision {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let (answer, rx) = oneshot::channel();
        self.slack.pending.lock().insert(
            id.clone(),
            PendingApproval {
                answer,
                requester: self.requester.clone(),
                channel: self.channel.clone(),
            },
        );
        let (text, blocks) = approval_card(&id, approval, self.requester.as_deref());
        let ts = match self
            .slack
            .post_blocks(&self.channel, self.thread.as_deref(), &text, &blocks)
            .await
        {
            Ok(ts) => ts,
            Err(e) => {
                tracing::warn!("Could not post an approval card: {e:#}");
                self.slack.pending.lock().remove(&id);
                return Decision::Deny;
            }
        };
        let (decision, outcome) = match tokio::time::timeout(APPROVAL_TIMEOUT, rx).await {
            Ok(Ok((decision, user))) => {
                let verb = match decision {
                    Decision::Allow => ":white_check_mark: Allowed once",
                    Decision::AllowAlways => ":white_check_mark: Allowed for this conversation",
                    Decision::Deny => ":no_entry: Denied",
                };
                (decision, format!("{verb} by <@{user}>"))
            }
            _ => {
                self.slack.pending.lock().remove(&id);
                (
                    Decision::Deny,
                    format!(
                        ":hourglass: No answer within {} minutes, so it did not run",
                        APPROVAL_TIMEOUT.as_secs() / 60
                    ),
                )
            }
        };
        let summary = format!(
            "{outcome}: `{}` {}",
            approval.tool,
            short(&approval.target, 200)
        );
        let _ = self
            .slack
            .update_section(&self.channel, &ts, &crate::markdown::escape(&summary))
            .await;
        decision
    }
}

/// The approval message: fallback text and Block Kit with three buttons.
fn approval_card(id: &str, approval: &Approval, requester: Option<&str>) -> (String, Value) {
    let who = requester.map_or(String::new(), |r| format!(" <@{r}>,"));
    let mut body = format!(
        ":lock:{who} I'd like to use `{}` on:\n```{}```",
        approval.tool,
        short(&approval.target, 500)
    );
    if let Some(detail) = &approval.detail {
        body.push_str(&format!("\nwith:\n```{}```", short(detail, 1500)));
    }
    let body = crate::markdown::escape(&body);
    let button = |label: &str, action: &str, style: Option<&str>| {
        let mut b = json!({
            "type": "button",
            "text": {"type": "plain_text", "text": label},
            "action_id": action,
            "value": id,
        });
        if let Some(style) = style {
            b["style"] = json!(style);
        }
        b
    };
    let blocks = json!([
        {"type": "section", "text": {"type": "mrkdwn", "text": body}},
        {"type": "actions", "elements": [
            button("Allow once", "ost_allow", Some("primary")),
            button("Always in this conversation", "ost_always", None),
            button("Deny", "ost_deny", Some("danger")),
        ]}
    ]);
    (
        format!(
            "Approval needed: {} {}",
            approval.tool,
            short(&approval.target, 100)
        ),
        blocks,
    )
}

fn short(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

/// Split `text` into messages of at most `max` characters, at line breaks
/// where possible.
fn split_message(text: &str, max: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        let length = line.chars().count();
        if !current.is_empty() && current.chars().count() + length > max {
            chunks.push(std::mem::take(&mut current));
        }
        if length > max {
            let chars: Vec<char> = line.chars().collect();
            chunks.extend(chars.chunks(max).map(|c| c.iter().collect::<String>()));
        } else {
            current.push_str(line);
        }
    }
    chunks.push(current);
    let chunks: Vec<String> = chunks
        .into_iter()
        .map(|c| c.trim_end().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    if chunks.is_empty() {
        vec![String::new()]
    } else {
        chunks
    }
}

fn is_auth_error(e: &anyhow::Error) -> bool {
    let msg = format!("{e:#}");
    [
        "invalid_auth",
        "not_authed",
        "token_revoked",
        "account_inactive",
        "not_allowed_token_type",
    ]
    .iter()
    .any(|code| msg.contains(code))
}

/// Remove `<@BOT>` / `<@BOT|name>` mentions of the bot itself.
fn strip_mention(text: &str, bot: &str) -> String {
    let open = format!("<@{bot}");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        match after.chars().next() {
            Some('>' | '|') => {
                out.push_str(&rest[..start]);
                rest = after.find('>').map_or("", |end| &after[end + 1..]);
            }
            _ => {
                out.push_str(&rest[..start + open.len()]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Bounded set of recently seen ids, oldest evicted first.
struct RecentIds {
    order: VecDeque<String>,
    set: HashSet<String>,
    cap: usize,
}

impl RecentIds {
    fn new(cap: usize) -> Self {
        Self {
            order: VecDeque::new(),
            set: HashSet::new(),
            cap,
        }
    }

    /// Returns false if the id was already seen.
    fn insert(&mut self, id: String) -> bool {
        if !self.set.insert(id.clone()) {
            return false;
        }
        self.order.push_back(id);
        if self.order.len() > self.cap
            && let Some(old) = self.order.pop_front()
        {
            self.set.remove(&old);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slack(allowed: &[&str], approvers: &[&str]) -> Arc<Slack> {
        Arc::new(Slack {
            http: reqwest::Client::new(),
            api_url: String::new(),
            bot_token: String::new(),
            app_token: String::new(),
            bot_user_id: "UBOT".into(),
            team_id: "T1".into(),
            allowed_users: allowed.iter().map(|s| s.to_string()).collect(),
            approvers: approvers.iter().map(|s| s.to_string()).collect(),
            seen: Mutex::new(RecentIds::new(10)),
            pending: Mutex::default(),
            names: Mutex::default(),
            channel_ids: Mutex::default(),
            steps: Mutex::default(),
            feedback_file: Mutex::default(),
        })
    }

    fn payload(id: &str, event: &Value) -> Value {
        json!({"event_id": id, "event": event})
    }

    #[test]
    fn accepts_people_and_rejects_everything_else() {
        let s = slack(&[], &[]);
        let base = json!({"type": "app_mention", "user": "U1", "text": "hi", "ts": "1.0", "channel": "C1"});
        assert!(s.accept(&payload("E1", &base)).is_some());
        assert!(s.accept(&payload("E1", &base)).is_none(), "duplicate");

        let mut bot = base.clone();
        bot["bot_id"] = json!("B1");
        assert!(s.accept(&payload("E2", &bot)).is_none(), "bots");

        let mut edit = base.clone();
        edit["subtype"] = json!("message_changed");
        assert!(s.accept(&payload("E3", &edit)).is_none(), "edits");

        let mut file = base.clone();
        file["subtype"] = json!("file_share");
        assert!(
            s.accept(&payload("E4", &file)).is_some(),
            "file shares are people"
        );

        let mut guest = base.clone();
        guest["user_team"] = json!("T_OTHER");
        assert!(
            s.accept(&payload("E5", &guest)).is_none(),
            "Slack Connect guests"
        );

        let channel_message = json!({"type": "message", "channel_type": "channel", "user": "U1", "ts": "2.0", "channel": "C1"});
        assert!(
            s.accept(&payload("E6", &channel_message)).is_none(),
            "plain channel messages"
        );

        let dm = json!({"type": "message", "channel_type": "im", "user": "U1", "ts": "3.0", "channel": "D1"});
        assert!(s.accept(&payload("E7", &dm)).is_some(), "DMs");
    }

    #[test]
    fn the_allowlist_limits_who_is_answered() {
        let s = slack(&["U1"], &[]);
        let from = |user: &str, id: &str| {
            payload(
                id,
                &json!({"type": "app_mention", "user": user, "ts": "1.0", "channel": "C1"}),
            )
        };
        assert!(s.accept(&from("U1", "E1")).is_some());
        assert!(s.accept(&from("U2", "E2")).is_none());
    }

    fn click(user: &str, action: &str, id: &str) -> Value {
        json!({"type": "block_actions", "user": {"id": user}, "actions": [{"action_id": action, "value": id}]})
    }

    #[tokio::test]
    async fn only_the_requester_or_an_approver_can_decide() {
        // Without approvers: only the person who asked.
        let s = slack(&[], &[]);
        let (answer, mut rx) = oneshot::channel();
        s.pending.lock().insert(
            "a1".into(),
            PendingApproval {
                answer,
                requester: Some("U1".into()),
                channel: "C1".into(),
            },
        );
        s.on_interaction(&click("U2", "ost_allow", "a1"));
        assert!(rx.try_recv().is_err(), "a bystander's click is ignored");
        s.on_interaction(&click("U1", "ost_always", "a1"));
        assert_eq!(
            rx.try_recv().unwrap(),
            (Decision::AllowAlways, "U1".to_string())
        );
        assert!(s.pending.lock().is_empty());

        // With approvers: only they, not even the person who asked.
        let s = slack(&[], &["UBOSS"]);
        let (answer, mut rx) = oneshot::channel();
        s.pending.lock().insert(
            "a2".into(),
            PendingApproval {
                answer,
                requester: Some("U1".into()),
                channel: "C1".into(),
            },
        );
        s.on_interaction(&click("U1", "ost_allow", "a2"));
        assert!(rx.try_recv().is_err());
        s.on_interaction(&click("UBOSS", "ost_deny", "a2"));
        assert_eq!(rx.try_recv().unwrap().0, Decision::Deny);
    }

    #[test]
    fn approval_cards_escape_and_carry_the_id() {
        let approval = Approval {
            tool: "mcp".into(),
            target: "github_create_issue".into(),
            detail: Some(r#"{"title":"<!channel> hi"}"#.into()),
        };
        let (text, blocks) = approval_card("abc", &approval, Some("U1"));
        assert!(text.contains("github_create_issue"));
        let body = blocks[0]["text"]["text"].as_str().unwrap();
        assert!(
            body.contains("&lt;!channel&gt;") && body.contains("<@U1>"),
            "{body}"
        );
        let buttons = blocks[1]["elements"].as_array().unwrap();
        assert_eq!(buttons.len(), 3);
        assert!(buttons.iter().all(|b| b["value"] == "abc"));
    }

    #[test]
    fn strips_only_the_bot_mention() {
        assert_eq!(
            strip_mention("<@UBOT> summarize this", "UBOT"),
            "summarize this"
        );
        assert_eq!(
            strip_mention("hey <@UBOT|tag> ask <@U2>", "UBOT"),
            "hey ask <@U2>"
        );
        assert_eq!(strip_mention("<@UBOTX> hi", "UBOT"), "<@UBOTX> hi");
        assert_eq!(strip_mention("<@UBOT", "UBOT"), "<@UBOT");
    }

    #[test]
    fn recent_ids_dedupes_and_evicts() {
        let mut ids = RecentIds::new(2);
        assert!(ids.insert("a".into()));
        assert!(!ids.insert("a".into()));
        assert!(ids.insert("b".into()));
        assert!(ids.insert("c".into())); // evicts "a"
        assert!(ids.insert("a".into()));
        assert!(!ids.insert("c".into()));
    }

    #[test]
    fn long_messages_split_at_lines() {
        let (a, b, c) = ("a".repeat(30), "b".repeat(30), "c".repeat(30));
        let chunks = split_message(&format!("{a}\n{b}\n{c}"), 70);
        assert_eq!(chunks, vec![format!("{a}\n{b}"), c]);
        assert_eq!(split_message(&"é".repeat(25), 10).len(), 3);
        assert_eq!(split_message("short", 70), vec!["short"]);
        assert_eq!(split_message("", 70), vec![""]);
    }

    #[test]
    fn auth_errors_are_fatal() {
        assert!(is_auth_error(&anyhow::anyhow!(
            "Slack auth.test failed: invalid_auth"
        )));
        assert!(!is_auth_error(&anyhow::anyhow!("connection reset")));
    }
}
