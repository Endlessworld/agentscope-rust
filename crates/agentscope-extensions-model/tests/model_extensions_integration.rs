//! Cross-provider integration tests exercising the shared `common` layer and the
//! reverse response encoder through the crate's public API.
//!
//! These live outside `src/` so they validate the *exported* surface (not
//! `#[cfg(test)]` internals), and use a custom [`MediaResolver`] to prove the
//! pluggable seam works end to end.

use std::sync::Arc;

use agentscope_core::message::{ChatUsage, ContentBlock, JsonMap, TextBlock, ToolUseBlock};
use agentscope_core::model::ChatResponse;
use agentscope_core::Source;
use agentscope_extensions_model::{
    encode_error, encode_response, find_route, merge_non_zero, resolve_capabilities, resolve_media,
    set_media_resolver, strip_reasoning_suffix, ConverterQuality, MediaResolver, ModelError,
    RelayFormat, ResolvedMedia,
};

/// A resolver that fabricates bytes for any remote URL, proving the seam is
/// consulted instead of the default "remote URL rejected" behaviour.
struct StaticResolver;

impl MediaResolver for StaticResolver {
    fn resolve(
        &self,
        source: &Source,
        _purpose: &str,
    ) -> Result<ResolvedMedia, agentscope_core::Error> {
        match source {
            Source::Url(_) => Ok(ResolvedMedia {
                base64: "cmVtb3Rl".to_string(),
                mime_type: "image/png".to_string(),
            }),
            Source::Base64(b) => Ok(ResolvedMedia {
                base64: b.data.clone(),
                mime_type: b.media_type.clone(),
            }),
            Source::FileId(file) => Err(agentscope_core::Error::Other(format!(
                "provider file id '{}' has no inline bytes",
                file.file_id
            ))),
        }
    }
}

#[test]
fn registered_media_resolver_serves_remote_urls() {
    // Install once for this process; other tests here do not touch the global.
    let _ = set_media_resolver(Arc::new(StaticResolver));
    let source = Source::Url(agentscope_core::URLSource::new("https://example.com/a.png"));
    let resolved = resolve_media(&source, "integration test").expect("resolver");
    assert_eq!(resolved.base64, "cmVtb3Rl");
    assert_eq!(resolved.mime_type, "image/png");
}

#[test]
fn model_error_normalises_vendor_shapes() {
    // OpenAI nested object.
    let err = ModelError::parse(
        "openai",
        429,
        r#"{"error":{"message":"slow down","type":"rate_limit_error"}}"#,
    );
    assert!(err.retryable());
    assert_eq!(err.message, "slow down");

    // Alternative Chinese-gateway field.
    let err = ModelError::parse("glm", 500, r#"{"msg":"internal boom"}"#);
    assert_eq!(err.message, "internal boom");
    assert!(err.retryable());

    // Context-length failure is not retryable.
    let err = ModelError::parse(
        "openai",
        400,
        r#"{"error":{"message":"maximum context length exceeded"}}"#,
    );
    assert!(!err.retryable());
}

#[test]
fn reverse_encoder_covers_all_client_protocols() {
    let input: JsonMap = serde_json::from_str(r#"{"q":"hi"}"#).unwrap();
    let response = ChatResponse::new(
        "id-1".to_string(),
        vec![
            ContentBlock::Text(TextBlock::new("answer")),
            ContentBlock::ToolUse(ToolUseBlock::new("call_1", "search", input)),
        ],
        ChatUsage {
            input_tokens: 3,
            output_tokens: 2,
            cached_tokens: 0,
            time: 0.0,
            ..Default::default()
        },
        None,
        Some("tool_calls".to_string()),
    );

    let openai = encode_response(RelayFormat::OpenAiChat, &response, "gpt-4o");
    assert_eq!(
        openai["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "search"
    );

    let claude = encode_response(RelayFormat::ClaudeMessages, &response, "claude");
    assert_eq!(claude["content"][1]["type"], "tool_use");
    assert_eq!(claude["stop_reason"], "tool_use");

    let gemini = encode_response(RelayFormat::Gemini, &response, "gemini");
    assert_eq!(
        gemini["candidates"][0]["content"]["parts"][1]["functionCall"]["name"],
        "search"
    );

    let responses = encode_response(RelayFormat::OpenAiResponses, &response, "gpt-5");
    assert_eq!(responses["output"][1]["type"], "function_call");
}

#[test]
fn converter_registry_reports_multi_hop_quality() {
    let direct = find_route(RelayFormat::ClaudeMessages, RelayFormat::OpenAiChat).unwrap();
    assert_eq!(direct.quality, ConverterQuality::Good);

    let multi = find_route(RelayFormat::Gemini, RelayFormat::ClaudeMessages).unwrap();
    assert_eq!(multi.steps.len(), 3);
    assert_eq!(multi.quality, ConverterQuality::Discouraged);
    assert_eq!(
        encode_error(RelayFormat::OpenAiChat, "boom", "server_error")["error"]["message"],
        "boom"
    );
}

#[test]
fn capability_and_usage_and_naming_helpers_are_consistent() {
    // Reasoning models reject sampling params and prefer max_completion_tokens.
    assert!(!resolve_capabilities("o3-mini").supports_sampling);
    assert!(resolve_capabilities("o3-mini").use_max_completion_tokens);
    // Compatible gateways use the legacy field.
    assert!(!resolve_capabilities("deepseek-chat").use_max_completion_tokens);

    // Usage merges without a zero frame clobbering a positive value.
    let merged = merge_non_zero(
        &ChatUsage {
            input_tokens: 10,
            ..Default::default()
        },
        &ChatUsage {
            output_tokens: 5,
            ..Default::default()
        },
    );
    assert_eq!(merged.input_tokens, 10);
    assert_eq!(merged.output_tokens, 5);

    // Reasoning suffix parsing.
    let (base, hint) = strip_reasoning_suffix("claude-3-7-sonnet-thinking-2048");
    assert_eq!(base, "claude-3-7-sonnet");
    assert_eq!(hint.unwrap().budget_tokens, Some(2048));
}
