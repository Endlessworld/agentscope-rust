//! Recursively escapes and restores MongoDB field names inside arbitrary JSON
//! values, mirroring `io.agentscope.extensions.mongodb.MongoKeyEscaper`.
//!
//! MongoDB rejects `.` and a leading `$` in field names. Since state/BaseStore
//! values are caller-supplied JSON objects, their keys can contain such characters
//! and would otherwise fail the write at encode time. This utility uses a
//! backslash-prefix encoding on write and its inverse on read so callers are not
//! restricted by MongoDB naming rules.
//!
//! The encoding is collision-free: `.` → `\E`, `$` → `\$`, `\0` → `\N`, and a
//! literal backslash → `\\`. Escaping the backslash itself is what guarantees a
//! bijective round-trip — a key that literally contains `\E` is stored as `\\E`
//! and restored to `\E`, never confused with an escaped dot.

use serde_json::{Map, Value};

const ESCAPE_PREFIX: char = '\\';
const DOT_CODE: char = 'E';
const DOLLAR_CODE: char = '$';
const NULL_CODE: char = 'N';
const BACKSLASH_CODE: char = '\\';

/// Escapes every object key recursively (nested objects and array elements
/// included).
pub fn escape(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(escape_key(k), escape(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(escape).collect()),
        other => other.clone(),
    }
}

/// Restores every object key recursively (nested objects and array elements
/// included).
pub fn unescape(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(unescape_key(k), unescape(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(unescape).collect()),
        other => other.clone(),
    }
}

/// Escapes a single object key. Exposed for direct testing.
pub fn escape_key(key: &str) -> String {
    if !key.contains(['.', '$', '\0', ESCAPE_PREFIX]) {
        return key.to_string();
    }
    let mut sb = String::with_capacity(key.len() + 4);
    for c in key.chars() {
        match c {
            '.' => {
                sb.push(ESCAPE_PREFIX);
                sb.push(DOT_CODE);
            }
            '$' => {
                sb.push(ESCAPE_PREFIX);
                sb.push(DOLLAR_CODE);
            }
            '\0' => {
                sb.push(ESCAPE_PREFIX);
                sb.push(NULL_CODE);
            }
            ESCAPE_PREFIX => {
                sb.push(ESCAPE_PREFIX);
                sb.push(BACKSLASH_CODE);
            }
            other => sb.push(other),
        }
    }
    sb
}

/// Restores a single object key. Exposed for direct testing.
pub fn unescape_key(key: &str) -> String {
    if !key.contains(ESCAPE_PREFIX) {
        return key.to_string();
    }
    let chars: Vec<char> = key.chars().collect();
    let mut sb = String::with_capacity(key.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == ESCAPE_PREFIX && i + 1 < chars.len() {
            let next = chars[i + 1];
            match next {
                DOT_CODE => sb.push('.'),
                DOLLAR_CODE => sb.push('$'),
                NULL_CODE => sb.push('\0'),
                BACKSLASH_CODE => sb.push(ESCAPE_PREFIX),
                // Unknown escape sequence (can only come from pre-escaping data); keep as-is.
                _ => {
                    sb.push(c);
                    sb.push(next);
                }
            }
            i += 2;
        } else {
            sb.push(c);
            i += 1;
        }
    }
    sb
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_keys_are_unchanged() {
        assert_eq!(escape_key("hello"), "hello");
        assert_eq!(unescape_key("hello"), "hello");
    }

    #[test]
    fn dot_and_dollar_are_escaped() {
        assert_eq!(escape_key("a.b"), "a\\Eb");
        assert_eq!(escape_key("$set"), "\\$set");
        assert_eq!(unescape_key("a\\Eb"), "a.b");
        assert_eq!(unescape_key("\\$set"), "$set");
    }

    #[test]
    fn null_and_backslash_are_escaped() {
        assert_eq!(escape_key("a\0b"), "a\\Nb");
        assert_eq!(unescape_key("a\\Nb"), "a\0b");
        assert_eq!(escape_key("a\\b"), "a\\\\b");
        assert_eq!(unescape_key("a\\\\b"), "a\\b");
    }

    #[test]
    fn round_trip_is_bijective() {
        for key in ["a.b", "$x", "a\\Eb", "\\\\", "a\0b", "plain", ".$\\\0"] {
            assert_eq!(unescape_key(&escape_key(key)), key, "key={key:?}");
        }
    }

    #[test]
    fn unknown_escape_sequence_is_kept() {
        // "\Q" is not a recognised escape; keep both chars verbatim.
        assert_eq!(unescape_key("a\\Qb"), "a\\Qb");
    }

    #[test]
    fn recursive_escape_and_unescape() {
        let value = json!({
            "a.b": 1,
            "nested": { "c$d": { "e.f": 2 } },
            "list": [ { "g$h": 3 }, 4, "plain" ]
        });
        let escaped = escape(&value);
        let obj = escaped.as_object().unwrap();
        assert!(obj.contains_key("a\\Eb"));
        let nested = obj.get("nested").unwrap().as_object().unwrap();
        assert!(nested.contains_key("c\\$d"));
        let restored = unescape(&escaped);
        assert_eq!(restored, value);
    }
}
