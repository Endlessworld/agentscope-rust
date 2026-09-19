//! Interrupt reason / metadata constants, mirroring `AguiInterruptConstants`.

/// AG-UI interrupt reason for tool-bound decisions.
pub const TOOL_CALL_INTERRUPT_REASON: &str = "tool_call";

/// AG-UI interrupt reason for structured input requests.
pub const INPUT_REQUIRED_INTERRUPT_REASON: &str = "input_required";

/// Interrupt metadata key: AgentScope-specific interrupt kind.
pub const METADATA_AGENTSCOPE_INTERRUPT_KIND: &str = "agentscope.interruptKind";

/// Interrupt metadata value for permission-mode tool confirmations.
pub const INTERRUPT_KIND_PERMISSION_CONFIRM: &str = "permission_confirm";

/// Interrupt metadata key: the tool name.
pub const METADATA_TOOL_NAME: &str = "toolName";

/// Interrupt metadata key: the parsed tool arguments.
pub const METADATA_TOOL_INPUT: &str = "toolInput";

/// Interrupt metadata key: the tool arguments serialized as a JSON-object string.
pub const METADATA_TOOL_CONTENT: &str = "toolContent";

/// Interrupt metadata key: the originating reply id.
pub const METADATA_REPLY_ID: &str = "replyId";

/// Resume payload key: the user approved the tool call.
pub const RESUME_PAYLOAD_APPROVED: &str = "approved";

/// Resume payload key: full replacement tool arguments.
pub const RESUME_PAYLOAD_EDITED_ARGS: &str = "editedArgs";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_constants_match_the_java_wire_values() {
        assert_eq!(TOOL_CALL_INTERRUPT_REASON, "tool_call");
        assert_eq!(INPUT_REQUIRED_INTERRUPT_REASON, "input_required");
        assert_eq!(METADATA_AGENTSCOPE_INTERRUPT_KIND, "agentscope.interruptKind");
        assert_eq!(INTERRUPT_KIND_PERMISSION_CONFIRM, "permission_confirm");
        assert_eq!(METADATA_TOOL_NAME, "toolName");
        assert_eq!(METADATA_TOOL_INPUT, "toolInput");
        assert_eq!(METADATA_TOOL_CONTENT, "toolContent");
        assert_eq!(METADATA_REPLY_ID, "replyId");
        assert_eq!(RESUME_PAYLOAD_APPROVED, "approved");
        assert_eq!(RESUME_PAYLOAD_EDITED_ARGS, "editedArgs");
    }
}
