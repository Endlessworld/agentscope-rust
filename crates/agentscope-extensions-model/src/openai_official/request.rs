//! Maps AgentScope inputs to an OpenAI Responses API request body.
//!
//! Mirrors `ResponsesRequestMapper`: options mapping, history mapping
//! (including reasoning replay), tool definition mapping, server tools, and
//! all fail-fast validation for unsupported fields. The result is the raw JSON
//! body posted to `{base_url}/responses`, keeping the adapter on the shared
//! [`crate::common::HttpTransport`] seam (and therefore offline-testable).

use agentscope_core::error::{Error, Result};
use agentscope_core::formatter::ResponseFormat;
use agentscope_core::message::{
    ContentBlock, DataBlock, Msg, MsgRole, Source, ThinkingBlock, ToolResultBlock, ToolResultState,
    ToolUseBlock,
};
use agentscope_core::model::{GenerateOptions, ToolChoice, ToolSchema};
use serde_json::{json, Map, Value};

use super::constants::{
    is_whitelisted_body_param, MD_FUNCTION_CALL_NAMESPACE, MD_REASONING_ENCRYPTED_CONTENT,
};
use super::formatter::ResponsesMultiAgentFormatter;
use super::server_tool::{OpenAIServerTool, ResponsesServerToolHelper};
use crate::common::tool_search;

/// Maps messages, tools and options to a Responses API request body.
pub fn map_request(
    messages: &[Msg],
    tools: &[ToolSchema],
    server_tools: &[OpenAIServerTool],
    options: &GenerateOptions,
    strict_tools: Option<bool>,
    strict_json_schema: Option<bool>,
    formatter: Option<&ResponsesMultiAgentFormatter>,
) -> Result<Value> {
    validate_rejected_fields(options)?;
    validate_whitelist(options)?;

    let model = options
        .model_name
        .as_deref()
        .filter(|m| !m.trim().is_empty())
        .ok_or_else(|| Error::Model("modelName must be set for openai-official".to_string()))?;

    let mut body = json!({
        "store": false,
        "model": model,
    });

    map_options(&mut body, options, strict_json_schema)?;
    map_additional_body_params(&mut body, options)?;

    let input = match formatter {
        Some(formatter) => formatter.format_history(messages)?,
        None => map_history(messages)?,
    };
    // Client-executed tool search: the loaded definitions carry the strict
    // default of the model unless they declare their own (Java
    // `applyToolSearchStrictDefaults`).
    let input = apply_tool_search_strict_defaults(input, strict_tools);
    if let Some(map) = body.as_object_mut() {
        map.insert("input".to_string(), Value::Array(input));
    }

    let has_tool_search = server_tools
        .iter()
        .any(|tool| tool.kind() == super::server_tool::OpenAIServerToolKind::ToolSearch);
    // Hosted tool search (a server tool) and client-executed tool search (a
    // response tool) are mutually exclusive loading protocols.
    let has_client_tool_search = tools.iter().any(tool_search::is_tool_search_schema);
    if has_tool_search && has_client_tool_search {
        return Err(Error::Model(
            "Hosted and client-executed tool search cannot be combined".to_string(),
        ));
    }
    let client_tools = map_tools(
        tools,
        strict_tools,
        has_tool_search || has_client_tool_search,
    )?;
    let merged = ResponsesServerToolHelper::merge_tools(client_tools, server_tools);
    if !merged.is_empty() {
        body["tools"] = Value::Array(merged);
    }
    ResponsesServerToolHelper::apply_includes(&mut body, server_tools);

    if let Some(choice) = &options.tool_choice {
        map_tool_choice(&mut body, choice)?;
    }

    Ok(body)
}

// ── Option enums (fail-fast validation) ─────────────────────────────

/// Legal `reasoning.effort` values, mirroring openai-java `ReasoningEffort.Known`.
pub const REASONING_EFFORTS: &[&str] =
    &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
/// Legal `reasoning.summary` values (`Reasoning.Summary`).
pub const REASONING_SUMMARIES: &[&str] = &["auto", "concise", "detailed"];
/// Legal `reasoning.context` values (`Reasoning.Context`).
pub const REASONING_CONTEXTS: &[&str] = &["auto", "current_turn", "all_turns"];
/// Legal `reasoning.mode` values (`Reasoning.Mode`).
pub const REASONING_MODES: &[&str] = &["standard", "pro"];
/// Legal `service_tier` values (`ResponseCreateParams.ServiceTier`).
pub const SERVICE_TIERS: &[&str] = &[
    "auto",
    "default",
    "flex",
    "scale",
    "priority",
    "fast",
    "ultrafast",
];
/// Legal `prompt_cache_options.mode` values.
pub const PROMPT_CACHE_MODES: &[&str] = &["implicit", "explicit"];
/// Legal `prompt_cache_options.ttl` values.
pub const PROMPT_CACHE_TTLS: &[&str] = &["30m"];

/// Rejects a value outside `allowed`, naming the field and the legal set.
fn validate_enum(field: &str, value: &str, allowed: &[&str]) -> Result<()> {
    if allowed.contains(&value) {
        return Ok(());
    }
    Err(Error::Model(format!(
        "Invalid {field} value '{value}'; expected one of: {}",
        allowed.join(", ")
    )))
}

