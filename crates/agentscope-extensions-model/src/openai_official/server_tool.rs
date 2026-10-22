//! Provider-executed (server-side) tools for the OpenAI Responses API.
//!
//! Mirrors `tool.OpenAIServerTool` + `tool.ResponsesServerToolHelper`:
//!
//! * [`OpenAIServerTool`] validates the provider-executed built-in tools
//!   (`web_search`, `code_interpreter`, `image_generation`, `tool_search`) and
//!   rejects custom functions / client-executed tool search.
//! * [`ResponsesServerToolHelper`] merges server tools into the request,
//!   computes the `include` list, decodes output items into
//!   [`ToolUseBlock`]/[`ToolResultBlock`] pairs and restores raw items from
//!   metadata for history replay.
//!
//! The Java implementation wraps the official SDK's typed `Tool`. The Rust
//! adapter speaks raw JSON, so the definitions are kept as
//! [`serde_json::Value`] — the same wire shape the SDK serializes.

use std::collections::VecDeque;

use agentscope_core::message::{
    Base64Source, ContentBlock, DataBlock, JsonMap, Source, TextBlock, ToolCallState,
    ToolResultBlock, ToolResultState, ToolUseBlock, URLSource,
};
use serde_json::{json, Value};

use super::constants::{
    MD_CODE_INTERPRETER_CONTAINER_ID, MD_FUNCTION_CALL_NAMESPACE,
    MD_RESPONSES_SERVER_TOOL_ITEM,
};

/// Stable tool type for the current web search tool.
pub const WEB_SEARCH: &str = "web_search";
/// Stable tool type for code interpreter.
pub const CODE_INTERPRETER: &str = "code_interpreter";
/// Stable tool type for image generation.
pub const IMAGE_GENERATION: &str = "image_generation";
/// Stable tool type for tool search.
pub const TOOL_SEARCH: &str = "tool_search";

/// Supported provider-executed tool kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAIServerToolKind {
    WebSearch,
    CodeInterpreter,
    ImageGeneration,
    ToolSearch,
}

impl OpenAIServerToolKind {
    /// The stable Responses API tool type.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::WebSearch => WEB_SEARCH,
            Self::CodeInterpreter => CODE_INTERPRETER,
            Self::ImageGeneration => IMAGE_GENERATION,
            Self::ToolSearch => TOOL_SEARCH,
        }
    }

    fn from_wire_type(value: &str) -> Option<Self> {
        match value {
            "web_search" | "web_search_preview" | "web_search_2025_08_26" => Some(Self::WebSearch),
            "code_interpreter" => Some(Self::CodeInterpreter),
            "image_generation" => Some(Self::ImageGeneration),
            "tool_search" => Some(Self::ToolSearch),
            _ => None,
        }
    }
}

/// A validated provider-executed OpenAI built-in tool.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenAIServerTool {
    tool: Value,
    kind: OpenAIServerToolKind,
}

