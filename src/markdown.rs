//! Models write GitHub Markdown even when asked not to; Slack uses its own
//! "mrkdwn". Convert the common cases, leaving code untouched, then escape
//! everything Slack would treat as markup.

pub fn to_mrkdwn(md: &str) -> String {
    let mut out = Vec::new();
    let mut in_code_block = false;
    for line in md.lines() {
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            out.push(line.to_string());
        } else if in_code_block {
            out.push(line.to_string());
        } else {
            out.push(convert_line(line));
        }
    }
    escape(&out.join("\n"))
}

/// Escape `&`, `<` and `>` as Slack requires, keeping only the tokens a
/// teammate should be able to send: user mentions `<@U…>`, channel links
/// `<#C…>` and web links `<https://…|label>`. Everything else is shown as
/// text. In particular `<!channel>`, `<!here>` and `<!subteam^…>` never
/// notify anyone, even if a fetched page talks the model into writing them.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(['&', '<', '>']) {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        if tail.starts_with('<')
            && let Some(end) = tail.find('>')
            && is_allowed_token(&tail[1..end])
        {
            out.push_str(&tail[..=end]);
            rest = &tail[end + 1..];
            continue;
        }
        out.push_str(match tail.as_bytes()[0] {
            b'&' => "&amp;",
            b'<' => "&lt;",
            _ => "&gt;",
        });
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

fn is_allowed_token(inner: &str) -> bool {
    let target = inner.split('|').next().unwrap_or_default();
    let id_like = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric());
    if let Some(id) = target.strip_prefix('@') {
        return id_like(id) && (id.starts_with('U') || id.starts_with('W'));
    }
    if let Some(id) = target.strip_prefix('#') {
        return id_like(id) && id.starts_with('C');
    }
    let is_link = ["http://", "https://", "mailto:"]
        .iter()
        .any(|p| target.starts_with(p));
    is_link && !target.contains(char::is_whitespace) && !inner.contains('\n')
}

fn convert_line(line: &str) -> String {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
        return format!(
            "*{}*",
            convert_inline(trimmed[hashes..].trim()).trim_matches('*')
        );
    }
    convert_inline(line)
}

/// Convert `**bold**`, `[label](url)` and `~~strike~~`, skipping `inline code`.
/// `__bold__` is left alone so identifiers like `__init__` survive outside backticks.
fn convert_inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, part) in text.split('`').enumerate() {
        if i > 0 {
            out.push('`');
        }
        if i % 2 == 1 {
            out.push_str(part); // inside inline code
        } else {
            out.push_str(&convert_links(&part.replace("**", "*").replace("~~", "~")));
        }
    }
    out
}

fn convert_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let link = after.find("](").and_then(|mid| {
            let url_part = &after[mid + 2..];
            let close = url_part.find(')')?;
            let (label, url) = (&after[..mid], &url_part[..close]);
            let is_url = url.starts_with("http://")
                || url.starts_with("https://")
                || url.starts_with("mailto:");
            (is_url && !label.contains('[') && !url.contains(' '))
                .then(|| (format!("<{url}|{label}>"), mid + 2 + close + 1))
        });
        match link {
            Some((slack_link, consumed)) => {
                out.push_str(&rest[..open]);
                out.push_str(&slack_link);
                rest = &after[consumed..];
            }
            None => {
                out.push_str(&rest[..=open]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::to_mrkdwn;

    #[test]
    fn converts_common_markdown() {
        assert_eq!(to_mrkdwn("## Summary"), "*Summary*");
        assert_eq!(to_mrkdwn("This is **important**."), "This is *important*.");
        assert_eq!(
            to_mrkdwn("See [the docs](https://example.com/a_b) and [x](https://y.z)."),
            "See <https://example.com/a_b|the docs> and <https://y.z|x>."
        );
        assert_eq!(to_mrkdwn("~~old~~ new"), "~old~ new");
        assert_eq!(to_mrkdwn("- item\n- [ ] todo"), "- item\n- [ ] todo");
    }

    #[test]
    fn escapes_markup_but_keeps_mentions_and_links() {
        assert_eq!(
            to_mrkdwn("<!channel> urgent <!here> <!subteam^S1>"),
            "&lt;!channel&gt; urgent &lt;!here&gt; &lt;!subteam^S1&gt;"
        );
        assert_eq!(
            to_mrkdwn("ask <@U123> in <#C456|general>, see <https://a.b|docs>"),
            "ask <@U123> in <#C456|general>, see <https://a.b|docs>"
        );
        assert_eq!(
            to_mrkdwn("Vec<String> & a > b"),
            "Vec&lt;String&gt; &amp; a &gt; b"
        );
        assert_eq!(to_mrkdwn("<javascript:x|y>"), "&lt;javascript:x|y&gt;");
        assert_eq!(to_mrkdwn("unclosed <@U1"), "unclosed &lt;@U1");
    }

    #[test]
    fn leaves_code_alone() {
        let md = "Run `a**b` then:\n```\nx = **kwargs\n[a](https://b)\n```\n**done**";
        assert_eq!(
            to_mrkdwn(md),
            "Run `a**b` then:\n```\nx = **kwargs\n[a](https://b)\n```\n*done*"
        );
    }

    #[test]
    fn ignores_non_links_and_slack_links() {
        assert_eq!(
            to_mrkdwn("arr[0](x) and [a](ftp://x)"),
            "arr[0](x) and [a](ftp://x)"
        );
        assert_eq!(to_mrkdwn("<https://a.b|label>"), "<https://a.b|label>");
    }
}
