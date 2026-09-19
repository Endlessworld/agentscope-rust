//! Query history for Bailian multi-turn conversation rewrite.
//!
//! Ports `QueryHistoryEntry` plus the `Msg` → history conversion that Java kept private inside
//! `BailianKnowledge.convertToQueryHistory` / `extractTextContent`.

use agentscope_core::message::{ContentBlock, Msg, MsgRole};
use serde::{Deserialize, Serialize};

use crate::error::{RagError, Result};

/// The role of a query-history entry.
///
/// Java modelled this as a plain `String` and rejected anything other than `user` / `assistant`
/// in the constructor; an enum makes that invariant unrepresentable while keeping the same two
/// wire values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueryRole {
    /// A user turn.
    User,
    /// An assistant turn.
    Assistant,
}

impl QueryRole {
    /// The wire value (`"user"` / `"assistant"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            QueryRole::User => "user",
            QueryRole::Assistant => "assistant",
        }
    }
}

impl TryFrom<&str> for QueryRole {
    type Error = RagError;

    /// Parses a role, mirroring the Java constructor's `IllegalArgumentException`.
    fn try_from(value: &str) -> Result<Self> {
        match value {
            "user" => Ok(QueryRole::User),
            "assistant" => Ok(QueryRole::Assistant),
            other => Err(RagError::InvalidArgument(format!(
                "Role must be 'user' or 'assistant', got: {other}"
            ))),
        }
    }
}

/// A single message of the conversation history used for query rewrite.
///
/// Ports `QueryHistoryEntry`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryHistoryEntry {
    /// Who sent the message.
    pub role: QueryRole,
    /// The message text.
    pub content: String,
}

impl QueryHistoryEntry {
    /// Creates a user turn.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: QueryRole::User,
            content: content.into(),
        }
    }

    /// Creates an assistant turn.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: QueryRole::Assistant,
            content: content.into(),
        }
    }
}

/// Concatenates the text blocks of a message.
///
/// Reproduces Java `BailianKnowledge.extractTextContent`: text blocks are joined with `\n`,
/// non-text blocks are ignored, and a null / empty content list yields an empty string.
fn extract_text_content(msg: &Msg) -> String {
    let mut text = String::new();
    for block in &msg.content {
        if let ContentBlock::Text(text_block) = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&text_block.text);
        }
    }
    text
}

/// Converts conversation history into Bailian query-history entries.
///
/// Reproduces Java `BailianKnowledge.convertToQueryHistory`: only `USER` / `ASSISTANT` messages
/// are kept, their text is concatenated, and entries with empty text are dropped.
pub fn query_history_from_messages(messages: &[Msg]) -> Vec<QueryHistoryEntry> {
    messages
        .iter()
        .filter_map(|msg| {
            let role = match msg.role {
                MsgRole::User => QueryRole::User,
                MsgRole::Assistant => QueryRole::Assistant,
                // Java filtered out SYSTEM / TOOL turns before mapping.
                MsgRole::System | MsgRole::Tool => return None,
            };
            let content = extract_text_content(msg);
            if content.is_empty() {
                None
            } else {
                Some(QueryHistoryEntry { role, content })
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::TextBlock;

    #[test]
    fn role_wire_values_match_java() {
        assert_eq!(QueryRole::User.as_str(), "user");
        assert_eq!(QueryRole::Assistant.as_str(), "assistant");
        assert_eq!(QueryRole::try_from("user").unwrap(), QueryRole::User);
        assert_eq!(
            QueryRole::try_from("assistant").unwrap(),
            QueryRole::Assistant
        );
    }

    #[test]
    fn role_rejects_unknown_values_like_java() {
        let err = QueryRole::try_from("system").unwrap_err();
        assert!(err.to_string().contains("must be 'user' or 'assistant'"), "{err}");
        assert!(QueryRole::try_from("User").is_err());
    }

    #[test]
    fn constructors_set_the_expected_roles() {
        assert_eq!(QueryHistoryEntry::user("hi").role, QueryRole::User);
        assert_eq!(
            QueryHistoryEntry::assistant("hey").role,
            QueryRole::Assistant
        );
    }

    #[test]
    fn only_user_and_assistant_turns_are_converted() {
        let messages = vec![
            Msg::text(MsgRole::System, "system prompt"),
            Msg::text(MsgRole::User, "question"),
            Msg::text(MsgRole::Assistant, "answer"),
            Msg::text(MsgRole::Tool, "tool output"),
        ];
        let history = query_history_from_messages(&messages);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0], QueryHistoryEntry::user("question"));
        assert_eq!(history[1], QueryHistoryEntry::assistant("answer"));
    }

    #[test]
    fn multiple_text_blocks_are_joined_with_newlines() {
        let msg = Msg::new(
            "id",
            None,
            MsgRole::User,
            vec![
                ContentBlock::Text(TextBlock::new("line 1")),
                ContentBlock::Text(TextBlock::new("line 2")),
            ],
            None,
            None,
            None,
        );
        assert_eq!(
            query_history_from_messages(&[msg]),
            vec![QueryHistoryEntry::user("line 1\nline 2")]
        );
    }

    #[test]
    fn non_text_content_is_ignored_and_empty_entries_dropped() {
        let msg = Msg::new(
            "id",
            None,
            MsgRole::User,
            vec![ContentBlock::Text(TextBlock::new(""))],
            None,
            None,
            None,
        );
        assert!(query_history_from_messages(&[msg]).is_empty());
        assert!(query_history_from_messages(&[]).is_empty());
    }
}