impl OpenAIServerTool {
    /// Wraps an already-built tool definition.
    ///
    /// Rejects custom functions, namespaces, MCP, computer use, shell, apply
    /// patch and any other non-provider-executed tool, mirroring
    /// `OpenAIServerTool.of`.
    pub fn from_json(tool: Value) -> Result<Self, String> {
        if !tool.is_object() {
            return Err("OpenAI server tool must be a JSON object".to_string());
        }
        let wire_type = tool
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| "OpenAI server tool is missing a string 'type' field".to_string())?;
        let kind = OpenAIServerToolKind::from_wire_type(wire_type).ok_or_else(|| {
            "OpenAIServerTool supports provider-executed built-in tools only. Custom functions, \
             namespaces, MCP, computer use, shell, apply patch, and local runtime tools are not \
             accepted."
                .to_string()
        })?;
        if kind == OpenAIServerToolKind::ToolSearch
            && tool
                .get("execution")
                .and_then(Value::as_str)
                .map(|v| v.eq_ignore_ascii_case("client"))
                .unwrap_or(false)
        {
            return Err(
                "Client-executed tool_search is not a provider-executed server tool.".to_string(),
            );
        }
        Ok(Self { tool, kind })
    }

    /// Creates a `web_search` server tool.
    pub fn web_search() -> Self {
        Self {
            tool: json!({ "type": WEB_SEARCH }),
            kind: OpenAIServerToolKind::WebSearch,
        }
    }

    /// Creates a `code_interpreter` server tool.
    pub fn code_interpreter() -> Self {
        Self {
            tool: json!({ "type": CODE_INTERPRETER, "container": { "type": "auto" } }),
            kind: OpenAIServerToolKind::CodeInterpreter,
        }
    }

    /// Creates an `image_generation` server tool.
    pub fn image_generation() -> Self {
        Self {
            tool: json!({ "type": IMAGE_GENERATION }),
            kind: OpenAIServerToolKind::ImageGeneration,
        }
    }

    /// Creates a hosted (`execution: server`) `tool_search` server tool.
    pub fn tool_search() -> Self {
        Self {
            tool: json!({ "type": TOOL_SEARCH, "execution": "server" }),
            kind: OpenAIServerToolKind::ToolSearch,
        }
    }

    /// Creates a server tool from a kind using a minimal definition.
    pub fn of_kind(kind: OpenAIServerToolKind) -> Self {
        match kind {
            OpenAIServerToolKind::WebSearch => Self::web_search(),
            OpenAIServerToolKind::CodeInterpreter => Self::code_interpreter(),
            OpenAIServerToolKind::ImageGeneration => Self::image_generation(),
            OpenAIServerToolKind::ToolSearch => Self::tool_search(),
        }
    }

    /// The stable Responses API tool type.
    pub fn kind(&self) -> OpenAIServerToolKind {
        self.kind
    }

    /// The stable Responses API tool type string.
    pub fn tool_type(&self) -> &'static str {
        self.kind.as_str()
    }

    /// Returns the underlying wire definition.
    pub fn to_json(&self) -> &Value {
        &self.tool
    }
}

/// Shared mapping/decoding logic for provider-executed Responses tools.
pub struct ResponsesServerToolHelper;

impl ResponsesServerToolHelper {
    /// Whether an output item is a supported server-tool item.
    pub fn is_supported_server_tool_item(item: &Value) -> bool {
        is_server_tool_call(item) || item_type(item) == Some("tool_search_output")
    }

    /// Whether the call item embeds its own result.
    pub fn has_embedded_server_tool_result(item: &Value) -> bool {
        matches!(
            item_type(item),
            Some("web_search_call" | "code_interpreter_call" | "image_generation_call")
        )
    }

    /// Merges client function tools followed by server tools.
    pub fn merge_tools(client_tools: Vec<Value>, server_tools: &[OpenAIServerTool]) -> Vec<Value> {
        let mut merged = client_tools;
        merged.extend(server_tools.iter().map(|t| t.tool.clone()));
        merged
    }

    /// Adds the `include` list required by web search / code interpreter.
    ///
    /// Writes the list onto the request body when non-empty.
    pub fn apply_includes(body: &mut Value, server_tools: &[OpenAIServerTool]) {
        if server_tools.is_empty() {
            return;
        }
        let mut includes: Vec<&str> = Vec::new();
        for tool in server_tools {
            match tool.kind() {
                OpenAIServerToolKind::WebSearch => {
                    push_unique(&mut includes, "web_search_call.results");
                    push_unique(&mut includes, "web_search_call.action.sources");
                }
                OpenAIServerToolKind::CodeInterpreter => {
                    push_unique(&mut includes, "code_interpreter_call.outputs");
                }
                _ => {}
            }
        }
        if !includes.is_empty() {
            if let Some(map) = body.as_object_mut() {
                map.insert(
                    "include".to_string(),
                    Value::Array(includes.into_iter().map(|s| json!(s)).collect()),
                );
            }
        }
    }

    /// Decodes a server-tool output item into call/result blocks.
    pub fn decode_blocks(item: &Value, result_id_override: Option<&str>) -> Vec<ContentBlock> {
        let mut blocks = Vec::new();
        if let Some(call) = Self::decode_call_block(item) {
            blocks.push(ContentBlock::ToolUse(call));
        }
        if let Some(result) = Self::decode_result_block(item, result_id_override) {
            blocks.push(ContentBlock::ToolResult(result));
        }
        blocks
    }

