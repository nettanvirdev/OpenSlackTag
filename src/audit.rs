//! The audit log: one line per tool call in `DATA_DIR/audit.jsonl`, with who
//! asked, what ran on what, and how it ended. Read it with `openslacktag audit`.

use std::io::Write;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

const MAX_ARGS_CHARS: usize = 500;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Line {
    pub at: String,
    pub conversation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester: Option<String>,
    pub tool: String,
    pub args: String,
    /// `ok`, `failed` or `denied`.
    pub outcome: String,
}

/// Append one tool call. Failures are logged, never fatal.
pub fn record(data_dir: &Path, line: &Line) {
    let mut line = line.clone();
    line.args = line.args.chars().take(MAX_ARGS_CHARS).collect();
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(data_dir.join("audit.jsonl"))?;
        writeln!(file, "{}", serde_json::to_string(&line)?)
    };
    if let Err(e) = write() {
        tracing::warn!("could not write the audit log: {e}");
    }
}

/// The last `count` calls, oldest first, one per line.
pub fn tail(data_dir: &Path, count: usize) -> Result<String> {
    let text = match std::fs::read_to_string(data_dir.join("audit.jsonl")) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok("No tool calls recorded yet.".into());
        }
        Err(e) => return Err(e.into()),
    };
    let lines: Vec<Line> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let start = lines.len().saturating_sub(count);
    Ok(lines[start..]
        .iter()
        .map(|l| {
            format!(
                "{}  {:<7} {:<24} by {:<12} in {}  {}",
                l.at.chars().take(19).collect::<String>(),
                l.outcome,
                l.tool,
                l.requester.as_deref().unwrap_or("-"),
                l.conversation,
                crate::one_line(&l.args, 120)
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn records_and_tails() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(tail(dir.path(), 5).unwrap(), "No tool calls recorded yet.");
        for i in 0..3 {
            record(
                dir.path(),
                &Line {
                    at: "2026-10-26T10:00:00Z".into(),
                    conversation: "C1-1".into(),
                    requester: Some("U1".into()),
                    tool: format!("tool_{i}"),
                    args: "x".repeat(2000),
                    outcome: "ok".into(),
                },
            );
        }
        let text = tail(dir.path(), 2).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("tool_2") && !text.contains("tool_0"));
        let raw = std::fs::read_to_string(dir.path().join("audit.jsonl")).unwrap();
        assert!(raw.lines().all(|l| l.len() < 800), "arguments are capped");
    }
}
