//! The SQL this crate sends, in one place.
//!
//! Every statement is a constant so that the driver adapter holds no SQL, the in-memory double
//! can recognise what it is asked, and a test can assert the placeholder count against the
//! parameter list that ships with it. Placeholders are numbered from `$1` in the order of the
//! parameters the corresponding [`crate::Param`] slice carries.

/// Creates the scope row when the scope is written for the first time.
///
/// Parameters: `$1` scope key, `$2` now.
pub const ENSURE_SCOPE: &str = "INSERT INTO agentscope_scope (scope_key, revision, updated_at) \
     VALUES ($1, 0, $2) ON CONFLICT (scope_key) DO NOTHING";

/// Takes the scope's row lock and answers the revision the writes will be checked against.
///
/// The lock is the serialisation primitive: two hosts writing one scope are serialised by this
/// row, which is what a scope lock file is on a single disk. Parameters: `$1` scope key.
pub const LOCK_SCOPE: &str =
    "SELECT revision FROM agentscope_scope WHERE scope_key = $1 FOR UPDATE";

/// Advances the revision, but only from the one the transaction read.
///
/// Zero rows updated means the read set was stale when the writes landed, which is a conflict
/// rather than a lost update. Parameters: `$1` scope key, `$2` expected revision, `$3` now.
pub const BUMP_REVISION: &str =
    "UPDATE agentscope_scope SET revision = revision + 1, updated_at = $3 \
     WHERE scope_key = $1 AND revision = $2";

/// One record. Parameters: `$1` scope key, `$2` part key.
pub const SELECT_PART: &str = "SELECT v FROM agentscope_part WHERE scope_key = $1 AND k = $2";

/// Several records in one round trip, for a control poll that needs one record and not a scope.
/// Parameters: `$1` scope key, `$2` part keys.
pub const SELECT_PARTS: &str =
    "SELECT k, v FROM agentscope_part WHERE scope_key = $1 AND k = ANY($2::text[]) ORDER BY k";

/// Writes one record. Parameters: `$1` scope key, `$2` part key, `$3` value as JSON text.
pub const UPSERT_PART: &str =
    "INSERT INTO agentscope_part (scope_key, k, v) VALUES ($1, $2, $3::jsonb) \
     ON CONFLICT (scope_key, k) DO UPDATE SET v = EXCLUDED.v";

/// Drops one record. Parameters: `$1` scope key, `$2` part key.
pub const DELETE_PART: &str = "DELETE FROM agentscope_part WHERE scope_key = $1 AND k = $2";

/// A page of keys under a prefix, starting at the prefix itself.
/// Parameters: `$1` scope key, `$2` prefix, `$3` limit.
pub const KEYS_FROM_PREFIX: &str =
    "SELECT k FROM agentscope_part WHERE scope_key = $1 AND k >= $2 ORDER BY k LIMIT $3";

/// A page of keys under a prefix, starting after a cursor.
/// Parameters: `$1` scope key, `$2` cursor, `$3` limit.
pub const KEYS_AFTER: &str =
    "SELECT k FROM agentscope_part WHERE scope_key = $1 AND k > $2 ORDER BY k LIMIT $3";

/// A page of keys under a prefix that has an exclusive upper bound.
/// Parameters: `$1` scope key, `$2` prefix, `$3` upper bound, `$4` limit.
pub const KEYS_FROM_PREFIX_BOUNDED: &str = "SELECT k FROM agentscope_part \
     WHERE scope_key = $1 AND k >= $2 AND k < $3 ORDER BY k LIMIT $4";

/// A page of keys after a cursor, with an exclusive upper bound.
/// Parameters: `$1` scope key, `$2` cursor, `$3` upper bound, `$4` limit.
pub const KEYS_AFTER_BOUNDED: &str = "SELECT k FROM agentscope_part \
     WHERE scope_key = $1 AND k > $2 AND k < $3 ORDER BY k LIMIT $4";

/// Due entries of one queue, in readiness order.
/// Parameters: `$1` queue, `$2` now, `$3` limit.
pub const READY: &str = "SELECT work_id, ready_at, payload FROM agentscope_work \
     WHERE queue = $1 AND ready_at >= 0 AND ready_at <= $2 ORDER BY ready_at, work_id LIMIT $3";

/// Writes or replaces one readiness entry.
/// Parameters: `$1` queue, `$2` work id, `$3` ready at, `$4` payload (nullable).
pub const UPSERT_WORK: &str = "INSERT INTO agentscope_work (queue, work_id, ready_at, payload) \
     VALUES ($1, $2, $3, $4::jsonb) ON CONFLICT (queue, work_id) \
     DO UPDATE SET ready_at = EXCLUDED.ready_at, payload = EXCLUDED.payload";

