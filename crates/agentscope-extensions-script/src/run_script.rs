use std::sync::Arc;

use agentscope_core::error::{Error, Result};
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};
use async_trait::async_trait;
use serde_json::json;

use crate::engine::ScriptEngine;

/// Exposes the script engine itself as a `run_script` tool so a model can trigger orchestration.
pub struct RunScriptTool {
    engine: Arc<ScriptEngine>,
}

impl RunScriptTool {
    pub fn new(engine: Arc<ScriptEngine>) -> Self {
        Self { engine }
    }
}

#[async_trait]
impl Tool for RunScriptTool {
    fn name(&self) -> &str {
        "run_script"
    }

    fn description(&self) -> &str {
        "Run a JavaScript snippet. It can call any registered tool via tools.<name>(args) and must \
         call result(v) to return."
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": { "script": { "type": "string" } },
            "required": ["script"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let script = args
            .get("script")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Tool("missing string argument: script".into()))?;
        let out = self.engine.run(script.to_string()).await?;
        Ok(ToolResultBlock::text(out))
    }
}
