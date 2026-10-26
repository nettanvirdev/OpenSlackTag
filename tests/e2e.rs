//! End to end, with no Slack account and no model: a fake Slack (Web API plus
//! the Socket Mode WebSocket) and a scripted model drive the real code paths.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use openslacktag::engine::{Engine, Incoming, Parts, Reply};
use openslacktag::slack::Slack;
use openslacktag_agent::STEER_NOTE;
use openslacktag_core::message::Entry;
use openslacktag_core::tool::{PermissionRequest, Tool, ToolContext, ToolOutcome, ToolSource};
use openslacktag_mcp::provider::McpProvider;
use openslacktag_mock::MockProvider;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};

const THREAD_TS: &str = "1700000000.000100";

// ── a fake Slack ────────────────────────────────────────────────────────

#[derive(Default)]
struct FakeSlack {
    ws_url: Mutex<String>,
    /// Socket Mode frames to push after `hello`.
    envelopes: Vec<String>,
    /// Every Web API call, as (method, form).
    calls: Mutex<Vec<(String, HashMap<String, String>)>>,
    /// Every frame the bot sent back over the socket.
    acks: Mutex<Vec<String>>,
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
}

async fn api(
    State(fake): State<Arc<FakeSlack>>,
    UrlPath(method): UrlPath<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Json<Value> {
    fake.calls.lock().push((method.clone(), form));
    Json(match method.as_str() {
        "auth.test" => json!({"ok": true, "user_id": "UBOT"}),
        "apps.connections.open" => json!({"ok": true, "url": *fake.ws_url.lock()}),
        "conversations.replies" => json!({"ok": true, "messages": [
            {"user": "U1", "text": "<@UBOT> remember that standup is at 10am", "ts": THREAD_TS}
        ]}),
        "chat.postMessage" => json!({"ok": true, "ts": "1700000000.000200"}),
        "chat.update" | "chat.delete" => json!({"ok": true}),
        _ => json!({"ok": false, "error": "unknown_method"}),
    })
}

async fn socket(State(fake): State<Arc<FakeSlack>>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |mut socket| async move {
        let _ = socket
            .send(Message::Text(json!({"type": "hello"}).to_string().into()))
            .await;
        for envelope in &fake.envelopes {
            let _ = socket.send(Message::Text(envelope.clone().into())).await;
        }
        while let Some(Ok(frame)) = socket.recv().await {
            if let Message::Text(text) = frame {
                fake.acks.lock().push(text.to_string());
            }
        }
    })
}

