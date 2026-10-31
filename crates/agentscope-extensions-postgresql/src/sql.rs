//! SQL identifier validation, namespace encoding, and the shipped DDL.
//!
//! Mirrors the helpers in the Java `PostgresAgentStateStore` / `PostgresBaseStore`:
//! identifier validation (`[A-Za-z_][A-Za-z0-9_]*`), the ASCII unit-separator namespace
//! encoding with `!`-escaped `LIKE` prefix patterns, and the `CREATE TABLE` DDL.

use crate::error::{PgResult, PostgresError};

/// ASCII unit separator (U+001F) used between namespace segments.
pub const NS_SEPARATOR: char = '\u{1F}';

/// Escape character used in `LIKE ... ESCAPE '!'` prefix searches.
pub const LIKE_ESCAPE: char = '!';

/// PostgreSQL identifier maximum length.
pub const MAX_IDENTIFIER_LENGTH: usize = 63;

/// Validates a schema/table identifier against `[A-Za-z_][A-Za-z0-9_]*`.
pub fn validate_identifier(value: &str, name: &str) -> PgResult<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(PostgresError::InvalidIdentifier(format!(
            "{name} must not be blank"
        )));
    }
    if trimmed.len() > MAX_IDENTIFIER_LENGTH {
        return Err(PostgresError::InvalidIdentifier(format!(
            "{name} exceeds {MAX_IDENTIFIER_LENGTH} characters: {trimmed}"
        )));
    }
    let mut chars = trimmed.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(PostgresError::InvalidIdentifier(format!(
            "{name} must match [A-Za-z_][A-Za-z0-9_]*, got: {trimmed}"
        )));
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(PostgresError::InvalidIdentifier(format!(
            "{name} must match [A-Za-z_][A-Za-z0-9_]*, got: {trimmed}"
        )));
    }
    Ok(trimmed.to_string())
}

/// Encodes namespace segments into a single stored `namespace_path`.
///
/// Each segment is followed by the ASCII unit separator, so `[a, b]` becomes
/// `"a\u{1F}b\u{1F}"`. A trailing separator lets `LIKE 'prefix%'` match both the exact
/// namespace and any deeper sub-namespace. Segments containing the separator are rejected.
pub fn namespace_path(namespace: &[String]) -> PgResult<String> {
    let mut sb = String::new();
    for segment in namespace {
        if segment.contains(NS_SEPARATOR) {
            return Err(PostgresError::InvalidIdentifier(format!(
                "namespace segment must not contain the unit separator (0x1F): {segment:?}"
            )));
        }
        sb.push_str(segment);
        sb.push(NS_SEPARATOR);
    }
    Ok(sb)
}

/// Builds a `LIKE` prefix pattern (without trailing `%`) from an encoded namespace path,
/// escaping `LIKE` metacharacters with [`LIKE_ESCAPE`].
pub fn like_prefix_pattern(namespace: &[String]) -> PgResult<String> {
    let path = namespace_path(namespace)?;
    let mut sb = String::new();
    for ch in path.chars() {
        if ch == LIKE_ESCAPE || ch == '%' || ch == '_' {
            sb.push(LIKE_ESCAPE);
        }
        sb.push(ch);
    }
    sb.push('%');
    Ok(sb)
}

/// Fully qualified table name, quoting schema when present.
pub fn qualified_table(schema: Option<&str>, table: &str) -> String {
    match schema {
        Some(s) => format!("\"{s}\".\"{table}\""),
        None => table.to_string(),
    }
}

/// SQL for the agent session-state table.
///
/// `version ... DEFAULT 1` (PG-03): every write path binds `version = 1` explicitly, so the
/// default only matters for rows inserted by a **foreign** writer. `0` is the sentinel
/// `get_versioned()` reports for "row absent", so defaulting to `0` made any row created
/// without an explicit version look absent to `save_if_version(.., 0)` and turned the
/// "create if absent" CAS into a phantom success on an existing row. Java still says
/// `DEFAULT 0` (its own comment calls that the wrong default); this is a deliberate,
/// documented DDL difference — see `docs/migrations/postgresql_session_version_default.md`.
pub fn sessions_ddl(full_table: &str) -> String {
    format!(
        r#"CREATE TABLE IF NOT EXISTS {full_table} (
    session_id VARCHAR(255) NOT NULL,
    state_key  VARCHAR(255) NOT NULL,
    item_index INT          NOT NULL DEFAULT 0,
    state_data TEXT         NOT NULL,
    version    BIGINT       NOT NULL DEFAULT 1,
    created_at TIMESTAMP    DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP    DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (session_id, state_key, item_index)
)"#
    )
}

