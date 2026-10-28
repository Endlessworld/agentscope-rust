//! Shared plumbing for the per-provider "multi-agent" formatters.
//!
//! Several Java adapters collapse a multi-agent conversation into a single user
//! message wrapped in `<history>` tags, and keep tool sequences as ordinary
//! messages. The grouping rule and the tag/prompt constants are identical across
//! providers, so they live here instead of being re-implemented per adapter:
//!
//! | Java | Rust |
//! |---|---|
//! | `*MultiAgentFormatter#groupMessagesSequentially` | [`group_messages`] |
//! | `*MultiAgentFormatter#isToolMessage` | [`is_tool_related`] |
//! | `*ConversationMerger` history tags | [`HISTORY_START_TAG`] / [`HISTORY_END_TAG`] |
//! | default history prompt | [`DEFAULT_CONVERSATION_HISTORY_PROMPT`] |

use agentscope_core::{ContentBlock, GenerateOptions, Msg, MsgRole};

/// Opening tag around a merged conversation history.
pub const HISTORY_START_TAG: &str = "<history>";
/// Closing tag around a merged conversation history.
pub const HISTORY_END_TAG: &str = "</history>";

/// Default prompt prepended before the first `<history>` block, matching
/// `DEFAULT_CONVERSATION_HISTORY_PROMPT` in the Java multi-agent formatters.
pub const DEFAULT_CONVERSATION_HISTORY_PROMPT: &str = "# Conversation History\nThe content between <history></history> tags contains your conversation history\n";

/// Message-group kind produced by [`group_messages`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupType {
    /// Run of ordinary (non tool-related) messages: merged into `<history>`.
    AgentMessage,
    /// Run of tool-role / tool-use / tool-result messages: converted normally.
    ToolSequence,
}

/// Returns `true` when `msg` belongs to a tool sequence, mirroring the Java
/// `getRole() == TOOL || hasContentBlocks(ToolUseBlock) ||
/// hasContentBlocks(ToolResultBlock)` check.
pub fn is_tool_related(msg: &Msg) -> bool {
    msg.role == MsgRole::Tool
        || msg
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolUse(_)))
        || msg
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult(_)))
}

/// Groups messages sequentially into [`GroupType::AgentMessage`] and
/// [`GroupType::ToolSequence`] runs, preserving order.
///
/// This mirrors `groupMessagesSequentially` byte for byte: only *consecutive*
/// messages of the same kind are grouped, so an agent turn that follows a tool
/// sequence starts a new group.
pub fn group_messages(messages: &[Msg]) -> Vec<(GroupType, Vec<Msg>)> {
    let mut groups: Vec<(GroupType, Vec<Msg>)> = Vec::new();
    let mut current: Option<GroupType> = None;

    for msg in messages {
        let kind = if is_tool_related(msg) {
            GroupType::ToolSequence
        } else {
            GroupType::AgentMessage
        };
        match current {
            Some(kind_now) if kind_now == kind => {
                if let Some((_, group)) = groups.last_mut() {
                    group.push(msg.clone());
                }
            }
            _ => {
                current = Some(kind);
                groups.push((kind, vec![msg.clone()]));
            }
        }
    }
    groups
}

/// Name used for an agent message in merged history: the message name, or
/// `"Unknown"` (same fallback as the Java mergers).
pub fn agent_name(msg: &Msg) -> String {
    msg.name.clone().unwrap_or_else(|| "Unknown".to_string())
}

/// Concatenates the text found in `TextBlock` / `HintBlock` / tool-result text
/// outputs of `blocks`. Media blocks are ignored; this matches the Java
/// `convertToolResultToString` used by the Ollama/DashScope mergers.
pub fn text_only(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text(t) => out.push_str(&t.text),
            ContentBlock::Hint(h) => out.push_str(h.hint.as_deref().unwrap_or_default()),
            ContentBlock::ToolResult(tr) => out.push_str(&text_only(&tr.output)),
            _ => {}
        }
    }
    out
}

/// Removes and returns a provider-level option from
/// `GenerateOptions::additional_body_params`.
///
/// Provider-selector options (`multiAgentFormatter`, `promoteToolResultImages`, ...)
/// are consumed at construction time and must never leak into the request body.
pub fn take_option(options: &mut GenerateOptions, key: &str) -> Option<serde_json::Value> {
    options.additional_body_params.remove(key)
}

/// Reads a boolean selector option, accepting JSON booleans and the strings
/// `"true"` / `"false"`.
pub fn take_bool_option(options: &mut GenerateOptions, key: &str) -> Option<bool> {
    take_option(options, key).and_then(|value| match value {
        serde_json::Value::Bool(value) => Some(value),
        serde_json::Value::String(value) if value.eq_ignore_ascii_case("true") => Some(true),
        serde_json::Value::String(value) if value.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    })
}

/// Reads the `multiAgentFormatter` selector: `true` selects the default prompt,
/// a string supplies a custom conversation-history prompt, anything else (or
/// `false`) leaves the plain chat formatter active.
pub fn take_multi_agent_prompt(options: &mut GenerateOptions) -> Option<String> {
    match take_option(options, "multiAgentFormatter")? {
        serde_json::Value::Bool(true) => Some(DEFAULT_CONVERSATION_HISTORY_PROMPT.to_string()),
        serde_json::Value::String(prompt) if !prompt.trim().is_empty() => Some(prompt),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::{JsonMap, TextBlock, ToolUseBlock};

    fn named(text: &str, name: &str) -> Msg {
        Msg::builder()
            .role(MsgRole::Assistant)
            .name(name)
            .text_content(text)
            .build()
    }

    #[test]
    fn groups_alternating_runs_in_order() {
        let messages = vec![
            named("a1", "Alice"),
            named("a2", "Alice"),
            Msg::tool("call-1", "search", "42"),
            named("b1", "Bob"),
            Msg::user("hi"),
        ];
        let groups = group_messages(&messages);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].0, GroupType::AgentMessage);
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].0, GroupType::ToolSequence);
        assert_eq!(groups[2].0, GroupType::AgentMessage);
        assert_eq!(groups[2].1.len(), 2);
    }

    #[test]
    fn assistant_tool_use_is_tool_related() {
        let msg = Msg::assistant_blocks(vec![ContentBlock::ToolUse(ToolUseBlock::new(
            "call-1",
            "search",
            JsonMap::new(),
        ))]);
        assert!(is_tool_related(&msg));
        assert!(!is_tool_related(&Msg::user("hi")));
    }

    #[test]
    fn unknown_name_falls_back_and_text_only_skips_media() {
        let msg = Msg::assistant_blocks(vec![
            ContentBlock::Text(TextBlock::new("hello")),
            ContentBlock::Hint(agentscope_core::HintBlock::new("h", "hint")),
        ]);
        assert_eq!(agent_name(&msg), "Unknown");
        assert_eq!(text_only(&msg.content), "hellohint");
    }
}
