//! Anthropic built-in server tools.
//!
//! Mirrors `anthropic.tool.AnthropicServerTool` + the declaration half of
//! `anthropic.formatter.AnthropicServerToolHelper`. Server tools (web search,
//! web fetch, code execution, tool search, ...) run on Anthropic's side inside a
//! single model call; they are declared next to the client tools but carry a
//! versioned `type` instead of an `input_schema`.
//!
//! Java wraps a strongly-typed SDK `ToolUnion`, validates it and caches the name.
//! Rust has no Anthropic SDK, so the definition is kept as validated wire JSON:
//! the same `type` values, the same “custom client tools are rejected” rule, and
//! the same “name must be present” rule. The result half (decoding
//! `*_tool_result` blocks back into `ToolResultBlock`) already lives in
//! [`crate::anthropic::parser`].

use agentscope_core::{Error, Result};
use serde_json::{json, Value};

/// Known server-tool `type` prefixes.
///
/// Anthropic versions every server tool (`web_search_20250305`), so validation
/// is prefix-based. `custom` (a client tool) is explicitly rejected, matching
/// Java's `toolUnion.isTool()` check.
pub const SERVER_TOOL_TYPE_PREFIXES: &[&str] = &[
    "web_search_",
    "web_fetch_",
    "code_execution_",
    "bash_code_execution_",
    "text_editor_code_execution_",
    "tool_search",
    "computer_",
    "memory_",
];

/// A validated Anthropic server-tool declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct AnthropicServerTool {
    definition: Value,
    name: String,
}

