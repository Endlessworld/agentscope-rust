//! JSON boundaries: value cloning, RFC 8785 digests and identity validation.
//!
//! Digests are computed over the RFC 8785 (JCS) canonical rendering, which is what the Java
//! implementation hashes. [`hash_legacy`] keeps the pre-RFC rendering so digests persisted by an
//! older build still verify; it is never used to produce a new digest.

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

/// SHA-256 over the RFC 8785 (JCS) canonical rendering of `value`.
///
/// This is the rendering the Java implementation hashes, so receipts, event ids and execution
/// ids agree across languages.
pub fn hash(value: &Value) -> Result<String> {
    let mut canonical = String::new();
    write_canonical(value, &mut canonical)?;
    Ok(sha256_hex(canonical.as_bytes()))
}

/// SHA-256 over the canonical rendering of a JSON *string*.
///
/// Escaping a string never fails, and every derived key in this crate hashes exactly that shape
/// (storage keys, part names, thread ids), so this entry point stays infallible. Its output is
/// identical to [`hash`] on a [`Value::String`] and to the pre-RFC rendering, which is why the
/// part layout of existing sessions does not move.
pub fn hash_text(value: &str) -> String {
    let mut canonical = String::with_capacity(value.len() + 2);
    write_string(value, &mut canonical);
    sha256_hex(canonical.as_bytes())
}

/// Whether `stored` was produced by either rendering of `value`.
///
/// A digest persisted before the RFC 8785 switch has to keep verifying, otherwise existing
/// sessions, admissions, receipts and checkpoints become unreadable. The two renderings only
/// differ for values that contain a non-ASCII object key or a non-representable number, so
/// accepting the legacy one adds no ambiguity where they agree. New digests are always RFC 8785.
pub fn matches(stored: &str, value: &Value) -> bool {
    matches!(hash(value), Ok(digest) if digest == stored) || hash_legacy(value) == stored
}

/// The pre-RFC-8785 digest: keys in UTF-8 byte order, numbers as `serde_json` renders them.
///
/// Frozen on purpose — it exists only to validate what older builds persisted.
pub fn hash_legacy(value: &Value) -> String {
    let mut canonical = String::new();
    write_canonical_legacy(value, &mut canonical);
    sha256_hex(canonical.as_bytes())
}

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: [char; 16] = [
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
    ];
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push(HEX[(byte >> 4) as usize]);
        hex.push(HEX[(byte & 0xf) as usize]);
    }
    hex
}

/// RFC 8785 canonical JSON.
fn write_canonical(value: &Value, out: &mut String) -> Result<()> {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            // §3.2.3: keys sort by their UTF-16 code units, not by their UTF-8 bytes.
            keys.sort_by(|left, right| utf16_cmp(left, right));
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_canonical(&map[key], out)?;
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out)?;
            }
            out.push(']');
        }
        Value::String(text) => write_string(text, out),
        Value::Number(number) => write_number(number, out)?,
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Null => out.push_str("null"),
    }
    Ok(())
}

/// Largest integer a double represents exactly; anything above it is rounded by the double.
const MAX_EXACT_INTEGER: i64 = 1 << 53;

/// Numbers are IEEE-754 doubles rendered with ECMAScript `Number::toString` (RFC 8785 §3.2.2.3).
fn write_number(number: &serde_json::Number, out: &mut String) -> Result<()> {
    if let Some(value) = number.as_i64() {
        // A double below 1e21 prints as plain digits, so an exactly representable integer is
        // already canonical; larger integers are rounded the way a double rounds them.
        if (-MAX_EXACT_INTEGER..=MAX_EXACT_INTEGER).contains(&value) {
            out.push_str(&value.to_string());
            return Ok(());
        }
        return write_f64(value as f64, out);
    }
    if let Some(value) = number.as_u64() {
        if value <= MAX_EXACT_INTEGER as u64 {
            out.push_str(&value.to_string());
            return Ok(());
        }
        return write_f64(value as f64, out);
    }
    match number.as_f64() {
        Some(value) => write_f64(value, out),
        None => Err(DurableError::invalid(
            "Number has no IEEE-754 representation",
        )),
    }
}