    /// Decodes a server-tool call from an output item.
    pub fn decode_call_block(item: &Value) -> Option<ToolUseBlock> {
        if !is_server_tool_call(item) {
            return None;
        }
        let tool_type = server_tool_call_type(item)?;
        let id = call_id(item)?;
        if id.trim().is_empty() {
            return None;
        }
        let input_node = input_node(item, tool_type);
        let input = input_map(input_node);
        let content = input_node
            .map(|n| n.to_string())
            .unwrap_or_else(|| "{}".to_string());

        let mut metadata = JsonMap::new();
        metadata.insert(
            ToolUseBlock::METADATA_SERVER_TOOL.to_string(),
            Value::Bool(true),
        );
        if !Self::has_embedded_server_tool_result(item) {
            metadata.insert(
                MD_RESPONSES_SERVER_TOOL_ITEM.to_string(),
                Value::String(item.to_string()),
            );
        }

        Some(
            ToolUseBlock::builder()
                .id(id)
                .name(tool_type)
                .input(input)
                .content(content)
                .metadata(metadata)
                .state(ToolCallState::Finished)
                .build(),
        )
    }

    /// Decodes a server-tool result from an output item.
    pub fn decode_result_block(
        item: &Value,
        result_id_override: Option<&str>,
    ) -> Option<ToolResultBlock> {
        if !Self::is_supported_server_tool_item(item) {
            return None;
        }
        // Call items whose result is emitted separately have no result here.
        if is_server_tool_call(item) && !Self::has_embedded_server_tool_result(item) {
            return None;
        }

        if item_type(item) == Some("code_interpreter_call") {
            return decode_code_interpreter_result(item);
        }

        let id = result_id_override
            .map(str::to_string)
            .or_else(|| call_id(item))?;
        if id.trim().is_empty() {
            return None;
        }
        let tool_type = server_tool_call_type(item)?;
        let status = item.get("status").and_then(Value::as_str);
        let result_node = result_node(item, tool_type);
        let output = output_blocks(result_node, tool_type, item, status);

        let mut metadata = JsonMap::new();
        metadata.insert(
            ToolResultBlock::METADATA_SERVER_TOOL.to_string(),
            Value::Bool(true),
        );
        metadata.insert(
            MD_RESPONSES_SERVER_TOOL_ITEM.to_string(),
            Value::String(item.to_string()),
        );

        Some(
            ToolResultBlock::builder()
                .id(id)
                .name(tool_type)
                .outputs(output)
                .metadata(metadata)
                .state(result_state(status))
                .build(),
        )
    }

    /// Decodes a streaming `image_generation_call.partial_image` event.
    pub fn decode_partial_image(event: &Value) -> ToolResultBlock {
        let item_id = event
            .get("item_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let index = event
            .get("partial_image_index")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let format = event
            .get("output_format")
            .and_then(Value::as_str)
            .unwrap_or("png");
        let data = event
            .get("partial_image_b64")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let image = DataBlock::builder()
            .source(Source::Base64(Base64Source::new(
                image_media_type(format),
                data,
            )))
            .id(format!("{item_id}-partial-{index}"))
            .name(format!("partial-image-{index}"))
            .build();
        ToolResultBlock::builder()
            .id(item_id)
            .name(IMAGE_GENERATION)
            .outputs(vec![ContentBlock::Data(image)])
            .metadata({
                let mut m = JsonMap::new();
                m.insert(
                    ToolResultBlock::METADATA_SERVER_TOOL.to_string(),
                    Value::Bool(true),
                );
                m
            })
            .state(ToolResultState::Running)
            .build()
    }

    /// Restores the raw Responses input item stored on a server-tool block.
    pub fn restore_server_tool_item(metadata: &JsonMap) -> Option<Value> {
        let raw = metadata.get(MD_RESPONSES_SERVER_TOOL_ITEM)?;
        let json = raw.as_str()?;
        if json.trim().is_empty() {
            return None;
        }
        serde_json::from_str(json).ok()
    }
}

/// Pairs hosted `tool_search` output items with their preceding call items.
#[derive(Debug, Default)]
pub struct HostedToolSearchPairing {
    unmatched_call_ids: VecDeque<String>,
}

impl HostedToolSearchPairing {
    /// Records a hosted `tool_search_call`, or resolves the call id of its output.
    pub fn companion_call_id(&mut self, item: &Value) -> Option<String> {
        let item_type = item_type(item)?;
        if item_type != "tool_search_call" && item_type != "tool_search_output" {
            return None;
        }
        if !item
            .get("execution")
            .and_then(Value::as_str)
            .map(|v| v.eq_ignore_ascii_case("server"))
            .unwrap_or(false)
        {
            return None;
        }
        if item_type == "tool_search_call" {
            if let Some(id) = call_id(item) {
                if !id.trim().is_empty() {
                    self.unmatched_call_ids.push_back(id);
                }
            }
            return None;
        }
        self.unmatched_call_ids.pop_front()
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

fn item_type(item: &Value) -> Option<&str> {
    item.get("type").and_then(Value::as_str)
}

fn is_server_tool_call(item: &Value) -> bool {
    matches!(
        item_type(item),
        Some(
            "web_search_call"
                | "code_interpreter_call"
                | "image_generation_call"
                | "tool_search_call"
        )
    )
}

fn server_tool_call_type(item: &Value) -> Option<&'static str> {
    match item_type(item)? {
        "web_search_call" => Some(WEB_SEARCH),
        "code_interpreter_call" => Some(CODE_INTERPRETER),
        "image_generation_call" => Some(IMAGE_GENERATION),
        "tool_search_call" | "tool_search_output" => Some(TOOL_SEARCH),
        _ => None,
    }
}

fn call_id(item: &Value) -> Option<String> {
    item.get("call_id")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| item.get("id").and_then(Value::as_str))
        .map(str::to_string)
}

fn input_node<'a>(item: &'a Value, tool_type: &str) -> Option<&'a Value> {
    match tool_type {
        WEB_SEARCH => item.get("action"),
        CODE_INTERPRETER => item.get("code"),
        TOOL_SEARCH => item.get("arguments"),
        _ => None,
    }
}