impl AnthropicServerTool {
    /// Wraps a wire-level server tool definition after validation.
    ///
    /// Rejected: non-objects, missing/blank `name`, missing `type`, `type`
    /// `custom` (that is a client tool), and `type` outside
    /// [`SERVER_TOOL_TYPE_PREFIXES`].
    pub fn new(definition: Value) -> Result<Self> {
        let object = definition.as_object().ok_or_else(|| {
            Error::Model("Anthropic server tool definition must be a JSON object".to_string())
        })?;
        let kind = object.get("type").and_then(Value::as_str).ok_or_else(|| {
            Error::Model("Anthropic server tool definition must carry a string 'type'".to_string())
        })?;
        if kind == "custom" {
            return Err(Error::Model(
                "AnthropicServerTool does not accept custom client tools".to_string(),
            ));
        }
        if !SERVER_TOOL_TYPE_PREFIXES
            .iter()
            .any(|prefix| kind.starts_with(prefix))
        {
            return Err(Error::Model(format!(
                "Unsupported Anthropic server tool type '{kind}'; expected one of the prefixes {}",
                SERVER_TOOL_TYPE_PREFIXES.join(", ")
            )));
        }
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                Error::Model("Anthropic server tool name must not be blank".to_string())
            })?
            .to_string();
        Ok(Self { definition, name })
    }

    /// Anthropic tool name (e.g. `web_search`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The validated wire definition, ready to be added to `tools`.
    pub fn definition(&self) -> &Value {
        &self.definition
    }

    /// `web_search_20250305` — Anthropic-hosted web search.
    pub fn web_search(
        max_uses: Option<u32>,
        allowed_domains: Option<Vec<String>>,
        blocked_domains: Option<Vec<String>>,
    ) -> Self {
        let mut definition = json!({
            "type": "web_search_20250305",
            "name": "web_search",
        });
        let map = definition.as_object_mut().expect("object literal");
        if let Some(max_uses) = max_uses {
            map.insert("max_uses".to_string(), json!(max_uses));
        }
        if let Some(domains) = allowed_domains {
            map.insert("allowed_domains".to_string(), json!(domains));
        }
        if let Some(domains) = blocked_domains {
            map.insert("blocked_domains".to_string(), json!(domains));
        }
        Self {
            definition,
            name: "web_search".to_string(),
        }
    }

    /// `web_fetch_20250910` — Anthropic-hosted page fetch.
    pub fn web_fetch(
        max_uses: Option<u32>,
        allowed_domains: Option<Vec<String>>,
        blocked_domains: Option<Vec<String>>,
    ) -> Self {
        let mut definition = json!({
            "type": "web_fetch_20250910",
            "name": "web_fetch",
        });
        let map = definition.as_object_mut().expect("object literal");
        if let Some(max_uses) = max_uses {
            map.insert("max_uses".to_string(), json!(max_uses));
        }
        if let Some(domains) = allowed_domains {
            map.insert("allowed_domains".to_string(), json!(domains));
        }
        if let Some(domains) = blocked_domains {
            map.insert("blocked_domains".to_string(), json!(domains));
        }
        Self {
            definition,
            name: "web_fetch".to_string(),
        }
    }

    /// `code_execution_20250522` — Python execution sandbox.
    pub fn code_execution() -> Self {
        Self {
            definition: json!({
                "type": "code_execution_20250522",
                "name": "code_execution",
            }),
            name: "code_execution".to_string(),
        }
    }

    /// `bash_code_execution_20250522` — bash execution sandbox.
    pub fn bash_code_execution() -> Self {
        Self {
            definition: json!({
                "type": "bash_code_execution_20250522",
                "name": "bash_code_execution",
            }),
            name: "bash_code_execution".to_string(),
        }
    }

    /// `text_editor_code_execution_20250522` — text-editor tool in the sandbox.
    pub fn text_editor_code_execution() -> Self {
        Self {
            definition: json!({
                "type": "text_editor_code_execution_20250522",
                "name": "text_editor_code_execution",
            }),
            name: "text_editor_code_execution".to_string(),
        }
    }

    /// `tool_search_tool_regex_20251119` — server-side tool search.
    pub fn tool_search() -> Self {
        Self {
            definition: json!({
                "type": "tool_search_tool_regex_20251119",
                "name": "tool_search_tool_regex",
            }),
            name: "tool_search_tool_regex".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_server_tools_validate_and_expose_their_name() {
        let search = AnthropicServerTool::web_search(
            Some(3),
            Some(vec!["docs.anthropic.com".to_string()]),
            None,
        );
        assert_eq!(search.name(), "web_search");
        assert_eq!(search.definition()["max_uses"], json!(3));
        assert_eq!(
            search.definition()["allowed_domains"],
            json!(["docs.anthropic.com"])
        );
        assert!(search.definition().get("blocked_domains").is_none());

        for tool in [
            AnthropicServerTool::code_execution(),
            AnthropicServerTool::bash_code_execution(),
            AnthropicServerTool::text_editor_code_execution(),
            AnthropicServerTool::tool_search(),
            AnthropicServerTool::web_fetch(None, None, None),
        ] {
            // Round-trips through the validator.
            let validated = AnthropicServerTool::new(tool.definition().clone()).unwrap();
            assert_eq!(validated, tool);
        }
    }

    #[test]
    fn custom_tools_and_malformed_definitions_are_rejected() {
        let custom = json!({"type": "custom", "name": "my_tool"});
        assert!(AnthropicServerTool::new(custom)
            .unwrap_err()
            .to_string()
            .contains("custom client tools"));

        let unknown = json!({"type": "mystery_20250101", "name": "mystery"});
        assert!(AnthropicServerTool::new(unknown)
            .unwrap_err()
            .to_string()
            .contains("Unsupported Anthropic server tool type"));

        let no_name = json!({"type": "web_search_20250305"});
        assert!(AnthropicServerTool::new(no_name)
            .unwrap_err()
            .to_string()
            .contains("name must not be blank"));

        let no_type = json!({"name": "web_search"});
        assert!(AnthropicServerTool::new(no_type).is_err());
        assert!(AnthropicServerTool::new(json!("nope")).is_err());
    }
}
