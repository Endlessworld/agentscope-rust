//! Extracts OpenTelemetry attributes from agent/model/tool/formatter
//! invocations, mirroring `AttributesExtractors`.
//!
//! Attributes are returned as an ordered `Vec<(String, serde_json::Value)>`
//! rather than OTel `Attributes`, so the extraction logic is testable without
//! depending on the `opentelemetry` crate (see tracker §4.2).
use agentscope_core::formatter::render_document_reference;
use agentscope_core::message::{ContentBlock, Msg, MsgRole};
use agentscope_core::model::{ChatResponse, GenerateOptions, ToolSchema};
use agentscope_core::tool::Toolkit;

use crate::telemetry::agent_scope_attributes as as_attrs;
use crate::telemetry::gen_ai_attributes as gen_ai;
use crate::telemetry::model::{
    InputMessage, MessagePart, OutputMessage, ReasoningPart, Role, TextPart, ToolCallRequestPart,
    ToolCallResponsePart, ToolDefinition,
};

/// A single extracted attribute (key, JSON-ish value).
pub type Attribute = (String, serde_json::Value);

/// Bundle of attributes extracted for a span.
pub type Attributes = Vec<Attribute>;

fn string_attr(out: &mut Attributes, key: &str, value: impl Into<String>) {
    out.push((key.to_string(), serde_json::Value::String(value.into())));
}

fn opt_string_attr(out: &mut Attributes, key: &str, value: Option<String>) {
    if let Some(v) = value {
        string_attr(out, key, v);
    }
}

// Retained as a helper for future attribute extractors that need to serialize a
// structured value; currently unused by the extractors migrated so far.
#[allow(dead_code)]
fn json_attr<T: serde::Serialize>(out: &mut Attributes, key: &str, value: &T) {
    if let Ok(v) = serde_json::to_value(value) {
        out.push((key.to_string(), v));
    }
}

/// Extracts the request attributes for an agent invocation.
pub fn get_agent_request_attributes(
    agent_id: &str,
    agent_name: &str,
    agent_description: Option<&str>,
    input_messages: &[Msg],
) -> Attributes {
    let mut attrs = Vec::new();
    string_attr(
        &mut attrs,
        gen_ai::GEN_AI_OPERATION_NAME,
        gen_ai::operation_name::INVOKE_AGENT,
    );
    string_attr(&mut attrs, gen_ai::GEN_AI_AGENT_ID, agent_id);
    string_attr(&mut attrs, gen_ai::GEN_AI_AGENT_NAME, agent_name);
    opt_string_attr(
        &mut attrs,
        gen_ai::GEN_AI_AGENT_DESCRIPTION,
        agent_description.map(|s| s.to_string()),
    );
    opt_string_attr(
        &mut attrs,
        gen_ai::GEN_AI_INPUT_MESSAGES,
        get_input_messages(input_messages),
    );
    let params = serde_json::json!({ "msgs": input_messages });
    string_attr(
        &mut attrs,
        as_attrs::AGENTSCOPE_FUNCTION_INPUT,
        params.to_string(),
    );
    attrs
}

/// Extracts the response attributes for an agent invocation.
pub fn get_agent_response_attributes(output_message: &Msg) -> Attributes {
    let mut attrs = Vec::new();
    opt_string_attr(
        &mut attrs,
        gen_ai::GEN_AI_OUTPUT_MESSAGES,
        get_output_messages_msg(output_message),
    );
    if let Ok(v) = serde_json::to_string(output_message) {
        string_attr(&mut attrs, as_attrs::AGENTSCOPE_FUNCTION_OUTPUT, v);
    }
    attrs
}