/// Reads an optional string-valued option and validates it against `allowed`.
fn validated_str<'a>(
    field: &str,
    value: Option<&'a Value>,
    allowed: &[&str],
) -> Result<Option<&'a str>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            validate_enum(field, text, allowed)?;
            Ok(Some(text))
        }
        Some(other) => Err(Error::Model(format!(
            "Invalid {field} value '{other}': expected a string, one of: {}",
            allowed.join(", ")
        ))),
    }
}

// ── Options mapping ─────────────────────────────────────────────────

fn map_options(
    body: &mut Value,
    options: &GenerateOptions,
    strict_json_schema: Option<bool>,
) -> Result<()> {
    if let Some(temperature) = options.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(top_p) = options.top_p {
        body["top_p"] = json!(top_p);
    }
    if let Some(max_tokens) = options.max_completion_tokens.or(options.max_tokens) {
        body["max_output_tokens"] = json!(max_tokens);
    }
    if let Some(parallel) = options.parallel_tool_calls {
        body["parallel_tool_calls"] = json!(parallel);
    }

    let reasoning_params = &options.additional_body_params;
    let has_reasoning_param = ["reasoning.summary", "reasoning.context", "reasoning.mode"]
        .iter()
        .any(|key| reasoning_params.get(*key).is_some_and(|v| !v.is_null()));
    if options.reasoning_effort.is_some() || has_reasoning_param {
        let mut reasoning = Map::new();
        if let Some(effort) = &options.reasoning_effort {
            // Mirrors `ReasoningEffort.of(x).validate()`: unknown values fail
            // fast instead of being forwarded to the API.
            validate_enum("reasoning.effort", effort, REASONING_EFFORTS)?;
            reasoning.insert("effort".to_string(), json!(effort));
        }
        for (key, field, allowed) in [
            ("reasoning.summary", "summary", REASONING_SUMMARIES),
            ("reasoning.context", "context", REASONING_CONTEXTS),
            ("reasoning.mode", "mode", REASONING_MODES),
        ] {
            if let Some(value) = validated_str(key, reasoning_params.get(key), allowed)? {
                reasoning.insert(field.to_string(), json!(value));
            }
        }
        body["reasoning"] = Value::Object(reasoning);
    }

    if let Some(format) = &options.response_format {
        map_response_format(body, format, strict_json_schema)?;
    }
    Ok(())
}

fn map_response_format(
    body: &mut Value,
    format: &ResponseFormat,
    strict_json_schema: Option<bool>,
) -> Result<()> {
    match format.format_type.as_str() {
        "json_object" => {
            body["text"] = json!({ "format": { "type": "json_object" } });
        }
        "json_schema" => {
            let schema = format.json_schema.as_ref().ok_or_else(|| {
                Error::Model(
                    "json_schema response format requires a non-null schema or schema name."
                        .to_string(),
                )
            })?;
            let name = schema
                .name
                .as_deref()
                .filter(|n| !n.trim().is_empty())
                .ok_or_else(|| {
                    Error::Model(
                        "json_schema response format requires a non-null schema or schema name."
                            .to_string(),
                    )
                })?;
            let mut wire = Map::new();
            wire.insert("type".to_string(), json!("json_schema"));
            wire.insert("name".to_string(), json!(name));
            wire.insert("schema".to_string(), Value::Object(schema.schema.clone()));
            let strict = schema.strict.or(strict_json_schema);
            if let Some(strict) = strict {
                wire.insert("strict".to_string(), json!(strict));
            }
            body["text"] = json!({ "format": Value::Object(wire) });
        }
        "text" => {}
        other => {
            return Err(Error::Model(format!(
                "Unsupported response format type: {other}"
            )))
        }
    }
    Ok(())
}

// ── additionalBodyParams mapping ────────────────────────────────────

fn validate_whitelist(options: &GenerateOptions) -> Result<()> {
    for key in options.additional_body_params.keys() {
        if !is_whitelisted_body_param(key) {
            return Err(Error::Model(format!(
                "additionalBodyParams key '{key}' is not in the whitelist for the \
                 openai-official provider."
            )));
        }
    }
    Ok(())
}