fn write_f64(value: f64, out: &mut String) -> Result<()> {
    if !value.is_finite() {
        return Err(DurableError::invalid(
            "NaN and Infinity have no canonical JSON rendering",
        ));
    }
    // Both zeroes canonicalize to "0".
    if value == 0.0 {
        out.push('0');
        return Ok(());
    }
    if value.is_sign_negative() {
        out.push('-');
    }
    // `{:e}` yields the shortest round-trip digits and their decimal exponent, which is the
    // (digits, n) pair the ECMAScript algorithm starts from.
    let rendered = format!("{:e}", value.abs());
    let (mantissa, exponent) = rendered
        .split_once('e')
        .ok_or_else(|| DurableError::invalid("Unrenderable number"))?;
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let exponent: i32 = exponent
        .parse()
        .map_err(|_| DurableError::invalid("Unrenderable number"))?;
    // `n` is where the decimal point sits: the value is digits × 10^(n - k).
    let k = digits.len() as i32;
    let n = exponent + 1;
    let digits = digits.into_bytes();
    if k <= n && n <= 21 {
        out.push_str(&String::from_utf8_lossy(&digits));
        for _ in k..n {
            out.push('0');
        }
    } else if 0 < n && n <= 21 {
        for (index, digit) in digits.iter().enumerate() {
            if index as i32 == n {
                out.push('.');
            }
            out.push(*digit as char);
        }
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        for _ in n..0 {
            out.push('0');
        }
        out.push_str(&String::from_utf8_lossy(&digits));
    } else {
        out.push(digits[0] as char);
        if k > 1 {
            out.push('.');
            for digit in &digits[1..] {
                out.push(*digit as char);
            }
        }
        push_exponent(out, n - 1);
    }
    Ok(())
}

fn push_exponent(out: &mut String, exponent: i32) {
    out.push('e');
    out.push(if exponent < 0 { '-' } else { '+' });
    out.push_str(&exponent.unsigned_abs().to_string());
}

