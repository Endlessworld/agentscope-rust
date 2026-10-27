//! Parses Gemini responses into AgentScope [`ChatResponse`]s.
//!
//! Mirrors `GeminiResponseParser`: the first candidate's parts become blocks in
//! `thinking → text → tool use` order, `usageMetadata` becomes [`ChatUsage`], and
//! `thoughtSignature` is persisted on the tool-use block's metadata so it can be
//! replayed on the next turn (Gemini rejects replayed tool calls without it).

use agentscope_core::{
    ChatResponse, ChatUsage, ContentBlock, JsonMap, TextBlock, ThinkingBlock, ToolCallState,
    ToolResultBlock, ToolResultState, ToolUseBlock,
};

use super::dto::{GeminiResponse, GeminiUsageMetadata};

/// Metadata key preserving the raw Gemini server-tool response payload.
pub const METADATA_SERVER_TOOL_RESPONSE: &str = "gemini.serverToolResponse";

/// Metadata key marking a block as produced by Gemini code execution.
const METADATA_CODE_EXECUTION: &str = "gemini.codeExecution";

/// Metadata key carrying the raw code-execution result.
const METADATA_CODE_EXECUTION_RESULT: &str = "gemini.codeExecutionResult";

/// Metadata key carrying the code-execution outcome (`OUTCOME_OK`, ...).
const METADATA_CODE_EXECUTION_OUTCOME: &str = "gemini.codeExecution.outcome";

/// Name AgentScope gives to the built-in Gemini code-execution tool.
const CODE_EXECUTION_NAME: &str = "CODE_EXECUTION";

/// Wraps the non-empty-`json` metadata insert used by the server-tool blocks.
fn insert_metadata(metadata: &mut JsonMap, key: &str, value: serde_json::Value) {
    metadata.insert(key.to_string(), value);
}

/// Builds the AgentScope usage from Gemini's `usageMetadata`.
///
/// Gemini reports `promptTokenCount` excluding tool-use prompt tokens, and
/// `candidatesTokenCount` excluding thoughts, so both are added back (mirroring
/// the Java parser's invariants).
fn usage_of(metadata: &GeminiUsageMetadata) -> ChatUsage {
    let input_tokens = metadata.prompt_token_count.unwrap_or(0)
        + metadata.tool_use_prompt_token_count.unwrap_or(0);
    let thinking_tokens = metadata.thoughts_token_count.unwrap_or(0);
    let output_tokens = match metadata.candidates_token_count {
        Some(candidates) => candidates + thinking_tokens,
        None => metadata
            .total_token_count
            .map(|total| total.saturating_sub(input_tokens))
            .unwrap_or(thinking_tokens),
    };
    ChatUsage {
        input_tokens,
        output_tokens,
        cached_tokens: metadata.cached_content_token_count.unwrap_or(0),
        cache_creation_tokens: 0,
        reasoning_tokens: thinking_tokens,
        tool_use_prompt_tokens: metadata.tool_use_prompt_token_count.unwrap_or(0),
        time: 0.0,
    }
}