fn map_additional_body_params(body: &mut Value, options: &GenerateOptions) -> Result<()> {
    let params = &options.additional_body_params;

    if let Some(value) = params.get("store") {
        let store = value_as_bool(value).ok_or_else(|| {
            Error::Model(format!(
                "Invalid store value '{value}': expected a boolean."
            ))
        })?;
        body["store"] = json!(store);
    }
    if let Some(value) = params.get("previous_response_id").and_then(Value::as_str) {
        body["previous_response_id"] = json!(value);
    }
    if let Some(value) = params.get("max_tool_calls") {
        let number = value
            .as_u64()
            .or_else(|| value.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
            .ok_or_else(|| {
                Error::Model(format!(
                    "Invalid max_tool_calls value '{value}': expected a long integer."
                ))
            })?;
        body["max_tool_calls"] = json!(number);
    }
    if let Some(value) = validated_str("service_tier", params.get("service_tier"), SERVICE_TIERS)? {
        body["service_tier"] = json!(value);
    }
    if let Some(value) = params.get("prompt_cache_key").and_then(Value::as_str) {
        body["prompt_cache_key"] = json!(value);
    }
    if let Some(value) = params.get("prompt_cache_options") {
        let map = value.as_object().ok_or_else(|| {
            Error::Model(format!("prompt_cache_options must be a Map, got: {value}"))
        })?;
        let mut options_body = Map::new();
        if let Some(mode) = validated_str(
            "prompt_cache_options.mode",
            map.get("mode"),
            PROMPT_CACHE_MODES,
        )? {
            options_body.insert("mode".to_string(), json!(mode));
        }
        if let Some(ttl) = validated_str(
            "prompt_cache_options.ttl",
            map.get("ttl"),
            PROMPT_CACHE_TTLS,
        )? {
            options_body.insert("ttl".to_string(), json!(ttl));
        }
        for key in map.keys() {
            if key != "mode" && key != "ttl" {
                return Err(Error::Model(format!(
                    "prompt_cache_options key '{key}' is not supported; expected 'mode' and/or 'ttl'"
                )));
            }
        }
        body["prompt_cache_options"] = Value::Object(options_body);
    }
    if let Some(value) = params.get("safety_identifier").and_then(Value::as_str) {
        body["safety_identifier"] = json!(value);
    }
    Ok(())
}

fn value_as_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::String(s) if s.eq_ignore_ascii_case("true") => Some(true),
        Value::String(s) if s.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

// ── Rejected fields validation ──────────────────────────────────────

fn validate_rejected_fields(options: &GenerateOptions) -> Result<()> {
    let reject = |field: &str| {
        Err(Error::Model(format!(
            "{field} is not supported by the openai-official provider."
        )))
    };
    if options
        .endpoint_path
        .as_deref()
        .is_some_and(|p| !p.trim().is_empty())
    {
        return reject("endpointPath");
    }
    if options.frequency_penalty.is_some() {
        return reject("frequencyPenalty");
    }
    if options.presence_penalty.is_some() {
        return reject("presencePenalty");
    }
    if options.top_k.is_some() {
        return reject("topK");
    }
    if options.seed.is_some() {
        return reject("seed");
    }
    if options.cache_control.is_some() {
        return reject("cacheControl");
    }
    if options.thinking_budget.is_some() {
        return reject("thinkingBudget");
    }
    if !options.additional_headers.is_empty() {
        return Err(Error::Model(
            "per-request additionalHeaders are not supported by the openai-official provider. \
             Use builder-level additionalHeaders instead."
                .to_string(),
        ));
    }
    if !options.additional_query_params.is_empty() {
        return Err(Error::Model(
            "per-request additionalQueryParams are not supported by the openai-official provider."
                .to_string(),
        ));
    }
    Ok(())
}

// ── History mapping ─────────────────────────────────────────────────

/// Maps messages one-to-one onto Responses API input items.
pub fn map_history(messages: &[Msg]) -> Result<Vec<Value>> {
    let mut items = Vec::new();
    for message in messages {
        map_message(message, &mut items)?;
    }
    Ok(items)
}

/// Maps a single message onto Responses API input items.
pub fn map_message(msg: &Msg, items: &mut Vec<Value>) -> Result<()> {
    match msg.role {
        MsgRole::System => map_system_message(msg, items),
        MsgRole::User => map_user_message(msg, items),
        MsgRole::Assistant => map_assistant_message(msg, items),
        MsgRole::Tool => map_tool_message(msg, items),
    }
}

/// Maps a system message to a `system` easy-input message.
pub fn map_system_message(msg: &Msg, items: &mut Vec<Value>) -> Result<()> {
    items.push(easy_message(
        "system",
        Value::String(collect_text(msg).unwrap_or_default()),
    ));
    Ok(())
}

/// Maps a user message (text-only or multimodal).
pub fn map_user_message(msg: &Msg, items: &mut Vec<Value>) -> Result<()> {
    if msg.content.is_empty() {
        items.push(easy_message("user", Value::String(String::new())));
        return Ok(());
    }
    if msg.content.len() == 1 {
        if let ContentBlock::Text(text) = &msg.content[0] {
            items.push(easy_message("user", Value::String(text.text.clone())));
            return Ok(());
        }
    }

    let mut parts = Vec::new();
    for block in &msg.content {
        match block {
            ContentBlock::Text(text) => parts.push(input_text(&text.text)),
            ContentBlock::Image(image) => {
                parts.push(input_image(&resolve_image_url(&image.source)?))
            }
            ContentBlock::Data(data) => {
                parts.push(input_image(&resolve_data_block_image_url(data)?))
            }
            other => {
                return Err(Error::Model(format!(
                    "Unsupported content block in user message: {other:?}"
                )))
            }
        }
    }
    items.push(easy_message("user", Value::Array(parts)));
    Ok(())
}

/// Maps an assistant message, including reasoning replay and server-tool replay.
pub fn map_assistant_message(msg: &Msg, items: &mut Vec<Value>) -> Result<()> {
    let thinking = msg.content.iter().find_map(|block| match block {
        ContentBlock::Thinking(t) => Some(t),
        _ => None,
    });
    let encrypted_content = thinking
        .and_then(|block| block.metadata.as_ref())
        .and_then(|metadata| metadata.get(MD_REASONING_ENCRYPTED_CONTENT))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());

    if let Some(encrypted) = encrypted_content {
        items.push(reasoning_replay(thinking, encrypted));
    }

    if let Some(text) = collect_text(msg).filter(|text| !text.is_empty()) {
        items.push(easy_message("assistant", Value::String(text)));
    }

    for block in &msg.content {
        match block {
            ContentBlock::ToolUse(tool) => map_tool_use_block(tool, items)?,
            ContentBlock::ToolResult(result) if result.is_server_tool() => {
                if let Some(item) =
                    ResponsesServerToolHelper::restore_server_tool_item(&result.metadata)
                {
                    items.push(item);
                }
            }
            ContentBlock::Thinking(_) => {
                if encrypted_content.is_none() {
                    return Err(Error::Model(
                        "Cannot map ThinkingBlock to a Responses reasoning input item without \
                         encrypted_content."
                            .to_string(),
                    ));
                }
            }
            ContentBlock::Text(_) => {}
            other => {
                return Err(Error::Model(format!(
                    "Unsupported content block in assistant message: {other:?}"
                )))
            }
        }
    }
    Ok(())
}

