//! The neutral message projection the compactor rewrites.
//!
//! Java mutates `List<Msg>`; this crate does not depend on `agentscope-core`, so the compactor
//! works on an owned, serializable projection of exactly the fields it reads: id, role, text,
//! content blocks and metadata. Hosts adapt their `Msg` into it and back (see
//! `docs/extensions/JUDGE__GAP_TRACKER.md` §4.2).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::JevError;

/// The system role spelling (`MsgRole.SYSTEM.name()`).
pub const ROLE_SYSTEM: &str = "SYSTEM";
/// The user role spelling.
pub const ROLE_USER: &str = "USER";
/// The assistant role spelling.
pub const ROLE_ASSISTANT: &str = "ASSISTANT";
/// The tool role spelling.
pub const ROLE_TOOL: &str = "TOOL";

/// The metadata key a host uses to pin a message (`jev.context.pinned`).
pub const PINNED_METADATA_KEY: &str = "jev.context.pinned";

/// The final state of a tool call (`ToolResultState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ToolResultState {
    /// The tool succeeded; its output may be dropped or truncated.
    Success,
    /// The tool failed; its output is pinned.
    Error,
    /// The tool was cancelled.
    #[serde(alias = "CANCELED")]
    Cancelled,
    /// The state is unknown; treated as not successful.
    #[default]
    Unknown,
}

impl ToolResultState {
    /// The Java enum name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "SUCCESS",
            Self::Error => "ERROR",
            Self::Cancelled => "CANCELLED",
            Self::Unknown => "UNKNOWN",
        }
    }
}

/// One part of a tool result output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContextResultPart {
    /// A text part.
    Text {
        /// The text.
        text: String,
    },
    /// Any non-text part (image, audio, ...).
    Other,
}

/// One content block of a [`ContextMessage`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContextBlock {
    /// Plain text.
    Text {
        /// The text.
        text: String,
    },
    /// Reasoning text; pins the message and is never sent back to the model.
    Thinking {
        /// The thinking text.
        text: String,
    },
    /// A tool invocation.
    ToolUse {
        /// The call id.
        id: Option<String>,
        /// The tool name.
        name: String,
        /// The JSON arguments.
        input: Value,
    },
    /// A tool result.
    ToolResult {
        /// The call id this result answers.
        id: Option<String>,
        /// The tool name.
        name: String,
        /// The final state.
        state: ToolResultState,
        /// The output parts.
        #[serde(default)]
        parts: Vec<ContextResultPart>,
        /// Whether the result was suspended (`ToolResultBlock.METADATA_SUSPENDED`).
        #[serde(default)]
        suspended: bool,
    },
    /// Any other block type; the compactor rejects transcripts containing it.
    Other,
}

impl ContextBlock {
    /// The call id a block refers to, if any.
    pub fn call_id(&self) -> Option<&str> {
        match self {
            Self::ToolUse { id, .. } | Self::ToolResult { id, .. } => id.as_deref(),
            _ => None,
        }
    }

