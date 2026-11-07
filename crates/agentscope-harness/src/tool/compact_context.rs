//! `compact_context` — the model's way to ask for a conversation compaction.
//!
//! The tool does **not** rewrite the history itself. A tool runs in the middle of the
//! acting phase, where the runner still owes a TOOL result message for the pending
//! `tool_use`, and it has no access to the session recorder. Setting a flag instead lets
//! [`ConversationCompactionMiddleware`] do the rewrite on the next reasoning step, where the
//! long-term facts (`compaction/start` / `compaction/end`) are journalled and the tool
//! result is already part of the history.
//!
//! [`ConversationCompactionMiddleware`]: crate::agent::middleware::ConversationCompactionMiddleware

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};
use async_trait::async_trait;

/// Name of the compaction tool.
pub const COMPACT_CONTEXT_TOOL_NAME: &str = "compact_context";

/// Records a compaction request for the middleware to pick up.
pub struct CompactContextTool {
    requested: Arc<AtomicBool>,
}

impl CompactContextTool {
    pub fn new(requested: Arc<AtomicBool>) -> Self {
        Self { requested }
    }
}

#[async_trait]
impl Tool for CompactContextTool {
    fn name(&self) -> &str {
        COMPACT_CONTEXT_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Compress the conversation so far into a summary to free up context window. Call this \
         when the user asks to compact, summarize or clear the context, or when you notice you \
         keep re-reading the same files across many turns. The most recent messages are kept \
         verbatim; the compaction runs before your next request."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn call(&self, _args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        self.requested.store(true, Ordering::SeqCst);
        Ok(ToolResultBlock::text(
            "Compaction scheduled; the conversation will be summarized before your next request.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::ContentBlock;

    #[tokio::test]
    async fn calling_the_tool_records_the_request() {
        let requested = Arc::new(AtomicBool::new(false));
        let tool = CompactContextTool::new(Arc::clone(&requested));
        let result = tool
            .call(JsonMap::new(), ToolExecutionContext::default())
            .await
            .unwrap();
        assert!(requested.load(Ordering::SeqCst));
        let text = result
            .output
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(text.contains("Compaction scheduled"), "{text}");
    }
}
