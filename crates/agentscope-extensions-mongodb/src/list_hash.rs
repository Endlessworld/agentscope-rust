//! List content hashing for state list change detection, mirroring
//! `io.agentscope.core.state.ListHashUtil`.
//!
//! The hash is used to decide whether a list write can be an incremental append
//! (hash unchanged, size grew) or requires a full rewrite. It must be stable across
//! processes, so it is a SHA-256 over the canonical JSON of each element joined with
//! a unit-separator delimiter.

use sha2::{Digest, Sha256};
use serde_json::Value;

/// Delimiter between hashed list elements (ASCII Unit Separator).
const ELEMENT_DELIMITER: u8 = 0x1F;

/// Computes a stable content hash for a list of state values.
pub fn compute_hash(values: &[Value]) -> String {
    let mut hasher = Sha256::new();
    for (idx, value) in values.iter().enumerate() {
        if idx > 0 {
            hasher.update([ELEMENT_DELIMITER]);
        }
        // `to_string` on serde_json::Value is canonical for objects (BTreeMap-backed
        // Map preserves insertion order, but element identity here is object equality
        // for our purposes; we serialise the value deterministically).
        hasher.update(serde_json::to_vec(value).unwrap_or_default());
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Decides whether a list write requires a full rewrite.
///
/// Mirrors Java `ListHashUtil.needsFullRewrite`:
/// * a shrunken list (fewer elements than already stored) forces a rewrite;
/// * a stored hash that is absent while data already exists forces a rewrite
///   (the pristine state cannot be verified);
/// * otherwise the hash of the *prefix* of length `existing_count` is compared with
///   the stored hash — a match means the previously stored elements are untouched
///   and a suffix append is sufficient.
pub fn needs_full_rewrite(values: &[Value], stored_hash: Option<&str>, existing_count: usize) -> bool {
    // Case 1: list shrank (elements were removed).
    if values.len() < existing_count {
        return true;
    }
    // Case 2: no stored hash but data already exists — cannot verify, rewrite.
    if stored_hash.is_none() && existing_count > 0 {
        return true;
    }
    // Case 3: compare the hash of the previously stored prefix against the stored hash.
    let prefix = &values[..existing_count];
    let prefix_hash = compute_hash(prefix);
    match stored_hash {
        None => {
            // No stored hash and no existing data: a non-empty list still needs a rewrite
            // to establish the initial document/content.
            !values.is_empty()
        }
        Some(hash) => prefix_hash != hash,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hash_is_stable_and_order_sensitive() {
        let a = vec![json!(1), json!(2)];
        let b = vec![json!(1), json!(2)];
        let c = vec![json!(2), json!(1)];
        assert_eq!(compute_hash(&a), compute_hash(&b));
        assert_ne!(compute_hash(&a), compute_hash(&c));
    }

    #[test]
    fn empty_hash_is_deterministic() {
        assert_eq!(compute_hash(&[]), compute_hash(&[]));
    }

    #[test]
    fn new_absent_key_with_content_needs_rewrite() {
        assert!(needs_full_rewrite(&[json!(1)], None, 0));
        assert!(!needs_full_rewrite(&[], None, 0));
    }

    #[test]
    fn appended_list_does_not_need_rewrite() {
        let stored = vec![json!(1)];
        let hash = compute_hash(&stored);
        let grown = vec![json!(1), json!(2)];
        // Prefix unchanged → incremental append.
        assert!(!needs_full_rewrite(&grown, Some(&hash), 1));
    }

    #[test]
    fn changed_or_shrunken_list_needs_rewrite() {
        let stored = vec![json!(1), json!(2)];
        let hash = compute_hash(&stored);
        assert!(needs_full_rewrite(&[json!(9), json!(9)], Some(&hash), 2));
        assert!(needs_full_rewrite(&[json!(1)], Some(&hash), 2));
    }
}