/// Removes one readiness entry. Parameters: `$1` queue, `$2` work id.
pub const DELETE_WORK: &str = "DELETE FROM agentscope_work WHERE queue = $1 AND work_id = $2";

#[cfg(test)]
mod tests {
    use super::*;

    /// The count of `$n` placeholders a statement uses, and therefore the parameters it takes.
    fn placeholders(statement: &str) -> usize {
        let mut highest = 0usize;
        let bytes: Vec<char> = statement.chars().collect();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == '$' {
                let mut number = String::new();
                while index + 1 < bytes.len() && bytes[index + 1].is_ascii_digit() {
                    index += 1;
                    number.push(bytes[index]);
                }
                if !number.is_empty() {
                    highest = highest.max(number.parse::<usize>().expect("placeholder number"));
                }
            }
            index += 1;
        }
        highest
    }

    /// Every statement is numbered from `$1` without gaps, so a parameter list cannot drift
    /// out of step with the text it belongs to.
    #[test]
    fn placeholders_are_numbered_from_one_without_gaps() {
        let statements: [(&str, &str); 12] = [
            ("ensure scope", ENSURE_SCOPE),
            ("lock scope", LOCK_SCOPE),
            ("bump revision", BUMP_REVISION),
            ("select part", SELECT_PART),
            ("select parts", SELECT_PARTS),
            ("upsert part", UPSERT_PART),
            ("delete part", DELETE_PART),
            ("keys from prefix", KEYS_FROM_PREFIX),
            ("keys after", KEYS_AFTER),
            ("keys from prefix bounded", KEYS_FROM_PREFIX_BOUNDED),
            ("keys after bounded", KEYS_AFTER_BOUNDED),
            ("ready", READY),
        ];
        for (name, statement) in statements {
            let highest = placeholders(statement);
            for number in 1..=highest {
                let needle = format!("${number}");
                // `$1` also matches inside `$10`; none of these statements reaches ten.
                assert!(
                    statement.contains(&needle),
                    "{name}: placeholder {needle} is missing"
                );
            }
        }
        let work = [
            ("upsert work", UPSERT_WORK, 4),
            ("delete work", DELETE_WORK, 2),
        ];
        for (name, statement, expected) in work {
            assert_eq!(placeholders(statement), expected, "{name}: placeholders");
        }
    }

    /// The parameter counts the store passes, one line per call site. This is the assertion that
    /// catches an edited statement and an unedited caller.
    #[test]
    fn the_parameter_counts_match_the_call_sites() {
        assert_eq!(placeholders(ENSURE_SCOPE), 2);
        assert_eq!(placeholders(LOCK_SCOPE), 1);
        assert_eq!(placeholders(BUMP_REVISION), 3);
        assert_eq!(placeholders(SELECT_PART), 2);
        assert_eq!(placeholders(SELECT_PARTS), 2);
        assert_eq!(placeholders(UPSERT_PART), 3);
        assert_eq!(placeholders(DELETE_PART), 2);
        assert_eq!(placeholders(KEYS_FROM_PREFIX), 3);
        assert_eq!(placeholders(KEYS_AFTER), 3);
        assert_eq!(placeholders(KEYS_FROM_PREFIX_BOUNDED), 4);
        assert_eq!(placeholders(KEYS_AFTER_BOUNDED), 4);
        assert_eq!(placeholders(READY), 3);
        assert_eq!(placeholders(UPSERT_WORK), 4);
        assert_eq!(placeholders(DELETE_WORK), 2);
    }

    /// A page cursor narrows the range, so the cursor is a bound of the index scan and not a
    /// filter applied after it — otherwise paging a long journal re-reads it from its start.
    #[test]
    fn every_key_page_bounds_the_index_range() {
        for statement in [
            KEYS_FROM_PREFIX,
            KEYS_AFTER,
            KEYS_FROM_PREFIX_BOUNDED,
            KEYS_AFTER_BOUNDED,
        ] {
            assert!(statement.contains("WHERE scope_key = $1"), "{statement}");
            assert!(statement.contains("ORDER BY k"), "{statement}");
            assert!(statement.contains("LIMIT"), "{statement}");
            assert!(!statement.contains("LIKE"), "{statement}");
            assert!(!statement.contains("OFFSET"), "{statement}");
        }
    }
}
