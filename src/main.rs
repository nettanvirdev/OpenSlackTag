use std::io::{BufRead, Write};
use std::sync::Arc;

use anyhow::Result;
use openslacktag::config::Config;
use openslacktag::engine::{Incoming, Reply};
use openslacktag::slack::Slack;
use openslacktag_core::message::Entry;
use tokio::sync::mpsc;

const HELP: &str = "\
OpenSlackTag: an open-source AI teammate for Slack.

Usage: openslacktag <command>

Commands:
  setup    Guided setup: pick a model, connect Slack, write .env
  slack    Run the teammate in Slack
  chat     Talk to the teammate here in the terminal (no Slack needed)
  doctor   Check the model, Slack, the sandbox and MCP servers
  help     Show this help

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

    let command = std::env::args().nth(1).unwrap_or_else(|| "help".into());
    let result = match command.as_str() {
        "setup" => openslacktag::setup::setup().await,
        "slack" => slack().await,
        "chat" => chat().await,
        "doctor" => match Config::from_env() {
            Ok(config) => openslacktag::setup::doctor(&config).await,
            Err(e) => Err(e),
        },
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
            .allow_only(config.allowed_users.clone()),
    );
    let engine = Arc::new(config.engine().await?);
    tracing::info!(
        "Using {} with tools: {}",
        engine.model(),
        engine.tool_names().await.join(", ")
    );
    slack.run(engine).await
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
                let (progress, mut notes) = mpsc::unbounded_channel::<String>();
                let printer = tokio::spawn(async move {
                    while let Some(note) = notes.recv().await {
                        println!("  … {}", openslacktag::one_line(&note, 120));
                    }
                });
                let incoming = vec![Incoming {
                    cursor: None,
                    entry: Entry::user(format!("<@you>: {text}")),
                }];
                let reply = engine.reply(&conversation, incoming, &progress).await;
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

fn new_conversation() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!("local-{secs}")
}