/// Extracts the request attributes for an LLM (chat) call.
pub fn get_llm_request_attributes(
    provider_name: &str,
    model_name: &str,
    input_messages: &[Msg],
    tool_schemas: &[ToolSchema],
    options: Option<&GenerateOptions>,
) -> Attributes {
    let mut attrs = Vec::new();
    string_attr(
        &mut attrs,
        gen_ai::GEN_AI_OPERATION_NAME,
        gen_ai::operation_name::CHAT,
    );
    string_attr(&mut attrs, gen_ai::GEN_AI_PROVIDER_NAME, provider_name);
    string_attr(&mut attrs, gen_ai::GEN_AI_REQUEST_MODEL, model_name);

    if let Some(options) = options {
        if let Some(v) = options.temperature {
            attrs.push((
                gen_ai::GEN_AI_REQUEST_TEMPERATURE.to_string(),
                serde_json::json!(v),
            ));
        }
        if let Some(v) = options.top_p {
            attrs.push((
                gen_ai::GEN_AI_REQUEST_TOP_P.to_string(),
                serde_json::json!(v),
            ));
        }
        if let Some(v) = options.top_k {
            attrs.push((
                gen_ai::GEN_AI_REQUEST_TOP_K.to_string(),
                serde_json::json!(v as f64),
            ));
        }
        if let Some(v) = options.max_tokens {
            attrs.push((
                gen_ai::GEN_AI_REQUEST_MAX_TOKENS.to_string(),
                serde_json::json!(v),
            ));
        }
        if let Some(v) = options.presence_penalty {
            attrs.push((
                gen_ai::GEN_AI_REQUEST_PRESENCE_PENALTY.to_string(),
                serde_json::json!(v),
            ));
        }
        if let Some(v) = options.frequency_penalty {
            attrs.push((
                gen_ai::GEN_AI_REQUEST_FREQUENCY_PENALTY.to_string(),
                serde_json::json!(v),
            ));
        }
        if let Some(v) = options.seed {
            attrs.push((
                gen_ai::GEN_AI_REQUEST_SEED.to_string(),
                serde_json::json!(v),
            ));
        }
    }

    opt_string_attr(
        &mut attrs,
        gen_ai::GEN_AI_INPUT_MESSAGES,
        get_input_messages(input_messages),
    );
    opt_string_attr(
        &mut attrs,
        gen_ai::GEN_AI_TOOL_DEFINITIONS,
        get_tool_definitions(tool_schemas),
    );

    // `GenerateOptions` lives in core and does not implement `Serialize`, so we
    // project the relevant knobs into a local JSON object instead of serializing
    // it directly (core must not be modified — see tracker §4.2).
    let options_json = options.map(|o| {
        serde_json::json!({
            "temperature": o.temperature,
            "top_p": o.top_p,
            "top_k": o.top_k,
            "max_tokens": o.max_tokens,
            "presence_penalty": o.presence_penalty,
            "frequency_penalty": o.frequency_penalty,
            "seed": o.seed,
        })
    });
    let params = serde_json::json!({
        "messages": input_messages,
        "tools": tool_schemas,
        "options": options_json,
    });
    string_attr(
        &mut attrs,
        as_attrs::AGENTSCOPE_FUNCTION_INPUT,
        params.to_string(),
    );
    attrs
}

/// Extracts the response attributes for an LLM (chat) call.
pub fn get_llm_response_attributes(response: Option<&ChatResponse>) -> Attributes {
    let mut attrs = Vec::new();
    if let Some(response) = response {
        if let Some(reason) = &response.finish_reason {
            attrs.push((
                gen_ai::GEN_AI_RESPONSE_FINISH_REASONS.to_string(),
                serde_json::json!([reason]),
            ));
        }
        string_attr(&mut attrs, gen_ai::GEN_AI_RESPONSE_ID, &response.id);
        attrs.push((
            gen_ai::GEN_AI_USAGE_INPUT_TOKENS.to_string(),
            serde_json::json!(response.usage.input_tokens),
        ));
        attrs.push((
            gen_ai::GEN_AI_USAGE_OUTPUT_TOKENS.to_string(),
            serde_json::json!(response.usage.output_tokens),
        ));
        // Java `AttributesExtractors.java:229-243` records every usage metric the response
        // carries, unconditionally (no zero filtering).
        attrs.push((
            gen_ai::GEN_AI_USAGE_CACHE_CREATION_INPUT_TOKENS.to_string(),
            serde_json::json!(response.usage.cache_creation_tokens),
        ));
        attrs.push((
            gen_ai::GEN_AI_USAGE_CACHE_READ_INPUT_TOKENS.to_string(),
            serde_json::json!(response.usage.cached_tokens),
        ));
        attrs.push((
            gen_ai::GEN_AI_USAGE_REASONING_OUTPUT_TOKENS.to_string(),
            serde_json::json!(response.usage.reasoning_tokens),
        ));
        attrs.push((
            as_attrs::AGENTSCOPE_USAGE_TOOL_USE_PROMPT_TOKENS.to_string(),
            serde_json::json!(response.usage.tool_use_prompt_tokens),
        ));
        opt_string_attr(
            &mut attrs,
            gen_ai::GEN_AI_OUTPUT_MESSAGES,
            get_output_messages_response(response),
        );
    }
    if let Ok(v) = serde_json::to_string(&response) {
        string_attr(&mut attrs, as_attrs::AGENTSCOPE_FUNCTION_OUTPUT, v);
    }
    attrs
}

