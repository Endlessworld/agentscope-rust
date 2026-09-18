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
        return Err(PostgresError::InvalidIdentifier(format!("{name} must not be blank")));
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
pub fn sessions_ddl(full_table: &str) -> String {
    format!(
        r#"CREATE TABLE IF NOT EXISTS {full_table} (
    session_id VARCHAR(255) NOT NULL,
    state_key  VARCHAR(255) NOT NULL,
    item_index INT          NOT NULL DEFAULT 0,
    state_data TEXT         NOT NULL,
    version    BIGINT       NOT NULL DEFAULT 0,
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
pub fn ensure_version_column_ddl(full_table: &str) -> String {
    format!(
        "ALTER TABLE {full_table} ADD COLUMN IF NOT EXISTS version BIGINT NOT NULL DEFAULT 0"
    )
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
        assert!(ddl.contains("ADD COLUMN IF NOT EXISTS version BIGINT NOT NULL DEFAULT 0"));
    }

    #[test]
    fn existence_queries_reference_information_schema() {
        assert!(SCHEMA_EXISTS_QUERY.contains("INFORMATION_SCHEMA.SCHEMATA"));
        assert!(TABLE_EXISTS_QUERY.contains("INFORMATION_SCHEMA.TABLES"));
        assert!(SCHEMA_EXISTS_QUERY.contains("$1"));
        assert!(TABLE_EXISTS_QUERY.contains("$1") && TABLE_EXISTS_QUERY.contains("$2"));
    }
}
