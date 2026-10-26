//! Tools that act on Slack itself: read channels and threads, list channels,
//! look people up, react, post, and share files from the workspace.
//!
//! Reading respects what the person asking can see: a private channel or DM
//! is only readable when they are in it, so nobody can use the teammate to
//! read a room they are not in.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use openslacktag_core::error::{Error, Result};
use openslacktag_core::tool::{PermissionRequest, Tool, ToolContext, ToolOutcome, ToolSource};
use serde_json::{Value, json};

use crate::engine::Target;
use crate::slack::Slack;

const MAX_SHARE_BYTES: u64 = 50_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    ReadChannel,
    ReadThread,
    ListChannels,
    UserInfo,
    React,
    PostMessage,
    ShareFile,
}

const OPS: [Op; 7] = [
    Op::ReadChannel,
    Op::ReadThread,
    Op::ListChannels,
    Op::UserInfo,
    Op::React,
    Op::PostMessage,
    Op::ShareFile,
];

#[derive(Debug)]
struct SlackTool {
    op: Op,
    slack: Arc<Slack>,
    target: Target,
    requester: Option<String>,
}

/// The Slack tools for one conversation.
pub fn slack_tools(
    slack: &Arc<Slack>,
    target: &Target,
    requester: Option<&str>,
) -> Vec<Arc<dyn Tool>> {
    OPS.into_iter()
        .map(|op| {
            Arc::new(SlackTool {
                op,
                slack: slack.clone(),
                target: target.clone(),
                requester: requester.map(str::to_string),
            }) as Arc<dyn Tool>
        })
        .collect()
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| Error::InvalidInput(format!("missing {key}")))
}

// By value so it can be passed straight to `map_err`.
#[allow(clippy::needless_pass_by_value)]
fn slack_error(e: anyhow::Error) -> Error {
    Error::Upstream {
        status: None,
        message: format!("{e:#}"),
    }
}

impl SlackTool {
    /// A channel id from `C123`, `#general`, `<#C123|general>` or `here`.
    async fn channel(&self, raw: &str) -> Result<String> {
        if raw == "here" {
            return Ok(self.target.channel.clone());
        }
        self.slack.resolve_channel(raw).await.map_err(slack_error)
    }

    /// Refuse to read a private room the person asking is not in.
    async fn check_readable(&self, channel: &str) -> Result<()> {
        if channel == self.target.channel {
            return Ok(());
        }
        let readable = match &self.requester {
            Some(user) => self
                .slack
                .can_read(channel, user)
                .await
                .map_err(slack_error)?,
            None => false,
        };
        if readable {
            Ok(())
        } else {
            Err(Error::Denied(
                "that channel is private and the person asking is not in it (or I am not in it)"
                    .into(),
            ))
        }
    }
}

#[async_trait]
impl Tool for SlackTool {
    fn id(&self) -> &str {
        match self.op {
            Op::ReadChannel => "slack_read_channel",
            Op::ReadThread => "slack_read_thread",
            Op::ListChannels => "slack_list_channels",
            Op::UserInfo => "slack_user_info",
            Op::React => "slack_react",
            Op::PostMessage => "slack_post_message",
            Op::ShareFile => "share_file",
        }
    }

    fn description(&self) -> &str {
        match self.op {
            Op::ReadChannel => {
                "Read recent messages in a Slack channel you are in (`here` for this one). Use it to \
                                summarise, catch people up or find something said recently."
            }
            Op::ReadThread => {
                "Read every message in a Slack thread, given its channel and the parent message ts."
            }
            Op::ListChannels => "List the Slack channels you are a member of.",
            Op::UserInfo => "Look up a Slack user: name, title, time zone.",
            Op::React => "Add an emoji reaction to a message, e.g. white_check_mark.",
            Op::PostMessage => {
                "Post a message to another Slack channel or thread (not this conversation: your reply \
                                already goes here). People may need to approve it."
            }
            Op::ShareFile => {
                "Share a file from your workspace in this conversation, e.g. a report or CSV you wrote."
            }
        }
    }