/// Extracts the request attributes for a tool call.
pub fn get_tool_request_attributes(
    toolkit: Option<&Toolkit>,
    tool_id: &str,
    tool_name: &str,
    tool_input: &serde_json::Map<String, serde_json::Value>,
) -> Attributes {
    let mut attrs = Vec::new();
    string_attr(
        &mut attrs,
        gen_ai::GEN_AI_OPERATION_NAME,
        gen_ai::operation_name::EXECUTE_TOOL,
    );
    string_attr(&mut attrs, gen_ai::GEN_AI_TOOL_CALL_ID, tool_id);
    string_attr(&mut attrs, gen_ai::GEN_AI_TOOL_NAME, tool_name);
    if let Ok(v) = serde_json::to_string(tool_input) {
        string_attr(&mut attrs, gen_ai::GEN_AI_TOOL_CALL_ARGUMENTS, v);
    }
    if let Some(tool) = toolkit.and_then(|t| t.get_tool(tool_name)) {
        string_attr(
            &mut attrs,
            gen_ai::GEN_AI_TOOL_DESCRIPTION,
            tool.description(),
        );
    }

    let params =
        serde_json::json!({ "param": { "id": tool_id, "name": tool_name, "input": tool_input } });
    string_attr(
        &mut attrs,
        as_attrs::AGENTSCOPE_FUNCTION_INPUT,
        params.to_string(),
    );
    attrs
}

/// Extracts the response attributes for a tool call.
pub fn get_tool_response_attributes(output: &[ContentBlock]) -> Attributes {
    let mut attrs = Vec::new();
    if !output.is_empty() {
        if let Ok(v) = serde_json::to_string(&get_tool_call_result(output)) {
            string_attr(&mut attrs, gen_ai::GEN_AI_TOOL_CALL_RESULT, v);
        }
    }
    if let Ok(v) = serde_json::to_string(output) {
        string_attr(&mut attrs, as_attrs::AGENTSCOPE_FUNCTION_OUTPUT, v);
    }
    attrs
}

/// Extracts the request attributes for a formatter invocation.
///
/// `msgs` is the message list the formatter is about to convert. Java writes
/// `AGENTSCOPE_FUNCTION_INPUT` only when the caller has one
/// (`AttributesExtractors.java:320` `if (msgList != null)`); `None` is that branch, which is
/// what the framework `OnFormat` hook sees — it carries the requested format, not the
/// messages.
pub fn get_format_request_attributes(format_target: &str, msgs: Option<&[Msg]>) -> Attributes {
    let mut attrs = Vec::new();
    string_attr(
        &mut attrs,
        gen_ai::GEN_AI_OPERATION_NAME,
        as_attrs::operation_name::FORMAT,
    );
    string_attr(
        &mut attrs,
        as_attrs::AGENTSCOPE_FORMAT_TARGET,
        format_target,
    );
    if let Some(msgs) = msgs {
        if let Ok(v) = serde_json::to_string(&redact_message_payloads(msgs)) {
            string_attr(&mut attrs, as_attrs::AGENTSCOPE_FUNCTION_INPUT, v);
        }
    }
    attrs
}

