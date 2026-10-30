//! OTel GenAI semantic-convention attribute keys, mirroring `GenAiIncubatingAttributes`.
//!
//! Java pins the copied key set to OpenTelemetry semantic conventions **1.41.0**
//! (`GenAiIncubatingAttributes.java:28`). That pin is independent of the `opentelemetry-bom`
//! version, so the dependabot bump `8690ab3f5` (1.66.0→1.67.0) does not change any key here.
//!
//! Keys are exposed as plain `&'static str` so they can be recorded on spans
//! without depending on the `opentelemetry` crate in this crate's public API
//! (see tracker §4.2).

// Attribute keys (`gen_ai.*`)
pub const GEN_AI_INPUT_MESSAGES: &str = "gen_ai.input.messages";
pub const GEN_AI_OUTPUT_MESSAGES: &str = "gen_ai.output.messages";
pub const GEN_AI_TOOL_DEFINITIONS: &str = "gen_ai.tool.definitions";
pub const GEN_AI_SYSTEM_INSTRUCTIONS: &str = "gen_ai.system_instructions";
pub const GEN_AI_AGENT_DESCRIPTION: &str = "gen_ai.agent.description";
pub const GEN_AI_AGENT_ID: &str = "gen_ai.agent.id";
pub const GEN_AI_AGENT_NAME: &str = "gen_ai.agent.name";
pub const GEN_AI_CONVERSATION_ID: &str = "gen_ai.conversation.id";
pub const GEN_AI_DATA_SOURCE_ID: &str = "gen_ai.data_source.id";
pub const GEN_AI_OPERATION_NAME: &str = "gen_ai.operation.name";
pub const GEN_AI_OUTPUT_TYPE: &str = "gen_ai.output.type";
pub const GEN_AI_PROVIDER_NAME: &str = "gen_ai.provider.name";
pub const GEN_AI_REQUEST_CHOICE_COUNT: &str = "gen_ai.request.choice.count";
pub const GEN_AI_REQUEST_ENCODING_FORMATS: &str = "gen_ai.request.encoding_formats";
pub const GEN_AI_REQUEST_FREQUENCY_PENALTY: &str = "gen_ai.request.frequency_penalty";
pub const GEN_AI_REQUEST_MAX_TOKENS: &str = "gen_ai.request.max_tokens";
pub const GEN_AI_REQUEST_MODEL: &str = "gen_ai.request.model";
pub const GEN_AI_REQUEST_PRESENCE_PENALTY: &str = "gen_ai.request.presence_penalty";
pub const GEN_AI_REQUEST_SEED: &str = "gen_ai.request.seed";
pub const GEN_AI_REQUEST_STOP_SEQUENCES: &str = "gen_ai.request.stop_sequences";
pub const GEN_AI_REQUEST_TEMPERATURE: &str = "gen_ai.request.temperature";
pub const GEN_AI_REQUEST_TOP_K: &str = "gen_ai.request.top_k";
pub const GEN_AI_REQUEST_TOP_P: &str = "gen_ai.request.top_p";
pub const GEN_AI_RESPONSE_FINISH_REASONS: &str = "gen_ai.response.finish_reasons";
pub const GEN_AI_RESPONSE_ID: &str = "gen_ai.response.id";
pub const GEN_AI_RESPONSE_MODEL: &str = "gen_ai.response.model";
pub const GEN_AI_TOKEN_TYPE: &str = "gen_ai.token.type";
pub const GEN_AI_TOOL_CALL_ID: &str = "gen_ai.tool.call.id";
pub const GEN_AI_TOOL_DESCRIPTION: &str = "gen_ai.tool.description";
pub const GEN_AI_TOOL_NAME: &str = "gen_ai.tool.name";
pub const GEN_AI_TOOL_TYPE: &str = "gen_ai.tool.type";
pub const GEN_AI_TOOL_CALL_ARGUMENTS: &str = "gen_ai.tool.call.arguments";
pub const GEN_AI_TOOL_CALL_RESULT: &str = "gen_ai.tool.call.result";
pub const GEN_AI_USAGE_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
pub const GEN_AI_USAGE_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
pub const GEN_AI_USAGE_CACHE_CREATION_INPUT_TOKENS: &str =
    "gen_ai.usage.cache_creation.input_tokens";
pub const GEN_AI_USAGE_CACHE_READ_INPUT_TOKENS: &str = "gen_ai.usage.cache_read.input_tokens";
pub const GEN_AI_USAGE_REASONING_OUTPUT_TOKENS: &str = "gen_ai.usage.reasoning.output_tokens";

/// Values for the `gen_ai.operation.name` attribute.
pub mod operation_name {
    /// Agent invocation.
    pub const INVOKE_AGENT: &str = "invoke_agent";
    /// Chat completion.
    pub const CHAT: &str = "chat";
    /// Tool execution.
    pub const EXECUTE_TOOL: &str = "execute_tool";
}

