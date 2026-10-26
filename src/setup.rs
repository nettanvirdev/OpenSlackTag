//! `setup` walks through everything once and writes `.env`; `doctor` checks
//! every piece and says how to fix what is broken.

use std::io::{BufRead, Write};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use openslacktag_core::message::Entry;
use openslacktag_core::provider::{ChatRequest, StreamEvent};
use openslacktag_mcp::provider::McpProvider;

use crate::config::{ANTHROPIC_BASE_URL, Config, DEFAULT_SLACK_API_URL, OPENAI_BASE_URL};
use crate::slack::Slack;

pub const MANIFEST: &str = include_str!("../slack-manifest.json");

struct Preset {
    label: &'static str,
    provider: &'static str,
    base_url: &'static str,
    needs_key: bool,
}

const PRESETS: &[Preset] = &[
    Preset {
        label: "Anthropic (Claude)",
        provider: "anthropic",
        base_url: ANTHROPIC_BASE_URL,
        needs_key: true,
    },
    Preset {
        label: "OpenAI",
        provider: "openai",
        base_url: OPENAI_BASE_URL,
        needs_key: true,
    },
    Preset {
        label: "OpenRouter (hundreds of models)",
        provider: "openai",
        base_url: "https://openrouter.ai/api/v1",
        needs_key: true,
    },
    Preset {
        label: "Ollama (local, free)",
        provider: "openai",
        base_url: "http://localhost:11434/v1",
        needs_key: false,
    },
    Preset {
        label: "Other OpenAI-compatible API",
        provider: "openai",
        base_url: "",
        needs_key: true,
    },
];

/// The link that opens Slack's "create app" page with our manifest filled in.
pub fn manifest_link() -> String {
    let compact: serde_json::Value = serde_json::from_str(MANIFEST).unwrap_or_default();
    let mut url =
        url::Url::parse("https://api.slack.com/apps?new_app=1").unwrap_or_else(|_| unreachable!());
    url.query_pairs_mut()
        .append_pair("manifest_json", &compact.to_string());
    url.into()
}

/// `.env` content. Values are single-quoted when they contain anything a
/// dotenv parser could misread.
pub fn render_env(pairs: &[(&str, String)]) -> String {
    let mut out = String::from(
        "# Written by `openslacktag setup`. Keep this file private: it holds your keys.\n",
    );
    for (key, value) in pairs {
        if value.is_empty() {
            continue;
        }
        let plain = value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.:/@+=,".contains(c));
        if plain {
            out.push_str(&format!("{key}={value}\n"));
        } else {
            out.push_str(&format!("{key}='{}'\n", value.replace('\'', "")));
        }
    }
    out
}

fn ask(question: &str, default: &str) -> Result<String> {
    if default.is_empty() {
        print!("{question}: ");
    } else {
        print!("{question} [{default}]: ");
    }
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        bail!("setup cancelled");
    }
    let line = line.trim();
    Ok(if line.is_empty() {
        default.to_string()
    } else {
        line.to_string()
    })
}

fn ask_secret(question: &str) -> Result<String> {
    Ok(
        rpassword::prompt_password(format!("{question} (hidden): "))?
            .trim()
            .to_string(),
    )
}

fn yes(question: &str, default_yes: bool) -> Result<bool> {
    let hint = if default_yes { "Y/n" } else { "y/N" };
    let answer = ask(&format!("{question} [{hint}]"), "")?.to_lowercase();
    Ok(if answer.is_empty() {
        default_yes
    } else {
        answer.starts_with('y')
    })
}