async fn start_fake_slack(envelopes: Vec<String>) -> (Arc<FakeSlack>, String) {
    let fake = Arc::new(FakeSlack {
        envelopes,
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    *fake.ws_url.lock() = format!("ws://{addr}/ws");
    let app = Router::new()
        .route("/api/{method}", post(api))
        .route("/ws", get(socket))
        .with_state(fake.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (fake, format!("http://{addr}/api"))
}

fn mention(envelope_id: &str, event_id: &str) -> String {
    json!({
        "type": "events_api",
        "envelope_id": envelope_id,
        "payload": {
            "event_id": event_id,
            "event": {
                "type": "app_mention",
                "user": "U1",
                "text": "<@UBOT> remember that standup is at 10am",
                "ts": THREAD_TS,
                "channel": "C1"
            }
        }
    })
    .to_string()
}

async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..200 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

fn engine(
    provider: MockProvider,
    data_dir: &Path,
    extra_tools: Vec<Arc<dyn Tool>>,
) -> (Engine, Arc<MockProvider>) {
    let provider = Arc::new(provider);
    let engine = Engine::new(Parts {
        provider: provider.clone(),
        model: "mock-model".into(),
        data_dir: data_dir.to_path_buf(),
        sandbox: None,
        mcp: Arc::new(McpProvider::new()),
        compact_at_tokens: 100_000,
        extra_tools,
    })
    .unwrap();
    (engine, provider)
}

// ── the tests ───────────────────────────────────────────────────────────

#[tokio::test]
async fn a_mention_is_answered_in_its_thread_after_using_a_tool() {
    let data = tempfile::tempdir().unwrap();
    let (engine, model) = engine(
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
    // The same event delivered twice, as Slack does on a retry.
    let (fake, api_url) =
        start_fake_slack(vec![mention("env-1", "Ev1"), mention("env-2", "Ev1")]).await;

    let slack = Arc::new(
        Slack::connect(&api_url, "xoxb-test", "xapp-test")
            .await
            .unwrap(),
    );
    assert_eq!(slack.bot_user_id(), "UBOT");
    let runner = tokio::spawn(slack.run(Arc::new(engine)));

    let is_final =
        |f: &HashMap<String, String>| f["text"].contains("10am") && !f["text"].starts_with('_');
    wait_for("the final answer", || {
        fake.calls_to("chat.update").iter().any(is_final)
    })
    .await;
    runner.abort();

    // Both envelopes were acknowledged; the duplicate event got no second answer.
    let acks = fake.acks.lock().clone();
    assert!(
        acks.iter().any(|a| a.contains("env-1")) && acks.iter().any(|a| a.contains("env-2")),
        "{acks:?}"
    );
    let posts = fake.calls_to("chat.postMessage");
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert_eq!(posts[0]["thread_ts"], THREAD_TS);
    assert_eq!(posts[0]["channel"], "C1");

    // Progress was shown, then the answer converted to Slack formatting.
    let updates = fake.calls_to("chat.update");
    assert!(
        updates
            .iter()
            .any(|u| u["text"].starts_with("_Using wiki_write: team/standup")),
        "{updates:?}"
    );
    assert_eq!(
        updates.last().unwrap()["text"],
        "Noted: standup is at *10am*. Saved to <https://example.com/wiki|the wiki>."
    );

    // The tool really ran, the model saw who asked, and the thread was saved.
    let page = std::fs::read_to_string(data.path().join("wiki/team/standup.md")).unwrap();
    assert_eq!(page, "Standup is at 10am.");
    let first = &model.requests()[0];
    assert_eq!(
        first.history.last().unwrap().text(),
        "<@U1>: remember that standup is at 10am"
    );
    assert!(first.system.contains("team wiki"));
    assert!(
        data.path()
            .join("conversations")
            .read_dir()
            .unwrap()
            .count()
            == 1
    );
}

#[tokio::test]
async fn a_known_conversation_only_takes_new_messages() {
    let data = tempfile::tempdir().unwrap();
    let (engine, model) = engine(
        MockProvider::new().replying("one").replying("two"),
        data.path(),
        Vec::new(),
    );
    let (tx, _rx) = mpsc::unbounded_channel();
    let first = Incoming {
        cursor: Some("1.0".into()),
        entry: Entry::user("<@U1>: hi"),
    };

    assert_eq!(
        engine.reply("c", vec![first.clone()], &tx).await.unwrap(),
        Reply::Answer("one".into())
    );
    assert_eq!(engine.cursor("c").as_deref(), Some("1.0"));

    // Redelivering what was seen does nothing.
    assert_eq!(
        engine.reply("c", vec![first.clone()], &tx).await.unwrap(),
        Reply::Nothing
    );

    let second = Incoming {
        cursor: Some("2.0".into()),
        entry: Entry::user("<@U1>: and now?"),
    };
    assert_eq!(
        engine.reply("c", vec![first, second], &tx).await.unwrap(),
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
#[derive(Debug)]
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

#[tokio::test]
async fn a_message_sent_mid_task_steers_the_running_task() {
    let data = tempfile::tempdir().unwrap();
    let gate = Arc::new(Gate {
        started: Notify::new(),
        release: Notify::new(),
    });
    let (engine, model) = engine(
        MockProvider::new()
            .calling("slow_task", json!({}))
            .replying("done, in French"),
        data.path(),
        vec![gate.clone()],
    );
    let engine = Arc::new(engine);
    let (tx, _rx) = mpsc::unbounded_channel();
    let first = Incoming {
        cursor: Some("1.0".into()),
        entry: Entry::user("<@U1>: write the report"),
    };

    let task = {
        let (engine, tx, first) = (engine.clone(), tx.clone(), first.clone());
        tokio::spawn(async move { engine.reply("c", vec![first], &tx).await })
    };
    gate.started.notified().await;

    let extra = Incoming {
        cursor: Some("2.0".into()),
        entry: Entry::user("<@U1>: in French please"),
    };
    let steered = engine
        .reply("c", vec![first.clone(), extra.clone()], &tx)
        .await
        .unwrap();
    assert_eq!(steered, Reply::Steered);

    gate.release.notify_one();
    assert_eq!(
        task.await.unwrap().unwrap(),
        Reply::Answer("done, in French".into())
    );

    // The model saw the steer as a user message, and only once.
    let history = &model.last_request().unwrap().history;
    let steers: Vec<&Entry> = history
        .iter()
        .filter(|e| e.text().contains("in French please"))
        .collect();
    assert_eq!(steers.len(), 1);
    assert!(steers[0].text().starts_with(STEER_NOTE));
    // Both messages count as seen, so a redelivery starts nothing.
    assert_eq!(engine.cursor("c").as_deref(), Some("2.0"));
    assert_eq!(
        engine.reply("c", vec![first, extra], &tx).await.unwrap(),
        Reply::Nothing
    );
}

#[tokio::test]
async fn a_long_conversation_is_compacted_before_the_model_sees_it() {
    let data = tempfile::tempdir().unwrap();
    let provider = Arc::new(
        MockProvider::new()
            .replying("## Goal\nsummary text")
            .replying("answer"),
    );
    let engine = Engine::new(Parts {
        provider: provider.clone(),
        model: "mock-model".into(),
        data_dir: data.path().to_path_buf(),
        sandbox: None,
        mcp: Arc::new(McpProvider::new()),
        compact_at_tokens: 200,
        extra_tools: Vec::new(),
    })
    .unwrap();
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
        engine.reply("c", incoming, &tx).await.unwrap(),
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
    // The tail starts at a message from a person, right after the summary exchange.
    assert!(matches!(seen[2], Entry::User(_)));
}

/// A model whose first call pauses until the test lets it go, so a message
/// can arrive while that call is in flight.
#[derive(Debug)]
struct PausedFirstCall {
    inner: MockProvider,
    calls: std::sync::atomic::AtomicUsize,
    started: Arc<Notify>,
    release: Arc<Notify>,
}

impl PausedFirstCall {
    fn new(inner: MockProvider) -> Self {
        Self {
            inner,
            calls: Default::default(),
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
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
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
    Arc::new(
        Engine::new(Parts {
            provider,
            model: "mock-model".into(),
            data_dir: data_dir.to_path_buf(),
            sandbox: None,
            mcp: Arc::new(McpProvider::new()),
            compact_at_tokens: 100_000,
            extra_tools: Vec::new(),
        })
        .unwrap(),
    )
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
    let first = Incoming {
        cursor: Some("1.0".into()),
        entry: Entry::user("<@U1>: summarise the doc"),
    };
    let follow_up = Incoming {
        cursor: Some("2.0".into()),
        entry: Entry::user("<@U1>: and list the owners"),
    };

    let task = {
        let (engine, tx, first) = (engine.clone(), tx.clone(), first.clone());
        tokio::spawn(async move { engine.reply("c", vec![first], &tx).await })
    };
    provider.started.notified().await;
    let steered = engine
        .reply("c", vec![first.clone(), follow_up.clone()], &tx)
        .await
        .unwrap();
    assert_eq!(steered, Reply::Steered);
    provider.release.notify_one();

    let answer = task.await.unwrap().unwrap();
    assert_eq!(answer, Reply::Answer("also covered the follow-up".into()));
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
    let first = Incoming {
        cursor: Some("1.0".into()),
        entry: Entry::user("<@U1>: draft the memo"),
    };
    let follow_up = Incoming {
        cursor: Some("2.0".into()),
        entry: Entry::user("<@U1>: keep it under a page"),
    };

    let task = {
        let (engine, tx, first) = (engine.clone(), tx.clone(), first.clone());
        tokio::spawn(async move { engine.reply("c", vec![first], &tx).await })
    };
    provider.started.notified().await;
    let steered = engine
        .reply("c", vec![first.clone(), follow_up.clone()], &tx)
        .await
        .unwrap();
    assert_eq!(steered, Reply::Steered);
    provider.release.notify_one();
    assert!(task.await.unwrap().is_err(), "the task failed");

    // The follow-up was acknowledged, so it must not vanish with the failed task.
    let next = Incoming {
        cursor: Some("3.0".into()),
        entry: Entry::user("<@U1>: try again"),
    };
    let answer = engine
        .reply("c", vec![first, follow_up, next], &tx)
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