/// Maps a tool message, requiring at least one `ToolResultBlock`.
pub fn map_tool_message(msg: &Msg, items: &mut Vec<Value>) -> Result<()> {
    if msg.content.is_empty() {
        return Err(Error::Model(
            "Tool message has no content blocks; at least one ToolResultBlock is required for \
             history replay."
                .to_string(),
        ));
    }
    for block in &msg.content {
        match block {
            ContentBlock::ToolResult(result) => map_tool_result_block(result, items)?,
            other => {
                return Err(Error::Model(format!(
                    "Unsupported content block in tool message: {other:?}"
                )))
            }
        }
    }
    Ok(())
}

fn map_tool_use_block(tool: &ToolUseBlock, items: &mut Vec<Value>) -> Result<()> {
    if tool.is_server_tool() {
        if let Some(item) = ResponsesServerToolHelper::restore_server_tool_item(&tool.metadata) {
            items.push(item);
        }
        return Ok(());
    }
    if tool_search::is_client_tool_search_call(&tool.metadata) {
        items.push(restore_client_tool_search(tool));
        return Ok(());
    }
    if tool.id.trim().is_empty() {
        return Err(Error::Model(
            "ToolUseBlock.id must not be null for history replay".to_string(),
        ));
    }
    if tool.name.trim().is_empty() {
        return Err(Error::Model(
            "ToolUseBlock.name must not be null for history replay".to_string(),
        ));
    }
    let arguments = serde_json::to_string(&tool.input).unwrap_or_default();
    let mut item = json!({
        "type": "function_call",
        "call_id": tool.id,
        "name": tool.name,
        "arguments": arguments,
    });
    if let Some(namespace) = tool
        .metadata
        .get(MD_FUNCTION_CALL_NAMESPACE)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        item["namespace"] = json!(namespace);
    }
    items.push(item);
    Ok(())
}

fn map_tool_result_block(result: &ToolResultBlock, items: &mut Vec<Value>) -> Result<()> {
    let call_id = result
        .id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| {
            Error::Model("ToolResultBlock.id must not be null for history replay".to_string())
        })?;

    // A result for a client-executed tool search is replayed as a
    // `tool_search_output` item carrying the definitions it loaded.
    if tool_search::has_client_tool_search_call(items, call_id) {
        items.push(client_tool_search_output(result, call_id));
        return Ok(());
    }

    let has_non_text = result
        .output
        .iter()
        .any(|block| !matches!(block, ContentBlock::Text(_)));

    let mut item = json!({
        "type": "function_call_output",
        "call_id": call_id,
    });
    if has_non_text {
        let mut parts = Vec::new();
        for block in &result.output {
            match block {
                ContentBlock::Text(text) => parts.push(input_text(&text.text)),
                ContentBlock::Image(image) => {
                    parts.push(input_image(&resolve_image_url(&image.source)?))
                }
                ContentBlock::Data(data) => {
                    parts.push(input_image(&resolve_data_block_image_url(data)?))
                }
                other => {
                    return Err(Error::Model(format!(
                        "Unsupported output block in ToolResultBlock: {other:?}"
                    )))
                }
            }
        }
        item["output"] = Value::Array(parts);
    } else {
        item["output"] = Value::String(join_output_text(result));
    }
    items.push(item);
    Ok(())
}

fn reasoning_replay(thinking: Option<&ThinkingBlock>, encrypted: &str) -> Value {
    let summary = thinking
        .map(|t| t.thinking.trim())
        .filter(|text| !text.is_empty())
        .map(|text| json!([{ "type": "summary_text", "text": text }]))
        .unwrap_or_else(|| json!([]));
    json!({
        "type": "reasoning",
        "summary": summary,
        "encrypted_content": encrypted,
    })
}

