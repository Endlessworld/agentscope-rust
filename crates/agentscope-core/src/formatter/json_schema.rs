//! JSON Schema specification used for structured output.
use serde::{Deserialize, Serialize};
use serde_json::Map;

/// JSON Schema specification, mirroring the Java `JsonSchema`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct JsonSchema {
    /// The name of the schema (used by providers as the response schema name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The schema definition (JSON Schema object).
    #[serde(default)]
    pub schema: Map<String, serde_json::Value>,
    /// Whether strict schema validation is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

impl JsonSchema {
    pub fn new(name: impl Into<String>, schema: Map<String, serde_json::Value>) -> Self {
        Self {
            name: Some(name.into()),
            schema,
            strict: None,
        }
    }

    pub fn builder() -> JsonSchemaBuilder {
        JsonSchemaBuilder::default()
    }
}

/// Builder for `JsonSchema`.
#[derive(Debug, Default)]
pub struct JsonSchemaBuilder {
    name: Option<String>,
    schema: Option<Map<String, serde_json::Value>>,
    strict: Option<bool>,
}

impl JsonSchemaBuilder {
    pub fn name(mut self, v: impl Into<String>) -> Self {
        self.name = Some(v.into());
        self
    }

    pub fn schema(mut self, v: Map<String, serde_json::Value>) -> Self {
        self.schema = Some(v);
        self
    }

    pub fn strict(mut self, v: bool) -> Self {
        self.strict = Some(v);
        self
    }

    pub fn build(self) -> JsonSchema {
        JsonSchema {
            name: self.name,
            schema: self.schema.unwrap_or_default(),
            strict: self.strict,
        }
    }
}
