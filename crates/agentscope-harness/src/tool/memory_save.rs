//! `MemorySaveTool` — saves/updates a long-term memory file, mirroring
//! `io.agentscope.harness.agent.tool.MemorySaveTool`.
use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};

use crate::filesystem::Filesystem;
use crate::tool::str_arg;

/// Saves a memory file under the agent's memory root (default `memory/`).
#[derive(Clone)]
pub struct MemorySaveTool {
    fs: Arc<dyn Filesystem>,
    memory_root: String,
}

impl MemorySaveTool {
    pub fn new(fs: Arc<dyn Filesystem>) -> Self {
        Self { fs, memory_root: "memory".into() }
    }

    /// Overrides the memory root directory (relative to the filesystem root).
    pub fn with_memory_root(mut self, root: impl Into<String>) -> Self {
        self.memory_root = root.into();
        self
    }

    fn resolve(&self, path: &str) -> String {
        let p = path.trim_start_matches('/');
        if p.is_empty() {
            self.memory_root.clone()
        } else {
            format!("{}/{}", self.memory_root, p)
        }
    }

    /// Registers the tool into a toolkit.
    pub fn register(toolkit: &agentscope_core::tool::Toolkit, fs: Arc<dyn Filesystem>) {
        toolkit.register_tool(Arc::new(Self::new(fs)));
    }
}

#[async_trait]
impl Tool for MemorySaveTool {
    fn name(&self) -> &str {
        "memory_save"
    }

    fn description(&self) -> &str {
        "Save or update a long-term memory file under the agent memory directory (memory/)."
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Memory file path relative to memory root (e.g. facts.md)"},
                "content": {"type": "string", "description": "Full content to write"}
            },
            "required": ["path", "content"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let path = str_arg(&args, "path")?;
        let content = str_arg(&args, "content")?;
        let full = self.resolve(&path);
        let rt = RuntimeContext::default();
        match self.fs.write(&rt, &full, &content).await {
            Ok(()) => Ok(ToolResultBlock::text(format!("Saved memory to {full}"))),
            Err(e) => Ok(ToolResultBlock::error(e.to_string())),
        }
    }
}