// ── Tool definition mapping ─────────────────────────────────────────

fn map_tools(
    tools: &[ToolSchema],
    strict_tools: Option<bool>,
    has_tool_search: bool,
) -> Result<Vec<Value>> {
    tools
        .iter()
        .map(|schema| {
            if tool_search::is_tool_search_schema(schema) {
                return Ok(map_tool_search_tool(schema));
            }
            map_function_tool(schema, strict_tools, has_tool_search)
        })
        .collect()
}

/// Maps the client-executed `tool_search` declaration.
///
/// The model is told to search (and call) client-side, so the discovered
/// definitions come back as `tool_search_output` items this crate replays
/// (`ResponsesRequestMapper` / `ResponsesToolHelper`).
fn map_tool_search_tool(schema: &ToolSchema) -> Value {
    let mut tool = json!({
        "type": "tool_search",
        "execution": "client",
        "parameters": Value::Object(schema.parameters.clone()),
    });
    if !schema.description.is_empty() {
        tool["description"] = json!(schema.description);
    }
    tool
}

/// Replays a decoded client tool search as a native `tool_search_call` item.
fn restore_client_tool_search(tool: &ToolUseBlock) -> Value {
    let arguments = if tool.input.is_empty() {
        tool.content
            .as_deref()
            .and_then(|content| serde_json::from_str::<Value>(content).ok())
            .unwrap_or_else(|| json!({}))
    } else {
        Value::Object(tool.input.clone())
    };
    json!({
        "type": "tool_search_call",
        "call_id": tool.id,
        "execution": "client",
        "arguments": arguments,
        "status": "completed",
    })
}

/// Replays a successful client tool search as a `tool_search_output` item
/// carrying the definitions it loaded (`ResponsesRequestMapper`).
///
/// A failed search contributes no tools: the model must not be able to call a
/// definition the search never returned.
fn client_tool_search_output(result: &ToolResultBlock, call_id: &str) -> Value {
    let mut tools = Vec::new();
    if result.state != ToolResultState::Error {
        for schema in tool_search::loaded_tool_schemas(&result.metadata) {
            tools.push(loaded_tool_definition(&schema));
        }
    }
    json!({
        "type": "tool_search_output",
        "call_id": call_id,
        "execution": "client",
        "status": "completed",
        "tools": tools,
    })
}

/// One loaded tool definition inside `tool_search_output.tools`.
///
/// `strict` is only emitted when the discovered schema declares it; a missing
/// value is filled in from the model's `strictTools` default by
/// [`apply_tool_search_strict_defaults`].
fn loaded_tool_definition(schema: &ToolSchema) -> Value {
    let parameters = if schema.parameters.is_empty() {
        json!({"type": "object", "properties": {}})
    } else {
        Value::Object(schema.parameters.clone())
    };
    let mut tool = json!({
        "type": "function",
        "name": schema.name,
        "parameters": parameters,
    });
    if !schema.description.is_empty() {
        tool["description"] = json!(schema.description);
    }
    if let Some(output_schema) = &schema.output_schema {
        if !output_schema.is_empty() {
            tool["output_schema"] = Value::Object(output_schema.clone());
        }
    }
    if let Some(strict) = schema.strict {
        tool["strict"] = json!(strict);
    }
    tool
}

/// Fills the model's `strictTools` default into client-loaded definitions that
/// do not declare `strict` (Java `applyToolSearchStrictDefaults`).
fn apply_tool_search_strict_defaults(items: Vec<Value>, strict_tools: Option<bool>) -> Vec<Value> {
    items
        .into_iter()
        .map(|mut item| {
            if item.get("type").and_then(Value::as_str) != Some("tool_search_output")
                || item.get("execution").and_then(Value::as_str) != Some("client")
            {
                return item;
            }
            if let Some(tools) = item.get_mut("tools").and_then(Value::as_array_mut) {
                for tool in tools {
                    let is_function = tool.get("type").and_then(Value::as_str) == Some("function");
                    if is_function && tool.get("strict").is_none() {
                        tool["strict"] = json!(strict_tools.unwrap_or(false));
                    }
                }
            }
            item
        })
        .collect()
}

fn map_function_tool(
    schema: &ToolSchema,
    strict_tools: Option<bool>,
    has_tool_search: bool,
) -> Result<Value> {
    let effective_strict = schema.strict.or(strict_tools).unwrap_or(false);
    let parameters = if schema.parameters.is_empty() {
        empty_tool_parameters(effective_strict)
    } else {
        Value::Object(schema.parameters.clone())
    };
    let mut tool = json!({
        "type": "function",
        "name": schema.name,
        "strict": effective_strict,
        "parameters": parameters,
    });
    if !schema.description.is_empty() {
        tool["description"] = json!(schema.description);
    }
    if let Some(output_schema) = &schema.output_schema {
        if !output_schema.is_empty() {
            tool["output_schema"] = Value::Object(output_schema.clone());
        }
    }
    if schema.defer_loading == Some(true) {
        if !has_tool_search {
            return Err(Error::Model(format!(
                "Tool '{}' requests deferLoading, but no tool_search server tool is configured; \
                 deferred tools can only be loaded through tool_search.",
                schema.name
            )));
        }
        tool["defer_loading"] = json!(true);
    }
    Ok(tool)
}

