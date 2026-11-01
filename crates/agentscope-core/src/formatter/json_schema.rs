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
    /// Optional description of what the schema represents.
    ///
    /// Mirrors Java `JsonSchema.description`; providers that expose it (e.g. the OpenAI
    /// structured-output schema) forward it, others ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl JsonSchema {
    pub fn new(name: impl Into<String>, schema: Map<String, serde_json::Value>) -> Self {
        Self {
            name: Some(name.into()),
            schema,
            strict: None,
            description: None,
        }
    }

    /// Sets the schema description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
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
    description: Option<String>,
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

    pub fn description(mut self, v: impl Into<String>) -> Self {
        self.description = Some(v.into());
        self
    }

    pub fn build(self) -> JsonSchema {
        JsonSchema {
            name: self.name,
            schema: self.schema.unwrap_or_default(),
            strict: self.strict,
            description: self.description,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn description_round_trips_and_is_omitted_when_unset() {
        let schema = JsonSchema::builder()
            .name("Person")
            .description("A person to extract")
            .build();
        let value = serde_json::to_value(&schema).unwrap();
        assert_eq!(value["description"], json!("A person to extract"));
        let back: JsonSchema = serde_json::from_value(value).unwrap();
        assert_eq!(back.description.as_deref(), Some("A person to extract"));

        let unset = JsonSchema::new("Person", Map::new());
        let value = serde_json::to_value(&unset).unwrap();
        assert!(value.get("description").is_none(), "{value}");
        let back: JsonSchema = serde_json::from_value(json!({"name": "Person", "schema": {}})).unwrap();
        assert_eq!(back.description, None);
    }
}
