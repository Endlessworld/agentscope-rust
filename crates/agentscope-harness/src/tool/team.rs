//! `TeamTool` — unified team coordination tool over a [`TeamClient`], mirroring
//! `io.agentscope.harness.agent.tool.TeamTool`.
//!
//! One tool, many operations (`operation` discriminator): list_tasks,
//! create_task, assign_task, claim_task, complete_task, fail_task,
//! send_message, broadcast_message, spawn_member, shutdown_member.
//!
//! Role clipping follows [`TeamContext`]: member-only callers cannot invoke
//! spawn_member/shutdown_member. Plan submission/approval operations are
//! pending until the plan-mode bridge lands.
use std::sync::Arc;

use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};
use async_trait::async_trait;

use crate::agent::team::{
    TeamClient, TeamContext, TeamError, TeamMemberSpec, TeamResult, TeamRole,
};

/// Unified team coordination tool.
pub struct TeamTool {
    client: Arc<dyn TeamClient>,
}

impl TeamTool {
    pub fn new(client: Arc<dyn TeamClient>) -> Self {
        Self { client }
    }

    /// Registers the tool into a toolkit.
    pub fn register(toolkit: &agentscope_core::tool::Toolkit, client: Arc<dyn TeamClient>) {
        toolkit.register_tool(Arc::new(Self::new(client)));
    }

    fn require<'a>(args: &'a JsonMap, key: &str) -> TeamResult<&'a str> {
        args.get(key)
            .and_then(|v| v.as_str())
            .ok_or_else(|| TeamError::Other(format!("missing argument `{key}`")))
    }

    fn status_list(tasks: &[crate::agent::team::TeamTask]) -> String {
        if tasks.is_empty() {
            return "No tasks on the board.".to_string();
        }
        let mut lines = vec!["Team task board:".to_string()];
        for t in tasks {
            lines.push(format!(
                "- {} [{}] {} (assignee: {}, v{}){}",
                t.id,
                t.status.as_str(),
                t.subject,
                t.assignee.as_deref().unwrap_or("-"),
                t.version,
                if t.blocked_by.is_empty() {
                    String::new()
                } else {
                    format!(" blocked_by={:?}", t.blocked_by)
                }
            ));
        }
        lines.join("\n")
    }

    fn call_inner(&self, args: &JsonMap) -> TeamResult<String> {
        let op = Self::require(args, "operation")?;
        let team_id = Self::require(args, "team_id")?;
        let agent = args
            .get("agent")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        // Role context for clipping; role is supplied by the caller middleware.
        let role = match args.get("role").and_then(|v| v.as_str()) {
            Some("LEAD") => TeamRole::Lead,
            _ => TeamRole::Member,
        };
        let ctx = TeamContext::for_role(team_id, role);

        match op {
            "list_tasks" => {
                let tasks = self.client.list_tasks(team_id)?;
                Ok(Self::status_list(&tasks))
            }
            "create_task" => {
                let subject = Self::require(args, "subject")?;
                let description = args
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let blocked_by = args
                    .get("blocked_by")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let t = self
                    .client
                    .create_task(team_id, subject, description, blocked_by)?;
                Ok(format!(
                    "Created task `{}` [{}] {}",
                    t.id,
                    t.status.as_str(),
                    t.subject
                ))
            }
            "assign_task" => {
                ctx.allows_or_err(op)?;
                let task_id = Self::require(args, "task_id")?;
                let assignee = Self::require(args, "assignee")?;
                let version = args.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
                let t = self
                    .client
                    .assign_task(team_id, task_id, assignee, version)?;
                Ok(format!(
                    "Assigned `{}` to {} (v{})",
                    t.id, assignee, t.version
                ))
            }
            "claim_task" => {
                let task_id = Self::require(args, "task_id")?;
                let t = self.client.claim_task(team_id, task_id, agent)?;
                Ok(format!(
                    "{} claimed `{}` → [{}] (v{})",
                    agent,
                    t.id,
                    t.status.as_str(),
                    t.version
                ))
            }
            "complete_task" | "fail_task" => {
                let task_id = Self::require(args, "task_id")?;
                let t = if op == "complete_task" {
                    self.client.complete_task(team_id, task_id, agent)?
                } else {
                    self.client.fail_task(team_id, task_id, agent)?
                };
                Ok(format!("Task `{}` → [{}]", t.id, t.status.as_str()))
            }
            "send_message" => {
                let to = Self::require(args, "to")?;
                let content = Self::require(args, "content")?;
                let m = self.client.send_message(team_id, agent, to, content)?;
                Ok(format!("Message `{}` delivered to {}", m.id, to))
            }
            "broadcast_message" => {
                let content = Self::require(args, "content")?;
                let sent = self.client.broadcast_message(team_id, agent, content)?;
                Ok(format!("Broadcast delivered to {} members", sent.len()))
            }
            "spawn_member" => {
                ctx.allows_or_err(op)?;
                let name = Self::require(args, "name")?;
                let m = self
                    .client
                    .spawn_member(team_id, TeamMemberSpec::new(name, TeamRole::Member))?;
                Ok(format!("Member `{}` spawned", m.name))
            }
            "shutdown_member" => {
                ctx.allows_op(op);
                let name = Self::require(args, "name")?;
                self.client.shutdown_member(team_id, name)?;
                Ok(format!("Member `{}` shut down", name))
            }
            other => Err(TeamError::Other(format!(
                "`{other}` is not a supported team operation"
            ))),
        }
    }
}

impl TeamContext {
    fn allows_op(&self, op: &str) -> bool {
        self.available_operations.iter().any(|o| o == op)
    }

    fn allows_or_err(&self, op: &str) -> TeamResult<()> {
        if self.allows_op(op) {
            Ok(())
        } else {
            Err(TeamError::Other(format!(
                "role {:?} may not perform `{op}`",
                self.role.map(|r| r.as_str()).unwrap_or("?")
            )))
        }
    }
}

#[async_trait]
impl Tool for TeamTool {
    fn name(&self) -> &str {
        "team"
    }

    fn description(&self) -> &str {
        "Coordinate with your agent team: manage the shared task board and exchange \
         messages. Operations: list_tasks, create_task, assign_task, claim_task, \
         complete_task, fail_task, send_message, broadcast_message, spawn_member \
         (LEAD only), shutdown_member (LEAD only)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "operation": {"type": "string", "description": "One of the supported team operations"},
                "team_id": {"type": "string", "description": "Team identifier"},
                "agent": {"type": "string", "description": "Your member name"},
                "role": {"type": "string", "description": "Your role: LEAD or MEMBER"},
                "task_id": {"type": "string"},
                "subject": {"type": "string"},
                "description": {"type": "string"},
                "blocked_by": {"type": "array", "items": {"type": "string"}},
                "assignee": {"type": "string"},
                "version": {"type": "integer", "description": "Expected task version for assign_task (CAS)"},
                "to": {"type": "string"},
                "content": {"type": "string"},
                "name": {"type": "string"}
            },
            "required": ["operation", "team_id"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        match self.call_inner(&args) {
            Ok(text) => Ok(ToolResultBlock::text(text)),
            Err(e) => Ok(ToolResultBlock::error(e.to_string())),
        }
    }
}