fn empty_tool_parameters(strict: bool) -> Value {
    let mut schema = Map::new();
    schema.insert("type".to_string(), json!("object"));
    schema.insert("properties".to_string(), json!({}));
    schema.insert("required".to_string(), json!([]));
    if strict {
        schema.insert("additionalProperties".to_string(), json!(false));
    }
    Value::Object(schema)
}

// ── ToolChoice mapping ──────────────────────────────────────────────

fn map_tool_choice(body: &mut Value, choice: &ToolChoice) -> Result<()> {
    body["tool_choice"] = match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Specific { tool_name } => json!({
            "type": "function",
            "name": tool_name,
        }),
    };
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────

pub(crate) fn easy_message(role: &str, content: Value) -> Value {
    json!({
        "type": "message",
        "role": role,
        "content": content,
    })
}

pub(crate) fn input_text(text: &str) -> Value {
    json!({ "type": "input_text", "text": text })
}

pub(crate) fn input_image(url: &str) -> Value {
    json!({ "type": "input_image", "image_url": url, "detail": "auto" })
}

/// Resolves an image source to a URL or data URI (no network download).
pub fn resolve_image_url(source: &Source) -> Result<String> {
    match source {
        Source::Url(url) => Ok(url.url.clone()),
        Source::Base64(base64) => Ok(format!("data:{};base64,{}", base64.media_type, base64.data)),
        Source::FileId(file) => Err(Error::Model(format!(
            "provider file id '{}' cannot be used as an image URL by the openai-official provider.",
            file.file_id
        ))),
    }
}

/// Resolves a `DataBlock` source, requiring image media for base64 payloads.
pub fn resolve_data_block_image_url(block: &DataBlock) -> Result<String> {
    match &block.source {
        Source::Url(url) => Ok(url.url.clone()),
        Source::Base64(base64) => {
            if !base64.media_type.starts_with("image/") {
                return Err(Error::Model(
                    "Non-image DataBlock is not supported by the openai-official provider."
                        .to_string(),
                ));
            }
            Ok(format!("data:{};base64,{}", base64.media_type, base64.data))
        }
        Source::FileId(file) => Err(Error::Model(format!(
            "provider file id '{}' cannot be used as a DataBlock image by the openai-official provider.",
            file.file_id
        ))),
    }
}

