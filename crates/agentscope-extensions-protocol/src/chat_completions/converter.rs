//! Converters between OpenAI-compatible chat-completion DTOs and AgentScope core
//! types, mirroring `io.agentscope.core.chat.completions.converter`.

use agentscope_core::message::{
    ContentBlock, Msg, MsgRole, TextBlock, ToolResultBlock, ToolUseBlock,
};
use agentscope_core::model::ToolSchema;
use serde_json::Map;

use super::model::{ChatMessage, OpenAITool};

/// Converts [`ChatMessage`] DTOs to core [`Msg`] objects.
///
/// Mirrors `io.agentscope.core.chat.completions.converter.ChatMessageConverter`.
#[derive(Debug, Default, Clone, Copy)]
pub struct ChatMessageConverter;

impl ChatMessageConverter {
    /// Creates a new converter.
    pub fn new() -> Self {
        Self
    }

    /// Converts a list of chat messages, skipping `None` entries.
    pub fn convert_messages(&self, chat_messages: &[ChatMessage]) -> Result<Vec<Msg>, String> {
        if chat_messages.is_empty() {
            return Ok(Vec::new());
        }
        chat_messages
            .iter()
            .map(|m| self.convert_message(m))
            .collect()
    }

    /// Converts a single chat message.
    ///
    /// A blank/missing role defaults to `user` (the Java SDK logs a warning and
    /// continues). Unknown non-blank roles are rejected, matching the Java
    /// `IllegalArgumentException` contract.
    pub fn convert_message(&self, chat_msg: &ChatMessage) -> Result<Msg, String> {
        let role_str = match chat_msg.role.as_deref() {
            Some(r) if !r.trim().is_empty() => r.to_string(),
            _ => "user".to_string(),
        };
        let role = convert_role(&role_str)?;

        if role == MsgRole::Tool {
            return Ok(self.convert_tool_result_message(chat_msg));
        }

        if role == MsgRole::Assistant
            && chat_msg
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty())
        {
            return Ok(self.convert_assistant_with_tool_calls(chat_msg));
        }

        let content = chat_msg.content.clone().unwrap_or_default();
        Ok(Msg::builder()
            .role(role)
            .text_content(content)
            .build())
    }

    fn convert_assistant_with_tool_calls(&self, chat_msg: &ChatMessage) -> Msg {
        let mut blocks: Vec<ContentBlock> = Vec::new();
        if let Some(content) = chat_msg.content.as_deref() {
            if !content.is_empty() {
                blocks.push(ContentBlock::Text(TextBlock::new(content)));
            }
        }
        if let Some(tool_calls) = chat_msg.tool_calls.as_ref() {
            for tool_call in tool_calls {
                let function = match tool_call.function.as_ref() {
                    Some(f) => f,
                    None => continue,
                };
                let input = parse_arguments(&function.arguments);
                let mut builder = ToolUseBlock::builder()
                    .name(function.name.clone())
                    .input(input);
                if let Some(id) = tool_call.id.as_ref() {
                    builder = builder.id(id.clone());
                }
                blocks.push(ContentBlock::ToolUse(builder.build()));
            }
        }
        Msg::builder()
            .role(MsgRole::Assistant)
            .content(blocks)
            .build()
    }

    fn convert_tool_result_message(&self, chat_msg: &ChatMessage) -> Msg {
        let tool_call_id = chat_msg.tool_call_id.clone().unwrap_or_default();
        let name = chat_msg.name.clone().unwrap_or_default();
        let content = chat_msg.content.clone().unwrap_or_default();
        let result = ToolResultBlock::new(
            &tool_call_id,
            &name,
            vec![ContentBlock::Text(TextBlock::new(content))],
        );
        Msg::builder()
            .role(MsgRole::Tool)
            .content(vec![ContentBlock::ToolResult(result)])
            .build()
    }
}

/// Converts a role string (case-insensitive) into a [`MsgRole`].
///
/// Returns an error for unsupported roles, mirroring the Java
/// `IllegalArgumentException` message.
fn convert_role(role_str: &str) -> Result<MsgRole, String> {
    match role_str.to_lowercase().as_str() {
        "user" => Ok(MsgRole::User),
        "assistant" => Ok(MsgRole::Assistant),
        "system" => Ok(MsgRole::System),
        "tool" => Ok(MsgRole::Tool),
        other => Err(format!(
            "Unknown message role: '{}'. Supported roles: user, assistant, system, tool",
            other
        )),
    }
}

