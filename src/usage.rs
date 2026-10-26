//! Token usage, one line per task in `DATA_DIR/usage.jsonl`, and the summary
//! `openslacktag usage` prints.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use openslacktag_core::provider::Usage;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct Line {
    at: String,
    conversation: String,
    model: String,
    input: u64,
    output: u64,
}

/// Append one task's usage. Failures are logged, never fatal: losing a usage
/// line must not lose an answer.
pub fn record(data_dir: &Path, conversation: &str, model: &str, usage: &Usage) {
    let line = Line {
        at: jiff::Timestamp::now().to_string(),
        conversation: conversation.to_string(),
        model: model.to_string(),
        input: usage.total_input(),
        output: usage.output_tokens,
    };
    let write = || -> std::io::Result<()> {
        crate::append_line(
            &data_dir.join("usage.jsonl"),
            &serde_json::to_string(&line)?,
        )
    };
    if let Err(e) = write() {
        tracing::warn!("could not record usage: {e}");
    }
}

/// Totals per day and model for the last `days` days.
pub fn summary(data_dir: &Path, days: i64) -> Result<String> {
    let text = match std::fs::read_to_string(data_dir.join("usage.jsonl")) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok("No usage recorded yet.".into());
        }
        Err(e) => return Err(e.into()),
    };
    let since = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(24 * days);
    let mut totals: BTreeMap<(String, String), (u64, u64, u64)> = BTreeMap::new();
    for line in text
        .lines()
        .filter_map(|l| serde_json::from_str::<Line>(l).ok())
    {
        let Ok(at) = line.at.parse::<jiff::Timestamp>() else {
            continue;
        };
        if at < since {
            continue;
        }
        let day = at.to_string().chars().take(10).collect::<String>();
        let entry = totals.entry((day, line.model)).or_default();
        entry.0 += 1;
        entry.1 += line.input;
        entry.2 += line.output;
    }
    if totals.is_empty() {
        return Ok(format!("No usage in the last {days} days."));
    }
    let mut out = format!(
        "{:<12} {:<32} {:>6} {:>12} {:>12}\n",
        "day", "model", "tasks", "input", "output"
    );
    let (mut tasks, mut input, mut output) = (0, 0, 0);
    for ((day, model), (t, i, o)) in &totals {
        out.push_str(&format!("{day:<12} {model:<32} {t:>6} {i:>12} {o:>12}\n"));
        tasks += t;
        input += i;
        output += o;
    }
    out.push_str(&format!(
        "{:<12} {:<32} {tasks:>6} {input:>12} {output:>12}",
        "total", ""
    ));
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn records_and_summarises() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(summary(dir.path(), 7).unwrap(), "No usage recorded yet.");
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 50,
            cache_write_tokens: 0,
        };
        record(dir.path(), "c1", "model-a", &usage);
        record(dir.path(), "c2", "model-a", &usage);
        let text = summary(dir.path(), 7).unwrap();
        let total = text.lines().last().unwrap();
        assert!(total.starts_with("total"), "{text}");
        assert!(
            total.contains(" 2 ") && total.contains("300") && total.contains("40"),
            "{text}"
        );
    }
}
