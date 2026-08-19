//! Response format configuration for model APIs.
use serde::{Deserialize, Serialize};

use super::json_schema::JsonSchema;

/// Response format configuration, mirroring the Java `ResponseFormat`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResponseFormat {
    /// The type of response format: "text", "json_object", or "json_schema".
    #[serde(rename = "type", default)]
    pub format_type: String,
    /// JSON Schema specification (only for json_schema type).
    #[serde(rename = "json_schema", skip_serializing_if = "Option::is_none")]
    pub json_schema: Option<JsonSchema>,
}

impl ResponseFormat {
    /// Plain text response format.
    pub fn text() -> Self {
        Self { format_type: "text".into(), json_schema: None }
    }

    /// JSON object response format.
    pub fn json_object() -> Self {
        Self { format_type: "json_object".into(), json_schema: None }
    }

    /// JSON Schema response format with structured output validation.
    pub fn json_schema(schema: JsonSchema) -> Self {
        Self { format_type: "json_schema".into(), json_schema: Some(schema) }
    }

    pub fn builder() -> ResponseFormatBuilder {
        ResponseFormatBuilder::default()
    }
}

/// Builder for `ResponseFormat`.
#[derive(Debug, Default)]
pub struct ResponseFormatBuilder {
    format_type: Option<String>,
    json_schema: Option<JsonSchema>,
}

impl ResponseFormatBuilder {
    pub fn format_type(mut self, v: impl Into<String>) -> Self {
        self.format_type = Some(v.into());
        self
    }

    pub fn json_schema(mut self, v: JsonSchema) -> Self {
        self.format_type = Some("json_schema".into());
        self.json_schema = Some(v);
        self
    }

    pub fn build(self) -> ResponseFormat {
        ResponseFormat {
            format_type: self.format_type.unwrap_or_else(|| "text".into()),
            json_schema: self.json_schema,
        }
    }
}
