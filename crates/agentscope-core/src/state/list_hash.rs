//! List-hash utilities for change detection, mirroring `io.agentscope.core.state.ListHashUtil`.

use std::hash::{Hash, Hasher};

/// Hash emitted for an empty list.
pub const EMPTY_HASH: &str = "empty:0";

/// Below this size every element is sampled; above it key positions are sampled.
pub const SAMPLING_THRESHOLD: usize = 5;

/// Computes a lightweight sampled hash for a list of state objects.
///
/// The hash includes the list size and the hash codes of sampled elements. Large
/// lists are sampled at `0`, `1/4`, `1/2`, `3/4` and `last` to avoid O(n) work.
pub fn compute_hash<T: Hash>(values: &[T]) -> String {
    if values.is_empty() {
        return EMPTY_HASH.to_string();
    }

    let size = values.len();
    let mut s = format!("size:{size};");
    for idx in sample_indices(size) {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        values[idx].hash(&mut h);
        s.push_str(&format!("{idx}:{},", h.finish()));
    }

    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:x}", h.finish())
}

/// Returns the indices to sample from a list of the given size.
fn sample_indices(size: usize) -> Vec<usize> {
    if size <= SAMPLING_THRESHOLD {
        (0..size).collect()
    } else {
        vec![0, size / 4, size / 2, size * 3 / 4, size - 1]
    }
}

/// Returns `true` when the current hash differs from the stored hash.
pub fn has_changed(current_hash: &str, stored_hash: Option<&str>) -> bool {
    match stored_hash {
        None => false,
        Some(stored) => stored != current_hash,
    }
}

/// Determines whether a full rewrite is required versus an incremental append.
pub fn needs_full_rewrite<T: Hash>(
    current_values: &[T],
    stored_hash: Option<&str>,
    existing_count: usize,
) -> bool {
    let current_size = current_values.len();

    // Case 1: list shrank (items were removed).
    if current_size < existing_count {
        return true;
    }

    // Case 2: missing hash but existing data found — cannot verify unmodified state.
    if stored_hash.is_none() && existing_count > 0 {
        return true;
    }

    // Case 3: check whether the previously-existing prefix was modified.
    let prefix = &current_values[..existing_count];
    has_changed(&compute_hash(prefix), stored_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_list_uses_constant() {
        let v: Vec<i32> = vec![];
        assert_eq!(compute_hash(&v), EMPTY_HASH);
    }

    #[test]
    fn identical_lists_hash_identically() {
        assert_eq!(compute_hash(&[1, 2, 3]), compute_hash(&[1, 2, 3]));
        assert_ne!(compute_hash(&[1, 2, 3]), compute_hash(&[1, 2, 4]));
    }

    #[test]
    fn size_change_changes_hash() {
        assert_ne!(compute_hash(&[1, 2]), compute_hash(&[1, 2, 3]));
    }

    #[test]
    fn large_list_samples_key_positions() {
        // Same content, reordered tail must still be detected by sampling.
        let a = (0..100).collect::<Vec<i32>>();
        let mut b = a.clone();
        b[99] = -1;
        assert_ne!(compute_hash(&a), compute_hash(&b));
    }

    #[test]
    fn has_changed_semantics() {
        assert!(!has_changed("abc", None));
        assert!(has_changed("abc", Some("def")));
        assert!(!has_changed("abc", Some("abc")));
    }

    #[test]
    fn full_rewrite_rules() {
        // Shrank.
        assert!(needs_full_rewrite(&[1], Some("h"), 2));
        // Missing hash but existing data.
        assert!(needs_full_rewrite(&[1, 2], None, 1));
        // Existing count zero with no hash: no rewrite.
        assert!(!needs_full_rewrite(&[1, 2], None, 0));
        // Unchanged prefix: append is fine.
        let values = vec![1, 2, 3];
        let hash = compute_hash(&values[..2]);
        assert!(!needs_full_rewrite(&values, Some(&hash), 2));
    }
}
