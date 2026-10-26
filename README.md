# OpenSlackTag

An open-source AI teammate for Slack. Tag it with a task, and it does the work with real tools and replies in the thread with the result and its sources.

```
you        @OpenSlackTag what changed in our deploy process this month? update the wiki
teammate   Using wiki_search: deploy…
           Using github_list_commits: infra…
           Deploys now go through the `release` workflow instead of manual tags (since Oct 3, <link|PR #412>).
           Rollbacks use `make rollback`. I updated *processes/deploys* in the wiki.
```

- **Does the work.** It runs an agent loop with tools: it fetches web pages, searches and writes the team wiki, runs code on its own sandboxed computer, and uses anything you connect over MCP.
- **Remembers your team.** A Markdown wiki it keeps up to date: processes, decisions, who owns what. It reads the wiki before answering and corrects it when things change.
- **Any model.** Anthropic natively, plus anything that speaks the OpenAI API: OpenAI, OpenRouter, Groq, DeepSeek, Ollama, LM Studio, vLLM.
- **MCP.** Connect GitHub, Linear, your database, or anything else with an MCP server (stdio or HTTP), using the same `mcp.json` format as Claude Desktop and Cursor.
- **Long threads stay sharp.** Each thread's full transcript, tool calls included, is kept. When it outgrows the context budget, older messages are compacted into a structured handoff note.
- **Steerable.** Mention it again while it's working ("actually, only the last week") and it folds that into the running task instead of starting over.
- **Live progress.** One message in the thread shows what it's doing ("Using web_fetch: …") and then turns into the answer.
- **Hardened for a shared workspace.** It answers only people in your own workspace (never Slack Connect guests) and, if you like, only an allowlist. Its replies can't ping `@channel` or `@here`, whatever a fetched page says.
- **Local-first.** One binary on your machine or server. Socket Mode means no public URL and no webhooks. Your wiki, transcripts and keys stay on disk where you put them.
- **Easy to set up.** `openslacktag setup` creates the Slack app from a prefilled manifest, lists your provider's models, tests everything and writes `.env`. `openslacktag doctor` tells you exactly what's broken.

## Quick start