/// Interactive first-time setup.
pub async fn setup() -> Result<()> {
    println!("\nOpenSlackTag setup. This takes about five minutes and writes a .env file here.\n");
    if Path::new(".env").exists()
        && !yes(
            "A .env file already exists. Replace it (a backup is kept)?",
            false,
        )?
    {
        println!("Nothing changed.");
        return Ok(());
    }

    // 1. The model.
    println!("Which model provider?");
    for (i, p) in PRESETS.iter().enumerate() {
        println!("  {}. {}", i + 1, p.label);
    }
    let choice: usize = ask("Number", "1")?.parse().unwrap_or(1);
    let preset = PRESETS.get(choice.saturating_sub(1)).unwrap_or(&PRESETS[0]);
    let base_url = if preset.base_url.is_empty() {
        ask("Base URL (e.g. https://api.example.com/v1)", "")?
    } else {
        preset.base_url.to_string()
    };
    let api_key = if preset.needs_key {
        ask_secret("API key")?
    } else {
        String::new()
    };

    let mut config = Config::from_env()?;
    config.protocol = if preset.provider == "anthropic" {
        crate::config::Protocol::Anthropic
    } else {
        crate::config::Protocol::OpenAi
    };
    config.llm_base_url = base_url.clone();
    config.llm_api_key = Some(api_key.clone()).filter(|k| !k.is_empty());

    let provider = config.direct_provider();
    let models = match provider.list_models().await {
        Ok(models) => models.into_iter().map(|m| m.id).collect::<Vec<_>>(),
        Err(e) => {
            println!("Could not list models ({e}). You can still type a model name.");
            Vec::new()
        }
    };
    if !models.is_empty() {
        println!("\nAvailable models ({}):", models.len());
        for (i, m) in models.iter().take(40).enumerate() {
            println!("  {}. {m}", i + 1);
        }
        if models.len() > 40 {
            println!("  … and {} more; type any name.", models.len() - 40);
        }
    }
    let picked = ask("Model (number or name)", "1")?;
    let model = match picked.parse::<usize>() {
        Ok(n) if n >= 1 && n <= models.len() => models[n - 1].clone(),
        _ => picked,
    };
    config.llm_model = Some(model.clone());

    print!("Testing {model}… ");
    std::io::stdout().flush()?;
    match test_model(&config).await {
        Ok(reply) => println!("ok ({})", crate::one_line(&reply, 60)),
        Err(e) => {
            println!("failed: {e}");
            if !yes("Save these settings anyway?", false)? {
                bail!("setup stopped; nothing was written");
            }
        }
    }

    // 2. Slack.
    let (mut bot_token, mut app_token) = (String::new(), String::new());
    if yes(
        "\nConnect Slack now? (skip to only use the local chat)",
        true,
    )? {
        println!(
            "\n1. Open this link and pick your workspace. It creates the app with everything preset:\n\n   {}\n",
            manifest_link()
        );
        println!(
            "   (If the link does not open, create an app \"From a manifest\" and paste slack-manifest.json.)"
        );
        println!(
            "2. Click \"Install to Workspace\", then copy the Bot User OAuth Token (starts with xoxb-)."
        );
        println!(
            "3. Under Basic Information > App-Level Tokens, create a token with the connections:write scope"
        );
        println!("   and copy it (starts with xapp-).\n");
        loop {
            bot_token = ask_secret("Bot token (xoxb-...)")?;
            app_token = ask_secret("App-level token (xapp-...)")?;
            if !bot_token.starts_with("xoxb-") || !app_token.starts_with("xapp-") {
                println!(
                    "Those don't look right: the bot token starts with xoxb- and the app token with xapp-."
                );
                continue;
            }
            match check_slack(DEFAULT_SLACK_API_URL, &bot_token, &app_token).await {
                Ok(bot) => {
                    println!(
                        "Slack works. The bot is <@{bot}>; invite it to a channel with /invite @OpenSlackTag."
                    );
                    break;
                }
                Err(e) => {
                    println!("Slack said no: {e:#}");
                    if !yes("Try again?", true)? {
                        bot_token.clear();
                        app_token.clear();
                        break;
                    }
                }
            }
        }
    }

    // 3. The sandbox.
    let mut sandbox_image = String::new();
    if docker_ok().await {
        println!(
            "\nDocker is available, so the teammate can have its own sandboxed computer for running code."
        );
        let suggested = if image_present("openslacktag-sandbox").await {
            "openslacktag-sandbox"
        } else {
            "python:3.12-slim"
        };
        sandbox_image = ask(
            "Docker image for it (any image with sh; empty to skip)",
            suggested,
        )?;
        if !sandbox_image.is_empty() && !image_present(&sandbox_image).await {
            println!("Note: pull it first with: docker pull {sandbox_image}");
        }
    } else {
        println!(
            "\nDocker was not found, so the shell tool stays off. Install Docker and rerun setup to enable it."
        );
    }

    // 4. The time zone routines run in. Inside Docker the system's is UTC,
    // so ask rather than assume.
    let detected = jiff::tz::TimeZone::system()
        .iana_name()
        .unwrap_or("UTC")
        .to_string();
    let tz = loop {
        let answer = ask("\nTime zone for routines (like Europe/London)", &detected)?;
        if jiff::tz::TimeZone::get(&answer).is_ok() {
            break answer;
        }
        println!("{answer} is not a time zone name; use one like America/New_York.");
    };

    let env = render_env(&[
        ("LLM_PROVIDER", preset.provider.to_string()),
        ("LLM_BASE_URL", base_url),
        ("LLM_API_KEY", api_key),
        ("LLM_MODEL", model),
        ("SLACK_BOT_TOKEN", bot_token.clone()),
        ("SLACK_APP_TOKEN", app_token),
        ("SANDBOX_IMAGE", sandbox_image),
        ("TZ", tz),
    ]);
    if Path::new(".env").exists() {
        std::fs::copy(".env", ".env.bak").context("could not back up .env")?;
    }
    write_private(Path::new(".env"), &env).context("could not write .env")?;
    println!("\nSaved .env.\n");
    println!("Next:");
    if !bot_token.is_empty() {
        println!("  openslacktag slack    run the teammate in Slack");
    }
    println!("  openslacktag chat     talk to it here in the terminal");
    println!("  openslacktag doctor   check everything");
    println!("\nTo add tools, list MCP servers in mcp.json (see mcp.example.json).");
    Ok(())
}

