//! Producer-supplied tool execution facts.
//! Mirrors `io.agentscope.core.message.ToolExecutionDetails`.
//!
//! These facts are separate from the model-visible text and from task verification: a
//! successful process is not a verified task, and an absent exit code means unknown, never
//! zero.

use serde::{Deserialize, Serialize};

/// What the producer observed while executing a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExecutionDetails {
    /// Producer-defined execution kind (`"shell"`, `"file_write"`, ...).
    pub kind: String,
    /// The execution outcome as the producer judges it.
    pub outcome: ToolExecutionOutcome,
    /// Process exit code; `None` means unknown, never zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Whether the producer cut the captured output at its limit.
    #[serde(default)]
    pub output_truncated: bool,
}

/// The producer's verdict for one execution, mirroring the Java enum constant names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ToolExecutionOutcome {
    Succeeded,
    Failed,
    Partial,
    Unknown,
}

impl ToolExecutionDetails {
    /// Builds the minimal details for a kind/outcome pair (no exit code, not truncated).
    pub fn new(kind: impl Into<String>, outcome: ToolExecutionOutcome) -> Self {
        Self {
            kind: kind.into(),
            outcome,
            exit_code: None,
            output_truncated: false,
        }
    }

    /// Records the process exit code.
    pub fn with_exit_code(mut self, exit_code: i32) -> Self {
        self.exit_code = Some(exit_code);
        self
    }

    /// Records that the captured output was truncated.
    pub fn with_output_truncated(mut self, truncated: bool) -> Self {
        self.output_truncated = truncated;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_names_match_the_java_enum_constants() {
        for (outcome, wire) in [
            (ToolExecutionOutcome::Succeeded, "SUCCEEDED"),
            (ToolExecutionOutcome::Failed, "FAILED"),
            (ToolExecutionOutcome::Partial, "PARTIAL"),
            (ToolExecutionOutcome::Unknown, "UNKNOWN"),
        ] {
            assert_eq!(serde_json::to_value(outcome).unwrap(), serde_json::json!(wire));
            assert_eq!(
                serde_json::from_value::<ToolExecutionOutcome>(serde_json::json!(wire)).unwrap(),
                outcome
            );
        }
    }

    #[test]
    fn serde_round_trip_keeps_an_absent_exit_code_absent() {
        let details = ToolExecutionDetails::new("shell", ToolExecutionOutcome::Partial);
        let json = serde_json::to_value(&details).unwrap();
        // An unknown exit code must not be serialized as zero.
        assert!(json.get("exit_code").is_none());
        assert_eq!(json["output_truncated"], serde_json::json!(false));
        assert_eq!(
            serde_json::from_value::<ToolExecutionDetails>(json).unwrap(),
            details
        );
    }

    #[test]
    fn serde_round_trip_keeps_exit_code_and_truncation() {
        let details = ToolExecutionDetails::new("shell", ToolExecutionOutcome::Succeeded)
            .with_exit_code(0)
            .with_output_truncated(true);
        let json = serde_json::to_value(&details).unwrap();
        assert_eq!(json["exit_code"], serde_json::json!(0));
        assert_eq!(json["output_truncated"], serde_json::json!(true));
        assert_eq!(
            serde_json::from_value::<ToolExecutionDetails>(json).unwrap(),
            details
        );
    }
}
