//! Parses the result of Higress's `x_higress_tool_search` tool.
//!
//! Mirrors `io.agentscope.extensions.higress.HigressToolSearchResult`.
//!
//! Higress returns recommended tools either in the MCP `structuredContent`
//! object (`{ "tools": [...] }`) or, when that is absent, as a JSON string in a
//! text content block. This module normalises both shapes into
//! [`HigressToolInfo`] values.
use serde::Deserialize;
use serde_json::Value;

use rust_mcp_sdk::schema::{CallToolResult, ContentBlock};

/// Describes a single tool returned from a Higress tool search.
///
/// Mirrors the Java `HigressToolSearchResult.ToolInfo` record.
#[derive(Debug, Clone, PartialEq)]
pub struct HigressToolInfo {
    /// The tool name (e.g. `map___maps_weather`).
    pub name: String,
    /// The tool description.
    pub description: Option<String>,
    /// The tool title (e.g. `maps_weather`).
    pub title: Option<String>,
    /// The input parameter JSON schema, if provided.
    pub input_schema: Option<Value>,
    /// The output parameter JSON schema, if provided.
    pub output_schema: Option<Value>,
}

impl HigressToolInfo {
    /// Creates a tool info with only the mandatory name populated.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            title: None,
            input_schema: None,
            output_schema: None,
        }
    }
}

/// The parsed outcome of a Higress tool search call.
///
/// Mirrors `io.agentscope.extensions.higress.HigressToolSearchResult`.
#[derive(Debug, Clone, PartialEq)]
pub struct HigressToolSearchResult {
    tools: Vec<HigressToolInfo>,
    success: bool,
    error_message: Option<String>,
}

impl HigressToolSearchResult {
    fn new(tools: Vec<HigressToolInfo>, success: bool, error_message: Option<String>) -> Self {
        Self {
            tools,
            success,
            error_message,
        }
    }

    fn success(tools: Vec<HigressToolInfo>) -> Self {
        Self::new(tools, true, None)
    }

    fn error(message: impl Into<String>) -> Self {
        Self::new(Vec::new(), false, Some(message.into()))
    }

    /// Parses a tool search result from an MCP [`CallToolResult`].
    ///
    /// `structuredContent` takes precedence over text `content`. Any parsing
    /// failure degrades to an unsuccessful result carrying an error message,
    /// matching the Java behaviour.
    pub fn parse(call_tool_result: &CallToolResult) -> Self {
        if call_tool_result.is_error == Some(true) {
            return Self::error("Tool call returned error");
        }

        // Prefer structuredContent when it is a valid `{ tools: [...] }` object.
        if let Some(structured) = &call_tool_result.structured_content {
            match Self::parse_from_structured_content(structured) {
                Ok(result) => return result,
                Err(err) => {
                    tracing::debug!("Failed to parse structuredContent, trying content: {}", err);
                }
            }
        }

        // Fall back to text content blocks.
        if !call_tool_result.content.is_empty() {
            return match Self::parse_from_content(&call_tool_result.content) {
                Ok(result) => result,
                Err(err) => {
                    tracing::error!("Failed to parse content: {}", err);
                    Self::error(format!("Failed to parse tool search result: {}", err))
                }
            };
        }

        Self::error("No content in tool search result")
    }

    /// Parses from the MCP `structuredContent` map.
    fn parse_from_structured_content(
        structured_content: &serde_json::Map<String, Value>,
    ) -> std::result::Result<Self, String> {
        let tools_value = structured_content
            .get("tools")
            .ok_or_else(|| "Invalid structuredContent format".to_string())?;
        let tools_array = tools_value
            .as_array()
            .ok_or_else(|| "Invalid structuredContent format".to_string())?;

        let tools = tools_array
            .iter()
            .filter_map(|item| item.as_object())
            .map(Self::parse_tool_info)
            .collect();
        Ok(Self::success(tools))
    }

    /// Parses from the list of MCP content blocks (text JSON).
    fn parse_from_content(content: &[ContentBlock]) -> std::result::Result<Self, String> {
        for block in content {
            if let ContentBlock::TextContent(text) = block {
                if !text.text.is_empty() {
                    let response: ToolSearchResponse =
                        serde_json::from_str(&text.text).map_err(|e| e.to_string())?;
                    if let Some(tools) = response.tools {
                        let infos = tools
                            .into_iter()
                            .filter_map(|v| v.as_object().map(Self::parse_tool_info))
                            .collect();
                        return Ok(Self::success(infos));
                    }
                }
            }
        }
        Err("No valid text content found".to_string())
    }

    /// Parses a single tool entry from a JSON object.
    fn parse_tool_info(map: &serde_json::Map<String, Value>) -> HigressToolInfo {
        HigressToolInfo {
            name: map
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            description: map
                .get("description")
                .and_then(|v| v.as_str())
                .map(String::from),
            title: map.get("title").and_then(|v| v.as_str()).map(String::from),
            input_schema: map.get("inputSchema").filter(|v| !v.is_null()).cloned(),
            output_schema: map.get("outputSchema").filter(|v| !v.is_null()).cloned(),
        }
    }

    /// Whether the search succeeded.
    pub fn is_success(&self) -> bool {
        self.success
    }

    /// The error message when the search failed, otherwise `None`.
    pub fn error_message(&self) -> Option<&str> {
        self.error_message.as_deref()
    }

    /// The list of recommended tools (possibly empty).
    pub fn tools(&self) -> &[HigressToolInfo] {
        &self.tools
    }