/// Converts the parts of the first candidate into AgentScope blocks.
///
/// Order per part mirrors the Java parser: a `thought: true` text part becomes a
/// [`ThinkingBlock`]; otherwise text becomes a [`TextBlock`]; a `functionCall`
/// becomes a [`ToolUseBlock`]. A part may contribute both a text block and a tool
/// call, matching the reference implementation.
pub fn blocks_of(response: &GeminiResponse) -> Vec<ContentBlock> {
    let mut blocks: Vec<ContentBlock> = Vec::new();
    let Some(candidate) = response.candidates.first() else {
        return blocks;
    };
    let Some(content) = candidate.content.as_ref() else {
        return blocks;
    };

    let mut pending_code_execution_id: Option<String> = None;
    for part in &content.parts {
        if part.thought == Some(true) {
            if let Some(text) = part.text.as_ref().filter(|t| !t.is_empty()) {
                blocks.push(ContentBlock::Thinking(ThinkingBlock::new(text.clone())));
            }
            continue;
        }

        if let Some(text) = part.text.as_ref().filter(|t| !t.is_empty()) {
            blocks.push(ContentBlock::Text(TextBlock::new(text.clone())));
        }

        // Gemini code execution uses dedicated parts instead of toolCall/toolResponse.
        if let Some(code) = &part.executable_code {
            let id = code
                .id
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| pending_code_execution_id.clone())
                .unwrap_or_else(|| format!("code_execution_{}", uuid::Uuid::new_v4().simple()));
            let mut input = JsonMap::new();
            if let Some(language) = &code.language {
                input.insert("language".to_string(), serde_json::json!(language));
            }
            if let Some(source) = &code.code {
                input.insert("code".to_string(), serde_json::json!(source));
            }
            let mut tool = ToolUseBlock::new(id.clone(), CODE_EXECUTION_NAME, input.clone());
            if !input.is_empty() {
                tool.content = Some(serde_json::Value::Object(input).to_string());
            }
            tool.state = ToolCallState::Finished;
            insert_metadata(
                &mut tool.metadata,
                ToolUseBlock::METADATA_SERVER_TOOL,
                serde_json::json!(true),
            );
            insert_metadata(
                &mut tool.metadata,
                METADATA_CODE_EXECUTION,
                serde_json::json!(true),
            );
            blocks.push(ContentBlock::ToolUse(tool));
            pending_code_execution_id = Some(id);
        }

        if let Some(exec) = &part.code_execution_result {
            let id = exec
                .id
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| pending_code_execution_id.clone())
                .unwrap_or_else(|| format!("code_execution_{}", uuid::Uuid::new_v4().simple()));
            let state = match exec.outcome.as_deref() {
                Some("OUTCOME_FAILED") | Some("OUTCOME_DEADLINE_EXCEEDED") => {
                    ToolResultState::Error
                }
                _ => ToolResultState::Success,
            };
            let mut metadata = JsonMap::new();
            insert_metadata(
                &mut metadata,
                ToolResultBlock::METADATA_SERVER_TOOL,
                serde_json::json!(true),
            );
            insert_metadata(
                &mut metadata,
                METADATA_CODE_EXECUTION,
                serde_json::json!(true),
            );
            if let Ok(raw) = serde_json::to_value(exec) {
                insert_metadata(&mut metadata, METADATA_CODE_EXECUTION_RESULT, raw);
            }
            if let Some(outcome) = &exec.outcome {
                insert_metadata(
                    &mut metadata,
                    METADATA_CODE_EXECUTION_OUTCOME,
                    serde_json::json!(outcome),
                );
            }
            let output = exec
                .output
                .as_ref()
                .map(|o| vec![ContentBlock::Text(TextBlock::new(o.clone()))])
                .unwrap_or_default();
            blocks.push(ContentBlock::ToolResult(ToolResultBlock {
                id: Some(id),
                name: Some(CODE_EXECUTION_NAME.to_string()),
                output,
                metadata,
                state,
            }));
            pending_code_execution_id = None;
        }

        if let Some(call) = &part.function_call {
            if call.name.is_empty() {
                continue;
            }
            let raw = serde_json::Value::Object(call.args.clone()).to_string();
            let id = call
                .id
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("tool_call_{}", uuid::Uuid::new_v4().simple()));
            let mut tool = ToolUseBlock::new(id, call.name.clone(), call.args.clone());
            tool.content = Some(raw);
            if let Some(signature) = &part.thought_signature {
                let mut metadata = JsonMap::new();
                metadata.insert(
                    ToolUseBlock::METADATA_THOUGHT_SIGNATURE.to_string(),
                    serde_json::json!(signature),
                );
                tool.metadata = metadata;
            }
            blocks.push(ContentBlock::ToolUse(tool));
        }

        // Provider-executed (server) tool call, e.g. Google Search.
        if let Some(call) = &part.tool_call {
            let id = call
                .id
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("tool_call_{}", uuid::Uuid::new_v4().simple()));
            let name = call.tool_type.clone().unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let mut tool = ToolUseBlock::new(id, name, call.args.clone());
            if !call.args.is_empty() {
                tool.content = Some(serde_json::Value::Object(call.args.clone()).to_string());
            }
            tool.state = ToolCallState::Finished;
            insert_metadata(
                &mut tool.metadata,
                ToolUseBlock::METADATA_SERVER_TOOL,
                serde_json::json!(true),
            );
            if let Some(signature) = &part.thought_signature {
                insert_metadata(
                    &mut tool.metadata,
                    ToolUseBlock::METADATA_THOUGHT_SIGNATURE,
                    serde_json::json!(signature),
                );
            }
            blocks.push(ContentBlock::ToolUse(tool));
        }

        // Result of the matching server-tool call, inlined in the same message.
        if let Some(response) = &part.tool_response {
            let id = response
                .id
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("tool_call_{}", uuid::Uuid::new_v4().simple()));
            let name = response.tool_type.clone().unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let mut metadata = JsonMap::new();
            insert_metadata(
                &mut metadata,
                ToolResultBlock::METADATA_SERVER_TOOL,
                serde_json::json!(true),
            );
            insert_metadata(
                &mut metadata,
                METADATA_SERVER_TOOL_RESPONSE,
                response.response.clone().unwrap_or(serde_json::Value::Null),
            );
            let output = response
                .response
                .as_ref()
                .map(|r| vec![ContentBlock::Text(TextBlock::new(r.to_string()))])
                .unwrap_or_default();
            blocks.push(ContentBlock::ToolResult(ToolResultBlock {
                id: Some(id),
                name: Some(name),
                output,
                metadata,
                state: ToolResultState::Success,
            }));
        }
    }
    blocks
}