/// Extracts the response attributes for a formatter invocation.
pub fn get_format_response_attributes<T: serde::Serialize>(result: &T) -> Attributes {
    let mut attrs = Vec::new();
    if let Ok(v) = serde_json::to_string(result) {
        string_attr(&mut attrs, as_attrs::AGENTSCOPE_FUNCTION_OUTPUT, v);
    }
    attrs
}

/// Returns the AgentScope function name (`"<Type>.<method>"`).
pub fn get_function_name(type_name: &str, method_name: &str) -> String {
    format!("{type_name}.{method_name}")
}

/// Builds common attributes (currently the conversation id when Studio is initialized).
pub fn get_common_attributes() -> Attributes {
    let mut attrs = Vec::new();
    if crate::studio::StudioManager::is_initialized() {
        if let Some(config) = crate::studio::StudioManager::config() {
            string_attr(&mut attrs, gen_ai::GEN_AI_CONVERSATION_ID, config.run_id());
        }
    }
    attrs
}

fn block_to_part(block: &ContentBlock) -> Option<MessagePart> {
    match block {
        ContentBlock::Text(tb) => Some(MessagePart::Text(TextPart::create(&tb.text))),
        ContentBlock::Thinking(tb) => {
            Some(MessagePart::Reasoning(ReasoningPart::create(&tb.thinking)))
        }
        ContentBlock::ToolUse(tub) => {
            let arguments = serde_json::Value::Object(tub.input.clone());
            Some(MessagePart::ToolCall(ToolCallRequestPart::create(
                &tub.id, &tub.name, arguments,
            )))
        }
        ContentBlock::ToolResult(trb) => {
            let response = serde_json::to_value(&trb.output).unwrap_or(serde_json::Value::Null);
            let id = trb.id.clone().unwrap_or_default();
            Some(MessagePart::ToolCallResponse(
                ToolCallResponsePart::create_with_id(id, response),
            ))
        }
        // Multi-modal content is not yet supported (mirrors Java TODO).
        _ => None,
    }
}

fn get_input_messages(messages: &[Msg]) -> Option<String> {
    if messages.is_empty() {
        return None;
    }
    let input_messages: Vec<InputMessage> = messages
        .iter()
        .map(|msg| {
            let parts: Vec<MessagePart> = msg.content().iter().filter_map(block_to_part).collect();
            InputMessage::from_role(
                role_of(msg.role()),
                parts,
                msg.name().map(|s| s.to_string()),
            )
        })
        .collect();
    serde_json::to_string(&input_messages).ok()
}

fn get_output_messages_msg(output_message: &Msg) -> Option<String> {
    let parts: Vec<MessagePart> = output_message
        .content()
        .iter()
        .filter_map(block_to_part)
        .collect();
    let output_messages = vec![OutputMessage::from_role(
        role_of(output_message.role()),
        parts,
        output_message.name().map(|s| s.to_string()),
        "stop",
    )];
    serde_json::to_string(&output_messages).ok()
}

fn get_output_messages_response(response: &ChatResponse) -> Option<String> {
    let parts: Vec<MessagePart> = response.content.iter().filter_map(block_to_part).collect();
    let output_messages = vec![OutputMessage::from_role(
        Role::Assistant,
        parts,
        None,
        response.finish_reason.clone().unwrap_or_default(),
    )];
    serde_json::to_string(&output_messages).ok()
}

fn get_tool_definitions(tool_schemas: &[ToolSchema]) -> Option<String> {
    if tool_schemas.is_empty() {
        return None;
    }
    let definitions: Vec<ToolDefinition> = tool_schemas
        .iter()
        .map(|schema| {
            ToolDefinition::create_full(
                "function",
                &schema.name,
                Some(schema.description.clone()),
                Some(schema.parameters.clone()),
            )
        })
        .collect();
    serde_json::to_string(&definitions).ok()
}

fn get_tool_call_result(result: &[ContentBlock]) -> Vec<ContentBlock> {
    redact_media_payloads(result)
}

