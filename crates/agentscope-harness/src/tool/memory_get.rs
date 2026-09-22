//! `MemoryGetTool` — reads specific lines from a long-term memory file, mirroring
//! `io.agentscope.harness.agent.tool.MemoryGetTool`.
use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};

use crate::agent::workspace::WorkspaceManager;
use crate::filesystem::validate_path;

/// Reads specific lines from a memory file (`MEMORY.md` or `memory/*.md`).
///
/// Paths are workspace-relative and read through the configured filesystem, so the
/// tool works across Local, Sandbox and Store backends.
pub struct MemoryGetTool {
    workspace: Arc<WorkspaceManager>,
}

impl MemoryGetTool {
    pub fn new(workspace: Arc<WorkspaceManager>) -> Self {
        Self { workspace }
    }

    /// Registers the tool into a toolkit.
    pub fn register(toolkit: &agentscope_core::tool::Toolkit, workspace: Arc<WorkspaceManager>) {
        toolkit.register_tool(Arc::new(Self::new(workspace)));
    }
}

fn int_arg(args: &JsonMap, key: &str) -> Option<i64> {
    args.get(key).and_then(|v| v.as_i64())
}

#[async_trait]
impl Tool for MemoryGetTool {
    fn name(&self) -> &str {
        "memory_get"
    }

    fn description(&self) -> &str {
        "Read specific lines from a memory file. Use after memory_search to pull full \
         context around matched lines. Path is relative to workspace."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Relative path to the memory file (e.g., MEMORY.md or memory/2026-04-01.md)"},
                "startLine": {"type": "integer", "description": "Start line number (1-based, inclusive)"},
                "endLine": {"type": "integer", "description": "End line number (1-based, inclusive)"}
            },
            "required": ["path", "startLine", "endLine"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) if !p.trim().is_empty() => p.to_string(),
            _ => return Ok(ToolResultBlock::text("Error: path is required")),
        };
        let start_line = int_arg(&args, "startLine").unwrap_or(0);
        let end_line = int_arg(&args, "endLine").unwrap_or(0);

        // Reject `..` traversal, mirroring Java's `resolved.startsWith(workspace)` guard.
        if validate_path(&path).is_err() {
            return Ok(ToolResultBlock::text("Error: path traversal not allowed"));
        }

        let rc = RuntimeContext::default();
        let text = self.workspace.read_managed_workspace_file_utf8(&rc, &path).await;
        if text.trim().is_empty() {
            return Ok(ToolResultBlock::text(format!("Error: file not found: {path}")));
        }

        let lines: Vec<&str> = text.split('\n').collect();
        // Java: start = Math.max(0, startLine - 1); end = Math.min(lines.size(), endLine)
        let start = if start_line > 1 { (start_line - 1) as usize } else { 0 };
        let end = if end_line < 0 { 0 } else { (end_line as usize).min(lines.len()) };

        if start >= lines.len() {
            return Ok(ToolResultBlock::text(format!(
                "Error: startLine {start_line} exceeds file length {}",
                lines.len()
            )));
        }

        let mut sb = String::new();
        for i in start..end {
            sb.push_str(&format!("{}|{}\n", i + 1, lines[i]));
        }
        Ok(ToolResultBlock::text(sb))
    }
}