fn result_node<'a>(item: &'a Value, tool_type: &str) -> Option<&'a Value> {
    match tool_type {
        WEB_SEARCH => item.get("results").or_else(|| item.get("action")),
        CODE_INTERPRETER => item.get("outputs"),
        IMAGE_GENERATION => item.get("result"),
        TOOL_SEARCH => item.get("tools"),
        _ => None,
    }
}

fn input_map(node: Option<&Value>) -> JsonMap {
    match node {
        None | Some(Value::Null) => JsonMap::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(Value::Array(values)) => {
            let mut map = JsonMap::new();
            map.insert("value".to_string(), Value::Array(values.clone()));
            map
        }
        Some(other) => {
            let mut map = JsonMap::new();
            map.insert("value".to_string(), other.clone());
            map
        }
    }
}

fn output_blocks(
    result: Option<&Value>,
    tool_type: &str,
    root: &Value,
    status: Option<&str>,
) -> Vec<ContentBlock> {
    if tool_type == IMAGE_GENERATION {
        if let Some(Value::String(data)) = result {
            if !data.is_empty() {
                let image = DataBlock::builder()
                    .source(Source::Base64(Base64Source::new(
                        image_media_type(
                            root.get("output_format")
                                .and_then(Value::as_str)
                                .unwrap_or("png"),
                        ),
                        data,
                    )))
                    .name(IMAGE_GENERATION)
                    .build();
                return vec![ContentBlock::Data(image)];
            }
        }
    }
    vec![ContentBlock::Text(TextBlock::new(result_text(
        result, status,
    )))]
}

fn result_text(result: Option<&Value>, status: Option<&str>) -> String {
    match result {
        Some(Value::Null) | None => status
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("{}")
            .to_string(),
        Some(value) => value.to_string(),
    }
}

