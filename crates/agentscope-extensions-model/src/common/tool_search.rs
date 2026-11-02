//! On-demand tool search and the provider-native loading protocols.
//!
//! Mirrors `io.agentscope.core.tool.search.{ToolSearchMode, ToolSearchTool}` and
//! `io.agentscope.extensions.model.openaiofficial.tool.ResponsesToolHelper`.
//!
//! Search itself lives in core (`agentscope_core::tool::search::ToolSearchMiddleware`,
//! which also owns `ToolSearchMode` and the result metadata keys re-exported below); this
//! module only carries the **model-layer** contract so a provider can encode the discovery
//! loop into its own protocol:
//!
//! * `Anthropic` sends the eligible catalog as **deferred** tool definitions and
//!   returns `tool_reference` blocks for the matches (`ToolSearchMode::Deferred`).
//! * `OpenAI Responses` declares a client-executed `tool_search` tool, decodes
//!   `tool_search_call` output items into a local `tool_search` call, and replays
//!   the loaded definitions through `tool_search_output` items
//!   (`ToolSearchMode::Client`).
//! * Every other provider exposes the discovered schemas through ordinary
//!   function calling (`ToolSearchMode::Function`, the default).
//!
//! Core's `Model` trait exposes `tool_search_mode()`, so the two native
//! providers override it in their `Model` impl; the mode enum and the metadata keys are
//! core's, so a search result is spelled the same on both sides of the seam.

use agentscope_core::message::JsonMap;
use agentscope_core::ToolSchema;
use serde_json::{json, Map, Value};

// Single source of truth: core defines the discovery contract.
pub use agentscope_core::model::ToolSearchMode;
pub use agentscope_core::tool::search::{
    DEFAULT_LIMIT, METADATA_MODE, METADATA_TOOLS, TOOL_SEARCH_TOOL_NAME,
};

/// Metadata key marking a decoded **client-executed** OpenAI `tool_search_call`
/// (Java `ResponsesToolHelper.CLIENT_TOOL_SEARCH_CALL_METADATA`).
pub const METADATA_CLIENT_TOOL_SEARCH_CALL: &str = "openai.clientToolSearchCall";

/// Builds the `tool_search` declaration the middleware registers.
///
/// The schema is what [`is_tool_search_schema`] recognises, so a host that keeps
/// its own copy of the middleware does not need this crate's builder.
pub fn tool_search_schema(description: impl Into<String>) -> ToolSchema {
    let mut parameters = Map::new();
    parameters.insert("type".to_string(), json!("object"));
    parameters.insert(
        "properties".to_string(),
        json!({
            "query": {"type": "string", "description": "The capability to find"},
            "limit": {"type": "integer", "minimum": 1, "description": "Maximum results (default 5)"}
        }),
    );
    parameters.insert("required".to_string(), json!(["query"]));
    parameters.insert("additionalProperties".to_string(), json!(false));
    ToolSchema::new(TOOL_SEARCH_TOOL_NAME, description).parameters(parameters)
}

/// Whether a schema declares the native client-executed tool search rather than
/// an ordinary function.
///
/// Java has an explicit `ToolSchema.toolSearch` flag (set by
/// `ToolSearchMiddleware`). Core's `ToolSchema` has no such field, so the
/// declaration is recognised by its contract: the tool is named `tool_search`
/// and takes the middleware's required `query` argument. A user tool with that
/// exact name and a required `query` string is therefore treated as a discovery
/// declaration — which is the same tool, by construction.
pub fn is_tool_search_schema(schema: &ToolSchema) -> bool {
    schema.name == TOOL_SEARCH_TOOL_NAME
        && schema
            .parameters
            .get("properties")
            .and_then(Value::as_object)
            .map(|properties| properties.contains_key("query"))
            .unwrap_or(false)
}

/// The loading protocol recorded on a search result, if any.
pub fn mode_of(metadata: &JsonMap) -> Option<ToolSearchMode> {
    metadata
        .get(METADATA_MODE)
        .and_then(Value::as_str)
        .and_then(ToolSearchMode::from_wire)
}

/// The tool schemas discovered by a successful `tool_search` call.
///
/// The entries are the middleware's own description of each match
/// (`name`/`description`/`parameters`/`strict`/`output_schema`); they are
/// rebuilt as [`ToolSchema`] values with `defer_loading` set, because every
/// discovered tool is by definition deferred until it is discovered.
pub fn loaded_tool_schemas(metadata: &JsonMap) -> Vec<ToolSchema> {
    let Some(entries) = metadata.get(METADATA_TOOLS).and_then(Value::as_array) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name").and_then(Value::as_str)?;
            let description = entry
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut schema = ToolSchema::new(name, description);
            if let Some(parameters) = entry.get("parameters").and_then(Value::as_object) {
                schema = schema.parameters(parameters.clone());
            }
            if let Some(strict) = entry.get("strict").and_then(Value::as_bool) {
                schema = schema.strict(strict);
            }
            if let Some(output_schema) = entry.get("output_schema").and_then(Value::as_object) {
                schema = schema.output_schema(output_schema.clone());
            }
            schema = schema.defer_loading(Some(true));
            Some(schema)
        })
        .collect()
}

