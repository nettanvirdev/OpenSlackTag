//! Settings come from environment variables, usually through a `.env` file
//! written by `openslacktag setup`. MCP servers come from `mcp.json`, in the
//! same format Claude Desktop and Cursor use.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use openslacktag_core::Provider;
use openslacktag_mcp::client::{Kind, ServerRecord};
use openslacktag_mcp::provider::McpProvider;
use openslacktag_provider::{AnthropicProvider, OpenAiProvider, ProviderConfig, Resilient};
use serde::Deserialize;

use crate::engine::{Engine, Parts};
use crate::images::{GenerateImage, ImageConfig};
use crate::permissions::load_rules;
use crate::search::{SearchConfig, WebSearch};
use crate::skills::Skills;
use crate::tools::Sandbox;
use openslacktag_core::tool::Tool;

pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com/v1";
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_SLACK_API_URL: &str = "https://slack.com/api";
const DEFAULT_COMPACT_AT_TOKENS: usize = 80_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Anthropic,
    /// Anything that speaks the OpenAI chat-completions API.
    OpenAi,
}

#[derive(Clone)]
pub struct Config {
    pub protocol: Protocol,
    pub llm_base_url: String,
    pub llm_api_key: Option<String>,
    pub llm_model: Option<String>,
    pub data_dir: PathBuf,
    pub mcp_config: PathBuf,
    pub sandbox_image: Option<String>,
    pub sandbox_network: String,
    /// The data folder's path on the Docker host, when running in a container.
    pub sandbox_host_data_dir: Option<PathBuf>,
    pub compact_at_tokens: usize,
    pub slack_bot_token: Option<String>,
    pub slack_app_token: Option<String>,
    pub slack_api_url: String,
    /// Slack user ids allowed to use the teammate; empty means everyone in the workspace.
    pub allowed_users: Vec<String>,
    /// Slack user ids who may approve `ask` tool calls; empty means whoever asked.
    pub approvers: Vec<String>,
    /// Allow / ask / deny rules for tools.
    pub permissions: PathBuf,
    /// Folders of Agent Skills (`<name>/SKILL.md`).
    pub skills_dir: PathBuf,
    /// Standing instructions added to every system prompt.
    pub instructions: PathBuf,
    pub search: Option<SearchConfig>,
    /// Image generation, when IMAGE_MODEL is set.
    pub images: Option<ImageConfig>,
    /// Where weekly automation suggestions are posted, if anywhere.
    pub suggestions_channel: Option<String>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("protocol", &self.protocol)
            .field("llm_base_url", &self.llm_base_url)
            .field("llm_model", &self.llm_model)
            .field("data_dir", &self.data_dir)
            .finish_non_exhaustive()
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let base = var("LLM_BASE_URL");
        let protocol = match var("LLM_PROVIDER")
            .as_deref()
            .map(str::to_lowercase)
            .as_deref()
        {
            Some("anthropic") => Protocol::Anthropic,
            Some("openai") => Protocol::OpenAi,
            Some(other) => bail!(
                "LLM_PROVIDER must be `anthropic` or `openai` (any OpenAI-compatible API), not {other:?}"
            ),
            None if base.as_deref().is_some_and(|b| b.contains("anthropic.com")) => {
                Protocol::Anthropic
            }
            None => Protocol::OpenAi,
        };
        let default_base = match protocol {
            Protocol::Anthropic => ANTHROPIC_BASE_URL,
            Protocol::OpenAi => OPENAI_BASE_URL,
        };
        let sandbox_network = var("SANDBOX_NETWORK").unwrap_or_else(|| "none".into());
        if !matches!(sandbox_network.as_str(), "none" | "bridge") {
            bail!("SANDBOX_NETWORK must be `none` or `bridge`, not {sandbox_network:?}");
        }
        let compact_at_tokens = match var("COMPACT_AT_TOKENS") {
            Some(v) => v
                .parse()
                .context("COMPACT_AT_TOKENS must be a whole number")?,
            None => DEFAULT_COMPACT_AT_TOKENS,
        };
        Ok(Self {
            protocol,
            llm_base_url: base.clone().unwrap_or_else(|| default_base.into()),
            llm_api_key: var("LLM_API_KEY"),
            llm_model: var("LLM_MODEL"),
            data_dir: var("DATA_DIR").unwrap_or_else(|| "data".into()).into(),
            mcp_config: var("MCP_CONFIG")
                .unwrap_or_else(|| "mcp.json".into())
                .into(),
            sandbox_image: var("SANDBOX_IMAGE"),
            sandbox_network,
            sandbox_host_data_dir: var("SANDBOX_HOST_DATA_DIR").map(PathBuf::from),
            compact_at_tokens,
            slack_bot_token: var("SLACK_BOT_TOKEN"),
            slack_app_token: var("SLACK_APP_TOKEN"),
            slack_api_url: var("SLACK_API_URL").unwrap_or_else(|| DEFAULT_SLACK_API_URL.into()),
            allowed_users: var("ALLOWED_USERS")
                .map(|v| parse_list(&v))
                .unwrap_or_default(),
            approvers: var("APPROVERS").map(|v| parse_list(&v)).unwrap_or_default(),
            permissions: var("PERMISSIONS")
                .unwrap_or_else(|| "permissions.json".into())
                .into(),
            skills_dir: var("SKILLS_DIR").unwrap_or_else(|| "skills".into()).into(),
            instructions: var("INSTRUCTIONS")
                .unwrap_or_else(|| "instructions.md".into())
                .into(),
            search: match var("SEARCH_PROVIDER") {
                Some(provider) => Some(SearchConfig::new(
                    &provider,
                    var("SEARCH_API_KEY"),
                    var("SEARCH_URL"),
                )?),
                None => None,
            },
            suggestions_channel: var("SUGGESTIONS_CHANNEL"),
            images: var("IMAGE_MODEL").map(|model| ImageConfig {
                // Same provider as the chat model by default, if it is OpenAI-compatible.
                base_url: var("IMAGE_BASE_URL").unwrap_or_else(|| match protocol {
                    Protocol::OpenAi => base.clone().unwrap_or_else(|| OPENAI_BASE_URL.into()),
                    Protocol::Anthropic => OPENAI_BASE_URL.into(),
                }),
                api_key: var("IMAGE_API_KEY").or_else(|| match protocol {
                    Protocol::OpenAi => var("LLM_API_KEY"),
                    Protocol::Anthropic => None,
                }),
                model,
            }),
        })
    }

    pub fn model(&self) -> Result<&str> {
        self.llm_model
            .as_deref()
            .context("LLM_MODEL is not set. Run `openslacktag setup` first.")
    }

    /// The model provider, with retries for failures that clear on their own.
    pub fn provider(&self) -> Arc<dyn Provider> {
        Arc::new(Resilient::new(self.direct_provider()))
    }

    /// The model provider without retries, for checks that should fail fast.
    pub fn direct_provider(&self) -> Arc<dyn Provider> {
        let config = ProviderConfig::new(
            "llm",
            self.llm_base_url.trim_end_matches('/'),
            self.llm_api_key.clone().unwrap_or_default(),
        );
        match self.protocol {
            Protocol::Anthropic => Arc::new(AnthropicProvider::new(config)),
            Protocol::OpenAi => Arc::new(OpenAiProvider::new(config)),
        }
    }

    pub fn sandbox(&self) -> Option<Sandbox> {
        let host_data_dir = self.sandbox_host_data_dir.clone().map(|host| {
            let data =
                std::path::absolute(&self.data_dir).unwrap_or_else(|_| self.data_dir.clone());
            (data, host)
        });
        self.sandbox_image.clone().map(|image| Sandbox {
            image,
            network: self.sandbox_network.clone(),
            host_data_dir,
        })
    }

    /// MCP servers from `mcp.json`. A missing file means none.
    pub fn mcp_servers(&self) -> Result<Vec<ServerRecord>> {
        match std::fs::read_to_string(&self.mcp_config) {
            Ok(text) => parse_mcp_config(&text)
                .with_context(|| format!("could not read {}", self.mcp_config.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => {
                Err(e).with_context(|| format!("could not read {}", self.mcp_config.display()))
            }
        }
    }

    /// Build the engine, connecting every MCP server. A server that fails to
    /// start is reported and skipped, so one broken server cannot take the bot down.
    pub async fn engine(&self) -> Result<Engine> {
        let mcp = Arc::new(McpProvider::new());
        for record in self.mcp_servers()? {
            match mcp.add(record.clone()).await {
                Ok(count) => tracing::info!("MCP server {} connected ({count} tools)", record.name),
                Err(e) => tracing::warn!("MCP server {} failed to start: {e}", record.name),
            }
        }
        let instructions = match std::fs::read_to_string(&self.instructions) {
            Ok(text) if !text.trim().is_empty() => Some(text),
            Ok(_) => None,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("could not read {}", self.instructions.display()));
            }
        };
        let mut extra_tools: Vec<Arc<dyn Tool>> = Vec::new();
        if let Some(search) = &self.search {
            extra_tools.push(Arc::new(WebSearch::new(search.clone())));
        }
        if let Some(images) = &self.images {
            extra_tools.push(Arc::new(GenerateImage::new(images.clone())));
        }
        Engine::new(Parts {
            provider: self.provider(),
            model: self.model()?.to_string(),
            data_dir: self.data_dir.clone(),
            sandbox: self.sandbox(),
            mcp,
            compact_at_tokens: self.compact_at_tokens,
            rules: load_rules(&self.permissions)?,
            skills: Skills::load(&self.skills_dir)?,
            instructions,
            extra_tools,
        })
    }

    pub fn slack_tokens(&self) -> Result<(&str, &str)> {
        match (&self.slack_bot_token, &self.slack_app_token) {
            (Some(bot), Some(app)) => Ok((bot, app)),
            _ => bail!(
                "SLACK_BOT_TOKEN and SLACK_APP_TOKEN are not set. Run `openslacktag setup` to connect Slack, \
                 or `openslacktag chat` to talk to the teammate locally."
            ),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpFile {
    #[serde(default)]
    mcp_servers: HashMap<String, McpEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpEntry {
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    url: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    disabled: bool,
    timeout_ms: Option<u64>,
}

fn parse_mcp_config(text: &str) -> Result<Vec<ServerRecord>> {
    let file: McpFile = serde_json::from_str(text)?;
    let mut servers: Vec<ServerRecord> = Vec::new();
    for (name, entry) in file.mcp_servers {
        if entry.disabled {
            continue;
        }
        let kind = match (&entry.command, &entry.url) {
            (Some(_), None) => Kind::Stdio,
            (None, Some(_)) => Kind::Http,
            _ => bail!(
                "MCP server {name:?} needs exactly one of `command` (local) or `url` (remote)"
            ),
        };
        servers.push(ServerRecord {
            id: name.clone(),
            name,
            kind,
            enabled: true,
            command: entry.command.unwrap_or_default(),
            args: entry.args,
            env: entry.env,
            url: entry.url.unwrap_or_default(),
            headers: entry.headers,
            timeout_ms: entry.timeout_ms,
            ..ServerRecord::default()
        });
    }
    servers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(servers)
}

/// A comma- or space-separated list, e.g. `U123, U456`.
fn parse_list(raw: &str) -> Vec<String> {
    raw.split([',', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// An environment variable, treating empty values as unset.
fn var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn parses_claude_style_mcp_config() {
        let servers = parse_mcp_config(
            r#"{"mcpServers": {
                "github": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"], "env": {"TOKEN": "x"}},
                "remote": {"url": "https://mcp.example.com/mcp", "headers": {"Authorization": "Bearer y"}},
                "off": {"command": "x", "disabled": true}
            }}"#,
        )
        .unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "github");
        assert_eq!(servers[0].kind, Kind::Stdio);
        assert_eq!(servers[0].args[1], "@modelcontextprotocol/server-github");
        assert_eq!(servers[1].kind, Kind::Http);
        assert_eq!(servers[1].headers["Authorization"], "Bearer y");
    }

    #[test]
    fn parses_user_lists() {
        assert_eq!(parse_list("U1, U2 U3,,"), vec!["U1", "U2", "U3"]);
        assert!(parse_list(" , ").is_empty());
    }

    #[test]
    fn rejects_ambiguous_mcp_entries() {
        assert!(parse_mcp_config(r#"{"mcpServers": {"x": {}}}"#).is_err());
        assert!(
            parse_mcp_config(r#"{"mcpServers": {"x": {"command": "a", "url": "b"}}}"#).is_err()
        );
        assert!(parse_mcp_config(r#"{}"#).unwrap().is_empty());
    }
}
