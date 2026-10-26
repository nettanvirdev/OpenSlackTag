//! Who may do what. Every tool call is checked against allow / ask / deny
//! rules; an `ask` goes to a person (Slack buttons, or a prompt in the
//! terminal chat) and waits for their answer.
//!
//! Rules come in three layers, later winning on an identical tool + pattern:
//! the built-in defaults, the operator's `permissions.json`, and what people
//! approved "always, in this conversation".

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use openslacktag_agent::DOOM_LOOP_KEY;
use openslacktag_core::permission::{Action, Rule, evaluate, merge};
use openslacktag_core::tool::{Decision, PermissionGate, PermissionRequest};
use parking_lot::Mutex;
use serde::Deserialize;
use tokio::sync::mpsc::UnboundedSender;

/// A tool call waiting for someone's yes or no.
#[derive(Debug, Clone)]
pub struct Approval {
    /// The permission key: a tool name, or `mcp` for MCP tools.
    pub tool: String,
    /// What it acts on: a URL, a command, an MCP tool name.
    pub target: String,
    /// Extra context for the person deciding, such as an MCP call's arguments.
    pub detail: Option<String>,
}

/// Whoever answers `ask` rules for a conversation.
#[async_trait]
pub trait Approver: Send + Sync + std::fmt::Debug {
    async fn approve(&self, approval: &Approval) -> Decision;
}

/// For contexts with nobody to ask (listing tools, scheduled runs without a
/// channel): every `ask` is refused.
#[derive(Debug)]
pub struct NobodyToAsk;

#[async_trait]
impl Approver for NobodyToAsk {
    async fn approve(&self, _approval: &Approval) -> Decision {
        Decision::Deny
    }
}

