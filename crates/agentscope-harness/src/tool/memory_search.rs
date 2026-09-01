//! `MemorySearchTool` — searches long-term memory files, mirroring
//! `io.agentscope.harness.agent.tool.MemorySearchTool`.
use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};

use crate::filesystem::Filesystem;
use crate::tool::{int_arg, opt_str_arg};

/// Searches the agent's long-term memory files (`MEMORY.md` + `memory/*.md`).
#[derive(Clone)]
pub struct MemorySearchTool {
    fs: Arc<dyn Filesystem>,
    memory_root: String,
}

impl MemorySearchTool {
    pub fn new(fs: Arc<dyn Filesystem>) -> Self {
        Self { fs, memory_root: "memory".into() }
    }

    /// Overrides the memory root directory (relative to the filesystem root).
    pub fn with_memory_root(mut self, root: impl Into<String>) -> Self {
        self.memory_root = root.into();
        self
    }

    /// Registers the tool into a toolkit.
    pub fn register(toolkit: &agentscope_core::tool::Toolkit, fs: Arc<dyn Filesystem>) {
        toolkit.register_tool(Arc::new(Self::new(fs)));
    }
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }

    fn description(&self) -> &str {
        "Search long-term memory files (MEMORY.md and memory/*.md) for a literal text pattern."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Literal text pattern to search for"},
                "limit": {"type": "integer", "description": "Max matches to return (default 20)", "default": 20}
            },
            "required": ["query"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let query = opt_str_arg(&args, "query").ok_or_else(|| {
            agentscope_core::Error::Tool("Missing string argument: query".into())
        })?;
        let limit = int_arg(&args, "limit").unwrap_or(20);
        let rt = RuntimeContext::default();
        let glob = "*.md";
        match self.fs.grep(&rt, &query, Some(&self.memory_root), Some(glob)).await {
            Ok(matches) => {
                if matches.is_empty() {
                    return Ok(ToolResultBlock::text("No matching memories found"));
                }
                let shown: Vec<String> = matches
                    .iter()
                    .take(limit)
                    .map(|m| format!("{}:{}:{}", m.path, m.line, m.text))
                    .collect();
                Ok(ToolResultBlock::text(shown.join("\n")))
            }
            Err(e) => Ok(ToolResultBlock::error(e.to_string())),
        }
    }
}