/// Parses a JSON arguments string into a JSON object map.
///
/// Returns an empty map when the string is empty or not a JSON object.
pub fn parse_arguments(arguments: &str) -> Map<String, serde_json::Value> {
    if arguments.is_empty() {
        return Map::new();
    }
    match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(serde_json::Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// Converts OpenAI tool definitions to AgentScope [`ToolSchema`] values.
///
/// Mirrors `io.agentscope.core.chat.completions.converter.OpenAIToolConverter`:
/// only `type == "function"` tools with a non-blank name are converted, blank
/// descriptions fall back to an empty string.
#[derive(Debug, Default, Clone, Copy)]
pub struct OpenAIToolConverter;

impl OpenAIToolConverter {
    /// Creates a new converter.
    pub fn new() -> Self {
        Self
    }

    /// Converts a list of OpenAI tools, silently skipping invalid entries.
    pub fn convert_to_tool_schemas(&self, tools: &[OpenAITool]) -> Vec<ToolSchema> {
        let mut schemas = Vec::new();
        for tool in tools {
            if tool.r#type != "function" {
                continue;
            }
            let function = match tool.function.as_ref() {
                Some(f) => f,
                None => continue,
            };
            let name = match function.name.as_deref() {
                Some(n) if !n.trim().is_empty() => n.to_string(),
                _ => continue,
            };
            let description = function.description.clone().unwrap_or_default();
            let mut schema = ToolSchema::new(name, description);
            if let Some(parameters) = function.parameters.clone() {
                schema = schema.parameters(parameters);
            }
            if let Some(strict) = function.strict {
                schema = schema.strict(strict);
            }
            schemas.push(schema);
        }
        schemas
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_completions::model::{FunctionCall, OpenAIToolFunction, ToolCall};
    use serde_json::json;

    fn text_block_text(msg: &Msg) -> String {
        msg.text_content()
    }

    #[test]
    fn user_message_maps_to_user_role_with_text() {
        let converter = ChatMessageConverter::new();
        let msg = converter
            .convert_message(&ChatMessage::new("user", "Hello"))
            .unwrap();
        assert_eq!(msg.role(), MsgRole::User);
        assert_eq!(text_block_text(&msg), "Hello");
    }

    #[test]
    fn blank_role_defaults_to_user() {
        let converter = ChatMessageConverter::new();
        let mut chat = ChatMessage::new("system", "x");
        chat.role = Some("   ".to_string());
        let msg = converter.convert_message(&chat).unwrap();
        assert_eq!(msg.role(), MsgRole::User);
    }

    #[test]
    fn unknown_role_is_rejected_like_java() {
        let converter = ChatMessageConverter::new();
        let err = converter
            .convert_message(&ChatMessage::new("developer", "x"))
            .unwrap_err();
        assert!(err.contains("Unknown message role: 'developer'"));
    }

    #[test]
    fn assistant_tool_calls_become_tool_use_blocks() {
        let converter = ChatMessageConverter::new();
        let chat = ChatMessage::assistant_with_tool_calls(
            Some("thinking".to_string()),
            vec![ToolCall {
                index: None,
                id: Some("call_1".to_string()),
                r#type: "function".to_string(),
                function: Some(FunctionCall::new("get_weather", "{\"city\":\"Hangzhou\"}")),
            }],
        );
        let msg = converter.convert_message(&chat).unwrap();
        assert_eq!(msg.role(), MsgRole::Assistant);
        assert_eq!(msg.content().len(), 2);
        let tool_use = msg.tool_use_blocks().next().unwrap();
        assert_eq!(tool_use.id, "call_1");
        assert_eq!(tool_use.name, "get_weather");
        assert_eq!(tool_use.input["city"], json!("Hangzhou"));
    }

    #[test]
    fn invalid_tool_arguments_fall_back_to_empty_map() {
        let converter = ChatMessageConverter::new();
        let chat = ChatMessage::assistant_with_tool_calls(
            None,
            vec![ToolCall {
                index: None,
                id: Some("c".to_string()),
                r#type: "function".to_string(),
                function: Some(FunctionCall::new("f", "not-json")),
            }],
        );
        let msg = converter.convert_message(&chat).unwrap();
        let tool_use = msg.tool_use_blocks().next().unwrap();
        assert!(tool_use.input.is_empty());
    }

    #[test]
    fn tool_result_message_becomes_tool_result_block() {
        let converter = ChatMessageConverter::new();
        let msg = converter
            .convert_message(&ChatMessage::tool_result("call_1", "get_weather", "sunny"))
            .unwrap();
        assert_eq!(msg.role(), MsgRole::Tool);
        let result = msg.tool_result_blocks().next().unwrap();
        assert_eq!(result.id.as_deref(), Some("call_1"));
        assert_eq!(result.name.as_deref(), Some("get_weather"));
        assert_eq!(result.text_content(), "sunny");
    }

    #[test]
    fn convert_messages_preserves_order() {
        let converter = ChatMessageConverter::new();
        let msgs = converter
            .convert_messages(&[
                ChatMessage::new("system", "sys"),
                ChatMessage::new("user", "hi"),
            ])
            .unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role(), MsgRole::System);
        assert_eq!(msgs[1].role(), MsgRole::User);
    }

    #[test]
    fn openai_tools_convert_to_schemas() {
        let converter = OpenAIToolConverter::new();
        let mut function = OpenAIToolFunction::new("get_weather", "Get the current weather");
        function.parameters = Some(
            serde_json::from_value(json!({"type": "object", "properties": {}})).unwrap(),
        );
        function.strict = Some(true);
        let schemas = converter.convert_to_tool_schemas(&[OpenAITool::new(function)]);
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "get_weather");
        assert_eq!(schemas[0].description, "Get the current weather");
        assert_eq!(schemas[0].strict, Some(true));
        assert_eq!(schemas[0].parameters["type"], json!("object"));
    }

    #[test]
    fn non_function_tools_and_nameless_tools_are_skipped() {
        let converter = OpenAIToolConverter::new();
        let mut not_function = OpenAITool::new(OpenAIToolFunction::new("f", "d"));
        not_function.r#type = "retrieval".to_string();
        let nameless = OpenAITool::new(OpenAIToolFunction {
            name: Some("  ".to_string()),
            description: Some("d".to_string()),
            parameters: None,
            strict: None,
        });
        let no_function = OpenAITool {
            r#type: "function".to_string(),
            function: None,
        };
        let schemas = converter.convert_to_tool_schemas(&[not_function, nameless, no_function]);
        assert!(schemas.is_empty());
    }

    #[test]
    fn missing_description_falls_back_to_empty_string() {
        let converter = OpenAIToolConverter::new();
        let schemas = converter.convert_to_tool_schemas(&[OpenAITool::new(OpenAIToolFunction {
            name: Some("f".to_string()),
            description: None,
            parameters: None,
            strict: None,
        })]);
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].description, "");
    }
}