You need [Rust](https://rustup.rs) 1.88 or newer. Docker is optional and only needed for the sandboxed shell.

```bash
git clone https://github.com/nettanvirdev/OpenSlackTag
```

```bash
cd OpenSlackTag
```

```bash
cargo install --path . --locked
```

```bash
openslacktag setup
```

Setup asks for your model provider and key, then gives you a link to Slack's "create app" page with everything filled in. You install the app, paste two tokens, and you're done. Then:

```bash
openslacktag slack
```

Invite it to a channel (`/invite @OpenSlackTag`) and mention it. You can also DM it.

**Try it without Slack:**

```bash
openslacktag chat
```

This is the same engine and tools, in your terminal.

## Commands

| Command | What it does |
|---|---|
| `openslacktag setup` | Guided setup: model, Slack app, sandbox. Writes `.env`. |
| `openslacktag slack` | Runs the teammate in Slack. |
| `openslacktag chat` | Talks to the teammate in the terminal. `/new` starts over, `/quit` leaves. |
| `openslacktag doctor` | Checks the model, Slack tokens, Docker image and MCP servers, and says how to fix each failure. |

## Configuration

Everything lives in `.env` in the folder you run from. See [.env.example](.env.example).

| Variable | Default | Meaning |
|---|---|---|
| `LLM_PROVIDER` | `openai` (or `anthropic` if the URL is Anthropic's) | `anthropic`, or `openai` for any OpenAI-compatible API |
| `LLM_BASE_URL` | the provider's official API | e.g. `http://localhost:11434/v1` for Ollama |
| `LLM_API_KEY` | | Your provider key (not needed for local models) |
| `LLM_MODEL` | | Model id, e.g. whatever `setup` lists |
| `SLACK_BOT_TOKEN` | | `xoxb-…` from *OAuth & Permissions* |
| `SLACK_APP_TOKEN` | | `xapp-…` app-level token with `connections:write` |
| `SANDBOX_IMAGE` | off | Docker image for the shell tool, e.g. `python:3.12-slim` |
| `SANDBOX_NETWORK` | `none` | `bridge` gives the sandbox network access (see Safety) |
| `ALLOWED_USERS` | everyone | Comma-separated Slack user ids allowed to use the teammate |
| `DATA_DIR` | `data` | Wiki, transcripts and sandbox workspaces |
| `MCP_CONFIG` | `mcp.json` | MCP servers file |
| `COMPACT_AT_TOKENS` | `80000` | When to compact a long conversation |

### MCP servers

Copy [mcp.example.json](mcp.example.json) to `mcp.json` and list your servers:

```json
{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "..." }
    },
    "remote": { "url": "https://mcp.example.com/mcp", "headers": { "Authorization": "Bearer ..." } }
  }
}
```

Their tools show up as `<server>_<tool>`. A server that fails to start is reported and skipped, so it never takes the bot down.

### The sandbox

With `SANDBOX_IMAGE` set, the teammate gets `run_shell`: every command runs in a fresh container (`--rm`, 1 CPU, 1 GB, no new privileges, no network by default). Each conversation gets its own `/work` folder that persists between commands.

## Safety

The teammate acts with your keys, so think of it as a colleague with your tokens. Anyone who can mention it can use its tools, MCP servers included.

- **Who can use it.** People in your workspace only: Slack Connect guests from other organisations are ignored. Set `ALLOWED_USERS` to narrow it further.
- **Web fetches can't reach private networks.** Any URL that resolves to a loopback, private, link-local or carrier-NAT address (including IPv6 forms like NAT64 and 6to4) is refused. The check runs on every redirect hop, with the checked address pinned against DNS rebinding and proxies bypassed.
- **The shell only runs in Docker,** never on the host. Containers get no capabilities, no new privileges, 1 CPU, 1 GB of memory, and on Linux your user instead of root. With `SANDBOX_NETWORK=none` (the default) they have no network at all. `bridge` lets them reach anything the host can reach, including your LAN and cloud metadata endpoints. Only use it on a machine where that's acceptable.
- **No channel-wide pings.** Replies are escaped for Slack, so only user mentions, channel links and web links survive. `@channel`, `@here` and group mentions never fire.
- **One shared wiki.** It's plain Markdown in `DATA_DIR`, shared by every channel. Something learned in a private channel can surface elsewhere, and text someone plants there persists. Review it like any shared doc. The teammate is told never to store secrets in it.
- **Keys stay in `.env`.** It's git-ignored, written readable only by you, and API keys are kept out of logs.
- **Every tool call goes through one registry.** It validates arguments against the tool's schema, and refuses a fourth identical call in a row.

## How it works

```
Slack (Socket Mode) ──► slack.rs ──► engine.rs ──► agent loop (crates/agent)
                                        │               │
                                        │               ├─► model providers (crates/provider)
                                        │               └─► tool registry (crates/tools)
                                        │                     ├─ wiki, web_fetch, run_shell (src/tools.rs)
                                        │                     └─ MCP servers (crates/mcp)
                                        └─► transcripts + compaction (src/compact.rs)
```

| Crate | Role |
|---|---|
| `crates/core` | Shared types: messages, tools, providers, permissions, errors |
| `crates/provider` | Anthropic and OpenAI-compatible streaming clients, retries |
| `crates/agent` | The turn loop: tool calls, repeated-call guard, steering |
| `crates/tools` | The tool registry: schema validation, permission checks, output limits |
| `crates/mcp` | MCP client (stdio and HTTP) exposing servers as tools |
| `crates/mock` | Scripted model and tools for tests |

## Development

```bash
cargo test --workspace
```

The tests need no Slack workspace and no API key. `tests/e2e.rs` runs the real Slack loop against a fake Slack (Web API plus the Socket Mode WebSocket) and a scripted model. It covers threaded replies, duplicate deliveries, steering mid-task, and compaction.

## Roadmap

- Approval buttons in Slack for risky tools, with per-tool allow/ask/deny rules
- Scheduled tasks and "you keep doing X, want me to automate it?" suggestions
- A browser in the sandbox
- Reading files and images people share in threads
- Prebuilt binaries and a Docker image

## License

MIT. OpenSlackTag is an independent project and is not affiliated with Slack.
