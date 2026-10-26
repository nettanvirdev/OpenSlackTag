//! `delegate`: hand independent pieces of a job to helpers that run in
//! parallel, each with its own context and the same tools, and get their
//! results back. Researching five vendors takes the time of one.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use futures::future::join_all;
use openslacktag_agent::{Agent, AgentConfig, AgentEvent, StopReason, Turn};
use openslacktag_core::error::{Error, Result};
use openslacktag_core::message::Entry;
use openslacktag_core::tool::{
    PermissionGate, PermissionRequest, Tool, ToolContext, ToolOutcome, ToolRegistry, ToolSource,
};
use openslacktag_core::{Provider, SessionId};
use serde_json::{Value, json};

use crate::engine::last_answer;

const MAX_TASKS: usize = 8;
/// Helpers get a smaller step budget than the teammate itself.
const HELPER_STEPS: u32 = 40;

const HELPER_NOTE: &str = "\
# You are a helper

Another instance of you split a job into parts and gave you one. Do your part fully with your tools, then reply \
with the result only: findings, numbers, links, file paths. Your reply goes to that instance, not to a person, so \
skip greetings and formatting niceties. If you could not finish, say exactly what is missing.";

pub struct DelegateTool {
    provider: Arc<dyn Provider>,
    registry: Arc<dyn ToolRegistry>,
    gate: Arc<dyn PermissionGate>,
    config: AgentConfig,
    system: String,
}

impl std::fmt::Debug for DelegateTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DelegateTool").finish_non_exhaustive()
    }
}

impl DelegateTool {
    /// `registry` must not contain this tool, so helpers cannot delegate again.
    pub fn new(
        provider: Arc<dyn Provider>,
        registry: Arc<dyn ToolRegistry>,
        gate: Arc<dyn PermissionGate>,
        config: AgentConfig,
        system: &str,
    ) -> Self {
        Self {
            provider,
            registry,
            gate,
            config: AgentConfig {
                max_steps: HELPER_STEPS,
                ..config
            },
            system: format!("{system}\n\n{HELPER_NOTE}"),
        }
    }

    async fn run_helper(&self, task: String, ctx: &ToolContext) -> String {
        let agent = Agent::new(
            self.provider.clone(),
            self.registry.clone(),
            self.gate.clone(),
            self.config.clone(),
        );
        let turn = Turn {
            session: SessionId::new(),
            system: self.system.clone(),
            history: vec![Entry::user(task)],
            root: ctx.root.clone(),
        };
        let mut events = agent.run(turn);
        while let Some(event) = events.next().await {
            if let AgentEvent::Done {
                stopped, history, ..
            } = event
            {
                let answer = last_answer(&history);
                return match stopped {
                    StopReason::Error { message } => format!("(helper failed: {message})"),
                    StopReason::MaxSteps => format!("{answer}\n(helper ran out of steps)"),
                    _ if answer.trim().is_empty() => "(helper returned nothing)".into(),
                    _ => answer,
                };
            }
        }
        "(helper stopped without finishing)".into()
    }
}

#[async_trait]
impl Tool for DelegateTool {
    fn id(&self) -> &str {
        "delegate"
    }

    fn description(&self) -> &str {
        "Run up to 8 independent sub-tasks in parallel, each in a helper with your tools and its own context, and get \
         all their results back. Use it for work that splits cleanly (research several topics, check several sources, \
         process several files). Each task must be self-contained: the helper sees only its task text."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "items": {"type": "string"},
                    "minItems": 1,
                    "maxItems": MAX_TASKS,
                    "description": "Self-contained task descriptions, one per helper"
                }
            },
            "required": ["tasks"],
            "additionalProperties": false
        })
    }

    fn source(&self) -> ToolSource {
        ToolSource::Builtin
    }

    fn permission(&self, _args: &Value) -> PermissionRequest {
        PermissionRequest::new("delegate", "*")
    }

    fn render(&self, args: &Value) -> Option<String> {
        let count = args["tasks"].as_array().map_or(0, Vec::len);
        Some(format!("{count} helpers"))
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutcome> {
        let tasks: Vec<String> = args["tasks"]
            .as_array()
            .ok_or_else(|| Error::InvalidInput("tasks must be a list of strings".into()))?
            .iter()
            .filter_map(|t| t.as_str().map(str::to_string))
            .filter(|t| !t.trim().is_empty())
            .collect();
        if tasks.is_empty() || tasks.len() > MAX_TASKS {
            return Err(Error::InvalidInput(format!(
                "give between 1 and {MAX_TASKS} non-empty tasks"
            )));
        }
        let results = join_all(tasks.iter().map(|t| self.run_helper(t.clone(), ctx))).await;
        let report = tasks
            .iter()
            .zip(results)
            .enumerate()
            .map(|(i, (task, result))| {
                format!(
                    "## Task {}: {}\n\n{result}",
                    i + 1,
                    crate::one_line(task, 100)
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        Ok(ToolOutcome::text(report))
    }
}
