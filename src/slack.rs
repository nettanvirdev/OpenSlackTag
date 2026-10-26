//! Slack over Socket Mode: the bot opens an outbound WebSocket, so it needs no
//! public URL and runs fine on a laptop.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures::{SinkExt, StreamExt};
use openslacktag_core::message::Entry;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::engine::{Engine, Incoming, Reply};
use crate::markdown::to_mrkdwn;

const WORKING: &str = "_Working on it…_";
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

pub struct Slack {
    http: reqwest::Client,
    api_url: String,
    bot_token: String,
    app_token: String,
    bot_user_id: String,
    team_id: String,
    allowed_users: Vec<String>,
    seen: Mutex<RecentIds>,
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
                .timeout(Duration::from_secs(30))
                .build()?,
            api_url: api_url.trim_end_matches('/').to_string(),
            bot_token: bot_token.to_string(),
            app_token: app_token.to_string(),
            bot_user_id: String::new(),
            team_id: String::new(),
            allowed_users: Vec::new(),
            seen: Mutex::new(RecentIds::new(2000)),
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

    /// Only answer these user ids. Empty means everyone in the workspace.
    pub fn allow_only(mut self, users: Vec<String>) -> Self {
        self.allowed_users = users;
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

    /// Serve until the tokens stop working, reconnecting whenever Slack asks
    /// or the connection drops.
    pub async fn run(self: Arc<Self>, engine: Arc<Engine>) -> Result<()> {
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
        let Some(event) = payload.and_then(|p| self.accept(p)) else {
            return;
        };
        let (slack, engine) = (self.clone(), engine.clone());
        tokio::spawn(async move {
            if let Err(e) = slack.handle(&engine, &event).await {
                tracing::error!("Failed to answer message {}: {e:#}", event.ts);
            }
        });
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
        let allowed = self.allowed_users.is_empty() || self.allowed_users.iter().any(|u| u == user);
        if !(from_person && wanted && our_workspace && allowed) {
            return None;
        }
        let key = event_id.unwrap_or_else(|| format!("{}:{}", event.channel, event.ts));
        self.seen.lock().insert(key).then_some(event)
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
        let known = engine.knows(&conversation);
        let since = if known {
            engine.cursor(&conversation)
        } else {
            None
        };
        let incoming = self
            .incoming(event, thread.as_deref(), known, since.as_deref())
            .await?;
        let placeholder = self
            .post(&event.channel, thread.as_deref(), WORKING)
            .await?;

        let (progress, mut notes) = mpsc::unbounded_channel::<String>();
        let updater = {
            let (slack, channel, ts) = (self.clone(), event.channel.clone(), placeholder.clone());
            tokio::spawn(async move {
                while let Some(mut note) = notes.recv().await {
                    // Show only the latest note; older ones are already stale.
                    while let Ok(newer) = notes.try_recv() {
                        note = newer;
                    }
                    let text = format!("_{}…_", crate::one_line(&note, 150));
                    let _ = slack.update(&channel, &ts, &text).await;
                    tokio::time::sleep(PROGRESS_INTERVAL).await;
                }
            })
        };
        let reply = engine.reply(&conversation, incoming, &progress).await;
        drop(progress);
        let _ = updater.await;

        let (channel, thread) = (event.channel.as_str(), thread.as_deref());
        match reply {
            Ok(Reply::Answer(text)) => {
                self.deliver(channel, thread, &placeholder, &to_mrkdwn(&text))
                    .await
            }
            Ok(Reply::Steered) => self.deliver(channel, thread, &placeholder, STEERED).await,
            Ok(Reply::Nothing) => self.delete(channel, &placeholder).await,
            Err(e) => {
                let text = format!(":warning: Something went wrong: {e:#}");
                self.deliver(channel, thread, &placeholder, &to_mrkdwn(&text))
                    .await
            }
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
    /// replies are in its transcript); the first time, the whole thread is context.
    async fn incoming(
        &self,
        event: &Event,
        thread: Option<&str>,
        known: bool,
        since: Option<&str>,
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
        Ok(raw
            .iter()
            .filter_map(|m| self.to_incoming(m, known))
            .collect())
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

    fn to_incoming(&self, m: &Value, known: bool) -> Option<Incoming> {
        if let Some(subtype) = m["subtype"].as_str()
            && subtype != "bot_message"
            && !PERSON_SUBTYPES.contains(&subtype)
        {
            return None;
        }
        let text = m["text"].as_str().unwrap_or("");
        let user = m["user"].as_str().unwrap_or("");
        let cursor = m["ts"].as_str().map(str::to_string);
        let entry = if user == self.bot_user_id {
            // Skip our own placeholders and progress notes, and our replies when
            // the transcript already has them.
            let in_flight = text.starts_with('_') && text.ends_with("…_");
            if known || in_flight {
                return None;
            }
            Entry::assistant(text)
        } else {
            let author = if user.is_empty() {
                "A bot".to_string()
            } else {
                format!("<@{user}>")
            };
            let mut said = format!("{author}: {}", strip_mention(text, &self.bot_user_id));
            let files: Vec<&str> = m["files"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| f["name"].as_str())
                .collect();
            if !files.is_empty() {
                said.push_str(&format!(
                    "\n[attached: {}. You cannot open attachments yet; say so if the task needs them.]",
                    files.join(", ")
                ));
            }
            Entry::user(said)
        };
        Some(Incoming { cursor, entry })
    }

    async fn post(&self, channel: &str, thread: Option<&str>, text: &str) -> Result<String> {
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

    async fn update(&self, channel: &str, ts: &str, text: &str) -> Result<()> {
        let form = [("channel", channel), ("ts", ts), ("text", text)];
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

    /// Call a Web API method. Slack reports errors as HTTP 200 with `ok: false`
    /// and rate limits as 429 with Retry-After. Network errors and 5xx are
    /// retried a few times, since both usually clear in seconds.
    async fn call(&self, method: &str, token: &str, form: &[(&str, &str)]) -> Result<Value> {
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

    fn slack(allowed: &[&str]) -> Slack {
        Slack {
            http: reqwest::Client::new(),
            api_url: String::new(),
            bot_token: String::new(),
            app_token: String::new(),
            bot_user_id: "UBOT".into(),
            team_id: "T1".into(),
            allowed_users: allowed.iter().map(|s| s.to_string()).collect(),
            seen: Mutex::new(RecentIds::new(10)),
        }
    }

    fn payload(id: &str, event: &Value) -> Value {
        json!({"event_id": id, "event": event})
    }

    #[test]
    fn accepts_people_and_rejects_everything_else() {
        let s = slack(&[]);
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
        let s = slack(&["U1"]);
        let from = |user: &str, id: &str| {
            payload(
                id,
                &json!({"type": "app_mention", "user": user, "ts": "1.0", "channel": "C1"}),
            )
        };
        assert!(s.accept(&from("U1", "E1")).is_some());
        assert!(s.accept(&from("U2", "E2")).is_none());
    }

    #[test]
    fn attachments_are_named_for_the_model() {
        let s = slack(&[]);
        let m = json!({"user": "U1", "text": "summarise", "ts": "1.0", "subtype": "file_share",
                       "files": [{"name": "q3.pdf"}]});
        let incoming = s.to_incoming(&m, false).unwrap();
        assert!(incoming.entry.text().contains("[attached: q3.pdf."));
        let joined =
            json!({"user": "U1", "text": "joined", "ts": "2.0", "subtype": "channel_join"});
        assert!(s.to_incoming(&joined, false).is_none());
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
