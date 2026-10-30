//! Parses Responses API payloads into core [`ChatResponse`] values.
//!
//! Mirrors `ResponsesResponseParser` (non-streaming) and
//! `ResponsesStreamingAssembler` (SSE), plus `ResponsesHelper` (metadata and
//! usage extraction). The Rust adapter receives raw JSON from
//! [`crate::common::HttpTransport`], so items are inspected field-by-field
//! instead of through the official SDK's typed accessors.

use std::collections::HashMap;
use std::time::Instant;

use agentscope_core::error::{Error, Result};
use agentscope_core::message::{
    ChatUsage, ContentBlock, JsonMap, TextBlock, ThinkingBlock, ToolCallState, ToolUseBlock,
};
use agentscope_core::model::ChatResponse;
use serde_json::{json, Map, Value};

use super::constants::{
    MD_FUNCTION_CALL_NAMESPACE, MD_REASONING_ENCRYPTED_CONTENT, MD_REASONING_TEXT,
    MD_RESPONSE_CITATIONS, MD_RESPONSE_COMPLETED_AT, MD_RESPONSE_CREATED_AT, MD_RESPONSE_ERROR,
    MD_RESPONSE_ID, MD_RESPONSE_INCOMPLETE_REASON, MD_RESPONSE_STATUS,
};
use super::error::translate_optional_status;
use super::server_tool::{HostedToolSearchPairing, ResponsesServerToolHelper};
use crate::common::tool_search;

const FRAGMENT_PLACEHOLDER: &str = "__fragment__";

/// Parses a non-streaming Responses API `Response` object.
pub fn parse_response(
    response: &Value,
    model_name: &str,
    start_time: Instant,
) -> Result<ChatResponse> {
    let mut summary = String::new();
    let mut reasoning_text = String::new();
    let mut encrypted_content: Option<String> = None;
    let mut text = String::new();
    let mut tool_blocks: Vec<ContentBlock> = Vec::new();
    let mut pairing = HostedToolSearchPairing::default();

    for item in output_items(response) {
        // A client-executed tool search is a local call, not a provider tool:
        // it must be decoded first, otherwise the server-tool path would mark
        // it as provider-executed and the agent would never dispatch it.
        if let Some(block) = extract_client_tool_search(item) {
            tool_blocks.push(block);
            continue;
        }
        match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => {
                extract_reasoning(item, &mut summary, &mut reasoning_text);
                if encrypted_content.is_none() {
                    encrypted_content = item
                        .get("encrypted_content")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                }
            }
            Some("message") => extract_message(item, &mut text, model_name)?,
            Some("function_call") => {
                if let Some(block) = extract_function_call(item) {
                    tool_blocks.push(ContentBlock::ToolUse(block));
                }
            }
            _ => {
                if ResponsesServerToolHelper::is_supported_server_tool_item(item) {
                    let companion = pairing.companion_call_id(item);
                    tool_blocks.extend(ResponsesServerToolHelper::decode_blocks(
                        item,
                        companion.as_deref(),
                    ));
                }
            }
        }
    }

    let mut content = Vec::new();
    if !summary.is_empty() || encrypted_content.is_some() || !reasoning_text.is_empty() {
        let mut metadata = JsonMap::new();
        if let Some(encrypted) = &encrypted_content {
            metadata.insert(MD_REASONING_ENCRYPTED_CONTENT.to_string(), json!(encrypted));
        }
        if !reasoning_text.is_empty() {
            metadata.insert(MD_REASONING_TEXT.to_string(), json!(reasoning_text));
        }
        let mut thinking = ThinkingBlock::builder();
        if !summary.is_empty() {
            thinking = thinking.thinking(summary);
        }
        if !metadata.is_empty() {
            thinking = thinking.metadata(metadata);
        }
        content.push(ContentBlock::Thinking(thinking.build()));
    }
    if !text.is_empty() {
        content.push(ContentBlock::Text(TextBlock::new(text)));
    }
    content.extend(tool_blocks);

    let metadata = extract_response_metadata(response);
    let finish_reason = metadata
        .get(MD_RESPONSE_STATUS)
        .and_then(Value::as_str)
        .map(str::to_string);
    let usage = extract_usage(response, start_time).unwrap_or_default();
    let id = response
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    Ok(ChatResponse::new(
        id,
        content,
        usage,
        Some(metadata),
        finish_reason,
    ))
}

