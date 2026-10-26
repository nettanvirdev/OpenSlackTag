//! OpenSlackTag: an open-source AI teammate for Slack.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod audit;
pub mod compact;
pub mod config;
pub mod delegate;
pub mod engine;
pub mod files;
pub mod images;
pub mod markdown;
pub mod permissions;
pub mod schedule;
pub mod setup;
pub mod skills;
pub mod slack;
pub mod slack_tools;
pub mod steps;
pub mod suggest;
pub mod tools;
pub mod usage;

/// One completion with no tools, collected into a string. Errors are the
/// provider's own message.
pub async fn complete_text(
    provider: &dyn openslacktag_core::Provider,
    request: openslacktag_core::provider::ChatRequest,
) -> Result<String, String> {
    use futures::StreamExt;
    use openslacktag_core::provider::StreamEvent;
    let mut stream = provider.stream_chat(request);
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::Delta { text: t } => text.push_str(&t),
            StreamEvent::Error { message, .. } => return Err(message),
            _ => {}
        }
    }
    Ok(text)
}

/// Write a file through a temporary file and a rename, so a crash mid-write
/// never leaves a half-written file behind.
pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Logs start over past this size, keeping the previous one as `<name>.1`.
const MAX_LOG_BYTES: u64 = 20_000_000;

/// Append one line to a log file (audit, usage, feedback, wiki changes),
/// rotating it so it never grows without bound.
pub fn append_line(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        std::fs::rename(path, old)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")
}

/// The first line of `s`, cut to `max` characters.
pub fn one_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= max {
        line.to_string()
    } else {
        format!("{}…", line.chars().take(max).collect::<String>())
    }
}

/// A file name safe on every OS: path separators and odd characters become
/// `_`, and nothing is left that could climb out of a folder.
pub fn safe_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || "-_. ()".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches(['.', ' ']).to_string();
    if cleaned.is_empty() {
        "file".into()
    } else {
        cleaned
    }
}

/// A filesystem- and container-name-safe version of an id.
pub fn safe_id(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    #[test]
    fn logs_rotate_instead_of_growing_forever() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("audit.jsonl");
        std::fs::write(&log, vec![b'x'; super::MAX_LOG_BYTES as usize + 1]).unwrap();
        super::append_line(&log, "{}").unwrap();
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "{}
"
        );
        assert!(dir.path().join("audit.jsonl.1").exists());
    }

    #[test]
    fn one_line_cuts_on_characters() {
        assert_eq!(super::one_line("héllo world\nmore", 5), "héllo…");
        assert_eq!(super::one_line("short", 10), "short");
        assert_eq!(super::one_line("", 10), "");
    }

    #[test]
    fn file_names_stay_in_their_folder() {
        assert_eq!(super::safe_file_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(
            super::safe_file_name("Q3 report (final).pdf"),
            "Q3 report (final).pdf"
        );
        assert_eq!(super::safe_file_name(".."), "file");
        assert_eq!(super::safe_file_name(r"a\b:c"), "a_b_c");
    }

    #[test]
    fn safe_id_keeps_only_alphanumerics() {
        assert_eq!(
            super::safe_id("C123-1700000000.0001"),
            "C123-1700000000-0001"
        );
        assert_eq!(super::safe_id("../x"), "---x");
    }
}
