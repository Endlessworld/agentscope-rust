//! `MemorySearchTool` — searches long-term memory files, mirroring
//! `io.agentscope.harness.agent.tool.MemorySearchTool`.
//!
//! Uses case-insensitive literal substring search across every memory file visible
//! through the configured filesystem (`MEMORY.md` and `memory/*.md`).
use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};

use crate::agent::workspace::WorkspaceManager;

/// Searches the agent's long-term memory files (`MEMORY.md` + `memory/*.md`).
pub struct MemorySearchTool {
    workspace: Arc<WorkspaceManager>,
}

impl MemorySearchTool {
    pub fn new(workspace: Arc<WorkspaceManager>) -> Self {
        Self { workspace }
    }

    /// Registers the tool into a toolkit.
    pub fn register(toolkit: &agentscope_core::tool::Toolkit, workspace: Arc<WorkspaceManager>) {
        toolkit.register_tool(Arc::new(Self::new(workspace)));
    }
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }

    fn description(&self) -> &str {
        "Search through long-term memory files (MEMORY.md and memory/*.md) for relevant \
         information. Use before answering questions about prior work, decisions, dates, \
         people, preferences, or todos."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Keywords to search for in memory files"}
            },
            "required": ["query"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let query = match args.get("query").and_then(|v| v.as_str()) {
            Some(q) if !q.trim().is_empty() => q.to_string(),
            _ => return Ok(ToolResultBlock::text("No query provided")),
        };
        let rc = RuntimeContext::default();

        // Case-insensitive literal substring match (Java `Pattern.quote` + CASE_INSENSITIVE).
        let needle = query.to_lowercase();
        let mut results: Vec<String> = Vec::new();
        let mut match_count = 0usize;

        for rel in self.workspace.list_memory_file_paths(&rc).await {
            let content = self.workspace.read_managed_workspace_file_utf8(&rc, &rel).await;
            if content.is_empty() {
                continue;
            }
            for (i, line) in content.split('\n').enumerate() {
                if line.to_lowercase().contains(&needle) {
                    results.push(format!("Source: {rel}#{}: {line}", i + 1));
                    match_count += 1;
                }
            }
        }

        if match_count == 0 {
            return Ok(ToolResultBlock::text(format!(
                "No matching memories found for: {query}"
            )));
        }
        Ok(ToolResultBlock::text(format!(
            "Found {match_count} matches:\n\n{}",
            results.join("\n")
        )))
    }
}
