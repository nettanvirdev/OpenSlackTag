//! File tools for the conversation's workspace: the folder where files people
//! share land (`inbox/`), where the teammate writes reports and data, and
//! which the sandbox sees as `/work`. Paths can never leave that folder.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use openslacktag_core::error::{Error, Result};
use openslacktag_core::tool::{PermissionRequest, Tool, ToolContext, ToolOutcome, ToolSource};
use serde_json::{Value, json};

const MAX_READ_BYTES: usize = 200_000;
const MAX_WRITE_BYTES: usize = 5_000_000;
const MAX_LISTED: usize = 500;

/// A path inside `root`, from a relative path the model gave.
pub fn resolve(root: &Path, relative: &str) -> Result<PathBuf> {
    let relative = relative
        .trim()
        .trim_start_matches("/work/")
        .trim_start_matches("./");
    let path = Path::new(relative);
    if relative.is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(Error::InvalidInput(format!(
            "{relative:?} is not a path inside your workspace; use a relative path like reports/q3.md"
        )));
    }
    Ok(root.join(path))
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Read,
    Write,
    List,
}

#[derive(Debug)]
struct FileTool {
    op: Op,
}

pub fn file_tools() -> Vec<Arc<dyn Tool>> {
    [Op::Read, Op::Write, Op::List]
        .into_iter()
        .map(|op| Arc::new(FileTool { op }) as Arc<dyn Tool>)
        .collect()
}

#[async_trait]
impl Tool for FileTool {
    fn id(&self) -> &str {
        match self.op {
            Op::Read => "read_file",
            Op::Write => "write_file",
            Op::List => "list_files",
        }
    }

    fn description(&self) -> &str {
        match self.op {
            Op::Read => {
                "Read a text file from your workspace for this conversation. Files people shared are in inbox/."
            }
            Op::Write => {
                "Create or replace a file in your workspace (reports, CSVs, code). Share it with share_file."
            }
            Op::List => "List the files in your workspace for this conversation.",
        }
    }

    fn parameters(&self) -> Value {
        let (properties, required) = match self.op {
            Op::Read => (json!({"path": {"type": "string"}}), vec!["path"]),
            Op::Write => (
                json!({"path": {"type": "string"}, "content": {"type": "string"}}),
                vec!["path", "content"],
            ),
            Op::List => (json!({}), vec![]),
        };
        json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
    }

    fn source(&self) -> ToolSource {
        ToolSource::Builtin
    }

    fn permission(&self, args: &Value) -> PermissionRequest {
        PermissionRequest::new(self.id(), args["path"].as_str().unwrap_or("*"))
    }

    fn render(&self, args: &Value) -> Option<String> {
        args["path"].as_str().map(str::to_string)
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutcome> {
        let root = &ctx.root;
        let output = match self.op {
            Op::Read => {
                let path = resolve(root, args["path"].as_str().unwrap_or_default())?;
                let bytes = std::fs::read(&path).map_err(|_| {
                    Error::not_found("file", args["path"].as_str().unwrap_or_default())
                })?;
                if bytes.iter().take(8000).any(|b| *b == 0) {
                    return Err(Error::InvalidInput(
                        "that file is binary; inspect it with run_shell instead".into(),
                    ));
                }
                let mut text =
                    String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_READ_BYTES)]).into_owned();
                if bytes.len() > MAX_READ_BYTES {
                    text.push_str(&format!(
                        "\n… [only the first {MAX_READ_BYTES} of {} bytes]",
                        bytes.len()
                    ));
                }
                text
            }
            Op::Write => {
                let path = resolve(root, args["path"].as_str().unwrap_or_default())?;
                let content = args["content"].as_str().unwrap_or_default();
                if content.len() > MAX_WRITE_BYTES {
                    return Err(Error::InvalidInput(format!(
                        "files are limited to {MAX_WRITE_BYTES} bytes"
                    )));
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, content)?;
                format!(
                    "Wrote {} ({} bytes).",
                    args["path"].as_str().unwrap_or_default(),
                    content.len()
                )
            }
            Op::List => {
                let mut files = Vec::new();
                list(root, root, &mut files);
                files.sort();
                if files.is_empty() {
                    "The workspace is empty.".into()
                } else {
                    let total = files.len();
                    files.truncate(MAX_LISTED);
                    let mut text = files.join("\n");
                    if total > MAX_LISTED {
                        text.push_str(&format!("\n… and {} more", total - MAX_LISTED));
                    }
                    text
                }
            }
        };
        Ok(ToolOutcome::text(output))
    }
}

fn list(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            list(root, &path, out);
        } else if let (Ok(rel), Ok(meta)) = (path.strip_prefix(root), entry.metadata()) {
            out.push(format!(
                "{} ({} bytes)",
                rel.to_string_lossy().replace('\\', "/"),
                meta.len()
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use openslacktag_core::SessionId;
    use openslacktag_mock::MockGate;

    fn ctx(root: &Path) -> ToolContext {
        ToolContext {
            root: root.to_path_buf(),
            session: SessionId::new(),
            call_id: "c".into(),
            permissions: Arc::new(MockGate::allow_all()),
        }
    }

    fn tool(id: &str) -> Arc<dyn Tool> {
        file_tools().into_iter().find(|t| t.id() == id).unwrap()
    }

    #[tokio::test]
    async fn write_list_read_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        let wrote = tool("write_file")
            .execute(json!({"path": "reports/q3.md", "content": "# Q3"}), &ctx)
            .await
            .unwrap();
        assert!(wrote.output.contains("4 bytes"));
        let listed = tool("list_files").execute(json!({}), &ctx).await.unwrap();
        assert_eq!(listed.output, "reports/q3.md (4 bytes)");
        let read = tool("read_file")
            .execute(json!({"path": "/work/reports/q3.md"}), &ctx)
            .await
            .unwrap();
        assert_eq!(read.output, "# Q3");
    }

    #[tokio::test]
    async fn paths_cannot_leave_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir.path().join("ws"));
        for bad in ["../secret", "/etc/passwd", "a/../../b", "", "C:\\x"] {
            assert!(resolve(&ctx.root, bad).is_err() || bad == "C:\\x", "{bad}");
        }
        assert!(
            tool("read_file")
                .execute(json!({"path": "../x"}), &ctx)
                .await
                .is_err()
        );
        std::fs::create_dir_all(&ctx.root).unwrap();
        std::fs::write(ctx.root.join("bin"), [0u8, 1, 2]).unwrap();
        assert!(
            tool("read_file")
                .execute(json!({"path": "bin"}), &ctx)
                .await
                .is_err()
        );
    }
}
