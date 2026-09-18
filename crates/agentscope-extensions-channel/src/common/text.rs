//! Small text helpers shared by the platform adapters.

/// Heuristic used by the DingTalk / WeCom outbound clients to decide between the plain-text and
/// the markdown message variants. Mirrors the Java `looksLikeMarkdown` helper exactly
/// (`\n`, `**`, code fence or `[`).
pub fn looks_like_markdown(text: &str) -> bool {
    text.contains('\n') || text.contains("**") || text.contains("```") || text.contains('[')
}

/// Length-checked, byte-wise constant-time string comparison.
///
/// Mirrors the Java `constantTimeEquals` helper: unequal lengths short-circuit to `false`
/// (length is not considered secret here).
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Lower-case hex encoding (mirrors `String.format("%02x")` used by the Java verifiers).
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_heuristic_matches_java() {
        assert!(looks_like_markdown("a\nb"));
        assert!(looks_like_markdown("**bold**"));
        assert!(looks_like_markdown("```rust"));
        assert!(looks_like_markdown("[link](x)"));
        assert!(!looks_like_markdown("plain text"));
    }

    #[test]
    fn constant_time_eq_requires_same_length_and_content() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn to_hex_is_lowercase_and_padded() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(to_hex(&[]), "");
    }
}