/// Copies a message list with every media payload replaced by a description.
///
/// Span attributes are not a data channel: a base64 attachment would bloat or overflow the
/// attribute and leak content into telemetry backends. A message without media is passed
/// through untouched so the common case keeps its exact shape.
fn redact_message_payloads(msgs: &[Msg]) -> Vec<Msg> {
    if !msgs
        .iter()
        .any(|msg| msg.content().iter().any(is_media_payload))
    {
        return msgs.to_vec();
    }
    msgs.iter().map(redact_message_payload).collect()
}

fn redact_message_payload(msg: &Msg) -> Msg {
    if !msg.content().iter().any(is_media_payload) {
        return msg.clone();
    }
    let mut redacted = msg.clone();
    redacted.content = redact_media_payloads(msg.content());
    redacted
}

fn redact_media_payloads(blocks: &[ContentBlock]) -> Vec<ContentBlock> {
    blocks.iter().map(redact_media_payload).collect()
}

fn is_media_payload(block: &ContentBlock) -> bool {
    match block {
        // The payload often hides one level down: a tool result reporting a generated file
        // carries the document among its output blocks.
        ContentBlock::ToolResult(result) => result.output.iter().any(is_media_payload),
        ContentBlock::Video(_)
        | ContentBlock::Audio(_)
        | ContentBlock::Image(_)
        | ContentBlock::Document(_)
        | ContentBlock::Data(_) => true,
        _ => false,
    }
}

/// Media payloads have no place in a span attribute: they would bloat or overflow it, so
/// only a description is kept.
fn redact_media_payload(block: &ContentBlock) -> ContentBlock {
    match block {
        ContentBlock::ToolResult(result) => {
            if !result.output.iter().any(is_media_payload) {
                return block.clone();
            }
            let mut redacted = result.clone();
            redacted.output = redact_media_payloads(&result.output);
            ContentBlock::ToolResult(redacted)
        }
        ContentBlock::Video(_) => description("<VideoBlock is not supported>"),
        ContentBlock::Audio(_) => description("<AudioBlock is not supported>"),
        ContentBlock::Image(_) => description("<ImageBlock is not supported>"),
        // Keep the identity of the document (name, MIME type, location) without any
        // payload bytes.
        ContentBlock::Document(document) => description(&render_document_reference(document)),
        ContentBlock::Data(data) => description(&format!(
            "<DataBlock {} is not supported>",
            data.name.as_deref().unwrap_or(data.id.as_str())
        )),
        other => other.clone(),
    }
}

fn description(text: &str) -> ContentBlock {
    ContentBlock::Text(agentscope_core::message::TextBlock::new(text))
}

fn role_of(msg_role: MsgRole) -> Role {
    match msg_role {
        MsgRole::Assistant => Role::Assistant,
        MsgRole::Tool => Role::Tool,
        MsgRole::System => Role::System,
        MsgRole::User => Role::User,
    }
}

/// Maps a formatter simple class name to its provider name, mirroring
/// `AttributesExtractors.FormatterConverter`.
pub struct FormatterConverter;

impl FormatterConverter {
    /// Returns the provider target for a formatter class name.
    pub fn get_formatter_target(simple_class_name: &str) -> &'static str {
        match simple_class_name {
            "DashScopeChatFormatter"
            | "DashScopeMultiAgentFormatter"
            | "GeminiChatFormatter"
            | "GeminiMultiAgentFormatter"
            | "OpenAIChatFormatter"
            | "OpenAIMultiAgentFormatter" => as_attrs::provider_name::DASHSCOPE,
            "AnthropicChatFormatter" | "AnthropicMultiAgentFormatter" => {
                gen_ai::provider_name::ANTHROPIC
            }
            _ => "unknown",
        }
    }
}

/// Maps a model to its provider name, mirroring `ProviderNameConverter`.
///
/// Java uses runtime class-inheritance checks; Rust has no such reflection, so
/// this inspects a caller-supplied class/type identity string instead.
pub struct ProviderNameConverter;

