//! The peer's tool catalog, built from `tools/list`.
//!
//! The catalog exists for one reason: a call for a tool the peer never advertised is a
//! **hard error** here, never a silent empty result. A durable runtime that guesses "the tool
//! probably exists" turns a typo or a stale catalog into an effect that is recorded as
//! successful while nothing ran, which is unrecoverable.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::TransportError;
use crate::jsonrpc::{Request, RequestId};
use crate::mapping::tools_call_request;

/// One entry of `tools/list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    /// The name `tools/call` must use.
    pub name: String,
    /// Human/model-facing description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema of the tool's arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    /// Title hint (newer revisions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// The `tools/list` result envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsListResult {
    /// The page of tools; MCP always sends the member, even when the page is empty.
    pub tools: Vec<ToolDefinition>,
    /// Cursor for the next page, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

impl ToolsListResult {
    /// Reads a `tools/list` result.
    pub fn from_value(value: &Value) -> Result<Self, TransportError> {
        if !value.is_object() {
            return Err(TransportError::InvalidRequest(
                "tools/list result is not a JSON object".to_owned(),
            ));
        }
        let result: Self = serde_json::from_value(value.clone()).map_err(|error| {
            TransportError::InvalidRequest(format!("tools/list result is unusable: {error}"))
        })?;
        for tool in &result.tools {
            if tool.name.trim().is_empty() {
                return Err(TransportError::InvalidRequest(
                    "tools/list contains a tool with an empty name".to_owned(),
                ));
            }
        }
        Ok(result)
    }
}

/// The set of tools the peer declared.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolCatalog {
    tools: Vec<ToolDefinition>,
    names: BTreeSet<String>,
}

impl ToolCatalog {
    /// Builds a catalog from one `tools/list` result.
    pub fn from_tools_list(result: &Value) -> Result<Self, TransportError> {
        let mut catalog = Self::default();
        catalog.add_page(result)?;
        Ok(catalog)
    }

    /// Builds a catalog from several `tools/list` pages (pagination is the caller's loop).
    pub fn from_pages(pages: &[Value]) -> Result<Self, TransportError> {
        let mut catalog = Self::default();
        for page in pages {
            catalog.add_page(page)?;
        }
        Ok(catalog)
    }

    /// Adds one page; a duplicated tool name is a protocol error, not a silent overwrite.
    pub fn add_page(&mut self, page: &Value) -> Result<(), TransportError> {
        let result = ToolsListResult::from_value(page)?;
        for tool in result.tools {
            if !self.names.insert(tool.name.clone()) {
                return Err(TransportError::InvalidRequest(format!(
                    "tools/list advertises `{}` twice",
                    tool.name
                )));
            }
            self.tools.push(tool);
        }
        Ok(())
    }

    /// Whether the peer declared this tool.
    pub fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// The declared tool, if any.
    pub fn get(&self, name: &str) -> Option<&ToolDefinition> {
        self.tools.iter().find(|tool| tool.name == name)
    }

    /// The declared tools, in `tools/list` order.
    pub fn tools(&self) -> &[ToolDefinition] {
        &self.tools
    }

    /// How many tools are declared.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether no tool is declared.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Requires the tool to be declared, or fails with `-32001`.
    pub fn require(&self, name: &str) -> Result<&ToolDefinition, TransportError> {
        self.get(name)
            .ok_or_else(|| TransportError::ToolNotAdvertised(name.to_owned()))
    }

    /// Builds a `tools/call` request, refusing anything the peer did not advertise.
    pub fn call_request(
        &self,
        id: impl Into<RequestId>,
        name: &str,
        arguments: &Value,
    ) -> Result<Request, TransportError> {
        self.require(name)?;
        tools_call_request(id, name, arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_page_becomes_a_catalog() {
        let page = json!({
            "tools": [
                { "name": "echo", "description": "repeat", "inputSchema": { "type": "object" } },
                { "name": "clock" }
            ],
            "nextCursor": "page-2"
        });
        let catalog = ToolCatalog::from_tools_list(&page).expect("catalog");
        assert_eq!(catalog.len(), 2);
        assert!(catalog.contains("echo"));
        assert!(catalog.contains("clock"));
        assert!(!catalog.contains("missing"));
        assert_eq!(
            catalog.get("echo").expect("echo").description.as_deref(),
            Some("repeat")
        );
    }

    #[test]
    fn a_tool_the_peer_never_advertised_is_a_hard_error() {
        let catalog = ToolCatalog::from_tools_list(&json!({ "tools": [{ "name": "echo" }] }))
            .expect("catalog");

        let error = catalog
            .require("ghost")
            .expect_err("an undeclared tool must be rejected");
        assert_eq!(error.code(), Some(crate::jsonrpc::codes::TOOL_NOT_FOUND));

        let error = catalog
            .call_request(RequestId::number(1), "ghost", &json!({}))
            .expect_err("an undeclared tool must not be called");
        assert!(matches!(error, TransportError::ToolNotAdvertised(name) if name == "ghost"));

        catalog
            .call_request(RequestId::number(2), "echo", &json!({}))
            .expect("declared tools are callable");
    }

    #[test]
    fn duplicated_or_nameless_tools_are_protocol_errors() {
        let duplicated = json!({ "tools": [{ "name": "echo" }, { "name": "echo" }] });
        assert!(ToolCatalog::from_tools_list(&duplicated).is_err());

        let nameless = json!({ "tools": [{ "name": "  " }] });
        assert!(ToolCatalog::from_tools_list(&nameless).is_err());

        let not_a_result = json!([]);
        assert!(ToolCatalog::from_tools_list(&not_a_result).is_err());
    }

    #[test]
    fn pages_are_merged() {
        let catalog = ToolCatalog::from_pages(&[
            json!({ "tools": [{ "name": "a" }], "nextCursor": "p2" }),
            json!({ "tools": [{ "name": "b" }] }),
        ])
        .expect("catalog");
        assert_eq!(catalog.len(), 2);
        assert!(catalog.contains("a") && catalog.contains("b"));
    }
}
