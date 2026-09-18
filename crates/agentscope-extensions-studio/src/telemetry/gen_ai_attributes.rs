//! OTel GenAI semantic-convention attribute keys (v1.37.0), mirroring
//! `GenAiIncubatingAttributes`.
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
pub const GEN_AI_TOOL_CALL_ARGUMENTS: &str = "gen_ai.tool.call.arguments";
pub const GEN_AI_TOOL_CALL_RESULT: &str = "gen_ai.tool.call.result";
pub const GEN_AI_USAGE_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
pub const GEN_AI_USAGE_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";

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