/// Decodes a **client-executed** `tool_search_call` output item.
///
/// Mirrors `ResponsesToolHelper#decodeClientToolSearch`: the call becomes a local
/// `tool_search` tool use flagged with
/// [`tool_search::METADATA_CLIENT_TOOL_SEARCH_CALL`], so the marshaller keeps it
/// out of the provider-executed branch and the transcript can be replayed. A
/// provider-executed (`execution: server`) call returns `None` and follows the
/// server-tool path.
fn extract_client_tool_search(item: &Value) -> Option<ContentBlock> {
    if item.get("type").and_then(Value::as_str) != Some("tool_search_call")
        || item.get("execution").and_then(Value::as_str) != Some("client")
    {
        return None;
    }
    let id = item
        .get("call_id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())?;
    let arguments = item.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let input = arguments.as_object().cloned().unwrap_or_default();
    Some(ContentBlock::ToolUse(
        ToolUseBlock::builder()
            .id(id)
            .name(tool_search::TOOL_SEARCH_TOOL_NAME)
            .input(input)
            .content(arguments.to_string())
            .metadata(tool_search::client_tool_search_metadata())
            .build(),
    ))
}

/// Extracts response-level metadata (`openai.response.*`).
pub fn extract_response_metadata(response: &Value) -> JsonMap {
    let mut metadata = JsonMap::new();
    if let Some(id) = response.get("id").and_then(Value::as_str) {
        metadata.insert(MD_RESPONSE_ID.to_string(), json!(id));
    }
    if let Some(status) = response.get("status").and_then(Value::as_str) {
        metadata.insert(MD_RESPONSE_STATUS.to_string(), json!(status));
    }
    if let Some(created) = response.get("created_at") {
        metadata.insert(MD_RESPONSE_CREATED_AT.to_string(), created.clone());
    }
    if let Some(completed) = response.get("completed_at") {
        metadata.insert(MD_RESPONSE_COMPLETED_AT.to_string(), completed.clone());
    }
    if let Some(reason) = response
        .get("incomplete_details")
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str)
    {
        metadata.insert(MD_RESPONSE_INCOMPLETE_REASON.to_string(), json!(reason));
    }
    if let Some(error) = response.get("error").filter(|e| !e.is_null()) {
        let mut error_map = Map::new();
        if let Some(message) = error.get("message").and_then(Value::as_str) {
            error_map.insert("message".to_string(), json!(message));
        }
        if let Some(code) = error.get("code").and_then(Value::as_str) {
            error_map.insert("code".to_string(), json!(code));
        }
        metadata.insert(MD_RESPONSE_ERROR.to_string(), Value::Object(error_map));
    }

    let citations = extract_citations(response);
    if !citations.is_empty() {
        metadata.insert(MD_RESPONSE_CITATIONS.to_string(), Value::Array(citations));
    }
    metadata
}

/// Extracts usage from a terminal response.
pub fn extract_usage(response: &Value, start_time: Instant) -> Option<ChatUsage> {
    let usage = response.get("usage").filter(|u| !u.is_null())?;
    let input_details = usage.get("input_tokens_details");
    let output_details = usage.get("output_tokens_details");
    let field = |details: Option<&Value>, key: &str| -> Option<u64> {
        details.and_then(|d| d.get(key)).and_then(Value::as_u64)
    };
    Some(ChatUsage {
        input_tokens: usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cached_tokens: field(input_details, "cached_tokens").unwrap_or(0),
        cache_creation_tokens: field(input_details, "cache_write_tokens").unwrap_or(0),
        reasoning_tokens: field(output_details, "reasoning_tokens").unwrap_or(0),
        time: start_time.elapsed().as_secs_f64(),
        ..Default::default()
    })
}

/// Streaming assembler for Responses API SSE events.
#[derive(Debug)]
pub struct ResponsesStreamingAssembler {
    model_name: String,
    start_time: Instant,
    item_registry: HashMap<String, ToolCallInfo>,
    reasoning_text: String,
    pairing: HostedToolSearchPairing,
}

#[derive(Debug, Clone)]
struct ToolCallInfo {
    call_id: String,
    name: String,
    namespace: Option<String>,
}

impl ResponsesStreamingAssembler {
    /// Creates an assembler for the given model.
    pub fn new(model_name: impl Into<String>, start_time: Instant) -> Self {
        Self {
            model_name: model_name.into(),
            start_time,
            item_registry: HashMap::new(),
            reasoning_text: String::new(),
            pairing: HostedToolSearchPairing::default(),
        }
    }