/// The finish reason reported by the first candidate.
pub fn finish_reason_of(response: &GeminiResponse) -> Option<String> {
    response
        .candidates
        .first()
        .and_then(|c| c.finish_reason.clone())
}

/// Parses a single (non-streaming or terminal) Gemini response.
pub fn parse_response(response: &GeminiResponse) -> ChatResponse {
    let model_version = response.model_version.clone();
    let id = response
        .response_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let usage = response
        .usage_metadata
        .as_ref()
        .map(usage_of)
        .unwrap_or_default();

    let mut metadata = JsonMap::new();
    if let Some(version) = model_version {
        metadata.insert("modelVersion".to_string(), serde_json::json!(version));
    }

    ChatResponse::new(
        id,
        blocks_of(response),
        usage,
        Some(metadata),
        finish_reason_of(response),
    )
}

/// Accumulates streamed Gemini chunks.
///
/// Gemini's `streamGenerateContent` emits cumulative-ish chunks: text parts
/// arrive as deltas and are yielded immediately; tool calls, usage and the
/// finish reason are buffered and flushed by [`Self::into_final`].
#[derive(Default)]
pub struct GeminiStreamIncrementer {
    id: Option<String>,
    tool_calls: Vec<ToolUseBlock>,
    usage: Option<ChatUsage>,
    finish_reason: Option<String>,
}

impl GeminiStreamIncrementer {
    /// Creates an empty incrementer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Consumes one chunk, returning an incremental response for new
    /// text/thinking content.
    pub fn accumulate(&mut self, response: &GeminiResponse) -> Option<ChatResponse> {
        if self.id.is_none() {
            self.id = response
                .response_id
                .clone()
                .filter(|s| !s.trim().is_empty());
        }
        if let Some(metadata) = &response.usage_metadata {
            let incoming = usage_of(metadata);
            self.usage = Some(crate::common::usage::merge_non_zero(
                &self.usage.unwrap_or_default(),
                &incoming,
            ));
        }
        if let Some(reason) = finish_reason_of(response) {
            self.finish_reason = Some(reason);
        }

        let mut blocks: Vec<ContentBlock> = Vec::new();
        for block in blocks_of(response) {
            match block {
                ContentBlock::ToolUse(tool) => self.tool_calls.push(tool),
                other => blocks.push(other),
            }
        }

        if blocks.is_empty() {
            None
        } else {
            Some(ChatResponse::new(
                self.id.clone().unwrap_or_default(),
                blocks,
                ChatUsage::default(),
                None,
                None,
            ))
        }
    }

