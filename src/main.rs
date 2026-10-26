use std::io::{BufRead, Write};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use openslacktag::config::Config;
use openslacktag::engine::{Incoming, Progress, Reply, Request};
use openslacktag::permissions::{Approval, Approver};
use openslacktag::slack::Slack;
use openslacktag_core::message::Entry;
use openslacktag_core::tool::Decision;
use tokio::sync::mpsc;

const HELP: &str = "\
OpenSlackTag: an open-source AI teammate for Slack.

Usage: openslacktag <command>

Commands:
  setup                Guided setup: pick a model, connect Slack, write .env
  slack                Run the teammate in Slack
  chat                 Talk to the teammate here in the terminal (no Slack needed)
  doctor               Check the model, Slack, the sandbox and MCP servers
  schedules            List scheduled tasks (`schedules cancel <id>` to remove one)
  usage [days]         Token usage per day and model (default: 30 days)
  audit [count]        The last tool calls: who asked, what ran, how it ended (default: 50)
  suggest [days]       Suggest automations from recent conversations (default: 7 days)
  help                 Show this help

Settings are read from .env in the current folder. See README.md for all of them.";

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "openslacktag=info,openslacktag_mcp=warn,warn".into()),
        )
        .with_target(false)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map_or("help", String::as_str);
    let number = |default: u64| args.get(1).and_then(|n| n.parse().ok()).unwrap_or(default);
    let result = match command {
        "setup" => openslacktag::setup::setup().await,
        "slack" => slack().await,
        "chat" => chat().await,
        "doctor" => match Config::from_env() {
            Ok(config) => openslacktag::setup::doctor(&config).await,
            Err(e) => Err(e),
        },
        "schedules" => schedules(
            args.get(1).map(String::as_str),
            args.get(2).map(String::as_str),
        ),
        "usage" => Config::from_env().and_then(|config| {
            println!(
                "{}",
                openslacktag::usage::summary(&config.data_dir, number(30) as i64)?
            );
            Ok(())
        }),
        "suggest" => suggest(number(7)).await,
        "audit" => Config::from_env().and_then(|config| {
            println!(
                "{}",
                openslacktag::audit::tail(&config.data_dir, number(50) as usize)?
            );
            Ok(())
        }),
        "help" | "--help" | "-h" => {
            println!("{HELP}");
            Ok(())
        }
        other => Err(anyhow::anyhow!("unknown command {other:?}\n\n{HELP}")),
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn slack() -> Result<()> {
    let config = Config::from_env()?;
    let (bot, app) = config.slack_tokens()?;
    let slack = Arc::new(
        Slack::connect(&config.slack_api_url, bot, app)
            .await?
            .with_access(config.allowed_users.clone(), config.approvers.clone()),
    );
    let engine = Arc::new(config.engine().await?);
    tracing::info!(
        "Using {} with tools: {}",
        engine.model(),
        engine.tool_names().await.join(", ")
    );
    slack.run(engine, config.suggestions_channel.clone()).await
}

/// Asks in the terminal: the chat's stand-in for Slack's approval buttons.
#[derive(Debug)]
struct TerminalApprover;

#[async_trait]
impl Approver for TerminalApprover {
    async fn approve(&self, approval: &Approval) -> Decision {
        let detail = approval
            .detail
            .as_deref()
            .map(|d| format!(" with {}", openslacktag::one_line(d, 200)))
            .unwrap_or_default();
        let question = format!(
            "\n  ? Allow {} on {}{detail}? [y]es / [a]lways in this chat / [N]o: ",
            approval.tool, approval.target
        );
        let answer = tokio::task::spawn_blocking(move || {
            print!("{question}");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line);
            line.trim().to_lowercase()
        })
        .await
        .unwrap_or_default();
        match answer.as_str() {
            "y" | "yes" => Decision::Allow,
            "a" | "always" => Decision::AllowAlways,
            _ => Decision::Deny,
        }
    }
}

/// A terminal chat on the same engine Slack uses, so everything can be tried without Slack.
async fn chat() -> Result<()> {
    let config = Config::from_env()?;
    let engine = config.engine().await?;
    println!(
        "Chatting with {} (tools: {}).\nType a message and press Enter. /new starts over, /quit leaves.\n",
        engine.model(),
        engine.tool_names().await.join(", ")
    );
    let approver: Arc<dyn Approver> = Arc::new(TerminalApprover);
    let mut conversation = new_conversation();
    let stdin = std::io::stdin();
    loop {
        print!("you> ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Ok(());
        }
        match line.trim() {
            "" => continue,
            "/quit" | "/exit" => return Ok(()),
            "/new" => {
                conversation = new_conversation();
                println!("(new conversation)\n");
                continue;
            }
            text => {
                let (progress, mut notes) = mpsc::unbounded_channel::<Progress>();
                let printer = tokio::spawn(async move {
                    while let Some(note) = notes.recv().await {
                        if let Progress::Doing(note) = note {
                            println!("  … {}", openslacktag::one_line(&note, 120));
                        }
                    }
                });
                let reply = engine
                    .reply(Request {
                        conversation: &conversation,
                        incoming: vec![Incoming {
                            cursor: None,
                            entry: Entry::user(format!("<@you>: {text}")),
                        }],
                        progress: &progress,
                        approver: approver.clone(),
                        target: None,
                        requester: None,
                        context: Some(
                            "You are in a terminal chat with the person who runs you, not in Slack. Slack \
                             tools and scheduling are not available here."
                                .into(),
                        ),
                        tools: Vec::new(),
                    })
                    .await;
                drop(progress);
                let _ = printer.await;
                match reply {
                    Ok(Reply::Answer(answer)) => println!("\nteammate> {answer}\n"),
                    Ok(_) => {}
                    Err(e) => println!("\nerror: {e:#}\n"),
                }
            }
        }
    }
}

fn schedules(action: Option<&str>, id: Option<&str>) -> Result<()> {
    let config = Config::from_env()?;
    let schedules = openslacktag::schedule::Schedules::open(config.data_dir.join("schedules.json"));
    match (action, id) {
        (Some("cancel"), Some(id)) => {
            if schedules.cancel(id)? {
                println!("Cancelled {id}.");
            } else {
                anyhow::bail!("no routine {id}");
            }
        }
        (None, _) => {
            let (all, watches) = (schedules.list(), schedules.watches());
            if all.is_empty() && watches.is_empty() {
                println!("No routines. Ask the teammate in Slack to set one up.");
            }
            for s in &all {
                println!("- {}", schedules.describe(s));
            }
            for w in &watches {
                println!("- {}", openslacktag::schedule::describe_watch(w));
            }
        }
        _ => anyhow::bail!("usage: openslacktag schedules [cancel <id>]"),
    }
    Ok(())
}

async fn suggest(days: u64) -> Result<()> {
    let config = Config::from_env()?;
    let provider = config.provider();
    match openslacktag::suggest::suggest(provider.as_ref(), config.model()?, &config.data_dir, days)
        .await?
    {
        Some(text) => println!("{text}"),
        None => {
            println!("Nothing recurs often enough to suggest yet (looked at the last {days} days).")
        }
    }
    Ok(())
}

fn new_conversation() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!("local-{secs}")
}
