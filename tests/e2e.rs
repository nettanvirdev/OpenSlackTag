//! End to end, with no Slack account and no model: a fake Slack (Web API, file
//! hosting, and the Socket Mode WebSocket) and a scripted model drive the real
//! code paths.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::needless_pass_by_value
)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use openslacktag::engine::{Engine, Incoming, Parts, Progress, Reply, Request, Target};
use openslacktag::permissions::{Approval, Approver};
use openslacktag::slack::Slack;
use openslacktag_agent::STEER_NOTE;
use openslacktag_core::message::Entry;
use openslacktag_core::tool::{
    Decision, PermissionRequest, Tool, ToolContext, ToolOutcome, ToolSource,
};
use openslacktag_mock::{MockProvider, MockTool};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};

const THREAD_TS: &str = "1700000000.000100";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake image bytes";

// ── a fake Slack ────────────────────────────────────────────────────────

#[derive(Default)]
struct FakeSlack {
    base: Mutex<String>,
    /// Frames to push over the socket, drained as they are sent.
    outbox: Mutex<Vec<String>>,
    /// Every Web API call, as (method, form).
    calls: Mutex<Vec<(String, HashMap<String, String>)>>,
    /// Every frame the bot sent back over the socket.
    acks: Mutex<Vec<String>>,
    /// Bytes received at the file upload URL.
    uploads: Mutex<Vec<usize>>,
    /// Per-method response overrides.
    responses: Mutex<HashMap<String, Value>>,
    posted: AtomicUsize,
}

impl FakeSlack {
    fn calls_to(&self, method: &str) -> Vec<HashMap<String, String>> {
        self.calls
            .lock()
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, f)| f.clone())
            .collect()
    }

    fn send(&self, frame: Value) {
        self.outbox.lock().push(frame.to_string());
    }

    fn respond(&self, method: &str, body: Value) {
        self.responses.lock().insert(method.into(), body);
    }
}

async fn api(
    State(fake): State<Arc<FakeSlack>>,
    UrlPath(method): UrlPath<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Json<Value> {
    fake.calls.lock().push((method.clone(), form.clone()));
    if let Some(body) = fake.responses.lock().get(&method) {
        return Json(body.clone());
    }
    let base = fake.base.lock().clone();
    Json(match method.as_str() {
        "auth.test" => json!({"ok": true, "user_id": "UBOT", "team_id": "T1"}),
        "apps.connections.open" => {
            json!({"ok": true, "url": format!("{}/ws", base.replace("http", "ws"))})
        }
        "conversations.replies" => json!({"ok": true, "messages": [
            {"user": "U1", "text": "<@UBOT> remember that standup is at 10am", "ts": THREAD_TS}
        ]}),
        "chat.postMessage" => {
            let n = fake.posted.fetch_add(1, Ordering::SeqCst);
            json!({"ok": true, "ts": format!("1700000000.{:06}", 200 + n)})
        }
        "users.info" => {
            let user = form.get("user").cloned().unwrap_or_default();
            json!({"ok": true, "user": {"id": user, "name": "ana", "tz": "Europe/Berlin",
                   "profile": {"display_name": "Ana", "real_name": "Ana B", "title": "PM"}}})
        }
        "files.getUploadURLExternal" => {
            json!({"ok": true, "upload_url": format!("{base}/upload"), "file_id": "F1"})
        }
        _ => json!({"ok": true}),
    })
}

async fn upload(State(fake): State<Arc<FakeSlack>>, body: Bytes) -> &'static str {
    fake.uploads.lock().push(body.len());
    "OK"
}