/// Values for the `gen_ai.provider.name` attribute.
pub mod provider_name {
    pub const OPENAI: &str = "openai";
    pub const ANTHROPIC: &str = "anthropic";
    pub const GCP_GEMINI: &str = "gcp.gemini";
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key the Rust module exposes, in source order.
    const RUST_KEYS: [&str; 38] = [
        GEN_AI_INPUT_MESSAGES,
        GEN_AI_OUTPUT_MESSAGES,
        GEN_AI_TOOL_DEFINITIONS,
        GEN_AI_SYSTEM_INSTRUCTIONS,
        GEN_AI_AGENT_DESCRIPTION,
        GEN_AI_AGENT_ID,
        GEN_AI_AGENT_NAME,
        GEN_AI_CONVERSATION_ID,
        GEN_AI_DATA_SOURCE_ID,
        GEN_AI_OPERATION_NAME,
        GEN_AI_OUTPUT_TYPE,
        GEN_AI_PROVIDER_NAME,
        GEN_AI_REQUEST_CHOICE_COUNT,
        GEN_AI_REQUEST_ENCODING_FORMATS,
        GEN_AI_REQUEST_FREQUENCY_PENALTY,
        GEN_AI_REQUEST_MAX_TOKENS,
        GEN_AI_REQUEST_MODEL,
        GEN_AI_REQUEST_PRESENCE_PENALTY,
        GEN_AI_REQUEST_SEED,
        GEN_AI_REQUEST_STOP_SEQUENCES,
        GEN_AI_REQUEST_TEMPERATURE,
        GEN_AI_REQUEST_TOP_K,
        GEN_AI_REQUEST_TOP_P,
        GEN_AI_RESPONSE_FINISH_REASONS,
        GEN_AI_RESPONSE_ID,
        GEN_AI_RESPONSE_MODEL,
        GEN_AI_TOKEN_TYPE,
        GEN_AI_TOOL_CALL_ID,
        GEN_AI_TOOL_DESCRIPTION,
        GEN_AI_TOOL_NAME,
        GEN_AI_TOOL_TYPE,
        GEN_AI_TOOL_CALL_ARGUMENTS,
        GEN_AI_TOOL_CALL_RESULT,
        GEN_AI_USAGE_INPUT_TOKENS,
        GEN_AI_USAGE_OUTPUT_TOKENS,
        GEN_AI_USAGE_CACHE_CREATION_INPUT_TOKENS,
        GEN_AI_USAGE_CACHE_READ_INPUT_TOKENS,
        GEN_AI_USAGE_REASONING_OUTPUT_TOKENS,
    ];

    /// The same 38 keys written as string literals, transcribed from
    /// `GenAiIncubatingAttributes.java` (semantic conventions 1.41.0). Kept deliberately
    /// independent of [`RUST_KEYS`] so a typo on either side fails the cross-check.
    const JAVA_KEY_LITERALS: [&str; 38] = [
        "gen_ai.input.messages",
        "gen_ai.output.messages",
        "gen_ai.tool.definitions",
        "gen_ai.system_instructions",
        "gen_ai.agent.description",
        "gen_ai.agent.id",
        "gen_ai.agent.name",
        "gen_ai.conversation.id",
        "gen_ai.data_source.id",
        "gen_ai.operation.name",
        "gen_ai.output.type",
        "gen_ai.provider.name",
        "gen_ai.request.choice.count",
        "gen_ai.request.encoding_formats",
        "gen_ai.request.frequency_penalty",
        "gen_ai.request.max_tokens",
        "gen_ai.request.model",
        "gen_ai.request.presence_penalty",
        "gen_ai.request.seed",
        "gen_ai.request.stop_sequences",
        "gen_ai.request.temperature",
        "gen_ai.request.top_k",
        "gen_ai.request.top_p",
        "gen_ai.response.finish_reasons",
        "gen_ai.response.id",
        "gen_ai.response.model",
        "gen_ai.token.type",
        "gen_ai.tool.call.id",
        "gen_ai.tool.description",
        "gen_ai.tool.name",
        "gen_ai.tool.type",
        "gen_ai.tool.call.arguments",
        "gen_ai.tool.call.result",
        "gen_ai.usage.input_tokens",
        "gen_ai.usage.output_tokens",
        "gen_ai.usage.cache_creation.input_tokens",
        "gen_ai.usage.cache_read.input_tokens",
        "gen_ai.usage.reasoning.output_tokens",
    ];

    /// Tracker §6.4 P2-3: the 4 keys that used to be missing.
    #[test]
    fn the_previously_missing_keys_match_the_java_literals() {
        assert_eq!(GEN_AI_TOOL_TYPE, "gen_ai.tool.type");
        assert_eq!(
            GEN_AI_USAGE_CACHE_CREATION_INPUT_TOKENS,
            "gen_ai.usage.cache_creation.input_tokens"
        );
        assert_eq!(
            GEN_AI_USAGE_CACHE_READ_INPUT_TOKENS,
            "gen_ai.usage.cache_read.input_tokens"
        );
        assert_eq!(
            GEN_AI_USAGE_REASONING_OUTPUT_TOKENS,
            "gen_ai.usage.reasoning.output_tokens"
        );
    }

    #[test]
    fn the_key_set_matches_the_java_class_exactly() {
        let mut rust_keys = RUST_KEYS.to_vec();
        rust_keys.sort_unstable();
        rust_keys.dedup();
        assert_eq!(rust_keys.len(), RUST_KEYS.len(), "duplicate key in module");

        let mut java_keys = JAVA_KEY_LITERALS.to_vec();
        java_keys.sort_unstable();
        assert_eq!(rust_keys, java_keys);
    }
}