/// One tiny completion, without retries and with a time limit, so a wrong
/// URL or key fails in seconds instead of being retried for minutes.
/// Write a file only its owner can read: `.env` holds API keys.
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(contents.as_bytes())
}

async fn test_model(config: &Config) -> Result<String> {
    let request = ChatRequest {
        model: config.model()?.to_string(),
        system: "You are a connectivity check.".into(),
        history: vec![Entry::user("Reply with exactly: ready")],
        max_tokens: Some(20),
        ..Default::default()
    };
    let collect = async {
        let mut stream = config.direct_provider().stream_chat(request);
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            match event {
                StreamEvent::Delta { text: t } => text.push_str(&t),
                StreamEvent::Error { message, .. } => bail!(message),
                _ => {}
            }
        }
        Ok(text)
    };
    tokio::time::timeout(Duration::from_secs(30), collect)
        .await
        .map_err(|_| anyhow::anyhow!("no answer within 30 seconds"))?
}

async fn check_slack(api_url: &str, bot_token: &str, app_token: &str) -> Result<String> {
    let slack = Slack::connect(api_url, bot_token, app_token).await?;
    slack.socket_url().await?;
    Ok(slack.bot_user_id().to_string())
}

/// Run one command in the sandbox the way the shell tool does, and check it
/// sees a file put in its workspace: that catches a wrong host path when
/// OpenSlackTag itself runs in Docker.
async fn probe_sandbox(config: &Config) -> (bool, String) {
    let Some(sandbox) = config.sandbox() else {
        return (false, "SANDBOX_IMAGE is not set".into());
    };
    let dir = config.data_dir.join("workspaces").join(".doctor");
    let token = format!("probe-{}", std::process::id());
    if let Err(e) =
        std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(dir.join("probe.txt"), &token))
    {
        return (false, format!("could not write {}: {e}", dir.display()));
    }
    let result = crate::tools::run_shell(&sandbox, &dir, "cat probe.txt").await;
    let _ = std::fs::remove_dir_all(&dir);
    match result {
        Ok(out) if out.contains(&token) => (true, String::new()),
        Ok(out) => (
            false,
            format!(
                "commands run but don't see the workspace files. If OpenSlackTag runs in Docker, set                  SANDBOX_HOST_DATA_DIR to the host's path of the data folder. Output: {}",
                crate::one_line(&out, 300)
            ),
        ),
        Err(e) => (false, format!("{e}")),
    }
}

