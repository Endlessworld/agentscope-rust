//! AgentScope-specific OTel attribute keys, mirroring
//! `AgentScopeIncubatingAttributes`.

pub const AGENTSCOPE_FUNCTION_NAME: &str = "agentscope.function.name";
pub const AGENTSCOPE_FUNCTION_INPUT: &str = "agentscope.function.input";
pub const AGENTSCOPE_FUNCTION_OUTPUT: &str = "agentscope.function.output";
pub const AGENTSCOPE_FORMAT_TARGET: &str = "agentscope.format.output";
pub const AGENTSCOPE_FORMAT_COUNT: &str = "agentscope.format.count";
pub const AGENTSCOPE_USAGE_TOOL_USE_PROMPT_TOKENS: &str = "agentscope.usage.tool_use_prompt_tokens";

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The 6 keys Java declares (`AgentScopeIncubatingAttributes.java`).
    #[test]
    fn the_key_set_matches_the_java_class_exactly() {
        let mut keys = vec![
            AGENTSCOPE_FUNCTION_NAME,
            AGENTSCOPE_FUNCTION_INPUT,
            AGENTSCOPE_FUNCTION_OUTPUT,
            AGENTSCOPE_FORMAT_TARGET,
            AGENTSCOPE_FORMAT_COUNT,
            AGENTSCOPE_USAGE_TOOL_USE_PROMPT_TOKENS,
        ];
        keys.sort_unstable();
        let count = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), count, "duplicate key in module");
        assert_eq!(
            keys,
            vec![
                "agentscope.format.count",
                "agentscope.format.output",
                "agentscope.function.input",
                "agentscope.function.name",
                "agentscope.function.output",
                "agentscope.usage.tool_use_prompt_tokens",
            ]
        );
    }

    #[test]
    fn the_previously_missing_usage_key_matches_the_java_literal() {
        assert_eq!(
            AGENTSCOPE_USAGE_TOOL_USE_PROMPT_TOKENS,
            "agentscope.usage.tool_use_prompt_tokens"
        );
    }
}
