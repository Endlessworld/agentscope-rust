//! List-hash change-detection helper, mirroring `io.agentscope.core.state.ListHashUtil`.
//!
//! Rust `serde_json::Value` does not expose a stable `hashCode`, so this port hashes the
//! canonical JSON representation of the sampled elements. The *strategy* (size bound to
//! `"size:N;"`, sample-all for `<= 5`, else `[0, n/4, n/2, 3n/4, n-1]`) matches Java; only
//! the per-element hash input differs (JSON text vs. Java `hashCode`), which is fine because
//! the value is only ever compared against itself stored in the same Redis instance.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde_json::Value;

/// Empty list hash constant.
pub const EMPTY_HASH: &str = "empty:0";

/// Threshold for using the sampling strategy.
const SAMPLING_THRESHOLD: usize = 5;

fn sample_indices(size: usize) -> Vec<usize> {
    if size <= SAMPLING_THRESHOLD {
        (0..size).collect()
    } else {
        vec![0, size / 4, size / 2, size * 3 / 4, size - 1]
    }
}

/// Computes a lightweight hash of a list of JSON values.
pub fn compute_hash(values: &[Value]) -> String {
    if values.is_empty() {
        return EMPTY_HASH.to_string();
    }
    let mut sb = String::new();
    sb.push_str(&format!("size:{};", values.len()));
    for idx in sample_indices(values.len()) {
        let item_hash = element_hash(values.get(idx));
        sb.push_str(&format!("{idx}:{item_hash},"));
    }
    let mut hasher = DefaultHasher::new();
    sb.hash(&mut hasher);
    let h = hasher.finish() as u32;
    format!("{h:x}")
}

fn element_hash(value: Option<&Value>) -> u64 {
    match value {
        None | Some(Value::Null) => 0,
        Some(v) => {
            let mut hasher = DefaultHasher::new();
            v.to_string().hash(&mut hasher);
            hasher.finish()
        }
    }
}

/// Whether the list changed relative to a stored hash.
///
/// A `None` stored hash is treated as "no previous hash" → `false` (matches Java).
pub fn has_changed(current_hash: &str, stored_hash: Option<&str>) -> bool {
    match stored_hash {
        None => false,
        Some(h) => h != current_hash,
    }
}

/// Determines whether a full rewrite is required vs. an incremental append.
pub fn needs_full_rewrite(current: &[Value], stored_hash: Option<&str>, existing_count: usize) -> bool {
    let current_size = current.len();

    // Case 1: list shrunk (items deleted).
    if current_size < existing_count {
        return true;
    }
    // Case 2: missing hash but existing data found.
    if stored_hash.is_none() && existing_count > 0 {
        return true;
    }
    // Case 3: previously existing elements modified.
    let prefix = &current[..existing_count.min(current_size)];
    let prefix_hash = compute_hash(prefix);
    has_changed(&prefix_hash, stored_hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_hash_matches_java_constant() {
        assert_eq!(compute_hash(&[]), EMPTY_HASH);
    }

    #[test]
    fn unchanged_prefix_needs_no_rewrite() {
        let a = vec![json!({"v":1}), json!({"v":2})];
        let stored = compute_hash(&a);
        assert!(!needs_full_rewrite(&a, Some(&stored), 2));
    }

    #[test]
    fn append_growth_needs_no_rewrite() {
        let a = vec![json!({"v":1}), json!({"v":2})];
        let stored = compute_hash(&a);
        let b = vec![json!({"v":1}), json!({"v":2}), json!({"v":3})];
        assert!(!needs_full_rewrite(&b, Some(&stored), 2));
    }

    #[test]
    fn shrink_or_edit_needs_rewrite() {
        let a = vec![json!({"v":1}), json!({"v":2}), json!({"v":3})];
        let stored = compute_hash(&a);
        assert!(needs_full_rewrite(&a[..2], Some(&stored), 3)); // shrink
        let edited = vec![json!({"v":9}), json!({"v":2}), json!({"v":3})];
        assert!(needs_full_rewrite(&edited, Some(&stored), 3)); // prefix edit
    }

    #[test]
    fn missing_hash_with_existing_data_needs_rewrite() {
        let a = vec![json!({"v":1})];
        assert!(needs_full_rewrite(&a, None, 1));
        assert!(!needs_full_rewrite(&a, None, 0));
    }
}
