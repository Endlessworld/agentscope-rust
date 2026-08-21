//! `AgentGenerateTool` — creates a subagent declaration from a natural-language
//! description, mirroring `io.agentscope.harness.agent.tool.AgentGenerateTool`.
//!
//! Writes `subagents/<name>.md` with YAML front matter (`description`, optional
//! `tools`) and the generated system prompt as the body, so the standard
//! `subagents/` loader picks it up on the next session.
use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};

use crate::filesystem::Filesystem;
use crate::tool::str_arg;

/// Directory that holds subagent declarations.
pub use crate::agent::subagent::SUBAGENTS_DIR;

/// Generates a subagent spec file from a natural-language description.
#[derive(Clone)]
pub struct AgentGenerateTool {
    fs: Arc<dyn Filesystem>,
    subagents_dir: String,
}

impl AgentGenerateTool {
    pub fn new(fs: Arc<dyn Filesystem>) -> Self {
        Self { fs, subagents_dir: SUBAGENTS_DIR.into() }
    }

    /// Overrides the directory declarations are written to.
    pub fn with_subagents_dir(mut self, dir: impl Into<String>) -> Self {
        self.subagents_dir = dir.into();
        self
    }

    fn sanitize_name(name: &str) -> Option<String> {
        let cleaned: String = name
            .trim()
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .collect();
        let cleaned = cleaned.trim_matches('-').to_string();
        if cleaned.is_empty() {
            None
        } else {
            Some(cleaned)
        }
    }

    fn render_spec(name: &str, description: &str, tools: Option<&str>) -> String {
        let mut fm = format!("---\nname: {name}\ndescription: {description}\n");
        if let Some(t) = tools {
            let t = t.trim();
            if !t.is_empty() {
                fm.push_str(&format!("tools: {t}\n"));
            }
        }
        fm.push_str("---\n\n");
        fm.push_str(&format!(
            "You are `{name}`, a focused subagent.\n\nMission:\n{description}\n\n\
             Work autonomously toward the mission, use only the tools you need, \
             and return a concise final answer.\n"
        ));
        fm
    }

    /// Registers the tool into a toolkit.
    pub fn register(toolkit: &agentscope_core::tool::Toolkit, fs: Arc<dyn Filesystem>) {
        toolkit.register_tool(Arc::new(Self::new(fs)));
    }
}

#[async_trait]
impl Tool for AgentGenerateTool {
    fn name(&self) -> &str {
        "agent_generate"
    }

    fn description(&self) -> &str {
        "Create a new subagent from a natural-language description. \
         Writes subagents/<name>.md which is loaded as a subagent declaration."
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Subagent name (letters, digits, '-', '_')"},
                "description": {"type": "string", "description": "What this subagent does, in one or two sentences"},
                "tools": {"type": "string", "description": "Optional comma-separated tool allow-list"}
            },
            "required": ["name", "description"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let raw_name = str_arg(&args, "name")?;
        let description = str_arg(&args, "description")?;
        let tools = args.get("tools").and_then(|v| v.as_str()).map(String::from);

        let Some(name) = Self::sanitize_name(&raw_name) else {
            return Ok(ToolResultBlock::error(format!(
                "Invalid subagent name: '{raw_name}'"
            )));
        };
        if description.trim().is_empty() {
            return Ok(ToolResultBlock::error("description must not be empty"));
        }

        let path = format!("{}/{name}.md", self.subagents_dir);
        let rt = RuntimeContext::default();
        if self.fs.exists(&rt, &path).await.unwrap_or(false) {
            return Ok(ToolResultBlock::error(format!(
                "Subagent already exists: {path}. Choose another name."
            )));
        }
        let content = Self::render_spec(&name, description.trim(), tools.as_deref());
        match self.fs.write(&rt, &path, &content).await {
            Ok(()) => Ok(ToolResultBlock::text(format!(
                "Created subagent '{name}' at {path}"
            ))),
            Err(e) => Ok(ToolResultBlock::error(e.to_string())),
        }
    }
}