async fn socket(State(fake): State<Arc<FakeSlack>>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |mut socket| async move {
        let _ = socket
            .send(Message::Text(json!({"type": "hello"}).to_string().into()))
            .await;
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        loop {
            tokio::select! {
                frame = socket.recv() => match frame {
                    Some(Ok(Message::Text(text))) => fake.acks.lock().push(text.to_string()),
                    Some(Ok(_)) => {}
                    _ => return,
                },
                _ = tick.tick() => {
                    let frames: Vec<String> = fake.outbox.lock().drain(..).collect();
                    for frame in frames {
                        if socket.send(Message::Text(frame.into())).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
    })
}

async fn start_fake_slack() -> (Arc<FakeSlack>, String) {
    let fake = Arc::new(FakeSlack::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    *fake.base.lock() = base.clone();
    let app = Router::new()
        .route("/api/{method}", post(api))
        .route("/upload", post(upload))
        .route("/files/cat.png", get(|| async { PNG }))
        .route("/ws", get(socket))
        .with_state(fake.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (fake, format!("{base}/api"))
}

fn mention(envelope_id: &str, event_id: &str, user: &str, text: &str, ts: &str) -> Value {
    json!({
        "type": "events_api",
        "envelope_id": envelope_id,
        "payload": {
            "event_id": event_id,
            "event": {
                "type": "app_mention", "user": user, "text": text, "ts": ts,
                "thread_ts": THREAD_TS, "channel": "C1"
            }
        }
    })
}

fn click(user: &str, action: &str, id: &str) -> Value {
    json!({
        "type": "interactive",
        "envelope_id": format!("click-{user}-{action}"),
        "payload": {"type": "block_actions", "user": {"id": user},
                    "actions": [{"action_id": action, "value": id}]}
    })
}

async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..300 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Says yes to every approval.
#[derive(Debug)]
struct Yes;

#[async_trait]
impl Approver for Yes {
    async fn approve(&self, _approval: &Approval) -> Decision {
        Decision::Allow
    }
}

fn engine_with(
    provider: MockProvider,
    data_dir: &Path,
    tools: Vec<Arc<dyn Tool>>,
) -> (Arc<Engine>, Arc<MockProvider>) {
    let provider = Arc::new(provider);
    let mut parts = Parts::minimal(provider.clone(), "mock-model", data_dir);
    parts.extra_tools = tools;
    (Arc::new(Engine::new(parts).unwrap()), provider)
}

fn request<'a>(
    conversation: &'a str,
    incoming: Vec<Incoming>,
    progress: &'a mpsc::UnboundedSender<Progress>,
) -> Request<'a> {
    Request {
        conversation,
        incoming,
        progress,
        approver: Arc::new(Yes),
        target: None,
        requester: None,
        context: None,
        tools: Vec::new(),
    }
}

fn user(cursor: &str, text: &str) -> Incoming {
    Incoming {
        cursor: Some(cursor.into()),
        entry: Entry::user(text),
    }
}

async fn start_bot(
    engine: Arc<Engine>,
    api_url: &str,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let slack = Arc::new(
        Slack::connect(api_url, "xoxb-test", "xapp-test")
            .await
            .unwrap(),
    );
    tokio::spawn(slack.run(engine, None))
}

/// A second bot on the same data folder, for clicks after the first finished.
async fn start_bot_again(
    _fake: &FakeSlack,
    api_url: &str,
    data: &tempfile::TempDir,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let (engine, _) = engine_with(MockProvider::new(), data.path(), Vec::new());
    start_bot(engine, api_url).await
}

/// The value of an approval card's button, once one is posted.
fn card_button(fake: &FakeSlack, index: usize) -> Option<String> {
    fake.calls_to("chat.postMessage").iter().find_map(|p| {
        let blocks: Value = serde_json::from_str(p.get("blocks")?).ok()?;
        Some(blocks[1]["elements"][index]["value"].as_str()?.to_string())
    })
}

fn answered(fake: &FakeSlack, text: &str) -> bool {
    fake.calls_to("chat.update")
        .iter()
        .any(|u| u["text"] == text)
}

// ── Slack ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_mention_is_answered_in_its_thread_after_using_a_tool() {
    let data = tempfile::tempdir().unwrap();
    let (engine, model) = engine_with(
        MockProvider::new()
            .calling(
                "wiki_write",
                json!({"page": "team/standup", "content": "Standup is at 10am."}),
            )
            .replying(
                "Noted: standup is at **10am**. Saved to [the wiki](https://example.com/wiki).",
            ),
        data.path(),
        Vec::new(),
    );
    let (fake, api_url) = start_fake_slack().await;
    let text = "<@UBOT> remember that standup is at 10am";
    // The same event delivered twice, as Slack does on a retry.
    fake.send(mention("env-1", "Ev1", "U1", text, THREAD_TS));
    fake.send(mention("env-2", "Ev1", "U1", text, THREAD_TS));
    let bot = start_bot(engine, &api_url).await;

    let is_final =
        |f: &HashMap<String, String>| f["text"].contains("10am") && !f["text"].starts_with('_');
    wait_for("the final answer", || {
        fake.calls_to("chat.update").iter().any(is_final)
    })
    .await;
    bot.abort();

    let acks = fake.acks.lock().clone();
    assert!(
        acks.iter().any(|a| a.contains("env-1")) && acks.iter().any(|a| a.contains("env-2")),
        "{acks:?}"
    );
    let posts = fake.calls_to("chat.postMessage");
    assert_eq!(
        posts.len(),
        1,
        "the duplicate got no second answer: {posts:?}"
    );
    assert_eq!(posts[0]["thread_ts"], THREAD_TS);

    let updates = fake.calls_to("chat.update");
    assert!(
        updates
            .iter()
            .any(|u| u["text"].starts_with("_Updating wiki page team/standup")),
        "{updates:?}"
    );
    assert_eq!(
        updates.last().unwrap()["text"],
        "Noted: standup is at *10am*. Saved to <https://example.com/wiki|the wiki>."
    );

    // Under the answer: what was done, which model, and feedback buttons.
    let blocks: Value = serde_json::from_str(&updates.last().unwrap()["blocks"]).unwrap();
    let context = blocks[1]["elements"].to_string();
    assert!(
        context.contains("Updated wiki page team/standup"),
        "{context}"
    );
    assert!(context.contains("mock-model"), "{context}");
    let feedback = blocks[2]["elements"][1]["value"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(blocks[2]["elements"][1]["action_id"], "ost_bad");

    // A thumbs-down is recorded and answered privately.
    let bot = start_bot_again(&fake, &api_url, &data).await;
    fake.send(click("U1", "ost_bad", &feedback));
    wait_for("the feedback reply", || {
        !fake.calls_to("chat.postEphemeral").is_empty()
    })
    .await;
    bot.abort();
    let recorded = std::fs::read_to_string(data.path().join("feedback.jsonl")).unwrap();
    assert!(
        recorded.contains("\"good\":false") && recorded.contains("U1"),
        "{recorded}"
    );

    let page = std::fs::read_to_string(data.path().join("wiki/team/standup.md")).unwrap();
    assert_eq!(page, "Standup is at 10am.");
    let first = &model.requests()[0];
    // The model sees who asked, by id and by name.
    assert_eq!(
        first.history.last().unwrap().text(),
        "<@U1> (Ana): remember that standup is at 10am"
    );
    assert!(first.system.contains("The person asking is <@U1> (Ana)"));
    let tools: Vec<&str> = first.tools.iter().map(|t| t.name.as_str()).collect();
    for expected in [
        "slack_read_channel",
        "share_file",
        "schedule_task",
        "delegate",
        "read_file",
    ] {
        assert!(
            tools.contains(&expected),
            "{expected} missing from {tools:?}"
        );
    }
}

#[tokio::test]
async fn risky_tools_wait_for_the_requesters_click() {
    let data = tempfile::tempdir().unwrap();
    let risky = Arc::new(MockTool::new("risky").with_permission_key("risky"));
    let (engine, _) = engine_with(
        MockProvider::new()
            .calling("risky", json!({"input": "drop table"}))
            .replying("done"),
        data.path(),
        vec![risky.clone()],
    );
    let (fake, api_url) = start_fake_slack().await;
    fake.send(mention(
        "env-1",
        "Ev1",
        "U1",
        "<@UBOT> do the risky thing",
        THREAD_TS,
    ));
    let bot = start_bot(engine, &api_url).await;

    wait_for("the approval card", || card_button(&fake, 0).is_some()).await;
    let id = card_button(&fake, 0).unwrap();
    assert_eq!(risky.call_count(), 0, "nothing runs before the click");

    // A bystander's click is refused privately; the requester's counts.
    fake.send(click("U2", "ost_allow", &id));
    wait_for("the bystander notice", || {
        !fake.calls_to("chat.postEphemeral").is_empty()
    })
    .await;
    assert_eq!(fake.calls_to("chat.postEphemeral")[0]["user"], "U2");
    assert_eq!(risky.call_count(), 0);
    fake.send(click("U1", "ost_allow", &id));

    wait_for("the answer", || answered(&fake, "done")).await;
    bot.abort();
    assert_eq!(risky.call_count(), 1);
    let resolved = fake.calls_to("chat.update");
    assert!(
        resolved
            .iter()
            .any(|u| u["text"].contains("Allowed once by <@U1>")),
        "{resolved:?}"
    );
}

#[tokio::test]
async fn a_denied_tool_does_not_run_and_the_model_is_told() {
    let data = tempfile::tempdir().unwrap();
    let risky = Arc::new(MockTool::new("risky").with_permission_key("risky"));
    let (engine, model) = engine_with(
        MockProvider::new().calling("risky", json!({})),
        data.path(),
        vec![risky.clone()],
    );
    let (fake, api_url) = start_fake_slack().await;
    fake.send(mention("env-1", "Ev1", "U1", "<@UBOT> do it", THREAD_TS));
    let bot = start_bot(engine, &api_url).await;
    wait_for("the approval card", || card_button(&fake, 2).is_some()).await;
    fake.send(click("U1", "ost_deny", &card_button(&fake, 2).unwrap()));
    let stopped = |u: &HashMap<String, String>| u["text"].contains("wasn't approved");
    wait_for("the answer", || {
        fake.calls_to("chat.update").iter().any(stopped)
    })
    .await;
    bot.abort();
    assert_eq!(risky.call_count(), 0);
    assert_eq!(
        model.requests().len(),
        1,
        "no extra model call after a refusal"
    );
    // The refusal is in the saved transcript, so the next message knows about it.
    let saved = std::fs::read_to_string(
        data.path()
            .join("conversations")
            .join(format!("C1-{}.json", THREAD_TS.replace('.', "-"))),
    )
    .unwrap();
    assert!(saved.contains("was refused"), "{saved}");
}

#[tokio::test]
async fn shared_images_are_saved_and_shown_to_the_model() {
    let data = tempfile::tempdir().unwrap();
    let (engine, model) = engine_with(
        MockProvider::new().replying("a cat"),
        data.path(),
        Vec::new(),
    );
    let (fake, api_url) = start_fake_slack().await;
    let base = fake.base.lock().clone();
    let event = json!({
        "type": "events_api", "envelope_id": "env-1",
        "payload": {"event_id": "Ev1", "event": {
            "type": "message", "channel_type": "im", "subtype": "file_share", "user": "U1",
            "text": "what is this?", "ts": "1700000001.000100", "channel": "D1",
            "files": [{"name": "cat.png", "mimetype": "image/png", "size": PNG.len(),
                       "url_private_download": format!("{base}/files/cat.png")}]
        }}
    });
    fake.respond("conversations.history", json!({"ok": true, "messages": []}));
    fake.send(event);
    let bot = start_bot(engine, &api_url).await;
    wait_for("the answer", || answered(&fake, "a cat")).await;
    bot.abort();

    let saved = std::fs::read(data.path().join("workspaces/D1/inbox/cat.png")).unwrap();
    assert_eq!(saved, PNG);
    let request = model.last_request().unwrap();
    let json = serde_json::to_string(&request.history.last().unwrap()).unwrap();
    assert!(
        json.contains("data:image/png;base64,"),
        "the image went to the model"
    );
    assert!(json.contains("inbox/cat.png"));
}

#[tokio::test]
async fn share_file_uploads_into_the_thread() {
    let data = tempfile::tempdir().unwrap();
    let (engine, _) = engine_with(
        MockProvider::new()
            .calling(
                "write_file",
                json!({"path": "report.csv", "content": "a,b\n1,2"}),
            )
            .calling(
                "share_file",
                json!({"path": "report.csv", "comment": "here you go"}),
            )
            .replying("Shared."),
        data.path(),
        Vec::new(),
    );
    let (fake, api_url) = start_fake_slack().await;
    fake.send(mention(
        "env-1",
        "Ev1",
        "U1",
        "<@UBOT> make me a csv",
        THREAD_TS,
    ));
    let bot = start_bot(engine, &api_url).await;
    wait_for("the answer", || answered(&fake, "Shared.")).await;
    bot.abort();

    assert_eq!(*fake.uploads.lock(), vec![7]);
    let complete = fake.calls_to("files.completeUploadExternal");
    assert_eq!(complete[0]["channel_id"], "C1");
    assert_eq!(complete[0]["thread_ts"], THREAD_TS);
    assert_eq!(complete[0]["initial_comment"], "here you go");
    assert!(complete[0]["files"].contains("\"F1\""));
}

#[tokio::test]
async fn private_channels_are_only_read_for_their_members() {
    let data = tempfile::tempdir().unwrap();
    let (engine, model) = engine_with(
        MockProvider::new()
            .calling("slack_read_channel", json!({"channel": "C2PRIVATE1"}))
            .replying("could not read it"),
        data.path(),
        Vec::new(),
    );
    let (fake, api_url) = start_fake_slack().await;
    fake.respond(
        "conversations.info",
        json!({"ok": true, "channel": {"id": "C2PRIVATE1", "is_private": true, "is_member": true}}),
    );
    fake.respond(
        "conversations.members",
        json!({"ok": true, "members": ["U9"]}),
    );
    fake.send(mention(
        "env-1",
        "Ev1",
        "U1",
        "<@UBOT> what did #secret say?",
        THREAD_TS,
    ));
    let bot = start_bot(engine, &api_url).await;
    wait_for("the answer", || answered(&fake, "could not read it")).await;
    bot.abort();
    assert!(
        fake.calls_to("conversations.history").is_empty(),
        "the private channel was never read"
    );
    let told = model
        .last_request()
        .unwrap()
        .history
        .iter()
        .any(|e| e.text().contains("private"));
    assert!(told, "the model was told why");
}

#[tokio::test]
async fn saying_stop_cancels_the_running_task() {
    let data = tempfile::tempdir().unwrap();
    let gate = Arc::new(Gate::default());
    let (engine, _) = allow_slow_task(
        data.path(),
        MockProvider::new()
            .calling("slow_task", json!({}))
            .replying("never reached"),
        gate.clone(),
    );
    let (fake, api_url) = start_fake_slack().await;
    fake.respond("conversations.replies", json!({"ok": true, "messages": []}));
    fake.send(mention(
        "env-1",
        "Ev1",
        "U1",
        "<@UBOT> start the long job",
        THREAD_TS,
    ));
    let bot = start_bot(engine.clone(), &api_url).await;
    gate.started.notified().await;
    fake.send(mention(
        "env-2",
        "Ev2",
        "U1",
        "<@UBOT> stop",
        "1700000000.000500",
    ));

    wait_for("the stop", || answered(&fake, "_Stopped._")).await;
    bot.abort();
    assert_eq!(fake.calls_to("reactions.add")[0]["name"], "octagonal_sign");
    assert!(!engine.is_running(&format!("C1-{THREAD_TS}")));
}

#[tokio::test]
async fn scheduled_tasks_run_and_post_in_their_channel() {
    let data = tempfile::tempdir().unwrap();
    let (engine, model) = engine_with(
        MockProvider::new()
            .calling(
                "schedule_task",
                json!({"prompt": "Summarise #support", "when": "every day at 09:00"}),
            )
            .replying("Scheduled.")
            .replying("Support summary: all quiet."),
        data.path(),
        Vec::new(),
    );
    let (fake, api_url) = start_fake_slack().await;
    fake.send(mention(
        "env-1",
        "Ev1",
        "U1",
        "<@UBOT> every morning summarise #support",
        THREAD_TS,
    ));
    let slack = Arc::new(
        Slack::connect(&api_url, "xoxb-test", "xapp-test")
            .await
            .unwrap(),
    );
    let bot = tokio::spawn(slack.clone().run(engine.clone(), None));
    wait_for("the schedule", || !engine.schedules().list().is_empty()).await;
    wait_for("the confirmation", || answered(&fake, "Scheduled.")).await;
    let schedule = engine.schedules().list()[0].clone();
    assert_eq!(
        schedule.target,
        Target {
            channel: "C1".into(),
            thread: None
        }
    );
    assert_eq!(schedule.created_by.as_deref(), Some("U1"));

    // Make it due and run it, as the ticker would.
    engine.schedules().set_next_run_for_test(
        &schedule.id,
        jiff::Timestamp::now() - jiff::SignedDuration::from_mins(1),
    );
    assert_eq!(slack.run_due_schedules(&engine).await, 1);
    bot.abort();

    let runs: Vec<_> = fake
        .calls_to("chat.postMessage")
        .into_iter()
        .filter(|p| p["text"].contains("Running scheduled task"))
        .collect();
    assert_eq!(runs.len(), 1);
    assert!(
        !runs[0].contains_key("thread_ts"),
        "posted in the channel, not the thread"
    );
    assert!(answered(&fake, "Support summary: all quiet."));
    assert!(
        model
            .last_request()
            .unwrap()
            .history
            .last()
            .unwrap()
            .text()
            .contains("Scheduled task: Summarise #support")
    );
    assert!(
        engine.schedules().list()[0].next_run > jiff::Timestamp::now(),
        "it repeats"
    );
}

// ── the engine ──────────────────────────────────────────────────────────

#[tokio::test]
async fn a_known_conversation_only_takes_new_messages() {
    let data = tempfile::tempdir().unwrap();
    let (engine, model) = engine_with(
        MockProvider::new().replying("one").replying("two"),
        data.path(),
        Vec::new(),
    );
    let (tx, _rx) = mpsc::unbounded_channel();
    let first = user("1.0", "<@U1>: hi");

    assert_eq!(
        engine
            .reply(request("c", vec![first.clone()], &tx))
            .await
            .unwrap(),
        Reply::Answer("one".into())
    );
    assert_eq!(engine.cursor("c").as_deref(), Some("1.0"));
    assert_eq!(
        engine
            .reply(request("c", vec![first.clone()], &tx))
            .await
            .unwrap(),
        Reply::Nothing
    );

    let second = user("2.0", "<@U1>: and now?");
    assert_eq!(
        engine
            .reply(request("c", vec![first, second], &tx))
            .await
            .unwrap(),
        Reply::Answer("two".into())
    );
    let texts: Vec<String> = model
        .last_request()
        .unwrap()
        .history
        .iter()
        .map(|e| e.text().to_string())
        .collect();
    assert_eq!(texts, ["<@U1>: hi", "one", "<@U1>: and now?"]);
}

/// A tool that holds the turn open until the test lets it go.
#[derive(Debug, Default)]
struct Gate {
    started: Notify,
    release: Notify,
}

#[async_trait]
impl Tool for Gate {
    fn id(&self) -> &str {
        "slow_task"
    }
    fn description(&self) -> &str {
        "Takes a while."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn source(&self) -> ToolSource {
        ToolSource::Builtin
    }
    fn permission(&self, _args: &Value) -> PermissionRequest {
        PermissionRequest::new("slow_task", "*")
    }
    async fn execute(
        &self,
        _args: Value,
        _ctx: &ToolContext,
    ) -> openslacktag_core::Result<ToolOutcome> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(ToolOutcome::text("finished"))
    }
}

/// An engine with the gate tool, allowed by rule so no approval is needed.
fn allow_slow_task(
    data: &Path,
    provider: MockProvider,
    gate: Arc<Gate>,
) -> (Arc<Engine>, Arc<MockProvider>) {
    let provider = Arc::new(provider);
    let mut parts = Parts::minimal(provider.clone(), "mock-model", data);
    parts.extra_tools = vec![gate];
    parts
        .rules
        .push(openslacktag_core::permission::Rule::for_any(
            "slow_task",
            openslacktag_core::permission::Action::Allow,
        ));
    (Arc::new(Engine::new(parts).unwrap()), provider)
}

#[tokio::test]
async fn a_message_sent_mid_task_steers_the_running_task() {
    let data = tempfile::tempdir().unwrap();
    let gate = Arc::new(Gate::default());
    let (engine, model) = allow_slow_task(
        data.path(),
        MockProvider::new()
            .calling("slow_task", json!({}))
            .replying("done, in French"),
        gate.clone(),
    );
    let (tx, _rx) = mpsc::unbounded_channel();
    let first = user("1.0", "<@U1>: write the report");

    let task = {
        let (engine, tx, first) = (engine.clone(), tx.clone(), first.clone());
        tokio::spawn(async move { engine.reply(request("c", vec![first], &tx)).await })
    };
    gate.started.notified().await;

    let extra = user("2.0", "<@U1>: in French please");
    let steered = engine
        .reply(request("c", vec![first.clone(), extra.clone()], &tx))
        .await
        .unwrap();
    assert_eq!(steered, Reply::Steered);

    gate.release.notify_one();
    assert_eq!(
        task.await.unwrap().unwrap(),
        Reply::Answer("done, in French".into())
    );

    let history = &model.last_request().unwrap().history;
    let steers: Vec<&Entry> = history
        .iter()
        .filter(|e| e.text().contains("in French please"))
        .collect();
    assert_eq!(steers.len(), 1);
    assert!(steers[0].text().starts_with(STEER_NOTE));
    assert_eq!(engine.cursor("c").as_deref(), Some("2.0"));
    assert_eq!(
        engine
            .reply(request("c", vec![first, extra], &tx))
            .await
            .unwrap(),
        Reply::Nothing
    );
}

#[tokio::test]
async fn delegate_runs_helpers_and_returns_their_results() {
    let data = tempfile::tempdir().unwrap();
    let (engine, model) = engine_with(
        MockProvider::new()
            .calling("delegate", json!({"tasks": ["Find the price of plan A"]}))
            .replying("Plan A costs $10.")
            .replying("Plan A is $10 a month."),
        data.path(),
        Vec::new(),
    );
    let (tx, _rx) = mpsc::unbounded_channel();
    let answer = engine
        .reply(request(
            "c",
            vec![user("1.0", "<@U1>: price of plan A?")],
            &tx,
        ))
        .await
        .unwrap();
    assert_eq!(answer, Reply::Answer("Plan A is $10 a month.".into()));
    let requests = model.requests();
    // The helper got only its task, and a prompt that says it is a helper.
    assert_eq!(requests[1].history.len(), 1);
    assert_eq!(requests[1].history[0].text(), "Find the price of plan A");
    assert!(requests[1].system.contains("You are a helper"));
    assert!(
        !requests[1].tools.iter().any(|t| t.name == "delegate"),
        "helpers cannot delegate"
    );
    let result = requests[2]
        .history
        .iter()
        .any(|e| e.text().contains("## Task 1") && e.text().contains("Plan A costs $10."));
    assert!(result);
}

#[tokio::test]
async fn skills_and_instructions_reach_the_prompt() {
    let data = tempfile::tempdir().unwrap();
    let skills_dir = data.path().join("skills/release-notes");
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::write(
        skills_dir.join("SKILL.md"),
        "---\nname: Release notes\ndescription: How we write release notes\n---\nUse past tense.",
    )
    .unwrap();
    let provider = Arc::new(
        MockProvider::new()
            .calling("skill_read", json!({"name": "release-notes"}))
            .replying("Done, in past tense."),
    );
    let mut parts = Parts::minimal(provider.clone(), "mock-model", data.path());
    parts.skills = openslacktag::skills::Skills::load(&data.path().join("skills")).unwrap();
    parts.instructions = Some("Always answer in British English.".into());
    let engine = Engine::new(parts).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    engine
        .reply(request("c", vec![user("1.0", "<@U1>: write notes")], &tx))
        .await
        .unwrap();

    let requests = provider.requests();
    assert!(
        requests[0]
            .system
            .contains("- Release notes (`release-notes`): How we write release notes")
    );
    assert!(
        requests[0]
            .system
            .contains("Always answer in British English.")
    );
    let read = requests[1]
        .history
        .iter()
        .any(|e| e.text().contains("Use past tense."));
    assert!(read, "the skill body came back from skill_read");
}

#[tokio::test]
async fn usage_is_recorded_per_task() {
    let data = tempfile::tempdir().unwrap();
    let (engine, _) = engine_with(MockProvider::new().replying("hi"), data.path(), Vec::new());
    let (tx, _rx) = mpsc::unbounded_channel();
    engine
        .reply(request("c", vec![user("1.0", "<@U1>: hi")], &tx))
        .await
        .unwrap();
    let summary = openslacktag::usage::summary(data.path(), 1).unwrap();
    assert!(summary.contains("mock-model"), "{summary}");
}

#[tokio::test]
async fn a_long_conversation_is_compacted_before_the_model_sees_it() {
    let data = tempfile::tempdir().unwrap();
    let provider = Arc::new(
        MockProvider::new()
            .replying("## Goal\nsummary text")
            .replying("answer"),
    );
    let mut parts = Parts::minimal(provider.clone(), "mock-model", data.path());
    parts.compact_at_tokens = 200;
    let engine = Engine::new(parts).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    // Ends with a message from a person, as every real batch does.
    let incoming: Vec<Incoming> = (0..21)
        .map(|i| Incoming {
            cursor: Some(format!("{i}.0")),
            entry: if i % 2 == 0 {
                Entry::user(format!("<@U1>: message {i} {}", "x".repeat(200)))
            } else {
                Entry::assistant(format!("reply {i}"))
            },
        })
        .collect();

    assert_eq!(
        engine.reply(request("c", incoming, &tx)).await.unwrap(),
        Reply::Answer("answer".into())
    );
    let requests = provider.requests();
    assert!(
        requests[0].system.contains("handoff note"),
        "the first call is the summary"
    );
    let seen = &requests[1].history;
    assert!(seen[0].is_pinned() && seen[0].text().contains("summary text"));
    assert!(seen.len() < 21);
    assert_eq!(
        seen.last().unwrap().text(),
        format!("<@U1>: message 20 {}", "x".repeat(200))
    );
}

/// A model whose first call pauses until the test lets it go, so a message
/// can arrive while that call is in flight.
#[derive(Debug)]
struct PausedFirstCall {
    inner: MockProvider,
    calls: AtomicUsize,
    started: Arc<Notify>,
    release: Arc<Notify>,
}

impl PausedFirstCall {
    fn new(inner: MockProvider) -> Self {
        Self {
            inner,
            calls: AtomicUsize::default(),
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        }
    }
}

#[async_trait]
impl openslacktag_core::Provider for PausedFirstCall {
    fn id(&self) -> &str {
        "paused"
    }

    async fn list_models(
        &self,
    ) -> openslacktag_core::Result<Vec<openslacktag_core::provider::ModelInfo>> {
        Ok(Vec::new())
    }

    fn stream_chat(
        &self,
        request: openslacktag_core::provider::ChatRequest,
    ) -> futures::stream::BoxStream<'static, openslacktag_core::provider::StreamEvent> {
        use futures::StreamExt;
        let stream = self.inner.stream_chat(request);
        if self.calls.fetch_add(1, Ordering::SeqCst) > 0 {
            return stream;
        }
        let (started, release) = (self.started.clone(), self.release.clone());
        futures::stream::once(async move {
            started.notify_one();
            release.notified().await;
            stream
        })
        .flatten()
        .boxed()
    }
}

fn paused_engine(provider: Arc<PausedFirstCall>, data_dir: &Path) -> Arc<Engine> {
    Arc::new(Engine::new(Parts::minimal(provider, "mock-model", data_dir)).unwrap())
}

#[tokio::test]
async fn a_message_sent_during_the_last_model_call_is_still_answered() {
    let data = tempfile::tempdir().unwrap();
    let provider = Arc::new(PausedFirstCall::new(
        MockProvider::new()
            .replying("first answer")
            .replying("also covered the follow-up"),
    ));
    let engine = paused_engine(provider.clone(), data.path());
    let (tx, _rx) = mpsc::unbounded_channel();
    let first = user("1.0", "<@U1>: summarise the doc");
    let follow_up = user("2.0", "<@U1>: and list the owners");

    let task = {
        let (engine, tx, first) = (engine.clone(), tx.clone(), first.clone());
        tokio::spawn(async move { engine.reply(request("c", vec![first], &tx)).await })
    };
    provider.started.notified().await;
    let steered = engine
        .reply(request("c", vec![first.clone(), follow_up.clone()], &tx))
        .await
        .unwrap();
    assert_eq!(steered, Reply::Steered);
    provider.release.notify_one();

    assert_eq!(
        task.await.unwrap().unwrap(),
        Reply::Answer("also covered the follow-up".into())
    );
    let history = &provider.inner.last_request().unwrap().history;
    let mentions = history
        .iter()
        .filter(|e| e.text().contains("and list the owners"))
        .count();
    assert_eq!(mentions, 1, "the follow-up reached the model exactly once");
}

#[tokio::test]
async fn a_message_accepted_by_a_task_that_then_fails_is_kept() {
    let data = tempfile::tempdir().unwrap();
    let provider = Arc::new(PausedFirstCall::new(
        MockProvider::new()
            .failing("400 bad request", Some(400))
            .replying("recovered"),
    ));
    let engine = paused_engine(provider.clone(), data.path());
    let (tx, _rx) = mpsc::unbounded_channel();
    let first = user("1.0", "<@U1>: draft the memo");
    let follow_up = user("2.0", "<@U1>: keep it under a page");

    let task = {
        let (engine, tx, first) = (engine.clone(), tx.clone(), first.clone());
        tokio::spawn(async move { engine.reply(request("c", vec![first], &tx)).await })
    };
    provider.started.notified().await;
    let steered = engine
        .reply(request("c", vec![first.clone(), follow_up.clone()], &tx))
        .await
        .unwrap();
    assert_eq!(steered, Reply::Steered);
    provider.release.notify_one();
    assert!(task.await.unwrap().is_err(), "the task failed");

    let next = user("3.0", "<@U1>: try again");
    let answer = engine
        .reply(request("c", vec![first, follow_up, next], &tx))
        .await
        .unwrap();
    assert_eq!(answer, Reply::Answer("recovered".into()));
    let history = &provider.inner.last_request().unwrap().history;
    assert!(
        history
            .iter()
            .any(|e| e.text().contains("keep it under a page")),
        "{history:#?}"
    );
}

fn channel_message(envelope_id: &str, event_id: &str, text: &str, ts: &str) -> Value {
    json!({
        "type": "events_api",
        "envelope_id": envelope_id,
        "payload": {
            "event_id": event_id,
            "event": {
                "type": "message", "channel_type": "channel", "user": "U1", "text": text,
                "ts": ts, "channel": "C0123456789"
            }
        }
    })
}

#[tokio::test]
async fn channel_triggers_answer_matching_messages_in_their_thread() {
    let data = tempfile::tempdir().unwrap();
    let (engine, model) = engine_with(
        MockProvider::new()
            .replying("Triaged: billing issue, assigned to <@U7>.")
            .replying("SKIP"),
        data.path(),
        Vec::new(),
    );
    engine
        .schedules()
        .add_watch(
            "C0123456789",
            Some("help"),
            "Triage support requests",
            Some("U9".into()),
        )
        .unwrap();
    let (fake, api_url) = start_fake_slack().await;
    fake.respond("conversations.replies", json!({"ok": true, "messages": []}));
    fake.respond(
        "conversations.info",
        json!({"ok": true, "channel": {"is_member": true}}),
    );
    fake.send(channel_message(
        "e1",
        "Ev1",
        "I need help with billing",
        "1700000100.000100",
    ));
    fake.send(channel_message(
        "e2",
        "Ev2",
        "lunch anyone?",
        "1700000101.000100",
    ));
    fake.send(channel_message(
        "e3",
        "Ev3",
        "help, is this normal?",
        "1700000102.000100",
    ));
    let bot = start_bot(engine, &api_url).await;

    wait_for("the triage", || {
        answered(&fake, "Triaged: billing issue, assigned to <@U7>.")
    })
    .await;
    wait_for("the skip", || model.requests().len() == 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    bot.abort();

    let posts = fake.calls_to("chat.postMessage");
    assert_eq!(
        posts.len(),
        1,
        "a skipped message gets no reply, not even a placeholder: {posts:?}"
    );
    assert_eq!(posts[0]["thread_ts"], "1700000100.000100");
    let leftovers: Vec<String> = ["transcripts", "workspaces"]
        .iter()
        .filter_map(|d| std::fs::read_dir(data.path().join(d)).ok())
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("1700000102"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "a skipped run leaves nothing: {leftovers:?}"
    );
    let requests = model.requests();
    assert!(requests[0].system.contains("Triage support requests"));
    assert!(
        requests[0]
            .history
            .last()
            .unwrap()
            .text()
            .contains("I need help with billing")
    );
}

#[tokio::test]
async fn app_posts_set_off_triggers() {
    let data = tempfile::tempdir().unwrap();
    let (engine, model) = engine_with(
        MockProvider::new().replying("Looking into db-1."),
        data.path(),
        Vec::new(),
    );
    engine
        .schedules()
        .add_watch(
            "C0123456789",
            Some("disk"),
            "Investigate alerts",
            Some("U9".into()),
        )
        .unwrap();
    let (fake, api_url) = start_fake_slack().await;
    fake.respond("conversations.replies", json!({"ok": true, "messages": []}));
    fake.respond(
        "conversations.info",
        json!({"ok": true, "channel": {"is_member": true}}),
    );
    fake.send(json!({
        "type": "events_api",
        "envelope_id": "e1",
        "payload": {
            "event_id": "Ev1",
            "event": {
                "type": "message", "subtype": "bot_message", "bot_id": "B1",
                "channel_type": "channel", "channel": "C0123456789", "ts": "1700000100.000100",
                "text": "",
                "attachments": [{"title": "Disk full", "text": "db-1 is at 99% disk"}]
            }
        }
    }));
    let bot = start_bot(engine, &api_url).await;
    wait_for("the answer", || answered(&fake, "Looking into db-1.")).await;
    bot.abort();
    let requests = model.requests();
    let asked = requests[0].history.last().unwrap().text();
    assert!(asked.contains("db-1 is at 99% disk"), "{asked}");
}

#[tokio::test]
async fn triggers_stop_when_their_creator_cannot_read_the_channel() {
    let data = tempfile::tempdir().unwrap();
    let (engine, model) = engine_with(
        MockProvider::new().replying("should not run"),
        data.path(),
        Vec::new(),
    );
    engine
        .schedules()
        .add_watch(
            "C0123456789",
            None,
            "Summarise everything",
            Some("U9".into()),
        )
        .unwrap();
    let (fake, api_url) = start_fake_slack().await;
    fake.respond(
        "conversations.info",
        json!({"ok": true, "channel": {"is_private": true}}),
    );
    fake.respond(
        "conversations.members",
        json!({"ok": true, "members": ["U1"]}),
    );
    fake.send(channel_message(
        "e1",
        "Ev1",
        "secret plans",
        "1700000100.000100",
    ));
    let bot = start_bot(engine, &api_url).await;
    wait_for("the membership check", || {
        !fake.calls_to("conversations.members").is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    bot.abort();
    assert!(model.requests().is_empty());
    assert!(fake.calls_to("chat.postMessage").is_empty());
}