    /// Feeds one SSE event, returning zero or more incremental responses.
    pub fn process_event(&mut self, event: &Value) -> Result<Vec<ChatResponse>> {
        let mut results = Vec::new();
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_item.added") => {
                self.handle_output_item_added(event.get("item").unwrap_or(&Value::Null));
            }
            Some("response.output_item.done") => {
                let item = event.get("item").unwrap_or(&Value::Null);
                if let Some(block) = extract_client_tool_search(item) {
                    results.push(block_response(block));
                } else {
                    let companion = self.pairing.companion_call_id(item);
                    for block in ResponsesServerToolHelper::decode_blocks(item, companion.as_deref())
                    {
                        results.push(ChatResponse::new(
                            String::new(),
                            vec![block],
                            ChatUsage::default(),
                            None,
                            None,
                        ));
                    }
                }
            }
            Some("response.image_generation_call.partial_image") => {
                results.push(ChatResponse::new(
                    String::new(),
                    vec![ContentBlock::ToolResult(
                        ResponsesServerToolHelper::decode_partial_image(event),
                    )],
                    ChatUsage::default(),
                    None,
                    None,
                ));
            }
            Some("response.output_text.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    if !delta.is_empty() {
                        results.push(block_response(ContentBlock::Text(TextBlock::new(delta))));
                    }
                }
            }
            Some("response.reasoning_summary_text.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    if !delta.is_empty() {
                        results.push(block_response(ContentBlock::Thinking(ThinkingBlock::new(
                            delta,
                        ))));
                    }
                }
            }
            Some("response.reasoning_text.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    self.reasoning_text.push_str(delta);
                }
            }
            Some("response.function_call_arguments.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    if !delta.is_empty() {
                        results.push(block_response(self.arguments_fragment(event, delta)));
                    }
                }
            }
            Some("response.completed") | Some("response.incomplete") => {
                results.push(self.handle_terminal(event.get("response").unwrap_or(&Value::Null))?);
            }
            Some("response.failed") => {
                return Err(self.handle_failed(event.get("response").unwrap_or(&Value::Null)));
            }
            Some("error") => {
                return Err(handle_error_event(event));
            }
            _ => {}
        }
        Ok(results)
    }

    fn handle_output_item_added(&mut self, item: &Value) {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return;
        }
        let Some(item_id) = item.get("id").and_then(Value::as_str) else {
            tracing::warn!(
                "Function call output item missing item ID; argument deltas will use placeholder"
            );
            return;
        };
        let call_id = item
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
        if call_id.trim().is_empty() || name.trim().is_empty() {
            return;
        }
        self.item_registry.insert(
            item_id.to_string(),
            ToolCallInfo {
                call_id: call_id.to_string(),
                name: name.to_string(),
                namespace: item
                    .get("namespace")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
        );
    }

    fn arguments_fragment(&self, event: &Value, delta: &str) -> ContentBlock {
        let info = event
            .get("item_id")
            .and_then(Value::as_str)
            .and_then(|id| self.item_registry.get(id));
        let mut metadata = JsonMap::new();
        if let Some(namespace) = info.and_then(|i| i.namespace.as_deref()) {
            metadata.insert(MD_FUNCTION_CALL_NAMESPACE.to_string(), json!(namespace));
        }
        let mut block = ToolUseBlock::builder()
            .id(info.map(|i| i.call_id.clone()).unwrap_or_default())
            .name(
                info.map(|i| i.name.clone())
                    .unwrap_or_else(|| FRAGMENT_PLACEHOLDER.to_string()),
            )
            .input(JsonMap::new())
            .content(delta)
            .metadata(metadata)
            .state(ToolCallState::Pending)
            .build();
        block.content = Some(delta.to_string());
        ContentBlock::ToolUse(block)
    }

    fn handle_terminal(&self, response: &Value) -> Result<ChatResponse> {
        let refusal = extract_refusal(response);
        if !refusal.is_empty() {
            return Err(translate_optional_status(
                None,
                Some(&format!("Model response was refused: {refusal}")),
                None,
                None,
            )
            .to_core());
        }

        let mut encrypted_content: Option<String> = None;
        for item in output_items(response) {
            if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                if encrypted_content.is_none() {
                    encrypted_content = item
                        .get("encrypted_content")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                }
            }
        }

        let mut metadata = extract_response_metadata(response);
        // Keep the model name available to future refinements of error messages.
        let _ = &self.model_name;
        let finish_reason = metadata
            .get(MD_RESPONSE_STATUS)
            .and_then(Value::as_str)
            .map(str::to_string);
        let usage = extract_usage(response, self.start_time).unwrap_or_default();

        let mut content = Vec::new();
        if encrypted_content.is_some() || !self.reasoning_text.is_empty() {
            let mut thinking_metadata = JsonMap::new();
            if let Some(encrypted) = &encrypted_content {
                thinking_metadata
                    .insert(MD_REASONING_ENCRYPTED_CONTENT.to_string(), json!(encrypted));
            }
            if !self.reasoning_text.is_empty() {
                thinking_metadata.insert(MD_REASONING_TEXT.to_string(), json!(self.reasoning_text));
            }
            content.push(ContentBlock::Thinking(
                ThinkingBlock::builder().metadata(thinking_metadata).build(),
            ));
        }

        let id = response
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok(ChatResponse::new(
            id,
            content,
            usage,
            Some(std::mem::take(&mut metadata)),
            finish_reason,
        ))
    }

    fn handle_failed(&self, response: &Value) -> Error {
        let Some(error) = response.get("error").filter(|e| !e.is_null()) else {
            return translate_optional_status(None, Some("OpenAI API stream failed"), None, None)
                .to_core();
        };
        let mut message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("OpenAI API stream failed")
            .to_string();
        let code = error
            .get("code")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(code) = &code {
            message = format!("{message} (code: {code})");
        }
        translate_optional_status(None, Some(&message), None, code).to_core()
    }
}

