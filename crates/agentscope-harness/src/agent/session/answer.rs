//! Typed answers to a pending interaction, mirroring `SessionAnswer`.

use agentscope_core::event::ConfirmResult;
use agentscope_core::message::{ContentBlock, TextBlock, ToolResultBlock, ToolUseBlock};

/// A caller-supplied confirmation result.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionConfirm {
    pub result: ConfirmResult,
}

/// An answer to one pending interaction request.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionAnswer {
    /// Free-form output for an `external_execution` request.
    Output { content: Vec<ContentBlock> },
    /// A plain approve/deny answer for a `confirmation` request.
    Approval {
        approved: bool,
        reason: Option<String>,
    },
    /// A fully-formed confirmation result (permission rules or edited arguments).
    Confirmation(SessionConfirm),
}

impl SessionAnswer {
    /// Text output for an `external_execution` request.
    pub fn text(output: impl Into<String>) -> Self {
        Self::Output {
            content: vec![ContentBlock::Text(TextBlock::new(output))],
        }
    }

    /// Custom result blocks for an `external_execution` request.
    pub fn output(content: Vec<ContentBlock>) -> Self {
        Self::Output { content }
    }

    /// Approve a confirmation request.
    pub fn approve() -> Self {
        Self::Approval {
            approved: true,
            reason: None,
        }
    }

    /// Deny a confirmation request with a reason.
    pub fn reject(reason: impl Into<String>) -> Self {
        Self::Approval {
            approved: false,
            reason: Some(reason.into()),
        }
    }

    /// Use a hand-built confirmation result.
    pub fn confirmation(result: ConfirmResult) -> Self {
        Self::Confirmation(SessionConfirm { result })
    }

    /// Whether this answer resolves an `external_execution` request.
    pub fn is_output(&self) -> bool {
        matches!(self, SessionAnswer::Output { .. })
    }

    /// Whether this answer resolves a `confirmation` request.
    pub fn is_confirmation(&self) -> bool {
        matches!(
            self,
            SessionAnswer::Approval { .. } | SessionAnswer::Confirmation(_)
        )
    }

    /// Builds the tool result for an `external_execution` answer.
    pub fn to_tool_result(&self, call_id: &str, tool_name: &str) -> Option<ToolResultBlock> {
        match self {
            SessionAnswer::Output { content } => {
                Some(ToolResultBlock::new(call_id, tool_name, content.clone()))
            }
            _ => None,
        }
    }

    /// Builds the confirmation result for a `confirmation` answer.
    pub fn to_confirm_result(&self, call: &ToolUseBlock) -> Option<ConfirmResult> {
        match self {
            SessionAnswer::Approval { approved, reason } => Some(ConfirmResult {
                confirmed: *approved,
                tool_call: Some(call.clone()),
                rules: Vec::new(),
                message: reason.clone().unwrap_or_default(),
            }),
            SessionAnswer::Confirmation(confirm) => Some(confirm.result.clone()),
            SessionAnswer::Output { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_answers_build_a_tool_result() {
        let result = SessionAnswer::text("USD 12,000")
            .to_tool_result("call-1", "create_order")
            .expect("output answers resolve external execution");
        assert_eq!(result.id.as_deref(), Some("call-1"));
        assert_eq!(result.name.as_deref(), Some("create_order"));
        assert!(result.text_content().contains("USD 12,000"));
    }

    #[test]
    fn approvals_build_a_confirm_result() {
        let call = ToolUseBlock::new("call-1", "purchase", Default::default());
        let approved = SessionAnswer::approve()
            .to_confirm_result(&call)
            .expect("approval resolves confirmation");
        assert!(approved.confirmed);
        assert_eq!(
            approved.tool_call.as_ref().map(|c| c.id.as_str()),
            Some("call-1")
        );

        let denied = SessionAnswer::reject("too expensive")
            .to_confirm_result(&call)
            .expect("rejection resolves confirmation");
        assert!(!denied.confirmed);
        assert_eq!(denied.message, "too expensive");
    }

    #[test]
    fn answer_kinds_do_not_cross() {
        let call = ToolUseBlock::new("c", "t", Default::default());
        assert!(SessionAnswer::text("x").to_confirm_result(&call).is_none());
        assert!(SessionAnswer::approve().to_tool_result("c", "t").is_none());
        assert!(SessionAnswer::text("x").is_output());
        assert!(!SessionAnswer::text("x").is_confirmation());
        assert!(SessionAnswer::reject("no").is_confirmation());
    }

    #[test]
    fn custom_output_blocks_are_preserved() {
        let blocks = vec![ContentBlock::Text(TextBlock::new("raw"))];
        let answer = SessionAnswer::output(blocks.clone());
        let result = answer.to_tool_result("c", "t").unwrap();
        assert_eq!(result.output, blocks);
    }
}
