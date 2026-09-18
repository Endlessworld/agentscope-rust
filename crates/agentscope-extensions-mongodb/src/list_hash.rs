//! List-hash utilities for state list change detection, re-exported from the
//! canonical core helper [`agentscope_core::state::list_hash`].
//!
//! This module is a **thin re-export**: the single canonical implementation lives in
//! `agentscope_core::state::list_hash` (a faithful port of Java `ListHashUtil`).
//! An earlier revision of this crate carried a parallel SHA-256-over-canonical-JSON
//! implementation here; it was removed because it duplicated core **and** diverged
//! from the Java semantics for the `stored_hash == None && existing_count == 0` case
//! (see `docs/extensions/MONGODB__GAP_TRACKER.md`).
//!
//! Re-exporting preserves the pre-existing public paths
//! `crate::list_hash::{compute_hash, needs_full_rewrite}`.

pub use agentscope_core::state::{compute_hash, needs_full_rewrite};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn absent_hash_with_no_existing_data_needs_no_rewrite() {
        // Java `ListHashUtil.needsFullRewrite`: with no stored hash and no existing
        // data the compared prefix is empty and `hasChanged(_h, null) == false`, so a
        // full rewrite is *not* required. The initial write still happens via the
        // incremental-append branch at the call site (plain upsert of the document).
        //
        // NOTE: the previous revision of this test asserted `true` here
        // (`new_absent_key_with_content_needs_rewrite`), which contradicted Java.
        assert!(!needs_full_rewrite(&[json!(1)], None, 0));
        assert!(!needs_full_rewrite(&[] as &[serde_json::Value], None, 0));
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
        // Prefix modified.
        assert!(needs_full_rewrite(&[json!(9), json!(9)], Some(&hash), 2));
        // List shrank.
        assert!(needs_full_rewrite(&[json!(1)], Some(&hash), 2));
    }

    #[test]
    fn missing_hash_with_existing_data_needs_rewrite() {
        // Java Case 2: data already exists but the stored hash is absent → cannot
        // verify the prefix → full rewrite.
        assert!(needs_full_rewrite(&[json!(1), json!(2)], None, 2));
    }
}