fn handle_error_event(event: &Value) -> Error {
    let mut message = event
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("OpenAI API stream error")
        .to_string();
    let code = event
        .get("code")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(code) = &code {
        message = format!("{message} (code: {code})");
    }
    translate_optional_status(None, Some(&message), None, code).to_core()
}

fn block_response(block: ContentBlock) -> ChatResponse {
    ChatResponse::new(String::new(), vec![block], ChatUsage::default(), None, None)
}

fn output_items(response: &Value) -> impl Iterator<Item = &Value> {
    response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn extract_reasoning(item: &Value, summary: &mut String, reasoning_text: &mut String) {
    if let Some(parts) = item.get("summary").and_then(Value::as_array) {
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                summary.push_str(text);
            }
        }
    }
    if let Some(parts) = item.get("content").and_then(Value::as_array) {
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                reasoning_text.push_str(text);
            }
        }
    }
}

fn extract_message(item: &Value, text: &mut String, model_name: &str) -> Result<()> {
    let Some(parts) = item.get("content").and_then(Value::as_array) else {
        return Ok(());
    };
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("output_text") => {
                if let Some(value) = part.get("text").and_then(Value::as_str) {
                    text.push_str(value);
                }
            }
            Some("refusal") => {
                let refusal = part
                    .get("refusal")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                return Err(translate_optional_status(
                    None,
                    Some(&format!("Model response was refused: {refusal}")),
                    None,
                    None,
                )
                .to_core());
            }
            _ => {}
        }
        let _ = model_name;
    }
    Ok(())
}

fn extract_function_call(item: &Value) -> Option<ToolUseBlock> {
    let call_id = item.get("call_id").and_then(Value::as_str)?;
    let name = item.get("name").and_then(Value::as_str)?;
    if call_id.trim().is_empty() || name.trim().is_empty() {
        return None;
    }
    let arguments = item
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let input = serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let mut metadata = JsonMap::new();
    if let Some(namespace) = item
        .get("namespace")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        metadata.insert(MD_FUNCTION_CALL_NAMESPACE.to_string(), json!(namespace));
    }
    Some(
        ToolUseBlock::builder()
            .id(call_id)
            .name(name)
            .input(input)
            .content(arguments)
            .metadata(metadata)
            .state(ToolCallState::Finished)
            .build(),
    )
}

fn extract_refusal(response: &Value) -> String {
    let mut refusal = String::new();
    for item in output_items(response) {
        if item.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        if let Some(parts) = item.get("content").and_then(Value::as_array) {
            for part in parts {
                if part.get("type").and_then(Value::as_str) == Some("refusal") {
                    if let Some(value) = part.get("refusal").and_then(Value::as_str) {
                        refusal.push_str(value);
                    }
                }
            }
        }
    }
    refusal
}

