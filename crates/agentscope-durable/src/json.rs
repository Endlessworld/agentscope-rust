//! JSON boundaries: value cloning, canonical digests and identity validation.

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{DurableError, Result};

/// Identity fields are opaque but must be present and bounded (mirrors `DurableJson.text`).
pub fn text(value: &str, name: &str) -> Result<()> {
    text_bounded(value, name, 256)
}

/// The same rule for a field whose bound is not the default one.
///
/// An identity composed of two bounded identities is legitimately longer than either of them (a
/// derived execution id is a turn id, a separator and a call id), and validating it against the
/// bound of its *parts* rejected legal input instead of malformed input.
pub fn text_bounded(value: &str, name: &str, max: usize) -> Result<()> {
    if value.trim().is_empty() || value.chars().count() > max {
        return Err(DurableError::invalid(format!(
            "{name} must contain 1..{max} characters"
        )));
    }
    Ok(())
}

/// Values are copied at durable boundaries; Java deep-copies `JsonNode` for the same reason.
pub fn copy(value: &Value) -> Value {
    value.clone()
}

/// SHA-256 over a key-sorted canonical rendering.
///
/// The Java implementation uses RFC 8785; sorting keys recursively yields the same digest
/// for the subset of values this runtime produces (no floats, no duplicate keys).
pub fn hash(value: &Value) -> String {
    let mut canonical = String::new();
    write_canonical(value, &mut canonical);
    let digest = Sha256::digest(canonical.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}
