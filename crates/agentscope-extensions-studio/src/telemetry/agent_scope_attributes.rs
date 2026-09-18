//! AgentScope-specific OTel attribute keys, mirroring
//! `AgentScopeIncubatingAttributes`.

pub const AGENTSCOPE_FUNCTION_NAME: &str = "agentscope.function.name";
pub const AGENTSCOPE_FUNCTION_INPUT: &str = "agentscope.function.input";
pub const AGENTSCOPE_FUNCTION_OUTPUT: &str = "agentscope.function.output";
pub const AGENTSCOPE_FORMAT_TARGET: &str = "agentscope.format.output";
pub const AGENTSCOPE_FORMAT_COUNT: &str = "agentscope.format.count";

/// Values for the AgentScope operation name (`gen_ai.operation.name`).
pub mod operation_name {
    /// Message formatting.
    pub const FORMAT: &str = "format";
    /// Generic function invocation.
    pub const INVOKE_GENERIC_FUNCTION: &str = "invoke_generic_function";
}

/// Extra provider names used by AgentScope extensions.
pub mod provider_name {
    pub const DASHSCOPE: &str = "dashscope";
    pub const MOONSHOT: &str = "moonshot";
}