fn extract_citations(response: &Value) -> Vec<Value> {
    let mut citations = Vec::new();
    for item in output_items(response) {
        if item.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(parts) = item.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in parts {
            if part.get("type").and_then(Value::as_str) != Some("output_text") {
                continue;
            }
            let Some(annotations) = part.get("annotations").and_then(Value::as_array) else {
                continue;
            };
            for annotation in annotations {
                match annotation.get("type").and_then(Value::as_str) {
                    Some("url_citation") => citations.push(json!({
                        "type": "url_citation",
                        "start_index": annotation.get("start_index").cloned().unwrap_or(Value::Null),
                        "end_index": annotation.get("end_index").cloned().unwrap_or(Value::Null),
                        "title": annotation.get("title").cloned().unwrap_or(Value::Null),
                        "url": annotation.get("url").cloned().unwrap_or(Value::Null),
                    })),
                    Some("container_file_citation") => citations.push(json!({
                        "type": "container_file_citation",
                        "container_id": annotation.get("container_id").cloned().unwrap_or(Value::Null),
                        "end_index": annotation.get("end_index").cloned().unwrap_or(Value::Null),
                        "file_id": annotation.get("file_id").cloned().unwrap_or(Value::Null),
                        "filename": annotation.get("filename").cloned().unwrap_or(Value::Null),
                        "start_index": annotation.get("start_index").cloned().unwrap_or(Value::Null),
                    })),
                    Some("file_citation") => citations.push(json!({
                        "type": "file_citation",
                        "file_id": annotation.get("file_id").cloned().unwrap_or(Value::Null),
                        "filename": annotation.get("filename").cloned().unwrap_or(Value::Null),
                        "index": annotation.get("index").cloned().unwrap_or(Value::Null),
                    })),
                    Some("file_path") => citations.push(json!({
                        "type": "file_path",
                        "file_id": annotation.get("file_id").cloned().unwrap_or(Value::Null),
                        "index": annotation.get("index").cloned().unwrap_or(Value::Null),
                    })),
                    _ => {}
                }
            }
        }
    }
    citations
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Instant {
        Instant::now()
    }

    #[test]
    fn non_streaming_response_assembles_thinking_text_and_tools() {
        let response = json!({
            "id": "resp_1",
            "status": "completed",
            "created_at": 1_700_000_000,
            "completed_at": 1_700_000_001,
            "output": [
                {
                    "type": "reasoning",
                    "summary": [{ "type": "summary_text", "text": "thought" }],
                    "encrypted_content": "enc"
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "hello", "annotations": [
                        { "type": "url_citation", "start_index": 0, "end_index": 5, "title": "T", "url": "https://e.test" }
                    ] }]
                },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "get_weather",
                    "arguments": "{\"city\":\"Beijing\"}"
                }
            ],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "input_tokens_details": { "cached_tokens": 4, "cache_write_tokens": 2 },
                "output_tokens_details": { "reasoning_tokens": 1 }
            }
        });
        let parsed = parse_response(&response, "gpt-5", now()).unwrap();
        assert_eq!(parsed.id, "resp_1");
        assert_eq!(parsed.finish_reason.as_deref(), Some("completed"));
        assert!(matches!(&parsed.content[0], ContentBlock::Thinking(t) if t.thinking == "thought"));
        assert!(matches!(&parsed.content[1], ContentBlock::Text(t) if t.text == "hello"));
        assert!(matches!(&parsed.content[2], ContentBlock::ToolUse(t) if t.id == "call_1"));
        assert_eq!(parsed.usage.input_tokens, 10);
        assert_eq!(parsed.usage.output_tokens, 5);
        assert_eq!(parsed.usage.cached_tokens, 4);
        assert_eq!(parsed.usage.cache_creation_tokens, 2);
        assert_eq!(parsed.usage.reasoning_tokens, 1);
        assert_eq!(parsed.metadata[MD_RESPONSE_STATUS], json!("completed"));
        assert_eq!(
            parsed.metadata[MD_RESPONSE_CREATED_AT],
            json!(1_700_000_000)
        );
        assert_eq!(
            parsed.metadata[MD_RESPONSE_CITATIONS][0]["type"],
            json!("url_citation")
        );
    }

    #[test]
    fn refusal_short_circuits_to_error() {
        let response = json!({
            "id": "resp_2",
            "status": "completed",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "refusal", "refusal": "not allowed" }]
            }]
        });
        let err = parse_response(&response, "gpt-5", now()).unwrap_err();
        assert!(err.to_string().contains("not allowed"));
    }

    #[test]
    fn streaming_text_and_reasoning_deltas_are_incremental() {
        let mut assembler = ResponsesStreamingAssembler::new("gpt-5", now());
        let out = assembler
            .process_event(&json!({ "type": "response.output_text.delta", "delta": "hel" }))
            .unwrap();
        assert!(matches!(&out[0].content[0], ContentBlock::Text(t) if t.text == "hel"));

        let out = assembler
            .process_event(&json!({
                "type": "response.reasoning_summary_text.delta",
                "delta": "think"
            }))
            .unwrap();
        assert!(matches!(&out[0].content[0], ContentBlock::Thinking(t) if t.thinking == "think"));
    }

    #[test]
    fn streaming_function_call_fragments_use_registered_call_info() {
        let mut assembler = ResponsesStreamingAssembler::new("gpt-5", now());
        assembler
            .process_event(&json!({
                "type": "response.output_item.added",
                "item": {
                    "type": "function_call",
                    "id": "item_1",
                    "call_id": "call_1",
                    "name": "get_weather",
                    "arguments": ""
                }
            }))
            .unwrap();
        let out = assembler
            .process_event(&json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "item_1",
                "delta": "{\"city\":"
            }))
            .unwrap();
        match &out[0].content[0] {
            ContentBlock::ToolUse(tool) => {
                assert_eq!(tool.id, "call_1");
                assert_eq!(tool.name, "get_weather");
                assert_eq!(tool.content.as_deref(), Some("{\"city\":"));
            }
            other => panic!("expected tool use, got {other:?}"),
        }
    }

    #[test]
    fn streaming_terminal_carries_usage_and_reasoning_replay_metadata() {
        let mut assembler = ResponsesStreamingAssembler::new("gpt-5", now());
        assembler
            .process_event(&json!({
                "type": "response.reasoning_text.delta",
                "delta": "internal"
            }))
            .unwrap();
        let out = assembler
            .process_event(&json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_3",
                    "status": "completed",
                    "output": [{
                        "type": "reasoning",
                        "summary": [],
                        "encrypted_content": "enc-3"
                    }],
                    "usage": { "input_tokens": 2, "output_tokens": 3 }
                }
            }))
            .unwrap();
        let terminal = &out[0];
        assert_eq!(terminal.id, "resp_3");
        assert_eq!(terminal.usage.input_tokens, 2);
        match &terminal.content[0] {
            ContentBlock::Thinking(t) => {
                let metadata = t.metadata.as_ref().unwrap();
                assert_eq!(metadata[MD_REASONING_ENCRYPTED_CONTENT], json!("enc-3"));
                assert_eq!(metadata[MD_REASONING_TEXT], json!("internal"));
            }
            other => panic!("expected thinking, got {other:?}"),
        }
    }

    #[test]
    fn streaming_failed_event_surfaces_error() {
        let mut assembler = ResponsesStreamingAssembler::new("gpt-5", now());
        let err = assembler
            .process_event(&json!({
                "type": "response.failed",
                "response": {
                    "id": "resp_4",
                    "status": "failed",
                    "error": { "message": "boom", "code": "server_error" }
                }
            }))
            .unwrap_err();
        assert!(err.to_string().contains("boom"));
        assert!(err.to_string().contains("server_error"));
    }

    #[test]
    fn streaming_server_tool_item_done_decodes_blocks() {
        let mut assembler = ResponsesStreamingAssembler::new("gpt-5", now());
        let out = assembler
            .process_event(&json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "web_search_call",
                    "id": "ws_1",
                    "status": "completed",
                    "action": { "type": "search", "query": "rust" },
                    "results": []
                }
            }))
            .unwrap();
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0].content[0], ContentBlock::ToolUse(t) if t.name == "web_search"));
        assert!(matches!(&out[1].content[0], ContentBlock::ToolResult(_)));
    }

    #[test]
    fn missing_usage_returns_none() {
        assert!(extract_usage(&json!({}), now()).is_none());
        let usage = extract_usage(
            &json!({ "usage": { "input_tokens": 1, "output_tokens": 2 } }),
            now(),
        )
        .unwrap();
        assert_eq!(usage.cached_tokens, 0);
        assert_eq!(usage.cache_creation_tokens, 0);
    }
}
