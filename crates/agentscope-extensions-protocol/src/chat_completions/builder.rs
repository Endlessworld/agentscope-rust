//! Response builder for the OpenAI-compatible Chat Completions API,
//! mirroring `io.agentscope.core.chat.completions.builder.ChatCompletionsResponseBuilder`.

use agentscope_core::message::{ContentBlock, Msg, MsgRole, ToolUseBlock};
use chrono::Utc;

use super::model::{
    ChatChoice, ChatCompletionsRequest, ChatCompletionsResponse, ChatMessage, ToolCall,
};

/// Builds OpenAI-compatible chat completion responses from core [`Msg`] values.
#[derive(Debug, Default, Clone, Copy)]
pub struct ChatCompletionsResponseBuilder;

impl ChatCompletionsResponseBuilder {
    /// Creates a new builder.
    pub fn new() -> Self {
        Self
    }

    /// Builds a successful response from the agent reply.
    pub fn build_response(
        &self,
        request: &ChatCompletionsRequest,
        reply: &Msg,
        request_id: &str,
    ) -> ChatCompletionsResponse {
        let message = self.convert_msg_to_chat_message(Some(reply));
        let finish_reason = finish_reason_for(reply, &message);
        ChatCompletionsResponse {
            id: Some(request_id.to_string()),
            object: "chat.completion".to_string(),
            created: Utc::now().timestamp(),
            model: request.model.clone(),
            choices: Some(vec![ChatChoice::message(0, message, Some(finish_reason))]),
            usage: None,
        }
    }

    /// Builds an error response with `finish_reason` set to `error`.
    pub fn build_error_response(
        &self,
        request: &ChatCompletionsRequest,
        error: Option<&str>,
        request_id: &str,
    ) -> ChatCompletionsResponse {
        let error_message = error.unwrap_or("Unknown error occurred");
        ChatCompletionsResponse {
            id: Some(request_id.to_string()),
            object: "chat.completion".to_string(),
            created: Utc::now().timestamp(),
            model: request.model.clone(),
            choices: Some(vec![ChatChoice::message(
                0,
                ChatMessage::new("assistant", format!("Error: {}", error_message)),
                Some("error".to_string()),
            )]),
            usage: None,
        }
    }

    /// Converts a core [`Msg`] into an OpenAI [`ChatMessage`].
    ///
    /// Text blocks are concatenated without separators (matching Java); tool-use
    /// blocks are emitted as `tool_calls` for assistant messages only.
    pub fn convert_msg_to_chat_message(&self, msg: Option<&Msg>) -> ChatMessage {
        let Some(msg) = msg else {
            return ChatMessage::new("assistant", "");
        };
        let role = role_str(msg.role());

        if msg.content().is_empty() {
            return ChatMessage {
                role: Some(role),
                content: Some(String::new()),
                ..Default::default()
            };
        }

        let text_content: String = msg
            .content()
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect();

        let tool_calls = if msg.role() == MsgRole::Assistant {
            let calls: Vec<ToolCall> = msg
                .content()
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse(t) => Some(t),
                    _ => None,
                })
                .map(tool_use_block_to_tool_call)
                .collect();
            if calls.is_empty() {
                None
            } else {
                Some(calls)
            }
        } else {
            None
        };

        ChatMessage {
            role: Some(role),
            content: Some(text_content),
            tool_calls,
            tool_call_id: None,
            name: None,
        }
    }

    /// Extracts text content from a [`Msg`], defaulting to an empty string.
    pub fn extract_text_content(&self, msg: Option<&Msg>) -> String {
        msg.map(|m| m.text_content()).unwrap_or_default()
    }
}

fn role_str(role: MsgRole) -> String {
    match role {
        MsgRole::User => "user",
        MsgRole::Assistant => "assistant",
        MsgRole::System => "system",
        MsgRole::Tool => "tool",
    }
    .to_string()
}

/// Converts a [`ToolUseBlock`] into an OpenAI [`ToolCall`].
///
/// Prefers the raw `content` JSON string over the `input` map, since providers
/// such as DashScope stream arguments into `content`. Empty input falls back to
/// `"{}"` for non-streaming responses.
pub(crate) fn tool_use_block_to_tool_call(block: &ToolUseBlock) -> ToolCall {
    let arguments = match block.content.as_deref() {
        Some(content) if !content.is_empty() => content.to_string(),
        _ if !block.input.is_empty() => {
            serde_json::to_string(&block.input).unwrap_or_else(|_| "{}".to_string())
        }
        _ => "{}".to_string(),
    };
    ToolCall::new(block.id.clone(), block.name.clone(), arguments)
}