/// The names of the tools discovered by a successful `tool_search` call.
pub fn loaded_tool_names(metadata: &JsonMap) -> Vec<String> {
    loaded_tool_schemas(metadata)
        .into_iter()
        .map(|schema| schema.name)
        .collect()
}

/// Metadata marking a decoded tool call as a client-executed tool search.
pub fn client_tool_search_metadata() -> JsonMap {
    let mut metadata = JsonMap::new();
    metadata.insert(METADATA_CLIENT_TOOL_SEARCH_CALL.to_string(), json!(true));
    metadata
}

/// Whether a tool call was decoded from a client-executed `tool_search_call`.
pub fn is_client_tool_search_call(metadata: &JsonMap) -> bool {
    metadata
        .get(METADATA_CLIENT_TOOL_SEARCH_CALL)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Whether the already-mapped history contains a client-executed
/// `tool_search_call` with the given id.
///
/// Mirrors `ResponsesRequestMapper`'s scan: a tool result becomes a
/// `tool_search_output` item only when the matching call was client-executed.
pub fn has_client_tool_search_call(items: &[Value], call_id: &str) -> bool {
    items.iter().any(|item| {
        item.get("type").and_then(Value::as_str) == Some("tool_search_call")
            && item.get("call_id").and_then(Value::as_str) == Some(call_id)
            && item.get("execution").and_then(Value::as_str) == Some("client")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_search_schema_is_recognised_by_its_contract() {
        let schema = tool_search_schema("Search for tools by capability");
        assert_eq!(schema.name, "tool_search");
        assert!(is_tool_search_schema(&schema));

        // A plain function with the same name but no `query` property is not a
        // discovery declaration.
        let other = ToolSchema::new("tool_search", "unrelated");
        assert!(!is_tool_search_schema(&other));

        let ordinary = ToolSchema::new("get_weather", "weather");
        assert!(!is_tool_search_schema(&ordinary));
    }

    #[test]
    fn loaded_tools_are_rebuilt_as_deferred_schemas() {
        let mut metadata = JsonMap::new();
        metadata.insert(METADATA_MODE.to_string(), json!("DEFERRED"));
        metadata.insert(
            METADATA_TOOLS.to_string(),
            json!([
                {
                    "name": "get_weather",
                    "description": "Weather",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
                    "strict": true,
                    "output_schema": {"type": "object"}
                },
                { "name": "get_time" }
            ]),
        );

        assert_eq!(mode_of(&metadata), Some(ToolSearchMode::Deferred));
        let schemas = loaded_tool_schemas(&metadata);
        assert_eq!(schemas.len(), 2);
        assert_eq!(schemas[0].name, "get_weather");
        assert_eq!(schemas[0].strict, Some(true));
        assert!(schemas[0].output_schema.is_some());
        assert_eq!(schemas[0].defer_loading, Some(true));
        assert_eq!(
            loaded_tool_names(&metadata),
            vec!["get_weather", "get_time"]
        );

        // Malformed / missing metadata yields no tools instead of panicking.
        assert!(loaded_tool_schemas(&JsonMap::new()).is_empty());
        let mut malformed = JsonMap::new();
        malformed.insert(
            METADATA_TOOLS.to_string(),
            json!([{"description": "no name"}]),
        );
        assert!(loaded_tool_schemas(&malformed).is_empty());
    }

    #[test]
    fn client_tool_search_marker_round_trips() {
        let metadata = client_tool_search_metadata();
        assert!(is_client_tool_search_call(&metadata));
        assert!(!is_client_tool_search_call(&JsonMap::new()));
    }

    #[test]
    fn client_tool_search_call_lookup_requires_client_execution() {
        let items = vec![
            json!({"type": "function_call", "call_id": "other"}),
            json!({"type": "tool_search_call", "call_id": "c1", "execution": "client"}),
            json!({"type": "tool_search_call", "call_id": "c2", "execution": "server"}),
        ];
        assert!(has_client_tool_search_call(&items, "c1"));
        assert!(!has_client_tool_search_call(&items, "c2"));
        assert!(!has_client_tool_search_call(&items, "missing"));
    }
}