    fn parameters(&self) -> Value {
        let (properties, required): (Value, Vec<&str>) = match self.op {
            Op::ReadChannel => (
                json!({
                    "channel": {"type": "string", "description": "#name, an id, or `here`"},
                    "hours": {"type": "number", "description": "How far back to read (default 24)"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200}
                }),
                vec!["channel"],
            ),
            Op::ReadThread => (
                json!({"channel": {"type": "string"}, "ts": {"type": "string", "description": "The parent message ts"}}),
                vec!["channel", "ts"],
            ),
            Op::ListChannels => (json!({}), vec![]),
            Op::UserInfo => (
                json!({"user": {"type": "string", "description": "A user id like U123"}}),
                vec!["user"],
            ),
            Op::React => (
                json!({"channel": {"type": "string"}, "ts": {"type": "string"}, "emoji": {"type": "string"}}),
                vec!["channel", "ts", "emoji"],
            ),
            Op::PostMessage => (
                json!({
                    "channel": {"type": "string"},
                    "text": {"type": "string", "description": "Slack-formatted text"},
                    "thread_ts": {"type": "string", "description": "Optional: reply in this thread"}
                }),
                vec!["channel", "text"],
            ),
            Op::ShareFile => (
                json!({
                    "path": {"type": "string", "description": "Path in your workspace"},
                    "comment": {"type": "string", "description": "Optional message with the file"}
                }),
                vec!["path"],
            ),
        };
        json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
    }

    fn source(&self) -> ToolSource {
        ToolSource::Builtin
    }

    fn permission(&self, args: &Value) -> PermissionRequest {
        let target = match self.op {
            Op::ShareFile => args["path"].as_str(),
            Op::UserInfo => args["user"].as_str(),
            _ => args["channel"].as_str(),
        }
        .unwrap_or("*");
        let request = PermissionRequest::new(self.id(), target);
        match self.op {
            Op::PostMessage => request.with_detail(args["text"].as_str().unwrap_or_default()),
            _ => request,
        }
    }

    fn render(&self, args: &Value) -> Option<String> {
        args["channel"]
            .as_str()
            .or(args["path"].as_str())
            .or(args["user"].as_str())
            .map(str::to_string)
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutcome> {
        let slack = &self.slack;
        let text = match self.op {
            Op::ReadChannel => {
                let channel = self.channel(str_arg(&args, "channel")?).await?;
                self.check_readable(&channel).await?;
                let hours = args["hours"]
                    .as_f64()
                    .unwrap_or(24.0)
                    .clamp(0.1, 24.0 * 90.0);
                let limit = args["limit"].as_u64().unwrap_or(100).clamp(1, 200);
                slack
                    .read_channel(&channel, hours, limit)
                    .await
                    .map_err(slack_error)?
            }
            Op::ReadThread => {
                let channel = self.channel(str_arg(&args, "channel")?).await?;
                self.check_readable(&channel).await?;
                slack
                    .read_thread(&channel, str_arg(&args, "ts")?)
                    .await
                    .map_err(slack_error)?
            }
            Op::ListChannels => slack.list_channels().await.map_err(slack_error)?,
            Op::UserInfo => slack
                .user_info(str_arg(&args, "user")?)
                .await
                .map_err(slack_error)?,
            Op::React => {
                let channel = self.channel(str_arg(&args, "channel")?).await?;
                let emoji = str_arg(&args, "emoji")?.trim_matches(':');
                slack
                    .react(&channel, str_arg(&args, "ts")?, emoji)
                    .await
                    .map_err(slack_error)?;
                format!("Reacted with :{emoji}:.")
            }
            Op::PostMessage => {
                let channel = self.channel(str_arg(&args, "channel")?).await?;
                let message = crate::markdown::to_mrkdwn(str_arg(&args, "text")?);
                let ts = slack
                    .post(&channel, args["thread_ts"].as_str(), &message)
                    .await
                    .map_err(slack_error)?;
                format!("Posted to <#{channel}> (ts {ts}).")
            }
            Op::ShareFile => {
                let path: PathBuf = crate::files::resolve(&ctx.root, str_arg(&args, "path")?)?;
                let size = std::fs::metadata(&path)
                    .map_err(|_| Error::not_found("file", path.display().to_string()))?
                    .len();
                if size > MAX_SHARE_BYTES {
                    return Err(Error::InvalidInput(format!(
                        "files over {MAX_SHARE_BYTES} bytes cannot be shared"
                    )));
                }
                let bytes = std::fs::read(&path)?;
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "file".into());
                slack
                    .upload_file(
                        &self.target.channel,
                        self.target.thread.as_deref(),
                        &name,
                        bytes,
                        args["comment"].as_str(),
                    )
                    .await
                    .map_err(slack_error)?;
                format!("Shared {name} ({size} bytes) in this conversation.")
            }
        };
        Ok(ToolOutcome::text(text))
    }
}
