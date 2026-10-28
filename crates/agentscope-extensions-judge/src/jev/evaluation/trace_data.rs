//! Immutable JSON snapshots for evaluation state (`evaluation/TraceData`).
//!
//! Java copies every state map through Jackson so that no mutable agent argument is retained and
//! sizes are measured on the serialized form. `serde_json::Value` already owns its data, so the
//! "freeze" step is a move/clone and only the serialized-size rule needs porting.

use serde_json::Value;

use crate::error::JevError;

/// Serialized length of `value`, in UTF-8 bytes widened to code units.
///
/// Java measures `writeValueAsString(value).length()`, i.e. UTF-16 code units. Budgets are
/// approximate by design, so the port measures the JSON text in bytes of the serialized form
/// (`String::len`), which is never smaller than the Java count for non-ASCII text.
pub fn size(value: &Value) -> Result<usize, JevError> {
    serde_json::to_string(value)
        .map(|text| text.len())
        .map_err(|_error| JevError::invalid("evaluation state must be JSON serializable"))
}

/// Serialized length of a state map, treating a map as a JSON object.
pub fn size_of_map(state: &std::collections::BTreeMap<String, Value>) -> Result<usize, JevError> {
    size(&Value::Object(state.iter().map(|(k, v)| (k.clone(), v.clone())).collect()))
}

/// Keeps the last `limit` characters of `value`, prefixing an omission marker when truncated
/// (`SupervisionEvidence`/`JevTaskSupervisor.tail`).
pub fn tail(value: &str, limit: usize) -> String {
    let count = value.chars().count();
    if count <= limit {
        return value.to_string();
    }
    let kept: String = value.chars().skip(count - limit).collect();
    format!("[earlier content omitted]\n{kept}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_counts_the_serialized_json() {
        let value = serde_json::json!({"a":1,"b":"xy"});
        assert_eq!(size(&value).unwrap(), serde_json::to_string(&value).unwrap().len());
        assert_eq!(size(&serde_json::json!(null)).unwrap(), 4);
    }

    #[test]
    fn size_of_map_matches_the_equivalent_object() {
        let mut map = std::collections::BTreeMap::new();
        map.insert("k".to_string(), serde_json::json!([1, 2]));
        assert_eq!(size_of_map(&map).unwrap(), size(&serde_json::json!({"k":[1,2]})).unwrap());
    }

    #[test]
    fn tail_keeps_the_end_and_marks_the_omission() {
        assert_eq!(tail("abcdef", 10), "abcdef");
        assert_eq!(tail("abcdef", 3), "[earlier content omitted]\ndef");
        assert_eq!(tail("héllo", 2), "[earlier content omitted]\nlo");
    }
}
