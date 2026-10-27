//! Multi-agent formatter for the OpenAI Responses API.
//!
//! Mirrors `ResponsesMultiAgentFormatter`. Messages are grouped into:
//!
//! * **SYSTEM** — emitted individually as `EasyInputMessage` with the system role.
//! * **TOOL_SEQUENCE** — tool calls/results passed through to
//!   [`super::request`] unchanged (including reasoning and server-tool replay).
//! * **AGENT_CONVERSATION** — merged into a single `user` message wrapped in
//!   `<history>` tags, with an optional per-message agent-name prefix.
//! * **BYPASS** — messages carrying
//!   `_bypass_multiagent_history_merge` are emitted individually as user
//!   messages.

use agentscope_core::message::keys::metadata_keys::BYPASS_MULTIAGENT_HISTORY_MERGE;
use agentscope_core::message::{
    ContentBlock, HintBlock, Msg, MsgRole, ThinkingBlock, ToolResultBlock, ToolUseBlock,
};
use serde_json::Value;

use super::constants::MD_REASONING_ENCRYPTED_CONTENT;
use super::request;
use super::server_tool::ResponsesServerToolHelper;

const DEFAULT_CONVERSATION_HISTORY_PROMPT: &str = "# Conversation History\nThe content between \
     <history></history> tags contains your conversation history\n";

const HISTORY_START_TAG: &str = "<history>";
const HISTORY_END_TAG: &str = "</history>";

/// Multi-agent formatter for the Responses API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponsesMultiAgentFormatter {
    conversation_history_prompt: String,
}

impl Default for ResponsesMultiAgentFormatter {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponsesMultiAgentFormatter {
    /// Creates a formatter with the default conversation history prompt.
    pub fn new() -> Self {
        Self {
            conversation_history_prompt: DEFAULT_CONVERSATION_HISTORY_PROMPT.to_string(),
        }
    }

    /// Creates a formatter with a custom conversation history prompt.
    pub fn with_prompt(prompt: impl Into<String>) -> Self {
        Self {
            conversation_history_prompt: prompt.into(),
        }
    }

    /// Formats messages into Responses API input items with multi-agent merging.
    pub fn format_history(&self, messages: &[Msg]) -> Result<Vec<Value>, agentscope_core::Error> {
        let groups = self.group_messages(messages);
        let mut result = Vec::new();
        let mut is_first_agent_group = true;

        for group in groups {
            match group.kind {
                GroupType::System => {
                    if let Some(msg) = group.messages.first() {
                        request::map_system_message(msg, &mut result)?;
                    }
                }
                GroupType::ToolSequence => {
                    for msg in &group.messages {
                        if matches!(msg.role, MsgRole::Assistant | MsgRole::Tool) {
                            request::map_message(msg, &mut result)?;
                        }
                    }
                }
                GroupType::AgentConversation => {
                    result.push(
                        self.merge_agent_conversation(&group.messages, is_first_agent_group)?,
                    );
                    is_first_agent_group = false;
                }
                GroupType::Bypass => {
                    if let Some(msg) = group.messages.first() {
                        request::map_message(msg, &mut result)?;
                    }
                }
            }
        }

        Ok(result)
    }

    // ── Grouping ────────────────────────────────────────────────────

    fn group_messages(&self, messages: &[Msg]) -> Vec<MessageGroup> {
        let mut groups = Vec::new();
        let mut current: Vec<Msg> = Vec::new();
        let mut current_type: Option<GroupType> = None;

        for msg in messages {
            let kind = determine_group_type(msg);
            let starts_new = current_type.is_none()
                || current_type != Some(kind)
                || kind == GroupType::System
                || kind == GroupType::Bypass;
            if starts_new {
                if !current.is_empty() {
                    groups.push(MessageGroup {
                        kind: current_type.unwrap_or(GroupType::AgentConversation),
                        messages: std::mem::take(&mut current),
                    });
                }
                current_type = Some(kind);
            }
            current.push(msg.clone());
        }

        if !current.is_empty() {
            groups.push(MessageGroup {
                kind: current_type.unwrap_or(GroupType::AgentConversation),
                messages: current,
            });
        }

        groups
    }

