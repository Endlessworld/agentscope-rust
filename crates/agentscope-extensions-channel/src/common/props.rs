//! Property-map helpers used by every `*ChannelProperties::from_map` parser.
//!
//! The Java records read a `Map<String, Object>` and stringify each value via `v.toString()`. The
//! harness `ChannelFactory::create` hands adapters a `BTreeMap<String, serde_json::Value>`, so the
//! helpers below bridge that type and reproduce Java's stringification semantics.

use std::collections::BTreeMap;

use serde_json::Value;

/// Convert the harness property map into a JSON object.
pub fn object_from_btree(props: &BTreeMap<String, Value>) -> Value {
    Value::Object(props.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

/// Read `key` as an optional string. Mirrors Java `Object.toString()` semantics: JSON null maps to
/// `None`, strings pass through, and other scalars use their textual representation.
pub fn get_str(props: &Value, key: &str) -> Option<String> {
    match props.get(key) {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(other.to_string()),
    }
}

/// Read `key` as a string, falling back to `fallback` when absent/null.
pub fn get_str_or(props: &Value, key: &str, fallback: &str) -> String {
    get_str(props, key).unwrap_or_else(|| fallback.to_string())
}

/// Read `key` as an integer (JSON number or numeric string); `None` when absent/unparseable.
pub fn get_int(props: &Value, key: &str) -> Option<i64> {
    match props.get(key) {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_strings_numbers_and_nulls() {
        let props = serde_json::json!({
            "s": "v",
            "n": 42,
            "b": true,
            "nul": null,
        });
        assert_eq!(get_str(&props, "s").as_deref(), Some("v"));
        assert_eq!(get_str(&props, "n").as_deref(), Some("42"));
        assert_eq!(get_str(&props, "b").as_deref(), Some("true"));
        assert_eq!(get_str(&props, "nul"), None);
        assert_eq!(get_str(&props, "missing"), None);
    }

    #[test]
    fn get_str_or_applies_fallback() {
        let props = serde_json::json!({ "present": "yes", "nul": null });
        assert_eq!(get_str_or(&props, "present", "dflt"), "yes");
        assert_eq!(get_str_or(&props, "nul", "dflt"), "dflt");
        assert_eq!(get_str_or(&props, "missing", "dflt"), "dflt");
    }

    #[test]
    fn get_int_accepts_numbers_and_numeric_strings() {
        let props = serde_json::json!({ "i": 7, "s": "8", "bad": "x" });
        assert_eq!(get_int(&props, "i"), Some(7));
        assert_eq!(get_int(&props, "s"), Some(8));
        assert_eq!(get_int(&props, "bad"), None);
        assert_eq!(get_int(&props, "missing"), None);
    }

    #[test]
    fn btree_conversion_preserves_entries() {
        let mut map = BTreeMap::new();
        map.insert("a".to_string(), serde_json::json!("1"));
        let obj = object_from_btree(&map);
        assert_eq!(get_str(&obj, "a").as_deref(), Some("1"));
    }
}