/// Derives the OpenAI `finish_reason` from the message generate reason or its
/// content, mirroring the Java builder logic.
fn finish_reason_for(reply: &Msg, message: &ChatMessage) -> String {
    use agentscope_core::message::GenerateReason;
    let reason = reply.generate_reason();
    let has_tool_calls = message
        .tool_calls
        .as_ref()
        .is_some_and(|calls| !calls.is_empty());
    if reason == GenerateReason::ToolSuspended || has_tool_calls {
        "tool_calls".to_string()
    } else if reason == GenerateReason::MaxIterations {
        "length".to_string()
    } else {
        "stop".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::{GenerateReason, TextBlock, ToolUseBlock};
    use serde_json::json;

    #[test]
    fn build_response_echoes_request_model_and_sets_object() {
        let builder = ChatCompletionsResponseBuilder::new();
        let request = ChatCompletionsRequest {
            model: Some("gpt-4".to_string()),
            ..Default::default()
        };
        let reply = Msg::assistant("Hello!");
        let response = builder.build_response(&request, &reply, "chatcmpl-1");
        assert_eq!(response.id.as_deref(), Some("chatcmpl-1"));
        assert_eq!(response.object, "chat.completion");
        assert_eq!(response.model.as_deref(), Some("gpt-4"));
        assert!(response.created > 0);
        let choice = &response.choices.as_ref().unwrap()[0];
        assert_eq!(choice.index, 0);
        assert_eq!(choice.finish_reason.as_deref(), Some("stop"));
        assert_eq!(choice.message.as_ref().unwrap().content.as_deref(), Some("Hello!"));
    }

    #[test]
    fn tool_use_reply_yields_tool_calls_finish_reason() {
        let builder = ChatCompletionsResponseBuilder::new();
        let request = ChatCompletionsRequest {
            model: Some("gpt-4".to_string()),
            ..Default::default()
        };
        let reply = Msg::assistant_blocks(vec![ContentBlock::ToolUse(ToolUseBlock::new(
            "call_1",
            "get_weather",
            serde_json::from_value(json!({"city": "Hangzhou"})).unwrap(),
        ))]);
        let response = builder.build_response(&request, &reply, "req");
        let choice = &response.choices.as_ref().unwrap()[0];
        assert_eq!(choice.finish_reason.as_deref(), Some("tool_calls"));
        let message = choice.message.as_ref().unwrap();
        let calls = message.tool_calls.as_ref().unwrap();
        assert_eq!(calls[0].id.as_deref(), Some("call_1"));
        assert_eq!(calls[0].function.as_ref().unwrap().name, "get_weather");
        assert_eq!(
            calls[0].function.as_ref().unwrap().arguments,
            "{\"city\":\"Hangzhou\"}"
        );
    }

    #[test]
    fn max_iterations_maps_to_length_finish_reason() {
        let builder = ChatCompletionsResponseBuilder::new();
        let request = ChatCompletionsRequest::default();
        let reply = Msg::builder()
            .role(MsgRole::Assistant)
            .text_content("partial")
            .generate_reason(GenerateReason::MaxIterations)
            .build();
        let response = builder.build_response(&request, &reply, "req");
        assert_eq!(
            response.choices.as_ref().unwrap()[0]
                .finish_reason
                .as_deref(),
            Some("length")
        );
    }

    #[test]
    fn tool_suspended_maps_to_tool_calls_finish_reason() {
        let builder = ChatCompletionsResponseBuilder::new();
        let request = ChatCompletionsRequest::default();
        let reply = Msg::builder()
            .role(MsgRole::Assistant)
            .text_content("suspending")
            .generate_reason(GenerateReason::ToolSuspended)
            .build();
        let response = builder.build_response(&request, &reply, "req");
        assert_eq!(
            response.choices.as_ref().unwrap()[0]
                .finish_reason
                .as_deref(),
            Some("tool_calls")
        );
    }

    #[test]
    fn build_error_response_uses_error_finish_reason() {
        let builder = ChatCompletionsResponseBuilder::new();
        let request = ChatCompletionsRequest {
            model: Some("gpt-4".to_string()),
            ..Default::default()
        };
        let response = builder.build_error_response(&request, Some("boom"), "req");
        let choice = &response.choices.as_ref().unwrap()[0];
        assert_eq!(choice.finish_reason.as_deref(), Some("error"));
        assert_eq!(
            choice.message.as_ref().unwrap().content.as_deref(),
            Some("Error: boom")
        );
    }

    #[test]
    fn convert_msg_concatenates_text_and_omits_tool_calls_for_non_assistant() {
        let builder = ChatCompletionsResponseBuilder::new();
        // `MsgRole::Tool` is the cheapest role that may legally carry tool-use blocks: core's
        // `validate_role_content` panics on `ToolUse` inside a `User` message, so the Java-shaped
        // fixture (USER + tool-use) is not constructible here.
        let msg = Msg::builder()
            .role(MsgRole::Tool)
            .content(vec![
                ContentBlock::Text(TextBlock::new("a")),
                ContentBlock::Text(TextBlock::new("b")),
                ContentBlock::ToolUse(ToolUseBlock::new("c", "f", Default::default())),
            ])
            .build();
        let chat = builder.convert_msg_to_chat_message(Some(&msg));
        assert_eq!(chat.role.as_deref(), Some("tool"));
        assert_eq!(chat.content.as_deref(), Some("ab"));
        assert!(chat.tool_calls.is_none());
    }

    #[test]
    fn convert_none_msg_yields_empty_assistant_message() {
        let builder = ChatCompletionsResponseBuilder::new();
        let chat = builder.convert_msg_to_chat_message(None);
        assert_eq!(chat.role.as_deref(), Some("assistant"));
        assert_eq!(chat.content.as_deref(), Some(""));
    }

    #[test]
    fn tool_call_arguments_prefer_raw_content_string() {
        let mut block = ToolUseBlock::new("c", "f", Default::default());
        block.content = Some("{\"raw\":1}".to_string());
        let call = tool_use_block_to_tool_call(&block);
        assert_eq!(call.function.unwrap().arguments, "{\"raw\":1}");
    }
}
