//! OpenSlackTag: an open-source AI teammate for Slack.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod compact;
pub mod config;
pub mod engine;
pub mod markdown;
pub mod setup;
pub mod slack;
pub mod tools;

/// The first line of `s`, cut to `max` characters.
pub fn one_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= max {
        line.to_string()
    } else {
        format!("{}…", line.chars().take(max).collect::<String>())
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
    #[test]
    fn one_line_cuts_on_characters() {
        assert_eq!(super::one_line("héllo world\nmore", 5), "héllo…");
        assert_eq!(super::one_line("short", 10), "short");
        assert_eq!(super::one_line("", 10), "");
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