impl ProviderNameConverter {
    /// Resolves a provider name from a fully-qualified model type name.
    pub fn get_provider_name(model_type_name: &str) -> &'static str {
        if model_type_name.ends_with("DashScopeChatModel") {
            as_attrs::provider_name::DASHSCOPE
        } else if model_type_name.ends_with("GeminiChatModel") {
            gen_ai::provider_name::GCP_GEMINI
        } else if model_type_name.ends_with("AnthropicChatModel") {
            gen_ai::provider_name::ANTHROPIC
        } else if model_type_name.ends_with("OpenAIChatModel") {
            gen_ai::provider_name::OPENAI
        } else {
            "unknown"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::{ChatUsage, TextBlock};

    fn attr<'a>(attrs: &'a Attributes, key: &str) -> Option<&'a serde_json::Value> {
        attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    fn attr_str<'a>(attrs: &'a Attributes, key: &str) -> &'a str {
        attr(attrs, key).unwrap().as_str().unwrap()
    }

    #[test]
    fn function_name_joins_type_and_method() {
        assert_eq!(get_function_name("ReActAgent", "reply"), "ReActAgent.reply");
    }

    #[test]
    fn common_attributes_empty_before_initialization() {
        assert!(get_common_attributes().is_empty());
    }

    #[test]
    fn llm_request_attributes_include_options() {
        let options = GenerateOptions::builder()
            .temperature(0.5)
            .top_p(0.9)
            .max_tokens(128)
            .seed(7)
            .build();
        let msgs = vec![Msg::user("hello")];
        let tools = vec![ToolSchema::new("calc", "calculator")];
        let attrs = get_llm_request_attributes("openai", "gpt-4o", &msgs, &tools, Some(&options));

        assert_eq!(attr_str(&attrs, gen_ai::GEN_AI_PROVIDER_NAME), "openai");
        assert_eq!(attr_str(&attrs, gen_ai::GEN_AI_REQUEST_MODEL), "gpt-4o");
        assert_eq!(
            attr_str(&attrs, gen_ai::GEN_AI_OPERATION_NAME),
            gen_ai::operation_name::CHAT
        );
        assert_eq!(
            attr(&attrs, gen_ai::GEN_AI_REQUEST_TEMPERATURE).unwrap(),
            &serde_json::json!(0.5)
        );
        assert_eq!(
            attr(&attrs, gen_ai::GEN_AI_REQUEST_MAX_TOKENS).unwrap(),
            &serde_json::json!(128)
        );
        assert_eq!(
            attr(&attrs, gen_ai::GEN_AI_REQUEST_SEED).unwrap(),
            &serde_json::json!(7)
        );

        // The AgentScope function input must be valid JSON that includes the
        // projected generation options (GenerateOptions is not Serialize itself).
        let raw = attr_str(&attrs, as_attrs::AGENTSCOPE_FUNCTION_INPUT);
        let parsed: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed["options"]["temperature"], 0.5);
        assert_eq!(parsed["options"]["max_tokens"], 128);
        assert_eq!(parsed["tools"][0]["name"], "calc");
    }

    /// Span attributes are not a data channel: an inline document must not carry its bytes
    /// into telemetry, but the reference stays (`85fa9a602`).
    #[test]
    fn format_request_replaces_an_inline_document_with_its_reference() {
        let mut msg = Msg::builder().role(MsgRole::User).build();
        msg.content.push(ContentBlock::Document(
            agentscope_core::message::DocumentBlock::builder()
                .source(agentscope_core::message::Source::Base64(
                    agentscope_core::message::Base64Source::new("application/pdf", "aGVsbG8="),
                ))
                .name("a.pdf")
                .build(),
        ));
        let attrs = get_format_request_attributes("DashScopeChatFormatter", Some(&[msg]));
        let raw = attr_str(&attrs, as_attrs::AGENTSCOPE_FUNCTION_INPUT);
        assert!(!raw.contains("aGVsbG8="), "payload leaked: {raw}");
        assert!(raw.contains("[document: a.pdf, application/pdf, inline base64 (~5 bytes)]"));
    }

    /// The redaction also reaches a document hiding in a tool result, one level down, and
    /// leaves a message without media byte-identical.
    #[test]
    fn format_request_redacts_a_tool_result_document_and_keeps_plain_messages() {
        let plain = Msg::user("hi");
        let mut with_tool = Msg::builder().role(MsgRole::Tool).build();
        with_tool.content.push(ContentBlock::ToolResult(
            agentscope_core::message::ToolResultBlock {
                id: Some("call-1".into()),
                name: Some("export".into()),
                output: vec![ContentBlock::Document(
                    agentscope_core::message::DocumentBlock::builder()
                        .source(agentscope_core::message::Source::Url(
                            agentscope_core::message::URLSource::new("https://example.com/r.pdf"),
                        ))
                        .name("r.pdf")
                        .build(),
                )],
                metadata: Default::default(),
                state: Default::default(),
            },
        ));

        let attrs = get_format_request_attributes("DashScopeChatFormatter", Some(&[with_tool]));
        let raw = attr_str(&attrs, as_attrs::AGENTSCOPE_FUNCTION_INPUT);
        assert!(raw.contains("[document: r.pdf, url: https://example.com/r.pdf]"));

        let attrs = get_format_request_attributes("DashScopeChatFormatter", Some(&[plain.clone()]));
        let raw = attr_str(&attrs, as_attrs::AGENTSCOPE_FUNCTION_INPUT);
        assert_eq!(raw, serde_json::to_string(&[plain]).unwrap());
    }

    #[test]
    fn tool_response_keeps_a_document_reference_and_names_a_data_block() {
        let output = vec![
            ContentBlock::Document(
                agentscope_core::message::DocumentBlock::builder()
                    .source(agentscope_core::message::Source::Url(
                        agentscope_core::message::URLSource::new("https://example.com/a.pdf"),
                    ))
                    .name("a.pdf")
                    .build(),
            ),
            ContentBlock::Data(
                agentscope_core::message::DataBlock::builder()
                    .source(agentscope_core::message::Source::Url(
                        agentscope_core::message::URLSource::new("https://example.com/b.bin"),
                    ))
                    .name("b.bin")
                    .build(),
            ),
        ];
        let attrs = get_tool_response_attributes(&output);
        let raw = attr_str(&attrs, gen_ai::GEN_AI_TOOL_CALL_RESULT);
        assert!(raw.contains("[document: a.pdf, url: https://example.com/a.pdf]"));
        assert!(raw.contains("<DataBlock b.bin is not supported>"));
    }

    #[test]
    fn llm_request_attributes_without_options() {
        let attrs = get_llm_request_attributes("anthropic", "claude", &[], &[], None);
        assert!(attr(&attrs, gen_ai::GEN_AI_REQUEST_TEMPERATURE).is_none());
        let raw = attr_str(&attrs, as_attrs::AGENTSCOPE_FUNCTION_INPUT);
        let parsed: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert!(parsed["options"].is_null());
    }

    #[test]
    fn agent_request_attributes_include_identity_and_input() {
        let msgs = vec![Msg::user("hi")];
        let attrs = get_agent_request_attributes("agent-1", "Bot", Some("desc"), &msgs);
        assert_eq!(attr_str(&attrs, gen_ai::GEN_AI_AGENT_ID), "agent-1");
        assert_eq!(attr_str(&attrs, gen_ai::GEN_AI_AGENT_NAME), "Bot");
        assert_eq!(attr_str(&attrs, gen_ai::GEN_AI_AGENT_DESCRIPTION), "desc");

        let input = attr_str(&attrs, gen_ai::GEN_AI_INPUT_MESSAGES);
        let parsed: serde_json::Value = serde_json::from_str(input).unwrap();
        assert_eq!(parsed[0]["role"], "user");
        assert_eq!(parsed[0]["parts"][0]["type"], "text");
        assert_eq!(parsed[0]["parts"][0]["content"], "hi");
    }

    #[test]
    fn llm_response_attributes_include_usage_and_finish_reason() {
        let response = ChatResponse::new(
            "resp-1",
            vec![ContentBlock::Text(TextBlock::new("ok"))],
            ChatUsage::builder()
                .input_tokens(10)
                .output_tokens(5)
                .build(),
            None,
            Some("stop".to_string()),
        );
        let attrs = get_llm_response_attributes(Some(&response));
        assert_eq!(attr_str(&attrs, gen_ai::GEN_AI_RESPONSE_ID), "resp-1");
        assert_eq!(
            attr(&attrs, gen_ai::GEN_AI_USAGE_INPUT_TOKENS).unwrap(),
            &serde_json::json!(10)
        );
        assert_eq!(
            attr(&attrs, gen_ai::GEN_AI_USAGE_OUTPUT_TOKENS).unwrap(),
            &serde_json::json!(5)
        );
        let reasons = attr(&attrs, gen_ai::GEN_AI_RESPONSE_FINISH_REASONS).unwrap();
        assert_eq!(reasons[0], "stop");

        // A null response yields no gen_ai.* attributes, but Java still records
        // the serialized (null) function output via Jackson.
        let empty = get_llm_response_attributes(None);
        assert!(attr(&empty, gen_ai::GEN_AI_RESPONSE_ID).is_none());
        assert_eq!(
            attr_str(&empty, as_attrs::AGENTSCOPE_FUNCTION_OUTPUT),
            "null"
        );
    }

    /// Tracker §6.4 P2-3: the four usage metrics core `ChatUsage` carries
    /// (`AttributesExtractors.java:229-243`), including the AgentScope-only one.
    #[test]
    fn llm_response_attributes_record_every_usage_metric() {
        let response = ChatResponse::new(
            "resp-2",
            vec![ContentBlock::Text(TextBlock::new("ok"))],
            ChatUsage::builder()
                .input_tokens(100)
                .output_tokens(20)
                .cache_creation_tokens(7)
                .cached_tokens(11)
                .reasoning_tokens(3)
                .tool_use_prompt_tokens(5)
                .build(),
            None,
            Some("stop".to_string()),
        );
        let attrs = get_llm_response_attributes(Some(&response));
        assert_eq!(
            attr(&attrs, gen_ai::GEN_AI_USAGE_CACHE_CREATION_INPUT_TOKENS).unwrap(),
            &serde_json::json!(7)
        );
        assert_eq!(
            attr(&attrs, gen_ai::GEN_AI_USAGE_CACHE_READ_INPUT_TOKENS).unwrap(),
            &serde_json::json!(11)
        );
        assert_eq!(
            attr(&attrs, gen_ai::GEN_AI_USAGE_REASONING_OUTPUT_TOKENS).unwrap(),
            &serde_json::json!(3)
        );
        assert_eq!(
            attr(&attrs, as_attrs::AGENTSCOPE_USAGE_TOOL_USE_PROMPT_TOKENS).unwrap(),
            &serde_json::json!(5)
        );
    }

    #[test]
    fn tool_request_attributes_include_call_identity() {
        let input = serde_json::json!({ "a": 1 }).as_object().unwrap().clone();
        let attrs = get_tool_request_attributes(None, "call-1", "calc", &input);
        assert_eq!(attr_str(&attrs, gen_ai::GEN_AI_TOOL_CALL_ID), "call-1");
        assert_eq!(attr_str(&attrs, gen_ai::GEN_AI_TOOL_NAME), "calc");
        assert_eq!(
            attr_str(&attrs, gen_ai::GEN_AI_OPERATION_NAME),
            gen_ai::operation_name::EXECUTE_TOOL
        );
    }

    #[test]
    fn converters_map_known_and_unknown_names() {
        assert_eq!(
            FormatterConverter::get_formatter_target("OpenAIChatFormatter"),
            "dashscope"
        );
        assert_eq!(
            FormatterConverter::get_formatter_target("AnthropicChatFormatter"),
            "anthropic"
        );
        assert_eq!(FormatterConverter::get_formatter_target("Nope"), "unknown");
        assert_eq!(
            ProviderNameConverter::get_provider_name("com.x.OpenAIChatModel"),
            "openai"
        );
        assert_eq!(
            ProviderNameConverter::get_provider_name("mystery"),
            "unknown"
        );
    }
}
