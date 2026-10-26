//! Tool calls described the way a person would: "Reading #general" while it
//! happens, "Read #general" in the summary under the answer.

use serde_json::Value;

/// What a step looks like while it runs and once it is done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub doing: String,
    pub done: String,
}

fn step(doing: impl Into<String>, done: impl Into<String>) -> Step {
    Step {
        doing: doing.into(),
        done: done.into(),
    }
}

/// A channel as Slack should show it: ids become links, names keep their `#`.
fn channel(raw: &str) -> String {
    let raw = raw.trim();
    let is_id = raw.len() > 8
        && raw.starts_with(['C', 'G', 'D'])
        && raw
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    match raw {
        "here" | "" => "this channel".into(),
        _ if is_id => format!("<#{raw}>"),
        _ if raw.starts_with('#') || raw.starts_with("<#") => raw.into(),
        _ => format!("#{raw}"),
    }
}

fn host(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| h.trim_start_matches("www.").to_string())
        })
        .unwrap_or_else(|| "a web page".into())
}

/// The labels for a call to `tool` with `args`.
pub fn label(tool: &str, args: &Value) -> Step {
    let s = |key: &str| args[key].as_str().unwrap_or_default();
    match tool {
        "web_fetch" => {
            let h = host(s("url"));
            step(format!("Reading {h}"), format!("Read {h}"))
        }
        "wiki_search" => step("Searching the wiki", "Searched the wiki"),
        "wiki_list" => step("Looking through the wiki", "Looked through the wiki"),
        "wiki_history" => step("Checking wiki history", "Checked wiki history"),
        "wiki_read" => step(
            format!("Reading wiki page {}", s("page")),
            format!("Read wiki page {}", s("page")),
        ),
        "wiki_write" => step(
            format!("Updating wiki page {}", s("page")),
            format!("Updated wiki page {}", s("page")),
        ),
        "run_shell" => {
            let c = crate::one_line(s("command"), 40);
            step(format!("Running `{c}`"), format!("Ran `{c}`"))
        }
        "read_file" => step(
            format!("Reading {}", s("path")),
            format!("Read {}", s("path")),
        ),
        "write_file" => step(
            format!("Writing {}", s("path")),
            format!("Wrote {}", s("path")),
        ),
        "list_files" => step("Checking the workspace", "Checked the workspace"),
        "share_file" => step(
            format!("Sharing {}", s("path")),
            format!("Shared {}", s("path")),
        ),
        "slack_read_channel" => {
            let c = channel(s("channel"));
            step(format!("Reading {c}"), format!("Read {c}"))
        }
        "slack_read_thread" => step("Reading a thread", "Read a thread"),
        "slack_list_channels" => step("Checking channels", "Checked channels"),
        "slack_user_info" => step(
            format!("Looking up <@{}>", s("user")),
            format!("Looked up <@{}>", s("user")),
        ),
        "slack_react" => step("Reacting", "Reacted"),
        "slack_post_message" => {
            let c = channel(s("channel"));
            step(format!("Posting to {c}"), format!("Posted to {c}"))
        }
        "schedule_task" => step("Creating a routine", "Created a routine"),
        "schedule_list" => step("Checking your routines", "Checked your routines"),
        "schedule_update" => step("Updating a routine", "Updated a routine"),
        "schedule_cancel" => step("Cancelling a routine", "Cancelled a routine"),
        "watch_channel" => step("Setting up a trigger", "Set up a trigger"),
        "skill_read" => step(
            format!("Reading skill {}", s("name")),
            format!("Read skill {}", s("name")),
        ),
        "generate_image" => step("Generating an image", "Generated an image"),
        "delegate" => {
            let n = args["tasks"].as_array().map_or(0, Vec::len);
            let things = if n == 1 {
                "1 task".to_string()
            } else {
                format!("{n} tasks")
            };
            step(
                format!("Working on {things} in parallel"),
                format!("Worked on {things} in parallel"),
            )
        }
        other => step(format!("Using {other}"), format!("Used {other}")),
    }
}

/// The one-line summary under an answer: "Read #general, searched the wiki
/// and 3 more". Repeats are counted once.
pub fn summarize(steps: &[String]) -> String {
    let mut unique: Vec<&String> = Vec::new();
    for s in steps {
        if !unique.contains(&s) {
            unique.push(s);
        }
    }
    let lower = |s: &str| {
        let mut chars = s.chars();
        match chars.next() {
            Some(first) => first.to_lowercase().chain(chars).collect(),
            None => String::new(),
        }
    };
    match unique.as_slice() {
        [] => String::new(),
        [one] => (*one).clone(),
        [a, b] => format!("{a} and {}", lower(b)),
        [a, b, rest @ ..] => format!("{a}, {} and {} more", lower(b), rest.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn labels_read_like_a_person() {
        assert_eq!(
            label("web_fetch", &json!({"url": "https://www.example.com/a"})).done,
            "Read example.com"
        );
        assert_eq!(
            label("slack_read_channel", &json!({"channel": "C0123456789"})).doing,
            "Reading <#C0123456789>"
        );
        assert_eq!(
            label("slack_read_channel", &json!({"channel": "general"})).done,
            "Read #general"
        );
        assert_eq!(
            label("slack_read_channel", &json!({"channel": "here"})).done,
            "Read this channel"
        );
        assert_eq!(
            label("delegate", &json!({"tasks": ["a", "b"]})).done,
            "Worked on 2 tasks in parallel"
        );
        assert_eq!(
            label("github_create_issue", &json!({})).done,
            "Used github_create_issue"
        );
    }

    #[test]
    fn summaries_stay_short() {
        let s = |v: &[&str]| summarize(&v.iter().map(|x| x.to_string()).collect::<Vec<_>>());
        assert_eq!(s(&[]), "");
        assert_eq!(s(&["Read #a"]), "Read #a");
        assert_eq!(
            s(&["Read #a", "Searched the wiki"]),
            "Read #a and searched the wiki"
        );
        assert_eq!(
            s(&[
                "Checked your routines",
                "Read #a",
                "Read #b",
                "Read #b",
                "Updated a routine"
            ]),
            "Checked your routines, read #a and 2 more"
        );
    }
}
