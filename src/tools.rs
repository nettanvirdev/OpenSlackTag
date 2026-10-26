//! The teammate's own tools: the team wiki, web fetching, and a sandboxed shell.
//! MCP servers add more through the registry; see `engine.rs`.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use openslacktag_core::error::{Error, Result};
use openslacktag_core::tool::{PermissionRequest, Tool, ToolContext, ToolOutcome, ToolSource};
use reqwest::Url;
use reqwest::header::{CONTENT_TYPE, LOCATION};
use serde_json::{Value, json};
use tokio::process::Command;

const MAX_FETCH_BYTES: usize = 2_000_000;
const MAX_WIKI_PAGE_BYTES: usize = 100_000;
const SHELL_TIMEOUT: Duration = Duration::from_secs(120);

/// Where the sandboxed shell runs: a Docker image and its network mode.
#[derive(Debug, Clone)]
pub struct Sandbox {
    pub image: String,
    pub network: String,
}

/// The team wiki: Markdown pages in a folder, the teammate's long-term memory.
#[derive(Debug)]
pub struct Wiki {
    dir: PathBuf,
}

impl Wiki {
    pub fn open(dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// All page names, sorted, e.g. `team/standup`.
    pub fn pages(&self) -> Result<Vec<String>> {
        let mut files = Vec::new();
        collect_markdown(&self.dir, &mut files)?;
        // Files a person added with names the tools cannot address (spaces,
        // dots) are left out rather than breaking every listing and search.
        let mut pages: Vec<String> = files
            .iter()
            .filter_map(|f| f.strip_prefix(&self.dir).ok())
            .map(|rel| {
                rel.with_extension("")
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .filter(|page| self.path(page).is_ok())
            .collect();
        pages.sort();
        Ok(pages)
    }

    fn path(&self, page: &str) -> Result<PathBuf> {
        let page = page.trim().trim_end_matches(".md");
        let valid = !page.is_empty()
            && page.split('/').all(|seg| {
                !seg.is_empty()
                    && seg
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            });
        if !valid {
            return Err(Error::InvalidInput(format!(
                "invalid page name {page:?}: use letters, digits, '-', '_' and '/' for folders"
            )));
        }
        Ok(self.dir.join(format!("{page}.md")))
    }

    pub fn read(&self, page: &str) -> Result<String> {
        std::fs::read_to_string(self.path(page)?).map_err(|_| {
            Error::not_found("wiki page", format!("{page} (use wiki_list to see pages)"))
        })
    }

    pub fn write(&self, page: &str, content: &str) -> Result<()> {
        if content.len() > MAX_WIKI_PAGE_BYTES {
            return Err(Error::InvalidInput(format!(
                "page too large ({} bytes, max {MAX_WIKI_PAGE_BYTES}); split it into smaller pages",
                content.len()
            )));
        }
        let path = self.path(page)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Lines ranked by how many query words they (or their page name) contain.
    pub fn search(&self, query: &str) -> Result<String> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        if words.is_empty() {
            return Err(Error::InvalidInput("query is empty".into()));
        }
        let mut hits = Vec::new();
        for page in self.pages()? {
            let text = self.read(&page)?;
            let page_lower = page.to_lowercase();
            for (n, line) in text.lines().enumerate() {
                let line_lower = line.to_lowercase();
                let score = words
                    .iter()
                    .filter(|w| line_lower.contains(w.as_str()) || page_lower.contains(w.as_str()))
                    .count();
                if score > 0 && !line.trim().is_empty() {
                    hits.push((score, format!("{page}:{}: {}", n + 1, line.trim())));
                }
            }
        }
        hits.sort_by_key(|h| std::cmp::Reverse(h.0));
        Ok(if hits.is_empty() {
            "No matches.".into()
        } else {
            hits.into_iter()
                .take(30)
                .map(|(_, h)| h)
                .collect::<Vec<_>>()
                .join("\n")
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    WebFetch,
    WikiList,
    WikiSearch,
    WikiRead,
    WikiWrite,
    Shell,
}

/// One of the teammate's own tools. A single type rather than six, because
/// they differ only in schema and body.
#[derive(Debug)]
struct Builtin {
    kind: Kind,
    description: String,
    wiki: Arc<Wiki>,
    sandbox: Option<Sandbox>,
}

/// Every tool the teammate has without MCP. The shell is included only when a sandbox image is configured.
pub fn builtin_tools(wiki: &Arc<Wiki>, sandbox: Option<&Sandbox>) -> Vec<Arc<dyn Tool>> {
    let mut kinds = vec![
        Kind::WebFetch,
        Kind::WikiList,
        Kind::WikiSearch,
        Kind::WikiRead,
        Kind::WikiWrite,
    ];
    if sandbox.is_some() {
        kinds.push(Kind::Shell);
    }
    kinds
        .into_iter()
        .map(|kind| {
            let description = describe(kind, sandbox);
            Arc::new(Builtin {
                kind,
                description,
                wiki: wiki.clone(),
                sandbox: sandbox.cloned(),
            }) as Arc<dyn Tool>
        })
        .collect()
}

fn describe(kind: Kind, sandbox: Option<&Sandbox>) -> String {
    match kind {
        Kind::WebFetch => {
            "Fetch a public web page or API URL and return its text. Cite the URL in your answer."
                .into()
        }
        Kind::WikiList => "List all pages in the team wiki.".into(),
        Kind::WikiSearch => {
            "Search the team wiki. Returns matching lines as page:line: text.".into()
        }
        Kind::WikiRead => "Read one wiki page.".into(),
        Kind::WikiWrite => {
            "Create or fully replace a wiki page with Markdown. Read the page first if it exists, \
                            so you keep what is still true."
                .into()
        }
        Kind::Shell => {
            let (image, network) =
                sandbox.map_or(("?", "?"), |s| (s.image.as_str(), s.network.as_str()));
            format!(
                "Run a shell command on your own sandboxed Linux computer (Docker image {image}, network {network}). \
                 The working directory /work persists for this conversation; files you create there stay. \
                 Commands time out after {} seconds.",
                SHELL_TIMEOUT.as_secs()
            )
        }
    }
}

#[async_trait]
impl Tool for Builtin {
    fn id(&self) -> &str {
        match self.kind {
            Kind::WebFetch => "web_fetch",
            Kind::WikiList => "wiki_list",
            Kind::WikiSearch => "wiki_search",
            Kind::WikiRead => "wiki_read",
            Kind::WikiWrite => "wiki_write",
            Kind::Shell => "run_shell",
        }
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        let (properties, required) = match self.kind {
            Kind::WebFetch => (
                json!({"url": {"type": "string", "description": "http(s) URL"}}),
                vec!["url"],
            ),
            Kind::WikiList => (json!({}), vec![]),
            Kind::WikiSearch => (json!({"query": {"type": "string"}}), vec!["query"]),
            Kind::WikiRead => (
                json!({"page": {"type": "string", "description": "Page name, e.g. team/standup"}}),
                vec!["page"],
            ),
            Kind::WikiWrite => (
                json!({
                    "page": {"type": "string", "description": "Letters, digits, '-', '_', and '/' for folders"},
                    "content": {"type": "string", "description": "The full Markdown content"}
                }),
                vec!["page", "content"],
            ),
            Kind::Shell => (
                json!({"command": {"type": "string", "description": "Run with sh -c"}}),
                vec!["command"],
            ),
        };
        json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
    }

    fn source(&self) -> ToolSource {
        ToolSource::Builtin
    }

    fn permission(&self, args: &Value) -> PermissionRequest {
        let target = match self.kind {
            Kind::WebFetch => str_arg(args, "url").unwrap_or("*"),
            Kind::WikiRead | Kind::WikiWrite => str_arg(args, "page").unwrap_or("*"),
            Kind::Shell => str_arg(args, "command").unwrap_or("*"),
            Kind::WikiList | Kind::WikiSearch => "*",
        };
        PermissionRequest::new(self.id(), target)
    }

    fn render(&self, args: &Value) -> Option<String> {
        let detail = match self.kind {
            Kind::WebFetch => str_arg(args, "url"),
            Kind::WikiSearch => str_arg(args, "query"),
            Kind::WikiRead | Kind::WikiWrite => str_arg(args, "page"),
            Kind::Shell => str_arg(args, "command"),
            Kind::WikiList => None,
        }?;
        Some(crate::one_line(detail, 80))
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutcome> {
        let output = match self.kind {
            Kind::WebFetch => web_fetch(required(&args, "url")?).await?,
            Kind::WikiList => {
                let pages = self.wiki.pages()?;
                if pages.is_empty() {
                    "The wiki is empty.".into()
                } else {
                    pages.join("\n")
                }
            }
            Kind::WikiSearch => self.wiki.search(required(&args, "query")?)?,
            Kind::WikiRead => self.wiki.read(required(&args, "page")?)?,
            Kind::WikiWrite => {
                let (page, content) = (required(&args, "page")?, required(&args, "content")?);
                self.wiki.write(page, content)?;
                format!("Saved wiki page {page} ({} bytes).", content.len())
            }
            Kind::Shell => {
                let sandbox = self
                    .sandbox
                    .as_ref()
                    .ok_or_else(|| Error::Other("the sandbox is disabled".into()))?;
                run_shell(sandbox, &ctx.root, required(&args, "command")?).await?
            }
        };
        Ok(ToolOutcome::text(output))
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    str_arg(args, key)
        .ok_or_else(|| Error::InvalidInput(format!("missing string argument {key:?}")))
}

fn collect_markdown(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_markdown(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
    Ok(())
}

fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{nanos}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// One command in a fresh container, with the conversation's workspace mounted
/// at /work. `--rm` plus the kill on timeout means no container outlives its command.
async fn run_shell(sandbox: &Sandbox, workspace: &Path, command: &str) -> Result<String> {
    let workspace = std::path::absolute(workspace)?;
    std::fs::create_dir_all(&workspace)?;
    let name = format!("openslacktag-{}", unique_suffix());
    let mut cmd = Command::new("docker");
    cmd.args([
        "run",
        "--rm",
        "--name",
        &name,
        "--network",
        &sandbox.network,
    ])
    .args(["--memory", "1g", "--cpus", "1", "--pids-limit", "256"])
    .args(["--cap-drop", "ALL", "--security-opt", "no-new-privileges"])
    .args(["-e", "HOME=/work"])
    .arg("-v")
    .arg(format!("{}:/work", workspace.display()));
    // On Linux, run as the owner of the workspace rather than root, so files
    // the sandbox creates on the host are the bot's, never root-owned (and
    // never a setuid-root binary). Docker Desktop maps ownership itself.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(&workspace)?;
        cmd.args(["--user", &format!("{}:{}", meta.uid(), meta.gid())]);
    }
    cmd.args(["-w", "/work", &sandbox.image, "sh", "-c", command])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(SHELL_TIMEOUT, cmd.output()).await {
        Ok(out) => {
            let out = out.map_err(|e| {
                Error::Other(format!(
                    "could not run docker ({e}); is Docker installed and running?"
                ))
            })?;
            Ok(format!(
                "exit code: {}\nstdout:\n{}\nstderr:\n{}",
                out.status.code().map_or("killed".into(), |c| c.to_string()),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ))
        }
        Err(_) => {
            // Killing the docker CLI does not stop the container, so stop it by name.
            let _ = Command::new("docker").args(["kill", &name]).output().await;
            Err(Error::Timeout(SHELL_TIMEOUT))
        }
    }
}

/// Fetch a URL, refusing anything that resolves to a private or local address:
/// anyone in the workspace can ask for a fetch, so the bot must not become a
/// proxy into the host's network. Redirects are followed by hand so every hop
/// is checked, and the checked address is pinned so DNS cannot change between
/// the check and the request.
async fn web_fetch(raw_url: &str) -> Result<String> {
    let mut url =
        Url::parse(raw_url).map_err(|e| Error::InvalidInput(format!("invalid URL: {e}")))?;
    for _ in 0..5 {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(Error::InvalidInput(
                "only http and https URLs are allowed".into(),
            ));
        }
        let port = url.port_or_known_default().unwrap_or(443);
        // No proxy: a proxy would resolve the name again itself, undoing the check.
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .user_agent(concat!("OpenSlackTag/", env!("CARGO_PKG_VERSION")));
        let public = match url.host() {
            Some(url::Host::Domain(domain)) => {
                builder = builder.resolve(domain, resolve_public(domain, port).await?);
                true
            }
            Some(url::Host::Ipv4(ip)) => is_public(ip.into()),
            Some(url::Host::Ipv6(ip)) => is_public(ip.into()),
            None => return Err(Error::InvalidInput("URL has no host".into())),
        };
        if !public {
            return Err(Error::Denied(
                "refusing to fetch a private or local address".into(),
            ));
        }
        let client = builder.build().map_err(|e| Error::Other(e.to_string()))?;
        let mut resp = client
            .get(url.clone())
            .send()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;
        let status = resp.status();
        if status.is_redirection() {
            let location = resp
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| Error::Protocol("redirect without a Location header".into()))?;
            url = url
                .join(location)
                .map_err(|e| Error::Protocol(format!("bad redirect: {e}")))?;
            continue;
        }
        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();
        let mut body = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| Error::Network(e.to_string()))?
        {
            body.extend_from_slice(&chunk);
            if body.len() >= MAX_FETCH_BYTES {
                body.truncate(MAX_FETCH_BYTES);
                break;
            }
        }
        let text = if content_type.contains("html") {
            html2text::from_read(body.as_slice(), 100)
                .unwrap_or_else(|_| String::from_utf8_lossy(&body).into_owned())
        } else {
            String::from_utf8_lossy(&body).into_owned()
        };
        return Ok(format!("URL: {url}\nStatus: {status}\n\n{text}"));
    }
    Err(Error::Protocol("too many redirects".into()))
}

async fn resolve_public(host: &str, port: u16) -> Result<SocketAddr> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| Error::Network(format!("could not resolve {host}: {e}")))?
        .collect();
    match addrs.first() {
        Some(first) if addrs.iter().all(|a| is_public(a.ip())) => Ok(*first),
        Some(_) => Err(Error::Denied(format!(
            "refusing to fetch {host}: it resolves to a private or local address"
        ))),
        None => Err(Error::Network(format!("could not resolve {host}"))),
    }
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || a == 0
                || a >= 240
                || (a == 100 && (b & 0xc0) == 64) // 100.64.0.0/10 carrier-grade NAT
                || (a == 198 && (b & 0xfe) == 18)) // 198.18.0.0/15 benchmarking
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            // Addresses that carry an IPv4 address are judged by it: mapped
            // (::ffff:a.b.c.d), NAT64 (64:ff9b::a.b.c.d) and 6to4 (2002:ab:cd::).
            let embedded = |hi: u16, lo: u16| {
                std::net::Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8)
            };
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(v4.into());
            }
            if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                return is_public(embedded(s[6], s[7]).into());
            }
            if s[0] == 0x2002 {
                return is_public(embedded(s[1], s[2]).into());
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || s[..6] == [0; 6] // IPv4-compatible ::a.b.c.d, deprecated
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link local
                || (s[0] & 0xffc0) == 0xfec0 // site local, deprecated
                || (s[0] == 0x2001 && s[1] == 0x0db8)) // documentation
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn wiki() -> (Arc<Wiki>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Arc::new(Wiki::open(dir.path().join("wiki")).unwrap()), dir)
    }

    fn tool(tools: &[Arc<dyn Tool>], id: &str) -> Arc<dyn Tool> {
        tools.iter().find(|t| t.id() == id).unwrap().clone()
    }

    #[test]
    fn blocks_private_and_local_addresses() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "64:ff9b::a9fe:a9fe", // NAT64 of 169.254.169.254
            "64:ff9b::0a00:0001", // NAT64 of 10.0.0.1
            "2002:c0a8:0101::1",  // 6to4 of 192.168.1.1
            "::127.0.0.1",        // IPv4-compatible
            "fec0::1",
            "2001:db8::1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip} should be blocked");
        }
        for ip in [
            "1.1.1.1",
            "93.184.216.34",
            "2606:4700:4700::1111",
            "64:ff9b::0101:0101", // NAT64 of 1.1.1.1
            "2002:0101:0101::1",  // 6to4 of 1.1.1.1
        ] {
            assert!(is_public(ip.parse().unwrap()), "{ip} should be allowed");
        }
    }

    #[tokio::test]
    async fn web_fetch_refuses_local_targets() {
        for url in [
            "http://127.0.0.1/",
            "http://localhost:8080/",
            "http://[::1]/",
            "file:///etc/passwd",
            "nope",
        ] {
            assert!(web_fetch(url).await.is_err(), "{url} should be refused");
        }
    }

    #[test]
    fn page_names_cannot_escape_the_wiki() {
        let (wiki, dir) = wiki();
        for bad in [
            "../secrets",
            "a/../../b",
            "/etc/passwd",
            "a\\b",
            "",
            "a//b",
            "C:evil",
            "a.b",
        ] {
            assert!(wiki.path(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(
            wiki.path("team/standup")
                .unwrap()
                .starts_with(dir.path().join("wiki"))
        );
    }

    #[test]
    fn wiki_write_list_read_search() {
        let (wiki, _dir) = wiki();
        wiki.write("team/standup", "# Standup\nDaily at 10am in #general.")
            .unwrap();
        assert_eq!(wiki.pages().unwrap(), vec!["team/standup"]);
        assert!(wiki.read("team/standup.md").unwrap().contains("10am"));
        assert!(
            wiki.search("STANDUP time 10am")
                .unwrap()
                .starts_with("team/standup:2:")
        );
        assert_eq!(wiki.search("zebra").unwrap(), "No matches.");
        assert!(matches!(wiki.read("missing"), Err(Error::NotFound { .. })));

        // A file a person dropped in with an awkward name is skipped, not fatal.
        std::fs::write(wiki.dir.join("Q3 plans.md"), "standup moved").unwrap();
        assert_eq!(wiki.pages().unwrap(), vec!["team/standup"]);
        assert!(wiki.search("standup").is_ok());
        assert!(matches!(
            wiki.write("big", &"x".repeat(MAX_WIKI_PAGE_BYTES + 1)),
            Err(Error::InvalidInput(_))
        ));
    }

    #[test]
    fn shell_is_offered_only_with_a_sandbox() {
        let (wiki, _dir) = wiki();
        let without = builtin_tools(&wiki, None);
        assert!(!without.iter().any(|t| t.id() == "run_shell"));
        let with = builtin_tools(
            &wiki,
            Some(&Sandbox {
                image: "alpine:3".into(),
                network: "none".into(),
            }),
        );
        let shell = tool(&with, "run_shell");
        assert!(shell.description().contains("alpine:3"));
        assert_eq!(shell.permission(&json!({"command": "ls"})).target, "ls");
    }

    #[test]
    fn schemas_require_their_arguments() {
        let (wiki, _dir) = wiki();
        let tools = builtin_tools(&wiki, None);
        let write = tool(&tools, "wiki_write");
        assert_eq!(write.parameters()["required"], json!(["page", "content"]));
        assert_eq!(
            write.render(&json!({"page": "team/people"})).as_deref(),
            Some("team/people")
        );
    }
}