    /// Flushes buffered tool calls / usage / finish reason at end of stream.
    pub fn into_final(self) -> Option<ChatResponse> {
        if self.tool_calls.is_empty() && self.usage.is_none() && self.finish_reason.is_none() {
            return None;
        }
        let content = self
            .tool_calls
            .into_iter()
            .map(ContentBlock::ToolUse)
            .collect();
        Some(ChatResponse::new(
            self.id.unwrap_or_default(),
            content,
            self.usage.unwrap_or_default(),
            None,
            self.finish_reason,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(value: serde_json::Value) -> GeminiResponse {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn parses_thinking_text_and_tool_call_in_order() {
        let parsed = parse_response(&response(serde_json::json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        { "thought": true, "text": "reasoning" },
                        { "text": "answer" },
                        { "functionCall": { "id": "call_1", "name": "get_weather", "args": { "city": "Paris" } } }
                    ]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 4,
                "thoughtsTokenCount": 2,
                "cachedContentTokenCount": 3,
                "toolUsePromptTokenCount": 1
            },
            "responseId": "resp_1",
            "modelVersion": "gemini-2.5-flash"
        })));
        assert_eq!(parsed.id, "resp_1");
        assert_eq!(parsed.usage.input_tokens, 11);
        assert_eq!(parsed.usage.output_tokens, 6);
        assert_eq!(parsed.usage.cached_tokens, 3);
        assert_eq!(parsed.finish_reason.as_deref(), Some("STOP"));
        assert!(
            matches!(&parsed.content[0], ContentBlock::Thinking(t) if t.thinking == "reasoning")
        );
        assert!(matches!(&parsed.content[1], ContentBlock::Text(t) if t.text == "answer"));
        match &parsed.content[2] {
            ContentBlock::ToolUse(t) => {
                assert_eq!(t.id, "call_1");
                assert_eq!(t.name, "get_weather");
                assert_eq!(t.input.get("city"), Some(&serde_json::json!("Paris")));
            }
            other => panic!("expected tool use, got {other:?}"),
        }
        assert_eq!(
            parsed.metadata.get("modelVersion"),
            Some(&serde_json::json!("gemini-2.5-flash"))
        );
    }

    #[test]
    fn thought_signature_is_persisted_on_the_tool_block() {
        let parsed = parse_response(&response(serde_json::json!({
            "candidates": [{ "content": { "role": "model", "parts": [{
                "functionCall": { "id": "call_1", "name": "f", "args": {} },
                "thoughtSignature": "sig-abc"
            }] } }]
        })));
        match &parsed.content[0] {
            ContentBlock::ToolUse(t) => {
                assert_eq!(
                    t.metadata.get(ToolUseBlock::METADATA_THOUGHT_SIGNATURE),
                    Some(&serde_json::json!("sig-abc"))
                );
            }
            other => panic!("expected tool use, got {other:?}"),
        }
    }

    #[test]
    fn server_tool_call_and_response_become_marked_blocks() {
        let parsed = parse_response(&response(serde_json::json!({
            "candidates": [{ "content": { "role": "model", "parts": [
                { "toolCall": { "id": "c1", "toolType": "GOOGLE_SEARCH_WEB", "args": { "query": "rust" } } },
                { "toolResponse": { "id": "c1", "toolType": "GOOGLE_SEARCH_WEB", "response": { "hits": 3 } } }
            ] } }]
        })));
        assert_eq!(parsed.content.len(), 2);
        match &parsed.content[0] {
            ContentBlock::ToolUse(t) => {
                assert_eq!(t.id, "c1");
                assert_eq!(t.name, "GOOGLE_SEARCH_WEB");
                assert!(t.is_server_tool());
                assert_eq!(t.state, ToolCallState::Finished);
            }
            other => panic!("expected server tool use, got {other:?}"),
        }
        match &parsed.content[1] {
            ContentBlock::ToolResult(r) => {
                assert_eq!(r.id.as_deref(), Some("c1"));
                assert!(r.is_server_tool());
                assert_eq!(r.state, ToolResultState::Success);
                assert!(r.metadata.contains_key(METADATA_SERVER_TOOL_RESPONSE));
            }
            other => panic!("expected server tool result, got {other:?}"),
        }
    }

    #[test]
    fn code_execution_parts_become_linked_server_tool_blocks() {
        let parsed = parse_response(&response(serde_json::json!({
            "candidates": [{ "content": { "role": "model", "parts": [
                { "executableCode": { "id": "e1", "language": "PYTHON", "code": "print(1)" } },
                { "codeExecutionResult": { "id": "e1", "outcome": "OUTCOME_OK", "output": "1\\n" } }
            ] } }]
        })));
        match &parsed.content[0] {
            ContentBlock::ToolUse(t) => {
                assert_eq!(t.name, CODE_EXECUTION_NAME);
                assert!(t.is_server_tool());
                assert_eq!(t.input.get("code"), Some(&serde_json::json!("print(1)")));
            }
            other => panic!("expected code-execution tool use, got {other:?}"),
        }
        match &parsed.content[1] {
            ContentBlock::ToolResult(r) => {
                assert_eq!(r.id.as_deref(), Some("e1"));
                assert_eq!(r.state, ToolResultState::Success);
                assert_eq!(
                    r.metadata.get(METADATA_CODE_EXECUTION_OUTCOME),
                    Some(&serde_json::json!("OUTCOME_OK"))
                );
            }
            other => panic!("expected code-execution result, got {other:?}"),
        }
    }

