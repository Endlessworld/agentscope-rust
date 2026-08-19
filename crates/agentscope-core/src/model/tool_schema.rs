//! Immutable tool schema definition, mirroring the Java `ToolSchema`.
use serde::{Deserialize, Serialize};
use serde_json::Map;

/// Describes a tool's interface using JSON Schema for parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ToolSchema {
    /// The tool name.
    pub name: String,
    /// The tool description.
    pub description: String,
    /// The tool parameters as a JSON Schema.
    #[serde(default)]
    pub parameters: Map<String, serde_json::Value>,
    /// Optional tool output schema as a JSON Schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Map<String, serde_json::Value>>,
    /// Strict mode flag for schema validation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

impl ToolSchema {
    /// Creates a new `ToolSchema` with the given name and description.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters: Map::new(),
            output_schema: None,
            strict: None,
        }
    }

    /// Sets the parameters schema.
    pub fn parameters(mut self, parameters: Map<String, serde_json::Value>) -> Self {
        self.parameters = parameters;
        self
    }

    /// Sets the output schema.
    pub fn output_schema(mut self, schema: Map<String, serde_json::Value>) -> Self {
        self.output_schema = Some(schema);
        self
    }

    /// Sets the strict mode flag.
    pub fn strict(mut self, strict: bool) -> Self {
        self.strict = Some(strict);
        self
    }
}
