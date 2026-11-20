//! How a reader narrows a session's committed event stream.
//!
//! The stream is one log per session and every committed event is in it. A reader that only
//! renders messages, or that only wants to know when a turn ended, says so once and the backend
//! drops the rest before the page is serialised. Filters never change what is durable: the log
//! keeps every event, and `All` returns exactly what `events` always returned.

use serde::{Deserialize, Serialize};

/// Which committed events a reader wants to receive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeltaFilter {
    /// Only the terminal events of a turn.
    None,
    /// Message events plus the terminal events.
    Message,
    /// Message and tool events plus the terminal events.
    ToolArguments,
    /// Every committed event.
    #[default]
    All,
}

impl DeltaFilter {
    /// The wire names, in the order a UI usually offers them.
    pub const NAMES: [&'static str; 4] = ["none", "message", "toolArguments", "all"];

    /// Reads a wire name; an unknown name is a caller error, not a silent default.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "message" => Some(Self::Message),
            "toolArguments" | "tool-arguments" | "tool_arguments" => Some(Self::ToolArguments),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Message => "message",
            Self::ToolArguments => "toolArguments",
            Self::All => "all",
        }
    }

    /// Whether one committed event passes the filter.
    pub fn allows(self, event_type: &str) -> bool {
        match self {
            Self::All => true,
            Self::None => is_terminal_event(event_type),
            Self::Message => is_terminal_event(event_type) || event_type.starts_with("message/"),
            Self::ToolArguments => {
                is_terminal_event(event_type)
                    || event_type.starts_with("message/")
                    || event_type.starts_with("tool/")
                    || event_type.starts_with("tool.")
            }
        }
    }
}

/// True for the three event types that end a turn.
pub fn is_terminal_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "turn.succeeded" | "turn.failed" | "turn.cancelled"
    )
}

/// The reader's stream preferences.
///
/// The default is `All`: a caller that never mentions options keeps the historical behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EventStreamOptions {
    pub deltas: DeltaFilter,
}

impl EventStreamOptions {
    pub fn defaults() -> Self {
        Self {
            deltas: DeltaFilter::All,
        }
    }

    pub fn with_deltas(mut self, deltas: DeltaFilter) -> Self {
        self.deltas = deltas;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_names_round_trip() {
        for filter in [
            DeltaFilter::None,
            DeltaFilter::Message,
            DeltaFilter::ToolArguments,
            DeltaFilter::All,
        ] {
            assert_eq!(DeltaFilter::parse(filter.as_str()), Some(filter));
        }
        assert_eq!(DeltaFilter::parse("everything"), None);
    }

    #[test]
    fn none_keeps_only_the_ending() {
        assert!(!DeltaFilter::None.allows("message/assistant"));
        assert!(!DeltaFilter::None.allows("tool/requested"));
        assert!(DeltaFilter::None.allows("turn.succeeded"));
        assert!(DeltaFilter::None.allows("turn.failed"));
        assert!(DeltaFilter::None.allows("turn.cancelled"));
    }

    #[test]
    fn message_keeps_messages_and_the_ending() {
        assert!(DeltaFilter::Message.allows("message/user"));
        assert!(DeltaFilter::Message.allows("message/assistant"));
        assert!(!DeltaFilter::Message.allows("tool/requested"));
        assert!(!DeltaFilter::Message.allows("model.chunk"));
        assert!(DeltaFilter::Message.allows("turn.succeeded"));
    }

    #[test]
    fn tool_arguments_adds_tools_to_messages() {
        assert!(DeltaFilter::ToolArguments.allows("message/assistant"));
        assert!(DeltaFilter::ToolArguments.allows("tool/requested"));
        assert!(DeltaFilter::ToolArguments.allows("tool.result.applied"));
        assert!(!DeltaFilter::ToolArguments.allows("model.chunk"));
        assert!(DeltaFilter::ToolArguments.allows("turn.failed"));
    }

    #[test]
    fn all_keeps_everything() {
        for event_type in ["message/user", "tool/result", "model.chunk", "turn.queued"] {
            assert!(DeltaFilter::All.allows(event_type));
        }
    }
}
