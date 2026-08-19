//! JSON Schema validation for tool inputs, mirroring
//! `io.agentscope.core.tool.ToolValidator`.
use std::collections::HashSet;

use serde_json::{Map, Value};

/// Validates tool input against a JSON Schema.
pub struct ToolValidator;

impl ToolValidator {
    /// Validates `input` against `schema`. Returns a human-readable error
    /// message on failure, or `None` when the input is valid (or the schema is
    /// empty, which means no validation is required).
    pub fn validate_input(schema: &Map<String, Value>, input: &Value) -> Option<String> {
        if schema.is_empty() {
            return None;
        }
        let normalized = normalize_optional_null_fields(input, schema);
        let validator = match jsonschema::validator_for(&Value::Object(schema.clone())) {
            Ok(v) => v,
            Err(e) => return Some(format!("Schema validation error: {}", e)),
        };
        match validator.validate(&normalized) {
            Ok(()) => None,
            Err(e) => Some(e.to_string()),
        }
    }
}

/// Recursively drops optional fields that are explicitly `null`, so an explicit
/// null is treated like an omitted optional field before validation.
fn normalize_optional_null_fields(input: &Value, schema: &Map<String, Value>) -> Value {
    let Value::Object(input_obj) = input else {
        return input.clone();
    };
    let props = match schema.get("properties").and_then(|p| p.as_object()) {
        Some(p) => p,
        None => return input.clone(),
    };
    let required: HashSet<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let mut out = Map::new();
    for (k, v) in input_obj {
        let keep = if v.is_null() {
            required.contains(k.as_str()) || !props.contains_key(k)
        } else {
            true
        };
        if !keep {
            continue;
        }
        let out_v = match props.get(k).and_then(|s| s.as_object()) {
            Some(sub) => normalize_optional_null_fields(v, sub),
            None => v.clone(),
        };
        out.insert(k.clone(), out_v);
    }
    Value::Object(out)
}