fn decode_code_interpreter_result(item: &Value) -> Option<ToolResultBlock> {
    let id = call_id(item)?;
    let outputs = item
        .get("outputs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut output = Vec::new();
    for (index, entry) in outputs.iter().enumerate() {
        match entry.get("type").and_then(Value::as_str) {
            Some("logs") => {
                let logs = entry.get("logs").and_then(Value::as_str).unwrap_or_default();
                output.push(ContentBlock::Text(TextBlock::new(logs)));
            }
            Some("image") => {
                if let Some(url) = entry.get("url").and_then(Value::as_str) {
                    let image = DataBlock::builder()
                        .source(Source::Url(URLSource {
                            url: url.to_string(),
                            mime_type: image_media_type_from_url(url),
                        }))
                        .id(format!("{id}-image-{index}"))
                        .name("code-interpreter-image")
                        .build();
                    output.push(ContentBlock::Data(image));
                }
            }
            _ => {}
        }
    }
    let status = item.get("status").and_then(Value::as_str);
    if output.is_empty() {
        output.push(ContentBlock::Text(TextBlock::new(
            status.unwrap_or("completed"),
        )));
    }

    let mut metadata = JsonMap::new();
    metadata.insert(
        ToolResultBlock::METADATA_SERVER_TOOL.to_string(),
        Value::Bool(true),
    );
    metadata.insert(
        MD_RESPONSES_SERVER_TOOL_ITEM.to_string(),
        Value::String(item.to_string()),
    );
    if let Some(container) = item.get("container_id").and_then(Value::as_str) {
        metadata.insert(
            MD_CODE_INTERPRETER_CONTAINER_ID.to_string(),
            Value::String(container.to_string()),
        );
    }

    Some(
        ToolResultBlock::builder()
            .id(id)
            .name(CODE_INTERPRETER)
            .outputs(output)
            .metadata(metadata)
            .state(result_state(status))
            .build(),
    )
}

fn result_state(status: Option<&str>) -> ToolResultState {
    match status.map(|s| s.to_ascii_lowercase()) {
        Some(s) if s == "failed" => ToolResultState::Error,
        Some(s) if s == "in_progress" || s == "incomplete" => ToolResultState::Running,
        _ => ToolResultState::Success,
    }
}

fn image_media_type(output_format: &str) -> String {
    match output_format.trim().to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg".to_string(),
        "webp" => "image/webp".to_string(),
        "gif" => "image/gif".to_string(),
        "" => "image/png".to_string(),
        other => format!("image/{other}"),
    }
}

fn image_media_type_from_url(url: &str) -> Option<String> {
    let lower = url.to_ascii_lowercase();
    let path = lower.split('?').next().unwrap_or(&lower);
    if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        Some("image/jpeg".to_string())
    } else if path.ends_with(".webp") {
        Some("image/webp".to_string())
    } else if path.ends_with(".gif") {
        Some("image/gif".to_string())
    } else if path.ends_with(".png") {
        Some("image/png".to_string())
    } else {
        None
    }
}

fn push_unique<'a>(values: &mut Vec<&'a str>, value: &'a str) {
    if !values.contains(&value) {
        values.push(value);
    }
}

