//! Tool result conversion and tool-result message building.
//! Mirrors `io.agentscope.core.tool.{ToolResultConverter, DefaultToolResultConverter, ToolResultMessageBuilder}`.
use crate::message::{ContentBlock, Msg, MsgBuilder, MsgRole, ToolResultBlock, ToolUseBlock};

/// Converts a raw tool return value into a [`ToolResultBlock`].
///
/// Custom implementations can override how results are serialized, filtered or
/// compressed before being presented to the model.
pub trait ToolResultConverter: Send + Sync {
    /// Convert a tool call result (may be `None`) into a [`ToolResultBlock`].
    fn convert(&self, result: Option<&serde_json::Value>) -> ToolResultBlock;
}

/// Default converter: `null` -> `"null"`, strings pass through, everything else
/// is compact-JSON serialized.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultToolResultConverter;

impl ToolResultConverter for DefaultToolResultConverter {
    fn convert(&self, result: Option<&serde_json::Value>) -> ToolResultBlock {
        match result {
            None | Some(serde_json::Value::Null) => ToolResultBlock::text("null"),
            Some(serde_json::Value::String(s)) => ToolResultBlock::text(s.clone()),
            Some(v) => {
                let text = serde_json::to_string(v).unwrap_or_else(|_| v.to_string());
                ToolResultBlock::text(text)
            }
        }
    }
}

/// Builds a `tool`-role [`Msg`] from a [`ToolResultBlock`] and its original call.
///
/// Mirrors `ToolResultMessageBuilder.buildToolResultMsg`: the result is keyed by the
/// original call's id/name and the message is named after the producing agent.
pub struct ToolResultMessageBuilder;

impl ToolResultMessageBuilder {
    /// Build a tool result message from a result and the original tool call.
    pub fn build_tool_result_msg(
        result: &ToolResultBlock,
        original_call: &ToolUseBlock,
        agent_name: &str,
    ) -> Msg {
        let result_with_id = result.with_id_and_name(&original_call.id, &original_call.name);
        MsgBuilder::new()
            .name(agent_name)
            .role(MsgRole::Tool)
            .content(vec![ContentBlock::ToolResult(result_with_id)])
            .build()
    }
}
