//! `MemorySaveTool` — appends facts to `MEMORY.md` and today's daily ledger, mirroring
//! `io.agentscope.harness.agent.tool.MemorySaveTool`.
//!
//! This is the only sanctioned entry point for the agent to write long-term memories.
//! Writes are append-only; deduplication and size-trimming are handled by the periodic
//! `MemoryConsolidator`.
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};
use async_trait::async_trait;

use crate::agent::workspace::constants::{MEMORY_DIR, MEMORY_MD};
use crate::agent::workspace::WorkspaceManager;

/// Persists memories to both `MEMORY.md` and `memory/YYYY-MM-DD.md`.
pub struct MemorySaveTool {
    workspace: Arc<WorkspaceManager>,
}

impl MemorySaveTool {
    pub fn new(workspace: Arc<WorkspaceManager>) -> Self {
        Self { workspace }
    }

    /// Registers the tool into a toolkit.
    pub fn register(toolkit: &agentscope_core::tool::Toolkit, workspace: Arc<WorkspaceManager>) {
        toolkit.register_tool(Arc::new(Self::new(workspace)));
    }
}

#[async_trait]
impl Tool for MemorySaveTool {
    fn name(&self) -> &str {
        "memory_save"
    }

    fn description(&self) -> &str {
        "Persist one or more facts to long-term memory. Use whenever the user asks you to \
         remember something, or when you observe important preferences, decisions, or context \
         worth keeping across conversations. Do NOT use write_file or edit_file on MEMORY.md — \
         always use this tool instead."
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {"type": "string", "description": "Markdown bullet list of facts to remember. Each bullet should be a concise, self-contained fact. Example:\n- User prefers dark mode\n- Project deadline is 2026-07-01"}
            },
            "required": ["content"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let content = match args.get("content").and_then(|v| v.as_str()) {
            Some(c) if !c.trim().is_empty() => c.trim().to_string(),
            _ => return Ok(ToolResultBlock::text("Error: content is required")),
        };
        let rc = RuntimeContext::default();

        let section = format!("\n{content}\n");
        self.workspace
            .append_utf8_workspace_relative(&rc, MEMORY_MD, &section)
            .await;

        let today = chrono::Local::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        let daily_path = format!("{MEMORY_DIR}/{today}.md");
        let daily_entry = format!(
            "\n## Memory Save — {}\n{content}\n",
            chrono::Utc::now().to_rfc3339()
        );
        self.workspace
            .append_utf8_workspace_relative(&rc, &daily_path, &daily_entry)
            .await;

        let mut count = content
            .lines()
            .filter(|l| l.trim_start().starts_with('-'))
            .count();
        if count == 0 {
            count = 1;
        }
        let noun = if count == 1 { "memory" } else { "memories" };
        Ok(ToolResultBlock::text(format!(
            "Saved {count} {noun} to MEMORY.md"
        )))
    }
}