    // ── Conversation merging ────────────────────────────────────────

    fn merge_agent_conversation(
        &self,
        messages: &[Msg],
        is_first_group: bool,
    ) -> Result<Value, agentscope_core::Error> {
        let mut parts: Vec<Value> = Vec::new();
        let mut text_buffer = String::new();

        if is_first_group {
            text_buffer.push_str(&self.conversation_history_prompt);
        }
        text_buffer.push_str(HISTORY_START_TAG);
        text_buffer.push('\n');

        let include_prefix = messages.len() > 1;
        for msg in messages {
            self.process_message(msg, &mut text_buffer, &mut parts, include_prefix)?;
        }

        text_buffer.push_str(HISTORY_END_TAG);
        text_buffer.push('\n');

        // Always non-empty: at least the history tags are present.
        parts.push(request::input_text(&text_buffer));

        Ok(request::easy_message("user", Value::Array(parts)))
    }

    fn process_message(
        &self,
        msg: &Msg,
        text_buffer: &mut String,
        parts: &mut Vec<Value>,
        include_prefix: bool,
    ) -> Result<(), agentscope_core::Error> {
        let agent_name = msg.name.as_deref();
        for block in &msg.content {
            match block {
                ContentBlock::Text(text) => {
                    if include_prefix {
                        append_name_prefix(text_buffer, agent_name);
                    }
                    text_buffer.push_str(&text.text);
                    text_buffer.push('\n');
                }
                ContentBlock::Hint(HintBlock { hint, .. }) => {
                    if include_prefix {
                        append_name_prefix(text_buffer, agent_name);
                    }
                    if let Some(hint) = hint {
                        text_buffer.push_str(hint);
                    }
                    text_buffer.push('\n');
                }
                ContentBlock::Image(image) => {
                    flush_text(text_buffer, parts);
                    match request::resolve_image_url(&image.source) {
                        Ok(url) => parts.push(request::input_image(&url)),
                        Err(error) => {
                            tracing::warn!(
                                "Failed to process ImageBlock in multi-agent conversation: {}",
                                error
                            );
                            if include_prefix {
                                append_name_prefix(text_buffer, agent_name);
                            }
                            text_buffer.push_str("[Image - processing failed]\n");
                        }
                    }
                }
                ContentBlock::Data(data) => {
                    flush_text(text_buffer, parts);
                    match request::resolve_data_block_image_url(data) {
                        Ok(url) => parts.push(request::input_image(&url)),
                        Err(error) => {
                            tracing::warn!(
                                "Failed to process DataBlock in multi-agent conversation: {}",
                                error
                            );
                            if include_prefix {
                                append_name_prefix(text_buffer, agent_name);
                            }
                            text_buffer.push_str("[Data - processing failed]\n");
                        }
                    }
                }
                ContentBlock::Thinking(ThinkingBlock { thinking, .. }) => {
                    if include_prefix {
                        append_name_prefix(text_buffer, agent_name);
                    }
                    if !thinking.is_empty() {
                        text_buffer.push_str("[Thinking]: ");
                        text_buffer.push_str(thinking);
                        text_buffer.push('\n');
                    }
                }
                // Audio/video/tool blocks are silently skipped (tool blocks can
                // never reach this branch — they route to TOOL_SEQUENCE).
                _ => {}
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupType {
    System,
    ToolSequence,
    AgentConversation,
    Bypass,
}

#[derive(Debug)]
struct MessageGroup {
    kind: GroupType,
    messages: Vec<Msg>,
}

fn determine_group_type(msg: &Msg) -> GroupType {
    if should_bypass_history(msg) {
        return GroupType::Bypass;
    }
    match msg.role {
        MsgRole::System => GroupType::System,
        MsgRole::Tool => GroupType::ToolSequence,
        MsgRole::User | MsgRole::Assistant => {
            let has_tool = msg.content.iter().any(|b| {
                matches!(
                    b,
                    ContentBlock::ToolUse(ToolUseBlock { .. })
                        | ContentBlock::ToolResult(ToolResultBlock { .. })
                )
            });
            if has_tool {
                return GroupType::ToolSequence;
            }
            if msg.role == MsgRole::Assistant && has_encrypted_reasoning(msg) {
                return GroupType::ToolSequence;
            }
            GroupType::AgentConversation
        }
    }
}

fn should_bypass_history(msg: &Msg) -> bool {
    msg.metadata
        .get(BYPASS_MULTIAGENT_HISTORY_MERGE)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn has_encrypted_reasoning(msg: &Msg) -> bool {
    msg.content.iter().any(|block| match block {
        ContentBlock::Thinking(thinking) => thinking
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get(MD_REASONING_ENCRYPTED_CONTENT))
            .and_then(Value::as_str)
            .map(|encrypted| !encrypted.is_empty())
            .unwrap_or(false),
        _ => false,
    })
}

fn flush_text(text_buffer: &mut String, parts: &mut Vec<Value>) {
    if !text_buffer.is_empty() {
        parts.push(request::input_text(text_buffer));
        text_buffer.clear();
    }
}

fn append_name_prefix(buffer: &mut String, agent_name: Option<&str>) {
    if let Some(name) = agent_name.filter(|name| !name.is_empty()) {
        buffer.push_str(name);
        buffer.push_str(": ");
    }
}

// Keep the server-tool helper import meaningful for future server-tool restore
// behaviour in merged groups (mirrors the Java formatter's delegation).
#[allow(dead_code)]
fn _server_tool_marker(metadata: &agentscope_core::message::JsonMap) -> Option<Value> {
    ResponsesServerToolHelper::restore_server_tool_item(metadata)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::{TextBlock, ToolUseBlock};
    use serde_json::json;

    fn named(role: MsgRole, name: &str, text: &str) -> Msg {
        Msg::builder()
            .role(role)
            .name(name)
            .content(vec![ContentBlock::Text(TextBlock::new(text))])
            .build()
    }

    fn history_text(items: &[Value]) -> String {
        items
            .iter()
            .filter_map(|item| {
                let content = item.get("content")?;
                if let Some(text) = content.as_str() {
                    return Some(text.to_string());
                }
                let parts = content.as_array()?;
                Some(
                    parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join(""),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn system_is_individual_and_agent_turns_merge_into_one_user_message() {
        let formatter = ResponsesMultiAgentFormatter::new();
        let messages = vec![
            Msg::system("be nice"),
            named(MsgRole::User, "alice", "hello"),
            named(MsgRole::Assistant, "bob", "hi alice"),
            named(MsgRole::User, "alice", "how are you?"),
        ];
        let items = formatter.format_history(&messages).unwrap();

        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["role"], json!("system"));
        assert_eq!(items[0]["content"], json!("be nice"));

        assert_eq!(items[1]["role"], json!("user"));
        let text = history_text(&items[1..]);
        assert!(text.contains("# Conversation History"), "{text}");
        assert!(text.contains("<history>"));
        assert!(text.contains("</history>"));
        // Multi-turn: each message is prefixed with the agent name.
        assert!(text.contains("alice: hello"));
        assert!(text.contains("bob: hi alice"));
        assert!(text.contains("alice: how are you?"));
    }

    #[test]
    fn single_turn_group_omits_name_prefix() {
        let formatter = ResponsesMultiAgentFormatter::new();
        let messages = vec![named(MsgRole::User, "alice", "hello")];
        let items = formatter.format_history(&messages).unwrap();
        let text = history_text(&items);
        assert!(!text.contains("alice: "), "{text}");
        assert!(text.contains("hello"));
    }

    #[test]
    fn only_first_agent_group_carries_the_prompt() {
        let formatter = ResponsesMultiAgentFormatter::with_prompt("PROMPT\n");
        let messages = vec![
            named(MsgRole::User, "alice", "one"),
            Msg::system("interrupt"),
            named(MsgRole::User, "alice", "two"),
        ];
        let items = formatter.format_history(&messages).unwrap();
        assert_eq!(items.len(), 3);
        assert!(history_text(&items[0..1]).contains("PROMPT\n"));
        // System message sits between the two agent groups.
        assert_eq!(items[1]["role"], json!("system"));
        assert!(!history_text(&items[2..]).contains("PROMPT\n"));
    }

    #[test]
    fn tool_sequence_passes_through_unchanged() {
        let formatter = ResponsesMultiAgentFormatter::new();
        let assistant = Msg::assistant_blocks(vec![ContentBlock::ToolUse(ToolUseBlock::new(
            "call_1",
            "get_weather",
            [("city".to_string(), json!("Beijing"))]
                .into_iter()
                .collect(),
        ))]);
        let tool = Msg::tool("call_1", "get_weather", "sunny");
        let messages = vec![named(MsgRole::User, "alice", "weather?"), assistant, tool];
        let items = formatter.format_history(&messages).unwrap();

        // user (agent conversation) + function_call + function_call_output.
        assert_eq!(items.len(), 3);
        assert_eq!(items[1]["type"], json!("function_call"));
        assert_eq!(items[2]["type"], json!("function_call_output"));
    }

    #[test]
    fn bypass_messages_stay_individual() {
        let formatter = ResponsesMultiAgentFormatter::new();
        let mut bypass = named(MsgRole::User, "alice", "pinned");
        bypass
            .metadata
            .insert(BYPASS_MULTIAGENT_HISTORY_MERGE.to_string(), json!(true));
        let messages = vec![
            named(MsgRole::User, "alice", "one"),
            bypass,
            named(MsgRole::User, "alice", "two"),
        ];
        let items = formatter.format_history(&messages).unwrap();
        // First group, bypass message, third group: no merging across bypass.
        assert_eq!(items.len(), 3);
        assert_eq!(items[1]["content"], json!("pinned"));
    }

    #[test]
    fn thinking_blocks_are_rendered_inside_history() {
        let formatter = ResponsesMultiAgentFormatter::new();
        let msg = Msg::assistant_blocks(vec![
            ContentBlock::Thinking(ThinkingBlock::new("hmm")),
            ContentBlock::Text(TextBlock::new("answer")),
        ]);
        let items = formatter.format_history(&[msg]).unwrap();
        let text = history_text(&items);
        assert!(text.contains("[Thinking]: hmm"), "{text}");
    }

    #[test]
    fn image_block_flushes_text_and_becomes_its_own_part() {
        let formatter = ResponsesMultiAgentFormatter::new();
        let image = ContentBlock::Image(agentscope_core::ImageBlock::new(
            agentscope_core::Source::Base64(agentscope_core::Base64Source::new(
                "image/png",
                "AAAA",
            )),
        ));
        let msg = Msg::user_blocks(vec![ContentBlock::Text(TextBlock::new("before")), image]);
        let items = formatter.format_history(&[msg]).unwrap();
        let content = items[0]["content"].as_array().unwrap();
        // text-before, image, closing `</history>` text (the Java formatter
        // flushes the text buffer, then appends the end tag as a final part).
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], json!("input_text"));
        assert!(content[0]["text"].as_str().unwrap().contains("before"));
        assert_eq!(content[1]["type"], json!("input_image"));
        assert_eq!(content[1]["image_url"], json!("data:image/png;base64,AAAA"));
        assert!(content[2]["text"].as_str().unwrap().contains("</history>"));
    }

    #[test]
    fn encrypted_reasoning_routes_to_tool_sequence_not_history() {
        let formatter = ResponsesMultiAgentFormatter::new();
        let thinking = ThinkingBlock::builder()
            .thinking("reasoning")
            .metadata(
                [(MD_REASONING_ENCRYPTED_CONTENT.to_string(), json!("enc"))]
                    .into_iter()
                    .collect(),
            )
            .build();
        let msg = Msg::assistant_blocks(vec![
            ContentBlock::Thinking(thinking),
            ContentBlock::Text(TextBlock::new("answer")),
        ]);
        let items = formatter.format_history(&[msg]).unwrap();
        assert_eq!(items[0]["type"], json!("reasoning"));
        assert_eq!(items[0]["encrypted_content"], json!("enc"));
        assert_eq!(items[1]["role"], json!("assistant"));
    }
}