/// SQL for the remote KV store table.
pub fn store_ddl(full_table: &str) -> String {
    format!(
        r#"CREATE TABLE IF NOT EXISTS {full_table} (
    namespace_path VARCHAR(2048) NOT NULL,
    item_key       VARCHAR(255)  NOT NULL,
    value_json     TEXT          NOT NULL,
    version        BIGINT        NOT NULL,
    updated_at     BIGINT        NOT NULL,
    PRIMARY KEY (namespace_path, item_key)
)"#
    )
}

/// SQL for the sandbox snapshot BYTEA table.
pub fn snapshots_ddl(full_table: &str) -> String {
    format!(
        r#"CREATE TABLE IF NOT EXISTS {full_table} (
    snapshot_id VARCHAR(512) NOT NULL PRIMARY KEY,
    data        BYTEA        NOT NULL,
    created_at  TIMESTAMP    DEFAULT CURRENT_TIMESTAMP
)"#
    )
}

/// `ALTER TABLE ... ADD COLUMN IF NOT EXISTS version` for pre-existing session tables.
///
/// Mirrors the Java `PostgresAgentStateStore.ensureVersionColumn()`, which back-fills the
/// `version` column on databases created before versioning was introduced.
///
/// `DEFAULT 1` (not `0`): the ALTER back-fills pre-existing rows with the default, and `0`
/// is the sentinel `getVersioned()` reports for "row absent". Back-filling `0` would make
/// every pre-existing row look absent to `save_if_version(..., 0)`, which takes the INSERT
/// branch and reports a phantom CAS conflict. Both write paths start at version 1, so 1 is
/// the correct resting value for migrated rows.
pub fn ensure_version_column_ddl(full_table: &str) -> String {
    format!("ALTER TABLE {full_table} ADD COLUMN IF NOT EXISTS version BIGINT NOT NULL DEFAULT 1")
}

/// Query used to verify a schema exists (mirrors Java `verifySchemaExists`).
pub const SCHEMA_EXISTS_QUERY: &str =
    "SELECT SCHEMA_NAME FROM INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME = $1";