    /// The names of all found tools.
    pub fn tool_names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name.as_str()).collect()
    }
}

impl std::fmt::Display for HigressToolSearchResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.success {
            write!(
                f,
                "HigressToolSearchResult{{error='{}'}}",
                self.error_message.as_deref().unwrap_or_default()
            )
        } else {
            write!(
                f,
                "HigressToolSearchResult{{tools={:?}}}",
                self.tool_names()
            )
        }
    }
}

/// Internal shape for JSON deserialisation of the text payload.
#[derive(Deserialize)]
struct ToolSearchResponse {
    #[serde(default)]
    tools: Option<Vec<Value>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call_result(json: Value) -> CallToolResult {
        serde_json::from_value(json).expect("valid CallToolResult")
    }

    #[test]
    fn parses_structured_content_first() {
        let result = call_result(serde_json::json!({
            "content": [ { "type": "text", "text": "{\"tools\":[]}" } ],
            "structuredContent": {
                "tools": [ {
                    "name": "map___maps_weather",
                    "title": "maps_weather",
                    "description": "Weather lookup",
                    "inputSchema": { "type": "object", "properties": { "city": { "type": "string" } } }
                } ]
            }
        }));

        let parsed = HigressToolSearchResult::parse(&result);
        assert!(parsed.is_success());
        assert!(parsed.error_message().is_none());
        assert_eq!(parsed.tool_names(), vec!["map___maps_weather"]);
        let tool = &parsed.tools()[0];
        assert_eq!(tool.title.as_deref(), Some("maps_weather"));
        assert_eq!(tool.description.as_deref(), Some("Weather lookup"));
        assert_eq!(
            tool.input_schema
                .as_ref()
                .and_then(|v| v.get("properties"))
                .and_then(|v| v.get("city"))
                .and_then(|v| v.get("type"))
                .and_then(|v| v.as_str()),
            Some("string")
        );
        assert!(tool.output_schema.is_none());
    }

    #[test]
    fn falls_back_to_text_content_when_structured_content_invalid() {
        let result = call_result(serde_json::json!({
            "content": [
                { "type": "text", "text": "" },
                { "type": "text", "text": "{\"tools\":[{\"name\":\"a\"},{\"name\":\"b\"}]}" }
            ],
            "structuredContent": { "unexpected": true }
        }));

        let parsed = HigressToolSearchResult::parse(&result);
        assert!(parsed.is_success());
        assert_eq!(parsed.tool_names(), vec!["a", "b"]);
    }

    #[test]
    fn parses_from_text_content_when_no_structured_content() {
        let result = call_result(serde_json::json!({
            "content": [ { "type": "text", "text": "{\"tools\":[{\"name\":\"solo\",\"outputSchema\":{\"type\":\"object\"}}]}" } ]
        }));

        let parsed = HigressToolSearchResult::parse(&result);
        assert!(parsed.is_success());
        assert_eq!(parsed.tool_names(), vec!["solo"]);
        assert!(parsed.tools()[0].output_schema.is_some());
    }

    #[test]
    fn error_result_is_not_success() {
        let result = call_result(serde_json::json!({
            "content": [],
            "isError": true
        }));

        let parsed = HigressToolSearchResult::parse(&result);
        assert!(!parsed.is_success());
        assert_eq!(parsed.error_message(), Some("Tool call returned error"));
        assert!(parsed.tools().is_empty());
    }

    #[test]
    fn empty_content_reports_missing_content() {
        let result = call_result(serde_json::json!({ "content": [] }));

        let parsed = HigressToolSearchResult::parse(&result);
        assert!(!parsed.is_success());
        assert_eq!(
            parsed.error_message(),
            Some("No content in tool search result")
        );
    }

    #[test]
    fn malformed_text_reports_parse_error() {
        let result = call_result(serde_json::json!({
            "content": [ { "type": "text", "text": "not-json" } ]
        }));

        let parsed = HigressToolSearchResult::parse(&result);
        assert!(!parsed.is_success());
        assert!(parsed
            .error_message()
            .unwrap()
            .starts_with("Failed to parse tool search result:"));
    }

    #[test]
    fn missing_name_defaults_to_empty_string() {
        let result = call_result(serde_json::json!({
            "content": [],
            "structuredContent": { "tools": [ { "description": "no name" } ] }
        }));

        let parsed = HigressToolSearchResult::parse(&result);
        assert!(parsed.is_success());
        assert_eq!(parsed.tools()[0].name, "");
        assert_eq!(parsed.tools()[0].description.as_deref(), Some("no name"));
    }

    #[test]
    fn null_schemas_are_dropped() {
        let result = call_result(serde_json::json!({
            "content": [],
            "structuredContent": { "tools": [
                { "name": "n", "inputSchema": null, "outputSchema": null }
            ] }
        }));

        let parsed = HigressToolSearchResult::parse(&result);
        assert!(parsed.tools()[0].input_schema.is_none());
        assert!(parsed.tools()[0].output_schema.is_none());
    }

    #[test]
    fn display_reflects_success_and_error() {
        let ok = call_result(serde_json::json!({
            "content": [],
            "structuredContent": { "tools": [ { "name": "x" } ] }
        }));
        assert_eq!(
            HigressToolSearchResult::parse(&ok).to_string(),
            "HigressToolSearchResult{tools=[\"x\"]}"
        );

        let err = call_result(serde_json::json!({ "content": [], "isError": true }));
        assert_eq!(
            HigressToolSearchResult::parse(&err).to_string(),
            "HigressToolSearchResult{error='Tool call returned error'}"
        );
    }
}