/// Check the images API's URL and key by listing its models, which costs
/// nothing, instead of generating an image.
async fn check_images(images: &crate::images::ImageConfig) -> Result<()> {
    let mut request = reqwest::Client::new()
        .get(format!("{}/models", images.base_url.trim_end_matches('/')))
        .timeout(std::time::Duration::from_secs(20));
    match &images.api_key {
        Some(key) => request = request.bearer_auth(key),
        None if images.base_url.contains("api.openai.com") => {
            bail!(
                "IMAGE_API_KEY is not set (your chat model isn't OpenAI-compatible, so its key can't be used)"
            )
        }
        None => {}
    }
    let status = request
        .send()
        .await
        .with_context(|| format!("could not reach {}", images.base_url))?
        .status();
    if !status.is_success() {
        bail!(
            "{} answered {status}; check IMAGE_BASE_URL and IMAGE_API_KEY",
            images.base_url
        );
    }
    Ok(())
}

async fn docker_ok() -> bool {
    tokio::process::Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

async fn image_present(image: &str) -> bool {
    tokio::process::Command::new("docker")
        .args(["image", "inspect", image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

/// Check every piece and print what is wrong and how to fix it. Fails if anything required is broken.
pub async fn doctor(config: &Config) -> Result<()> {
    let mut failures = 0;
    let mut report = |ok: bool, what: &str, detail: String| {
        if !ok {
            failures += 1;
        }
        println!("{} {what}: {detail}", if ok { "[ok]  " } else { "[FAIL]" });
    };

    match config.model() {
        Ok(model) => match test_model(config).await {
            Ok(_) => report(true, "model", format!("{model} at {}", config.llm_base_url)),
            Err(e) => report(
                false,
                "model",
                format!(
                    "{model} at {}: {e:#} (check LLM_BASE_URL, LLM_API_KEY and LLM_MODEL)",
                    config.llm_base_url
                ),
            ),
        },
        Err(e) => report(false, "model", e.to_string()),
    }

    match config.slack_tokens() {
        Ok((bot, app)) => match check_slack(&config.slack_api_url, bot, app).await {
            Ok(id) => report(true, "slack", format!("connected as <@{id}>")),
            Err(e) => report(
                false,
                "slack",
                format!("{e:#}. Rerun `openslacktag setup` to fix the tokens."),
            ),
        },
        Err(_) => println!("[skip] slack: not configured (the local chat still works)"),
    }

    match &config.sandbox_image {
        Some(image) if !docker_ok().await => report(
            false,
            "sandbox",
            format!("SANDBOX_IMAGE is {image} but Docker is not running"),
        ),
        Some(image) if !image_present(image).await => report(
            false,
            "sandbox",
            if image == "openslacktag-sandbox" {
                "image openslacktag-sandbox is not built. Run: docker build -t openslacktag-sandbox sandbox/".to_string()
            } else {
                format!("image {image} is not pulled. Run: docker pull {image}")
            },
        ),
        Some(image) => {
            let (ok, detail) = probe_sandbox(config).await;
            report(
                ok,
                "sandbox",
                if ok {
                    format!("{image}, network {}", config.sandbox_network)
                } else {
                    detail
                },
            );
        }
        None => println!("[skip] sandbox: SANDBOX_IMAGE is not set, so there is no shell tool"),
    }

    match &config.images {
        Some(images) => {
            let checked = check_images(images).await;
            report(
                checked.is_ok(),
                "images",
                match checked {
                    Ok(()) => format!("{} at {}", images.model, images.base_url),
                    Err(e) => format!("{e:#}"),
                },
            );
        }
        None => {
            println!("[skip] images: IMAGE_MODEL is not set, so there is no generate_image tool")
        }
    }

    match crate::permissions::load_rules(&config.permissions) {
        Ok(rules) => report(
            true,
            "permissions",
            format!(
                "{} rules (defaults plus {})",
                rules.len(),
                config.permissions.display()
            ),
        ),
        Err(e) => report(false, "permissions", format!("{e:#}")),
    }

    match crate::skills::Skills::load(&config.skills_dir) {
        Ok(skills) if skills.is_empty() => {
            println!("[skip] skills: none in {}", config.skills_dir.display())
        }
        Ok(skills) => report(
            true,
            "skills",
            skills
                .listing()
                .unwrap_or_default()
                .lines()
                .count()
                .to_string()
                + " skills",
        ),
        Err(e) => report(false, "skills", format!("{e:#}")),
    }

    match config.mcp_servers() {
        Ok(servers) if servers.is_empty() => {
            println!("[skip] mcp: no servers in {}", config.mcp_config.display())
        }
        Ok(servers) => {
            for record in servers {
                let name = record.name.clone();
                match McpProvider::new().add(record).await {
                    Ok(n) => report(true, "mcp", format!("{name}: {n} tools")),
                    Err(e) => report(false, "mcp", format!("{name}: {e}")),
                }
            }
        }
        Err(e) => report(false, "mcp", format!("{e:#}")),
    }

    let probe = config.data_dir.join(".write-test");
    let writable = std::fs::create_dir_all(&config.data_dir)
        .and_then(|()| std::fs::write(&probe, b"ok"))
        .is_ok();
    let _ = std::fs::remove_file(&probe);
    report(writable, "data", format!("{}", config.data_dir.display()));

    match std::env::var("TZ").ok().filter(|tz| !tz.is_empty()) {
        Some(tz) if jiff::tz::TimeZone::get(tz.trim_start_matches(':')).is_err() => report(
            false,
            "time zone",
            format!("TZ={tz} is not a time zone name; use one like Europe/London"),
        ),
        _ => {
            let now = jiff::Zoned::now();
            report(
                true,
                "time zone",
                format!(
                    "routines run in {} (now {}); set TZ in .env to change it",
                    now.time_zone().iana_name().unwrap_or("UTC"),
                    now.strftime("%a %H:%M")
                ),
            );
        }
    }

    if failures > 0 {
        bail!("{failures} check(s) failed");
    }
    println!("\nAll good.");
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn manifest_is_valid_and_uses_socket_mode() {
        let manifest: serde_json::Value = serde_json::from_str(MANIFEST).unwrap();
        assert_eq!(manifest["settings"]["socket_mode_enabled"], true);
        let events = manifest["settings"]["event_subscriptions"]["bot_events"]
            .as_array()
            .unwrap();
        assert!(events.iter().any(|e| e == "app_mention"));
        let scopes = manifest["oauth_config"]["scopes"]["bot"]
            .as_array()
            .unwrap();
        for needed in [
            "app_mentions:read",
            "chat:write",
            "channels:history",
            "im:history",
        ] {
            assert!(scopes.iter().any(|s| s == needed), "missing scope {needed}");
        }
    }

    #[test]
    fn manifest_link_round_trips() {
        let link = url::Url::parse(&manifest_link()).unwrap();
        let (_, json) = link
            .query_pairs()
            .find(|(k, _)| k == "manifest_json")
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["display_information"]["name"], "OpenSlackTag");
    }

    #[test]
    fn env_rendering_quotes_and_skips_empty() {
        let env = render_env(&[
            ("A", "plain-value_1".into()),
            ("B", String::new()),
            ("C", "has space#x".into()),
        ]);
        assert!(env.contains("A=plain-value_1\n"));
        assert!(!env.contains("B="));
        assert!(env.contains("C='has space#x'\n"));
    }
}