/// The defaults. Reading and memory are allowed; the shell is allowed because
/// it only runs in the sandbox; anything acting on the outside world through
/// someone's tokens (MCP tools, posting to Slack) asks first.
pub fn default_rules() -> Vec<Rule> {
    let allow = [
        "web_fetch",
        "web_search",
        "wiki_*",
        "run_shell",
        "read_file",
        "write_file",
        "list_files",
        "share_file",
        "delegate",
        "skill_read",
        "schedule_*",
        "slack_read_*",
        "slack_list_*",
        "slack_user_info",
        "slack_react",
    ];
    let ask = ["mcp", "slack_post_message"];
    allow
        .iter()
        .map(|t| Rule::for_any(*t, Action::Allow))
        .chain(ask.iter().map(|t| Rule::for_any(*t, Action::Ask)))
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesFile {
    #[serde(default)]
    rules: Vec<Rule>,
}

/// The defaults merged with the operator's rules file, if there is one.
pub fn load_rules(path: &Path) -> Result<Vec<Rule>> {
    let file_rules = match std::fs::read_to_string(path) {
        Ok(text) => {
            serde_json::from_str::<RulesFile>(&text)
                .with_context(|| {
                    format!(
                        "could not read {}: expected {{\"rules\": [{{\"tool\": ..., \"pattern\": ..., \"action\": \"allow|ask|deny\"}}]}}",
                        path.display()
                    )
                })?
                .rules
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
    };
    Ok(merge(&[&default_rules(), &file_rules]))
}

/// The permission gate for one conversation's task.
#[derive(Debug)]
pub struct Gate {
    rules: Vec<Rule>,
    /// "Always, in this conversation" approvals; saved with the transcript.
    session: Arc<Mutex<Vec<Rule>>>,
    approver: Arc<dyn Approver>,
    progress: Option<UnboundedSender<String>>,
}

impl Gate {
    pub fn new(
        rules: Vec<Rule>,
        session: Arc<Mutex<Vec<Rule>>>,
        approver: Arc<dyn Approver>,
        progress: Option<UnboundedSender<String>>,
    ) -> Self {
        Self {
            rules,
            session,
            approver,
            progress,
        }
    }

    fn resolve(&self, key: &str, target: &str) -> Action {
        // The loop guard asks about a fourth identical call in a row. Nobody
        // is watching closely enough to want that, so it is always refused.
        if key == DOOM_LOOP_KEY {
            return Action::Deny;
        }
        let session = self.session.lock();
        evaluate(&merge(&[&self.rules, &session]), key, target).action
    }
}

#[async_trait]
impl PermissionGate for Gate {
    async fn ask(&self, request: &PermissionRequest) -> openslacktag_core::Result<Decision> {
        match self.resolve(&request.key, &request.target) {
            Action::Allow => Ok(Decision::Allow),
            Action::Deny => Ok(Decision::Deny),
            Action::Ask => {
                if let Some(progress) = &self.progress {
                    let _ = progress.send(format!(
                        "Waiting for approval to use {}: {}",
                        request.key, request.target
                    ));
                }
                let approval = Approval {
                    tool: request.key.clone(),
                    target: request.target.clone(),
                    detail: request.detail.clone(),
                };
                let decision = self.approver.approve(&approval).await;
                if decision == Decision::AllowAlways {
                    let pattern = request
                        .always
                        .clone()
                        .unwrap_or_else(|| request.target.clone());
                    self.session.lock().push(Rule::new(
                        request.key.clone(),
                        Action::Allow,
                        pattern,
                    ));
                }
                Ok(decision)
            }
        }
    }

    async fn verdict(&self, key: &str, target: &str) -> Action {
        self.resolve(key, target)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Answers every approval the same way and counts how often it was asked.
    #[derive(Debug)]
    struct Always(Decision, Mutex<usize>);

    #[async_trait]
    impl Approver for Always {
        async fn approve(&self, _approval: &Approval) -> Decision {
            *self.1.lock() += 1;
            self.0
        }
    }

    fn gate(rules: Vec<Rule>, answer: Decision) -> (Gate, Arc<Always>) {
        let approver = Arc::new(Always(answer, Mutex::new(0)));
        let gate = Gate::new(rules, Arc::default(), approver.clone(), None);
        (gate, approver)
    }

    #[tokio::test]
    async fn defaults_allow_reading_and_ask_before_acting() {
        let (gate, asked) = gate(default_rules(), Decision::Deny);
        let allowed = PermissionRequest::new("web_fetch", "https://a.b");
        assert_eq!(gate.ask(&allowed).await.unwrap(), Decision::Allow);
        assert_eq!(gate.verdict("wiki_write", "team/x").await, Action::Allow);
        let mcp = PermissionRequest::new("mcp", "github_create_issue");
        assert_eq!(gate.ask(&mcp).await.unwrap(), Decision::Deny);
        assert_eq!(*asked.1.lock(), 1);
        // Unknown tools ask, too: nothing is silently allowed.
        assert_eq!(gate.verdict("something_new", "*").await, Action::Ask);
    }

    #[tokio::test]
    async fn always_is_remembered_for_the_conversation_only() {
        let (gate, asked) = gate(default_rules(), Decision::AllowAlways);
        let request =
            PermissionRequest::new("mcp", "github_create_issue").with_always("github_create_issue");
        assert_eq!(gate.ask(&request).await.unwrap(), Decision::AllowAlways);
        assert_eq!(
            gate.ask(&request).await.unwrap(),
            Decision::Allow,
            "not asked again"
        );
        assert_eq!(*asked.1.lock(), 1);
        // A different MCP tool still asks.
        assert_eq!(gate.verdict("mcp", "github_delete_repo").await, Action::Ask);
        assert_eq!(gate.session.lock().len(), 1);
    }

    #[tokio::test]
    async fn operator_rules_override_defaults_and_deny_never_asks() {
        let rules = merge(&[
            &default_rules(),
            &[
                Rule::new("mcp", Action::Allow, "linear_*"),
                Rule::new("run_shell", Action::Deny, "rm -rf *"),
            ],
        ]);
        let (gate, asked) = gate(rules, Decision::Allow);
        assert_eq!(
            gate.verdict("mcp", "linear_create_issue").await,
            Action::Allow
        );
        assert_eq!(
            gate.verdict("mcp", "github_create_issue").await,
            Action::Ask
        );
        let rm = PermissionRequest::new("run_shell", "rm -rf /work");
        assert_eq!(gate.ask(&rm).await.unwrap(), Decision::Deny);
        assert_eq!(gate.verdict("run_shell", "ls").await, Action::Allow);
        assert_eq!(*asked.1.lock(), 0);
    }

    #[tokio::test]
    async fn the_loop_guard_is_always_refused() {
        let (gate, _) = gate(vec![Rule::for_any("*", Action::Allow)], Decision::Allow);
        let request = PermissionRequest::new(DOOM_LOOP_KEY, "web_fetch");
        assert_eq!(gate.ask(&request).await.unwrap(), Decision::Deny);
    }

    #[test]
    fn rules_files_load_and_reject_typos() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        assert_eq!(
            load_rules(&path).unwrap(),
            default_rules(),
            "missing file: defaults"
        );

        std::fs::write(
            &path,
            r#"{"rules": [{"tool": "mcp", "pattern": "github_*", "action": "allow"}]}"#,
        )
        .unwrap();
        let rules = load_rules(&path).unwrap();
        assert_eq!(
            evaluate(&rules, "mcp", "github_list_issues").action,
            Action::Allow
        );

        std::fs::write(&path, r#"{"rulez": []}"#).unwrap();
        assert!(load_rules(&path).is_err());
        std::fs::write(&path, r#"{"rules": [{"tool": "x", "action": "maybe"}]}"#).unwrap();
        assert!(load_rules(&path).is_err());
    }
}