// Keep the namespace key referenced so metadata writes stay in sync with the
// Java constant (function-call namespaces are emitted by the response parser).
#[allow(dead_code)]
fn namespace_metadata_key() -> &'static str {
    MD_FUNCTION_CALL_NAMESPACE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fn_tool(name: &str) -> Value {
        json!({
            "type": "function",
            "name": name,
            "parameters": { "type": "object", "properties": {} }
        })
    }

    #[test]
    fn server_tool_accepts_builtin_types_and_rejects_others() {
        for (wire, kind) in [
            ("web_search", OpenAIServerToolKind::WebSearch),
            ("code_interpreter", OpenAIServerToolKind::CodeInterpreter),
            ("image_generation", OpenAIServerToolKind::ImageGeneration),
            ("tool_search", OpenAIServerToolKind::ToolSearch),
        ] {
            let tool = OpenAIServerTool::from_json(json!({ "type": wire })).unwrap();
            assert_eq!(tool.kind(), kind);
            assert_eq!(tool.tool_type(), wire);
        }

        assert!(OpenAIServerTool::from_json(json!({ "type": "function" })).is_err());
        assert!(OpenAIServerTool::from_json(json!({ "type": "mcp" })).is_err());
        assert!(OpenAIServerTool::from_json(json!({ "type": "computer_use" })).is_err());
        assert!(OpenAIServerTool::from_json(json!({ "type": "tool_search", "execution": "client" })).is_err());
    }

    #[test]
    fn merge_tools_keeps_client_first_then_server() {
        let client = vec![fn_tool("local")];
        let server = vec![OpenAIServerTool::web_search(), OpenAIServerTool::code_interpreter()];
        let merged = ResponsesServerToolHelper::merge_tools(client, &server);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0]["name"], json!("local"));
        assert_eq!(merged[1]["type"], json!("web_search"));
        assert_eq!(merged[2]["type"], json!("code_interpreter"));
    }

    #[test]
    fn apply_includes_adds_web_search_and_code_interpreter_requirements() {
        let mut body = json!({ "model": "gpt-5" });
        ResponsesServerToolHelper::apply_includes(
            &mut body,
            &[OpenAIServerTool::web_search(), OpenAIServerTool::code_interpreter()],
        );
        assert_eq!(
            body["include"],
            json!([
                "web_search_call.results",
                "web_search_call.action.sources",
                "code_interpreter_call.outputs"
            ])
        );
    }

    #[test]
    fn web_search_call_decodes_call_and_embedded_result() {
        let item = json!({
            "type": "web_search_call",
            "id": "ws_1",
            "status": "completed",
            "action": { "type": "search", "query": "rust" },
            "results": [{ "title": "Rust", "url": "https://rust-lang.org" }]
        });
        let blocks = ResponsesServerToolHelper::decode_blocks(&item, None);
        assert_eq!(blocks.len(), 2);
        match &blocks[0] {
            ContentBlock::ToolUse(call) => {
                assert_eq!(call.id, "ws_1");
                assert_eq!(call.name, "web_search");
                assert_eq!(call.input["query"], json!("rust"));
                assert!(call.is_server_tool());
                // Embedded result: the call must not carry the raw replay item.
                assert!(!call.metadata.contains_key(MD_RESPONSES_SERVER_TOOL_ITEM));
            }
            other => panic!("expected call, got {other:?}"),
        }
        match &blocks[1] {
            ContentBlock::ToolResult(result) => {
                assert_eq!(result.id.as_deref(), Some("ws_1"));
                assert!(result.text_content().contains("rust-lang.org"));
                assert!(result.metadata.contains_key(MD_RESPONSES_SERVER_TOOL_ITEM));
            }
            other => panic!("expected result, got {other:?}"),
        }
    }

    #[test]
    fn image_generation_result_becomes_data_block() {
        let item = json!({
            "type": "image_generation_call",
            "id": "img_1",
            "status": "completed",
            "output_format": "png",
            "result": "AAAA"
        });
        let blocks = ResponsesServerToolHelper::decode_blocks(&item, None);
        assert_eq!(blocks.len(), 2);
        match &blocks[1] {
            ContentBlock::ToolResult(result) => match &result.output[0] {
                ContentBlock::Data(data) => match &data.source {
                    Source::Base64(b) => {
                        assert_eq!(b.media_type, "image/png");
                        assert_eq!(b.data, "AAAA");
                    }
                    other => panic!("expected base64, got {other:?}"),
                },
                other => panic!("expected data block, got {other:?}"),
            },
            other => panic!("expected result, got {other:?}"),
        }
    }

    #[test]
    fn restore_round_trips_raw_item() {
        let item = json!({ "type": "tool_search_output", "id": "ts_1", "tools": [] });
        let mut metadata = JsonMap::new();
        metadata.insert(
            MD_RESPONSES_SERVER_TOOL_ITEM.to_string(),
            Value::String(item.to_string()),
        );
        let restored = ResponsesServerToolHelper::restore_server_tool_item(&metadata).unwrap();
        assert_eq!(restored, item);
        assert!(ResponsesServerToolHelper::restore_server_tool_item(&JsonMap::new()).is_none());
    }

    #[test]
    fn hosted_tool_search_output_pairs_with_earliest_call() {
        let mut pairing = HostedToolSearchPairing::default();
        let call = json!({ "type": "tool_search_call", "id": "c1", "execution": "server" });
        let output = json!({ "type": "tool_search_output", "id": "o1", "execution": "server" });
        assert_eq!(pairing.companion_call_id(&call), None);
        assert_eq!(pairing.companion_call_id(&output).as_deref(), Some("c1"));
        // Client-executed search is ignored.
        let client = json!({ "type": "tool_search_call", "id": "c2", "execution": "client" });
        assert_eq!(pairing.companion_call_id(&client), None);
    }

    #[test]
    fn code_interpreter_outputs_decode_logs_and_images() {
        let item = json!({
            "type": "code_interpreter_call",
            "id": "ci_1",
            "status": "completed",
            "container_id": "cnt_1",
            "outputs": [
                { "type": "logs", "logs": "hello" },
                { "type": "image", "url": "https://example.test/chart.png" }
            ]
        });
        let blocks = ResponsesServerToolHelper::decode_blocks(&item, None);
        match &blocks[1] {
            ContentBlock::ToolResult(result) => {
                assert_eq!(result.output.len(), 2);
                assert!(matches!(&result.output[0], ContentBlock::Text(t) if t.text == "hello"));
                assert!(matches!(&result.output[1], ContentBlock::Data(_)));
                assert_eq!(
                    result.metadata[MD_CODE_INTERPRETER_CONTAINER_ID],
                    json!("cnt_1")
                );
            }
            other => panic!("expected result, got {other:?}"),
        }
    }
}