/// Strings escape only what JSON requires: the quote, the backslash, the five short control
/// escapes and `\u00xx` for the remaining control characters. Non-ASCII stays raw UTF-8.
fn write_string(value: &str, out: &mut String) {
    const HEX: [char; 16] = [
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
    ];
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                let code = control as u32;
                out.push_str("\\u00");
                out.push(HEX[(code >> 4) as usize]);
                out.push(HEX[(code & 0xf) as usize]);
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

fn utf16_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

fn write_canonical_legacy(value: &Value, out: &mut String) {
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
                write_canonical_legacy(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical_legacy(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn canonical(value: &Value) -> String {
        let mut out = String::new();
        write_canonical(value, &mut out).expect("canonical rendering");
        out
    }

    fn parsed(input: &str) -> Value {
        serde_json::from_str(input).expect("valid JSON")
    }

    /// RFC 8785 §3.2.3: keys sort by UTF-16 code units, which puts the non-BMP emoji before
    /// U+FB33 even though its UTF-8 bytes sort after it.
    #[test]
    fn object_keys_sort_by_utf16_code_units() {
        let value = parsed(
            r#"{"\u20ac":"Euro Sign","\r":"Carriage Return","\ufb33":"Hebrew Letter Dalet With Dagesh","1":"One","\ud83d\ude00":"Emoji: Grinning Face","\u0080":"Control","\u00f6":"Latin Small Letter O With Diaeresis"}"#,
        );
        assert_eq!(
            canonical(&value),
            "{\"\\r\":\"Carriage Return\",\"1\":\"One\",\"\u{80}\":\"Control\",\"\u{f6}\":\"Latin Small Letter O With Diaeresis\",\"\u{20ac}\":\"Euro Sign\",\"\u{1f600}\":\"Emoji: Grinning Face\",\"\u{fb33}\":\"Hebrew Letter Dalet With Dagesh\"}"
        );
        // The point of the change: byte order and UTF-16 order disagree here.
        assert_ne!(hash(&value).unwrap(), hash_legacy(&value));
    }

    /// RFC 8785 Appendix B number vectors, i.e. ECMAScript `Number::toString`.
    ///
    /// The formatter is fed doubles rather than decimal text: `serde_json`'s default parser is
    /// not correctly rounded (`float_roundtrip` is off), so `333333333.33333329` parses one ulp
    /// away from what a correctly rounding parser produces. That parser deviation is outside the
    /// canonicalizer; what this test pins is the rendering of a given double.
    #[test]
    fn numbers_follow_ecmascript_to_string() {
        let cases: [(f64, &str); 14] = [
            (0.0, "0"),
            (-0.0, "0"),
            (1.0, "1"),
            (4.50, "4.5"),
            (100.0, "100"),
            (2e-3, "0.002"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (333333333.33333329, "333333333.3333333"),
            (9007199254740992.0, "9007199254740992"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (5e-324, "5e-324"),
        ];
        for (input, expected) in cases {
            assert_eq!(canonical(&Value::from(input)), expected, "input {input:?}");
        }
        // An integer literal beyond 2^53 is rounded the way a double rounds it, exactly as
        // `serde_json`'s integer-preserving representation would be in ECMAScript.
        assert_eq!(canonical(&parsed("9007199254740993")), "9007199254740992");
        assert_eq!(canonical(&parsed("1.0")), "1");
    }

    /// Only the characters JSON requires are escaped; everything else stays raw.
    #[test]
    fn strings_escape_only_what_json_requires() {
        let value = json!("\u{1}\u{8}\t\n\u{c}\r\"\\/\u{7f}\u{e9}\u{20ac}\u{1f600}");
        assert_eq!(
            canonical(&value),
            "\"\\u0001\\b\\t\\n\\f\\r\\\"\\\\/\u{7f}\u{e9}\u{20ac}\u{1f600}\""
        );
    }

    /// Every derived key in the crate hashes a bare string, so those digests must not move.
    #[test]
    fn string_digests_are_unchanged_by_the_switch() {
        for text in [
            "main",
            "s",
            "\u{4f1a}\u{8bdd}:1",
            "\u{1f600}",
            "a\"b\\c\n",
            "\u{7f}\u{1}",
        ] {
            let value = Value::String(text.to_string());
            assert_eq!(hash(&value).unwrap(), hash_text(text), "{text}");
            assert_eq!(hash_text(text), hash_legacy(&value), "{text}");
        }
    }

    #[test]
    fn identical_data_hashes_identically_whatever_the_key_order() {
        let left = parsed(r#"{"b":{"y":1,"x":[1,2]},"a":null}"#);
        let right = parsed(r#"{"a":null,"b":{"x":[1,2],"y":1}}"#);
        assert_eq!(hash(&left).unwrap(), hash(&right).unwrap());
        assert_eq!(hash(&left).unwrap(), hash(&left).unwrap());
    }

    #[test]
    fn non_finite_numbers_are_refused() {
        let mut out = String::new();
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(
                write_f64(value, &mut out).is_err(),
                "{value} must be refused"
            );
        }
        // `serde_json` renders a non-finite float as `null` *before* it reaches this crate, so a
        // `Serialize`-derived digest can never see one; pin that boundary so a change in
        // `serde_json` surfaces here instead of as a silently wrong digest.
        assert!(serde_json::to_value(f64::NAN).unwrap().is_null());
        assert!(serde_json::to_value(f64::INFINITY).unwrap().is_null());
    }

    #[test]
    fn a_persisted_digest_is_accepted_under_either_rendering() {
        let value = parsed(r#"{"\u00f6":1.0}"#);
        assert!(matches(&hash(&value).unwrap(), &value));
        assert!(matches(&hash_legacy(&value), &value));
        let mut changed = value.clone();
        changed["\u{f6}"] = json!(2.0);
        assert!(!matches(&hash_legacy(&value), &changed));
        assert!(!matches(&hash(&value).unwrap(), &changed));
    }

    /// The same records under both renderings. The switch is a no-op for the shapes this crate
    /// derives keys from and only moves digests where the encodings actually disagree.
    #[test]
    fn legacy_and_current_digests_differ_only_where_the_encodings_differ() {
        let cases = [
            ("ascii spec", r#"{"sessionId":"s","threadId":"main"}"#, true),
            (
                "nested ascii",
                r#"{"turn":{"revision":3},"status":"RUNNING"}"#,
                true,
            ),
            (
                "non-ascii key",
                "{\"\u{1f600}\":\"a\",\"\u{fb33}\":\"b\"}",
                false,
            ),
            ("float", r#"{"value":1.0}"#, false),
        ];
        let mut report = String::new();
        for (name, input, unchanged) in cases {
            let value = parsed(input);
            let current = hash(&value).unwrap();
            let legacy = hash_legacy(&value);
            assert_eq!(legacy == current, unchanged, "{name}");
            report.push_str(&format!(
                "{name}: legacy={legacy} rfc8785={current} same={}\n",
                legacy == current
            ));
        }
        println!("{report}");
    }
}