    #[test]
    fn failed_code_execution_maps_to_error_state() {
        let parsed = parse_response(&response(serde_json::json!({
            "candidates": [{ "content": { "parts": [
                { "codeExecutionResult": { "outcome": "OUTCOME_FAILED", "output": "boom" } }
            ] } }]
        })));
        match &parsed.content[0] {
            ContentBlock::ToolResult(r) => assert_eq!(r.state, ToolResultState::Error),
            other => panic!("expected code-execution result, got {other:?}"),
        }
    }

    #[test]
    fn missing_tool_call_id_is_synthesized() {
        let parsed = parse_response(&response(serde_json::json!({
            "candidates": [{ "content": { "parts": [{
                "functionCall": { "name": "f", "args": {} }
            }] } }]
        })));
        match &parsed.content[0] {
            ContentBlock::ToolUse(t) => assert!(t.id.starts_with("tool_call_")),
            other => panic!("expected tool use, got {other:?}"),
        }
    }

    #[test]
    fn empty_calls_and_parts_are_skipped() {
        let parsed = parse_response(&response(serde_json::json!({
            "candidates": [{ "content": { "parts": [
                { "text": "" },
                { "functionCall": { "name": "", "args": {} } }
            ] } }]
        })));
        assert!(parsed.content.is_empty());
    }

    #[test]
    fn usage_falls_back_to_total_when_candidates_absent() {
        let parsed = parse_response(&response(serde_json::json!({
            "usageMetadata": { "promptTokenCount": 7, "totalTokenCount": 20, "thoughtsTokenCount": 3 }
        })));
        assert_eq!(parsed.usage.input_tokens, 7);
        assert_eq!(parsed.usage.output_tokens, 13);
    }

    #[test]
    fn usage_defaults_to_zero_when_metadata_absent() {
        let parsed = parse_response(&response(serde_json::json!({ "candidates": [] })));
        assert_eq!(parsed.usage, ChatUsage::default());
        assert!(!parsed.id.is_empty());
    }

    #[test]
    fn stream_incrementer_emits_text_and_flushes_tools() {
        let mut inc = GeminiStreamIncrementer::new();
        let a = response(serde_json::json!({
            "responseId": "resp_9",
            "candidates": [{ "content": { "parts": [{ "text": "Hel" }] } }]
        }));
        let b = response(serde_json::json!({
            "candidates": [{ "content": { "parts": [{ "text": "lo" }] } }]
        }));
        let c = response(serde_json::json!({
            "candidates": [{ "content": { "parts": [{
                "functionCall": { "id": "call_1", "name": "f", "args": {} }
            }] }, "finishReason": "STOP" }],
            "usageMetadata": { "promptTokenCount": 2, "candidatesTokenCount": 3 }
        }));

        let first = inc.accumulate(&a).unwrap();
        assert_eq!(first.id, "resp_9");
        assert!(matches!(&first.content[0], ContentBlock::Text(t) if t.text == "Hel"));
        assert!(
            matches!(&inc.accumulate(&b).unwrap().content[0], ContentBlock::Text(t) if t.text == "lo")
        );
        // The tool-call chunk carries no incremental text.
        assert!(inc.accumulate(&c).is_none());

        let final_response = inc.into_final().unwrap();
        assert_eq!(final_response.finish_reason.as_deref(), Some("STOP"));
        assert_eq!(final_response.usage.output_tokens, 3);
        assert!(matches!(&final_response.content[0], ContentBlock::ToolUse(t) if t.name == "f"));
    }

    #[test]
    fn stream_incrementer_returns_none_when_nothing_to_flush() {
        let mut inc = GeminiStreamIncrementer::new();
        let chunk = response(serde_json::json!({
            "candidates": [{ "content": { "parts": [{ "text": "x" }] } }]
        }));
        assert!(inc.accumulate(&chunk).is_some());
        assert!(inc.into_final().is_none());
    }
}