/// Query used to verify a table exists (mirrors Java `verifyTableExists`).
pub const TABLE_EXISTS_QUERY: &str =
    "SELECT TABLE_NAME FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_SCHEMA = $1 AND TABLE_NAME = $2";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_path_appends_trailing_separator() {
        let p = namespace_path(&["a".into(), "b".into()]).unwrap();
        assert_eq!(p, format!("a{NS_SEPARATOR}b{NS_SEPARATOR}"));
    }

    #[test]
    fn namespace_path_rejects_separator_in_segment() {
        assert!(namespace_path(&[format!("bad{NS_SEPARATOR}seg")]).is_err());
    }

    #[test]
    fn like_pattern_escapes_metacharacters() {
        let pat = like_prefix_pattern(&["a%b".into(), "c_d".into()]).unwrap();
        assert!(pat.contains("!%"));
        assert!(pat.contains("!_"));
        assert!(pat.ends_with('%'));
    }

    #[test]
    fn identifier_validation() {
        assert!(validate_identifier("agentscope", "schema").is_ok());
        assert!(validate_identifier("a1_b", "table").is_ok());
        assert!(validate_identifier("1bad", "table").is_err());
        assert!(validate_identifier("bad-name", "table").is_err());
        assert!(validate_identifier("  ", "table").is_err());
    }

    #[test]
    fn qualified_table_quotes_schema() {
        assert_eq!(qualified_table(Some("s"), "t"), "\"s\".\"t\"");
        assert_eq!(qualified_table(None, "t"), "t");
    }

    #[test]
    fn ensure_version_column_ddl_targets_full_table() {
        let ddl = ensure_version_column_ddl("\"s\".\"t\"");
        assert!(ddl.starts_with("ALTER TABLE \"s\".\"t\""));
        // Back-fill default must be 1, not 0: 0 means "row absent" to the CAS path.
        assert!(ddl.contains("ADD COLUMN IF NOT EXISTS version BIGINT NOT NULL DEFAULT 1"));
    }

    #[test]
    fn existence_queries_reference_information_schema() {
        assert!(SCHEMA_EXISTS_QUERY.contains("INFORMATION_SCHEMA.SCHEMATA"));
        assert!(TABLE_EXISTS_QUERY.contains("INFORMATION_SCHEMA.TABLES"));
        assert!(SCHEMA_EXISTS_QUERY.contains("$1"));
        assert!(TABLE_EXISTS_QUERY.contains("$1") && TABLE_EXISTS_QUERY.contains("$2"));
    }

    // ---- namespace encoding / LIKE escaping ---------------------------------

    /// Java: `PostgresBaseStoreTest#namespaceSegmentCannotContainSeparator`
    #[test]
    fn namespace_path_of_empty_is_empty_and_each_segment_is_terminated() {
        assert_eq!(namespace_path(&[]).unwrap(), "");
        let p = namespace_path(&["root".into(), "child".into(), "leaf".into()]).unwrap();
        assert_eq!(
            p,
            format!("root{NS_SEPARATOR}child{NS_SEPARATOR}leaf{NS_SEPARATOR}")
        );
        // Trailing separator => `LIKE 'root<US>%'` matches the namespace and descendants.
        assert!(p.starts_with(&format!("root{NS_SEPARATOR}")));
    }

    /// Java: `PostgresBaseStoreTest#searchEscapesLikeSpecialCharacters`
    #[test]
    fn like_prefix_pattern_escapes_escape_percent_underscore_and_appends_wildcard() {
        assert_eq!(like_prefix_pattern(&[]).unwrap(), "%");
        let pat = like_prefix_pattern(&["a!b".into(), "c%d".into(), "e_f".into()]).unwrap();
        assert!(pat.starts_with("a!!b"));
        assert!(pat.contains("!%"));
        assert!(pat.contains("!_"));
        assert!(pat.ends_with('%'));
    }

    /// Java: namespace helpers reject the reserved separator.
    #[test]
    fn namespace_and_like_helpers_reject_embedded_separator() {
        let bad = vec![format!("x{NS_SEPARATOR}y")];
        assert!(namespace_path(&bad).is_err());
        assert!(like_prefix_pattern(&bad).is_err());
    }

    // ---- identifier validation ----------------------------------------------

    /// Java: `PostgresAgentStateStoreTest#rejectsTooLongIdentifier`
    #[test]
    fn identifier_length_limit_is_63() {
        assert_eq!(MAX_IDENTIFIER_LENGTH, 63);
        assert!(validate_identifier(&"a".repeat(63), "table").is_ok());
        let err = validate_identifier(&"a".repeat(64), "table").unwrap_err();
        assert!(matches!(err, PostgresError::InvalidIdentifier(_)));
    }

    /// Java: `PostgresAgentStateStoreTest#rejectsInvalidSchemaName` etc.
    #[test]
    fn identifier_rejects_illegal_first_and_subsequent_characters() {
        assert!(validate_identifier("_ok", "x").is_ok());
        assert!(validate_identifier("a_1", "x").is_ok());
        assert!(validate_identifier("9lives", "x").is_err());
        assert!(validate_identifier("has space", "x").is_err());
        assert!(validate_identifier("has-hyphen", "x").is_err());
        assert!(validate_identifier("has.dot", "x").is_err());
        assert!(validate_identifier("", "x").is_err());
    }

    /// Identifier validation trims surrounding whitespace before checking.
    #[test]
    fn identifier_trims_whitespace() {
        assert_eq!(validate_identifier("  users  ", "table").unwrap(), "users");
        assert!(validate_identifier("\t\n", "table").is_err());
    }

    /// SQL-injection samples must all be rejected by the identifier validator (these are the
    /// payloads that would otherwise be interpolated into DDL/statement text).
    #[test]
    fn identifier_rejects_injection_samples() {
        for sample in [
            "users; DROP TABLE agentscope_sessions;--",
            "users\"--",
            "users' OR '1'='1",
            "1); DELETE FROM t; --",
            "users--",
            "users/*x*/",
            "public.users",
            "a b",
        ] {
            assert!(
                validate_identifier(sample, "table").is_err(),
                "injection sample unexpectedly accepted: {sample:?}"
            );
        }
    }

    // ---- qualified table + DDL ----------------------------------------------

    /// Java: `PostgresBaseStoreTest#builderWithCustomSchemaNameUsesQualifiedTableName`
    #[test]
    fn qualified_table_quotes_each_component() {
        assert_eq!(
            qualified_table(Some("public"), "agentscope_store"),
            "\"public\".\"agentscope_store\""
        );
        assert_eq!(
            qualified_table(None, "agentscope_store"),
            "agentscope_store"
        );
    }

    /// Java: the three shipped `CREATE TABLE IF NOT EXISTS` templates.
    ///
    /// PG-03: `version` defaults to **1**, not Java's `0` — `0` is the "row absent" sentinel
    /// of the CAS path, so a `0` default made an existing row look absent to
    /// `save_if_version(.., 0)` and let it overwrite data it should have rejected.
    #[test]
    fn sessions_ddl_declares_primary_key_and_version_column() {
        let ddl = sessions_ddl("\"s\".\"t\"");
        assert!(ddl.starts_with("CREATE TABLE IF NOT EXISTS \"s\".\"t\""));
        assert!(ddl.contains("PRIMARY KEY (session_id, state_key, item_index)"));
        assert!(ddl.contains("state_data TEXT         NOT NULL"));
        assert!(ddl.contains("version    BIGINT       NOT NULL DEFAULT 1"));
        // Regression guard: the absent-row sentinel must never be the version default
        // (`item_index ... DEFAULT 0` is unrelated and stays).
        assert!(!ddl.contains("version    BIGINT       NOT NULL DEFAULT 0"));
    }

    /// Java: the KV store DDL.
    #[test]
    fn store_ddl_declares_namespace_key_primary_key() {
        let ddl = store_ddl("agentscope_store");
        assert!(ddl.starts_with("CREATE TABLE IF NOT EXISTS agentscope_store"));
        assert!(ddl.contains("namespace_path VARCHAR(2048) NOT NULL"));
        assert!(ddl.contains("value_json     TEXT          NOT NULL"));
        assert!(ddl.contains("PRIMARY KEY (namespace_path, item_key)"));
    }

    /// Java: the snapshot DDL (BYTEA column keyed by `snapshot_id`).
    #[test]
    fn snapshots_ddl_declares_bytea_primary_key() {
        let ddl = snapshots_ddl("agentscope_snapshots");
        assert!(ddl.starts_with("CREATE TABLE IF NOT EXISTS agentscope_snapshots"));
        assert!(ddl.contains("snapshot_id VARCHAR(512) NOT NULL PRIMARY KEY"));
        assert!(ddl.contains("data        BYTEA        NOT NULL"));
    }

    /// `ensure_version_column_ddl` and the existence queries are available for the
    /// pre-existing-database migration / verification paths.
    #[test]
    fn migration_and_verification_sql_are_well_formed() {
        let ddl = ensure_version_column_ddl("\"agentscope\".\"agentscope_sessions\"");
        assert!(ddl.starts_with("ALTER TABLE \"agentscope\".\"agentscope_sessions\""));
        assert!(ddl.ends_with("ADD COLUMN IF NOT EXISTS version BIGINT NOT NULL DEFAULT 1"));
        assert!(SCHEMA_EXISTS_QUERY.contains("WHERE SCHEMA_NAME = $1"));
        assert!(TABLE_EXISTS_QUERY.contains("WHERE TABLE_SCHEMA = $1 AND TABLE_NAME = $2"));
    }
}
