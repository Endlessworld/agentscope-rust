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
    /// Deferred tool definition loading flag.
    ///
    /// Providers that support tool search can defer loading this tool's parameter
    /// schema until the model searches for it. `None` means unspecified, which leaves
    /// the tool eagerly loaded. Mirrors Java `ToolSchema.getDeferLoading`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
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
            defer_loading: None,
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

    /// Sets the deferred tool definition loading flag. `None` leaves the tool eagerly
    /// loaded (Java `ToolSchema.Builder.deferLoading` accepts a nullable Boolean).
    pub fn defer_loading(mut self, defer_loading: Option<bool>) -> Self {
        self.defer_loading = defer_loading;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defer_loading_defaults_to_unspecified() {
        let schema = ToolSchema::new("t", "d");
        assert_eq!(schema.defer_loading, None);
        // Unspecified must not be serialized at all (Java sends `defer_loading` only
        // for explicitly marked tools).
        let value = serde_json::to_value(&schema).unwrap();
        assert!(value.get("deferLoading").is_none(), "{value}");
    }

    #[test]
    fn defer_loading_round_trips_when_set() {
        let schema = ToolSchema::new("t", "d").defer_loading(Some(true));
        let value = serde_json::to_value(&schema).unwrap();
        assert_eq!(value["deferLoading"], json!(true));
        let back: ToolSchema = serde_json::from_value(value).unwrap();
        assert_eq!(back.defer_loading, Some(true));

        // A legacy payload without the key deserializes to `None`.
        let legacy = json!({"name": "t", "description": "d", "parameters": {}});
        let back: ToolSchema = serde_json::from_value(legacy).unwrap();
        assert_eq!(back.defer_loading, None);
    }
}