fn collect_text(msg: &Msg) -> Option<String> {
    let mut parts = Vec::new();
    for block in &msg.content {
        if let ContentBlock::Text(text) = block {
            parts.push(text.text.clone());
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn join_output_text(result: &ToolResultBlock) -> String {
    if result.output.is_empty() {
        return String::new();
    }
    let mut parts = Vec::new();
    for block in &result.output {
        if let ContentBlock::Text(text) = block {
            parts.push(text.text.clone());
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::{ChatUsage, ImageBlock, JsonMap, TextBlock, ToolResultState};

    fn options(model: &str) -> GenerateOptions {
        GenerateOptions {
            model_name: Some(model.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn system_and_user_messages_map_to_easy_messages() {
        let messages = vec![Msg::system("be nice"), Msg::user("hi")];
        let body = map_request(&messages, &[], &[], &options("gpt-5"), None, None, None).unwrap();
        assert_eq!(body["store"], json!(false));
        assert_eq!(body["model"], json!("gpt-5"));
        assert_eq!(body["input"][0]["role"], json!("system"));
        assert_eq!(body["input"][0]["content"], json!("be nice"));
        assert_eq!(body["input"][1]["role"], json!("user"));
        assert_eq!(body["input"][1]["content"], json!("hi"));
    }

    #[test]
    fn tool_definitions_map_strict_description_and_empty_parameters() {
        let tools = vec![
            ToolSchema::new("get_weather", "Look up the weather"),
            ToolSchema::new("noop", "no params").strict(true),
        ];
        let body =
            map_request(&[], &tools, &[], &options("gpt-5"), Some(true), None, None).unwrap();
        assert_eq!(body["tools"][0]["type"], json!("function"));
        assert_eq!(body["tools"][0]["name"], json!("get_weather"));
        assert_eq!(
            body["tools"][0]["description"],
            json!("Look up the weather")
        );
        assert_eq!(body["tools"][0]["strict"], json!(true));
        assert_eq!(body["tools"][0]["parameters"]["type"], json!("object"));
        assert_eq!(body["tools"][0]["parameters"]["required"], json!([]));
        assert_eq!(
            body["tools"][0]["parameters"]["additionalProperties"],
            json!(false)
        );
    }

    #[test]
    fn server_tools_are_merged_and_include_list_is_added() {
        let server = vec![
            OpenAIServerTool::web_search(),
            OpenAIServerTool::code_interpreter(),
        ];
        let body = map_request(
            &[],
            &[ToolSchema::new("local", "")],
            &server,
            &options("gpt-5"),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(body["tools"].as_array().unwrap().len(), 3);
        assert_eq!(body["tools"][1]["type"], json!("web_search"));
        assert_eq!(body["tools"][2]["type"], json!("code_interpreter"));
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
    fn deferred_tool_requires_tool_search_server_tool() {
        let mut schema = ToolSchema::new("deferred", "d");
        schema.defer_loading = Some(true);
        let err = map_request(
            &[],
            &[schema.clone()],
            &[],
            &options("gpt-5"),
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("deferLoading"));

        let body = map_request(
            &[],
            &[schema],
            &[OpenAIServerTool::tool_search()],
            &options("gpt-5"),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(body["tools"][0]["defer_loading"], json!(true));
    }

    #[test]
    fn assistant_tool_use_becomes_function_call() {
        let msg = Msg::assistant_blocks(vec![ContentBlock::ToolUse(ToolUseBlock::new(
            "call_1",
            "get_weather",
            serde_json::from_str(r#"{"city":"Beijing"}"#).unwrap(),
        ))]);
        let body = map_request(&[msg], &[], &[], &options("gpt-5"), None, None, None).unwrap();
        assert_eq!(body["input"][0]["type"], json!("function_call"));
        assert_eq!(body["input"][0]["call_id"], json!("call_1"));
        assert_eq!(body["input"][0]["name"], json!("get_weather"));
        assert!(body["input"][0]["arguments"]
            .as_str()
            .unwrap()
            .contains("Beijing"));
    }

    #[test]
    fn tool_result_becomes_function_call_output_and_joins_text() {
        let result = ToolResultBlock::new(
            "call_9",
            "get_weather",
            vec![
                ContentBlock::Text(TextBlock::new("sunny")),
                ContentBlock::Text(TextBlock::new("25C")),
            ],
        );
        let msg = Msg::tool_blocks(vec![result]);
        let body = map_request(&[msg], &[], &[], &options("gpt-5"), None, None, None).unwrap();
        assert_eq!(body["input"][0]["type"], json!("function_call_output"));
        assert_eq!(body["input"][0]["output"], json!("sunny\n25C"));
    }

    #[test]
    fn reasoning_replay_requires_encrypted_content() {
        let thinking = ThinkingBlock::builder()
            .thinking("step")
            .metadata(
                [(MD_REASONING_ENCRYPTED_CONTENT.to_string(), json!("enc-1"))]
                    .into_iter()
                    .collect(),
            )
            .build();
        let msg = Msg::assistant_blocks(vec![
            ContentBlock::Thinking(thinking),
            ContentBlock::Text(TextBlock::new("answer")),
        ]);
        let body = map_request(&[msg], &[], &[], &options("gpt-5"), None, None, None).unwrap();
        assert_eq!(body["input"][0]["type"], json!("reasoning"));
        assert_eq!(body["input"][0]["encrypted_content"], json!("enc-1"));
        assert_eq!(body["input"][0]["summary"][0]["text"], json!("step"));
        assert_eq!(body["input"][1]["content"], json!("answer"));
    }

    #[test]
    fn thinking_without_encrypted_content_is_rejected() {
        let msg = Msg::assistant_blocks(vec![
            ContentBlock::Thinking(ThinkingBlock::new("step")),
            ContentBlock::Text(TextBlock::new("answer")),
        ]);
        let err = map_request(&[msg], &[], &[], &options("gpt-5"), None, None, None).unwrap_err();
        assert!(err.to_string().contains("encrypted_content"));
    }

    #[test]
    fn additonal_body_params_whitelist_and_values() {
        let mut opts = options("gpt-5");
        opts.additional_body_params
            .insert("store".to_string(), json!("true"));
        opts.additional_body_params
            .insert("max_tool_calls".to_string(), json!("3"));
        opts.additional_body_params
            .insert("service_tier".to_string(), json!("flex"));
        let body = map_request(&[], &[], &[], &opts, None, None, None).unwrap();
        assert_eq!(body["store"], json!(true));
        assert_eq!(body["max_tool_calls"], json!(3));
        assert_eq!(body["service_tier"], json!("flex"));

        let mut bad = options("gpt-5");
        bad.additional_body_params
            .insert("unknown".to_string(), json!(1));
        let err = map_request(&[], &[], &[], &bad, None, None, None).unwrap_err();
        assert!(err.to_string().contains("whitelist"));
    }

    #[test]
    fn option_enums_are_validated_against_the_java_whitelists() {
        // Valid values pass through.
        let mut good = options("gpt-5");
        good.reasoning_effort = Some("xhigh".to_string());
        good.additional_body_params
            .insert("reasoning.summary".to_string(), json!("concise"));
        good.additional_body_params
            .insert("reasoning.context".to_string(), json!("current_turn"));
        good.additional_body_params
            .insert("reasoning.mode".to_string(), json!("pro"));
        good.additional_body_params.insert(
            "prompt_cache_options".to_string(),
            json!({"mode": "explicit", "ttl": "30m"}),
        );
        let body = map_request(&[], &[], &[], &good, None, None, None).unwrap();
        assert_eq!(body["reasoning"]["effort"], json!("xhigh"));
        assert_eq!(body["reasoning"]["summary"], json!("concise"));
        assert_eq!(body["reasoning"]["context"], json!("current_turn"));
        assert_eq!(body["reasoning"]["mode"], json!("pro"));
        assert_eq!(body["prompt_cache_options"]["mode"], json!("explicit"));
        assert_eq!(body["prompt_cache_options"]["ttl"], json!("30m"));

        // Unknown values fail fast instead of hitting the API.
        let mut bad_effort = options("gpt-5");
        bad_effort.reasoning_effort = Some("turbo".to_string());
        let err = map_request(&[], &[], &[], &bad_effort, None, None, None).unwrap_err();
        assert!(err.to_string().contains("reasoning.effort"), "{err}");
        assert!(err.to_string().contains("xhigh"), "{err}");

        let mut bad_summary = options("gpt-5");
        bad_summary
            .additional_body_params
            .insert("reasoning.summary".to_string(), json!("verbose"));
        assert!(map_request(&[], &[], &[], &bad_summary, None, None, None)
            .unwrap_err()
            .to_string()
            .contains("reasoning.summary"));

        let mut bad_tier = options("gpt-5");
        bad_tier
            .additional_body_params
            .insert("service_tier".to_string(), json!("turbo"));
        assert!(map_request(&[], &[], &[], &bad_tier, None, None, None)
            .unwrap_err()
            .to_string()
            .contains("service_tier"));

        let mut bad_cache = options("gpt-5");
        bad_cache.additional_body_params.insert(
            "prompt_cache_options".to_string(),
            json!({"mode": "always"}),
        );
        assert!(map_request(&[], &[], &[], &bad_cache, None, None, None)
            .unwrap_err()
            .to_string()
            .contains("prompt_cache_options.mode"));

        let mut bad_ttl = options("gpt-5");
        bad_ttl
            .additional_body_params
            .insert("prompt_cache_options".to_string(), json!({"ttl": "1h"}));
        assert!(map_request(&[], &[], &[], &bad_ttl, None, None, None)
            .unwrap_err()
            .to_string()
            .contains("prompt_cache_options.ttl"));

        let mut bad_key = options("gpt-5");
        bad_key.additional_body_params.insert(
            "prompt_cache_options".to_string(),
            json!({"mode": "implicit", "scope": "all"}),
        );
        assert!(map_request(&[], &[], &[], &bad_key, None, None, None)
            .unwrap_err()
            .to_string()
            .contains("not supported"));
    }

    #[test]
    fn rejected_fields_fail_fast() {
        let mut opts = options("gpt-5");
        opts.seed = Some(7);
        let err = map_request(&[], &[], &[], &opts, None, None, None).unwrap_err();
        assert!(err.to_string().contains("seed"));

        let mut opts = options("gpt-5");
        opts.additional_headers
            .insert("x".to_string(), "y".to_string());
        let err = map_request(&[], &[], &[], &opts, None, None, None).unwrap_err();
        assert!(err.to_string().contains("additionalHeaders"));
    }

    #[test]
    fn tool_choice_specific_maps_to_named_function() {
        let mut opts = options("gpt-5");
        opts.tool_choice = Some(ToolChoice::Specific {
            tool_name: "get_weather".to_string(),
        });
        let body = map_request(
            &[],
            &[ToolSchema::new("get_weather", "")],
            &[],
            &opts,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            body["tool_choice"],
            json!({ "type": "function", "name": "get_weather" })
        );
    }

    #[test]
    fn multimodal_user_message_maps_image_parts() {
        let image = ImageBlock::new(Source::Base64(agentscope_core::Base64Source::new(
            "image/png",
            "AAAA",
        )));
        let msg = Msg::user_blocks(vec![
            ContentBlock::Text(TextBlock::new("look")),
            ContentBlock::Image(image),
        ]);
        let body = map_request(&[msg], &[], &[], &options("gpt-5"), None, None, None).unwrap();
        assert_eq!(body["input"][0]["content"][0]["type"], json!("input_text"));
        assert_eq!(
            body["input"][0]["content"][1]["image_url"],
            json!("data:image/png;base64,AAAA")
        );
    }

    #[test]
    fn server_tool_result_replays_raw_item() {
        let raw = json!({ "type": "web_search_call", "id": "ws_1" });
        let mut metadata = JsonMap::new();
        metadata.insert(
            super::super::constants::MD_RESPONSES_SERVER_TOOL_ITEM.to_string(),
            json!(raw.to_string()),
        );
        metadata.insert(
            ToolResultBlock::METADATA_SERVER_TOOL.to_string(),
            json!(true),
        );
        let result = ToolResultBlock {
            id: Some("ws_1".to_string()),
            name: Some("web_search".to_string()),
            output: vec![],
            metadata,
            state: ToolResultState::Success,
        };
        let msg = Msg::assistant_blocks(vec![ContentBlock::ToolResult(result)]);
        let body = map_request(&[msg], &[], &[], &options("gpt-5"), None, None, None).unwrap();
        assert_eq!(body["input"][0], raw);
    }

    #[test]
    fn usage_type_is_available_to_tests() {
        let _ = ChatUsage::default();
    }
}