    /// The concatenated text of a tool result (`CompactionTranscript.text`).
    pub fn result_text(&self) -> String {
        match self {
            Self::ToolResult { parts, .. } => parts
                .iter()
                .filter_map(|part| match part {
                    ContextResultPart::Text { text } => Some(text.as_str()),
                    ContextResultPart::Other => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        }
    }
}

/// One message in the transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextMessage {
    /// The message id; unique within a transcript.
    pub id: String,
    /// The role spelling (see [`ROLE_USER`]).
    pub role: String,
    /// The concatenated text content (`Msg.getTextContent`).
    #[serde(default)]
    pub text: String,
    /// The content blocks, in order.
    #[serde(default)]
    pub blocks: Vec<ContextBlock>,
    /// Message metadata; `jev.context.pinned` pins the message.
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

impl ContextMessage {
    /// Creates a message.
    pub fn new(
        id: impl Into<String>,
        role: impl Into<String>,
        text: impl Into<String>,
        blocks: Vec<ContextBlock>,
    ) -> Self {
        Self {
            id: id.into(),
            role: role.into(),
            text: text.into(),
            blocks,
            metadata: BTreeMap::new(),
        }
    }

    /// Creates a user message with a single text block.
    pub fn user(id: impl Into<String>, text: impl Into<String>) -> Self {
        let text = text.into();
        Self::new(
            id,
            ROLE_USER,
            text.clone(),
            vec![ContextBlock::Text { text }],
        )
    }

    /// Creates an assistant message with a single text block.
    pub fn assistant(id: impl Into<String>, text: impl Into<String>) -> Self {
        let text = text.into();
        Self::new(
            id,
            ROLE_ASSISTANT,
            text.clone(),
            vec![ContextBlock::Text { text }],
        )
    }

    /// Whether the message carries a thinking block.
    pub fn has_thinking(&self) -> bool {
        self.blocks
            .iter()
            .any(|block| matches!(block, ContextBlock::Thinking { .. }))
    }

    /// Whether the message was pinned by metadata.
    pub fn pinned_by_metadata(&self) -> bool {
        matches!(
            self.metadata.get(PINNED_METADATA_KEY),
            Some(Value::Bool(true))
        )
    }

    /// Sets the pinned metadata flag.
    pub fn with_pinned(mut self) -> Self {
        self.metadata
            .insert(PINNED_METADATA_KEY.to_string(), Value::Bool(true));
        self
    }

    /// Validates that the message id is usable.
    pub fn require_id(&self) -> Result<(), JevError> {
        if self.id.trim().is_empty() {
            return Err(JevError::invalid("duplicate message id"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_round_trip_as_json() {
        let message = ContextMessage {
            id: "m1".to_string(),
            role: ROLE_ASSISTANT.to_string(),
            text: String::new(),
            blocks: vec![
                ContextBlock::Thinking {
                    text: "hmm".to_string(),
                },
                ContextBlock::ToolUse {
                    id: Some("c1".to_string()),
                    name: "read".to_string(),
                    input: json!({"path": "a.rs"}),
                },
                ContextBlock::ToolResult {
                    id: Some("c1".to_string()),
                    name: "read".to_string(),
                    state: ToolResultState::Success,
                    parts: vec![
                        ContextResultPart::Text {
                            text: "a".to_string(),
                        },
                        ContextResultPart::Other,
                        ContextResultPart::Text {
                            text: "b".to_string(),
                        },
                    ],
                    suspended: false,
                },
            ],
            metadata: BTreeMap::new(),
        };
        let encoded = serde_json::to_string(&message).unwrap();
        let decoded: ContextMessage = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, message);
        assert!(decoded.has_thinking());
        assert_eq!(decoded.blocks[2].result_text(), "a\nb");
        assert_eq!(decoded.blocks[2].call_id(), Some("c1"));
        assert_eq!(decoded.blocks[0].call_id(), None);
        assert_eq!(decoded.blocks[0].result_text(), "");
    }

    #[test]
    fn tool_result_state_and_pinning_match_the_java_spelling() {
        assert_eq!(ToolResultState::Success.as_str(), "SUCCESS");
        assert_eq!(ToolResultState::Error.as_str(), "ERROR");
        assert_eq!(ToolResultState::Cancelled.as_str(), "CANCELLED");
        assert_eq!(ToolResultState::Unknown.as_str(), "UNKNOWN");
        assert!(serde_json::to_string(&ToolResultState::Cancelled)
            .unwrap()
            .contains("CANCELLED"));
        assert_eq!(
            serde_json::from_str::<ToolResultState>("\"CANCELED\"").unwrap(),
            ToolResultState::Cancelled
        );
        assert_eq!(ToolResultState::default(), ToolResultState::Unknown);

        let pinned = ContextMessage::user("m", "hi").with_pinned();
        assert!(pinned.pinned_by_metadata());
        assert!(!ContextMessage::user("m", "hi").pinned_by_metadata());
        assert!(ContextMessage::user(" ", "hi").require_id().is_err());
        assert!(pinned.clone().require_id().is_ok());
        assert_eq!(pinned.role, ROLE_USER);
        assert_eq!(ContextMessage::assistant("a", "x").role, ROLE_ASSISTANT);
    }
}
