//! Database-specific SQL for the MySQL/JDBC store, mirroring
//! `io.agentscope.extensions.mysql.store.JdbcStoreDialect` and its vendor
//! implementations.
//!
//! Each dialect renders the SQL statements that vary between vendors:
//!
//! * [`StoreDialect::create_table_sql`] — schema bootstrap.
//! * [`StoreDialect::upsert_sql`] — unconditional write, incrementing `version` on update.
//! * [`StoreDialect::insert_sql`] — create-only insert for the CAS path when
//!   `expected_version == 0` (a primary-key conflict means the row already exists).
//! * [`StoreDialect::cas_update_sql`] — conditional update for the CAS path when
//!   `expected_version > 0` (`WHERE version = ?`).
//! * [`StoreDialect::search_sql`] — prefix scan over the joined namespace path.
//!
//! The dialect also exposes its LIKE escape character and identifier quoting so the
//! store can build portable prefix queries.

use std::fmt;

/// The ASCII unit separator (U+001F) used between namespace segments in the stored
/// `namespace_path` column. Chosen because it does not collide with printable
/// identifiers and is rare in JSON / user input.
///
/// Mirrors `JdbcStore.NS_SEPARATOR`.
pub const NS_SEPARATOR: char = '\u{1F}';

/// SQL rendering rules that differ between relational databases.
pub trait StoreDialect: Send + Sync + fmt::Debug {
    /// Human-readable dialect name (e.g. `"mysql"`).
    fn name(&self) -> &str;

    /// `CREATE TABLE IF NOT EXISTS` statement. Uses `{table}` for the table name.
    fn create_table_sql(&self) -> &'static str;

    /// Insert-or-update statement. Bind order:
    /// `(namespace_path, item_key, value_json, updated_at)`; `version` is set to `1`
    /// on insert and `version + 1` on update. Uses `{table}` for the table name.
    fn upsert_sql(&self) -> &'static str;

    /// Create-only insert with `version = 1`. Bind order:
    /// `(namespace_path, item_key, value_json, updated_at)`. Uses `{table}`.
    fn insert_sql(&self) -> &'static str;

    /// Conditional update used by the CAS path when `expected_version > 0`. Bind order:
    /// `(value_json, updated_at, namespace_path, item_key, expected_version)`; sets
    /// `version = version + 1`. Uses `{table}`.
    fn cas_update_sql(&self) -> &'static str;

    /// Prefix search statement. Bind order: `(like_pattern, limit, offset)` and selects
    /// `(item_key, value_json, version)`. Uses `{table}`.
    fn search_sql(&self) -> &'static str {
        "SELECT item_key, value_json, version FROM {table} \
         WHERE namespace_path LIKE ? ESCAPE '!' \
         ORDER BY item_key ASC LIMIT ? OFFSET ?"
    }

    /// Single-row fetch. Bind order: `(namespace_path, item_key)`, selects
    /// `(value_json, version)`. Uses `{table}`.
    fn select_sql(&self) -> &'static str {
        "SELECT value_json, version FROM {table} \
         WHERE namespace_path = ? AND item_key = ?"
    }

    /// Delete statement. Bind order: `(namespace_path, item_key)`. Uses `{table}`.
    fn delete_sql(&self) -> &'static str {
        "DELETE FROM {table} WHERE namespace_path = ? AND item_key = ?"
    }

    /// Escape character used in the `LIKE ... ESCAPE` prefix query.
    fn like_escape_char(&self) -> char {
        '!'
    }

    /// Quotes `table` for this vendor.
    ///
    /// MySQL only accepts a table name containing `-` when it is backtick-quoted (and Java
    /// allows such names), so the MySQL dialect quotes **every** table name. The other
    /// dialects keep the bare name: their quoting rules are not exercised by this crate.
    fn quote_table(&self, table: &str) -> String {
        table.to_string()
    }

    /// Renders a dialect SQL template, substituting `{table}` with the table name and
    /// wrapping the placeholder-style bind markers for the target driver (kept as `?`).
    fn render(&self, template: &str, table: &str) -> String {
        template.replace("{table}", &self.quote_table(table))
    }
}

/// MySQL / MariaDB dialect, mirroring `MysqlJdbcStoreDialect`.
#[derive(Debug, Clone, Copy, Default)]
pub struct MysqlStoreDialect;

impl StoreDialect for MysqlStoreDialect {
    fn name(&self) -> &str {
        "mysql"
    }

    fn quote_table(&self, table: &str) -> String {
        crate::identifier::quote_identifier(table)
    }

    fn create_table_sql(&self) -> &'static str {
        // Keep the composite primary key under InnoDB's utf8mb4 3072-byte limit
        // ((512 + 255) * 4 = 3068 bytes, still under 3072).
        //
        // The key columns pin a binary collation: `DEFAULT CHARSET=utf8mb4` without an
        // explicit `COLLATE` resolves to the charset default, which is case-insensitive
        // (`utf8mb4_0900_ai_ci` on MySQL 8.0+, `utf8mb4_general_ci` on 5.7 and MariaDB).
        // Two keys differing only in letter case then collide on the primary key, so
        // `put(ns, "README.md", a)` followed by `put(ns, "readme.md", b)` silently
        // overwrites `a`. Payload columns keep the table default (Java 2349533a1, #3288).
        "CREATE TABLE IF NOT EXISTS {table} (\
           namespace_path VARCHAR(512) COLLATE utf8mb4_bin NOT NULL,\
           item_key       VARCHAR(255) COLLATE utf8mb4_bin NOT NULL,\
           value_json     LONGTEXT     NOT NULL,\
           version        BIGINT       NOT NULL,\
           updated_at     BIGINT       NOT NULL,\
           PRIMARY KEY (namespace_path, item_key)\
         ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"
    }

    fn upsert_sql(&self) -> &'static str {
        "INSERT INTO {table} (namespace_path, item_key, value_json, version, updated_at) \
         VALUES (?, ?, ?, 1, ?) \
         ON DUPLICATE KEY UPDATE \
           value_json = VALUES(value_json), \
           version    = version + 1, \
           updated_at = VALUES(updated_at)"
    }

    fn insert_sql(&self) -> &'static str {
        "INSERT INTO {table} (namespace_path, item_key, value_json, version, updated_at) \
         VALUES (?, ?, ?, 1, ?)"
    }

    fn cas_update_sql(&self) -> &'static str {
        "UPDATE {table} SET value_json = ?, version = version + 1, updated_at = ? \
         WHERE namespace_path = ? AND item_key = ? AND version = ?"
    }
}

/// PostgreSQL dialect, mirroring `PostgresJdbcStoreDialect`.
#[derive(Debug, Clone, Copy, Default)]
pub struct PostgresStoreDialect;

impl StoreDialect for PostgresStoreDialect {
    fn name(&self) -> &str {
        "postgresql"
    }

    fn create_table_sql(&self) -> &'static str {
        "CREATE TABLE IF NOT EXISTS {table} (\
           namespace_path VARCHAR(2048) NOT NULL,\
           item_key       VARCHAR(255) NOT NULL,\
           value_json     TEXT         NOT NULL,\
           version        BIGINT       NOT NULL,\
           updated_at     BIGINT       NOT NULL,\
           PRIMARY KEY (namespace_path, item_key)\
         )"
    }

    fn upsert_sql(&self) -> &'static str {
        "INSERT INTO {table} (namespace_path, item_key, value_json, version, updated_at) \
         VALUES (?, ?, ?, 1, ?) \
         ON CONFLICT (namespace_path, item_key) DO UPDATE \
           SET value_json = EXCLUDED.value_json, \
               version    = {table}.version + 1, \
               updated_at = EXCLUDED.updated_at"
    }

    fn insert_sql(&self) -> &'static str {
        "INSERT INTO {table} (namespace_path, item_key, value_json, version, updated_at) \
         VALUES (?, ?, ?, 1, ?)"
    }

    fn cas_update_sql(&self) -> &'static str {
        "UPDATE {table} SET value_json = ?, version = version + 1, updated_at = ? \
         WHERE namespace_path = ? AND item_key = ? AND version = ?"
    }
}

/// SQLite dialect, mirroring `SqliteJdbcStoreDialect`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqliteStoreDialect;

impl StoreDialect for SqliteStoreDialect {
    fn name(&self) -> &str {
        "sqlite"
    }

    fn create_table_sql(&self) -> &'static str {
        "CREATE TABLE IF NOT EXISTS {table} (\
           namespace_path TEXT    NOT NULL,\
           item_key       TEXT    NOT NULL,\
           value_json     TEXT    NOT NULL,\
           version        INTEGER NOT NULL,\
           updated_at     INTEGER NOT NULL,\
           PRIMARY KEY (namespace_path, item_key)\
         )"
    }

    fn upsert_sql(&self) -> &'static str {
        "INSERT INTO {table} (namespace_path, item_key, value_json, version, updated_at) \
         VALUES (?, ?, ?, 1, ?) \
         ON CONFLICT (namespace_path, item_key) DO UPDATE \
           SET value_json = excluded.value_json, \
               version    = {table}.version + 1, \
               updated_at = excluded.updated_at"
    }

    fn insert_sql(&self) -> &'static str {
        "INSERT INTO {table} (namespace_path, item_key, value_json, version, updated_at) \
         VALUES (?, ?, ?, 1, ?)"
    }

    fn cas_update_sql(&self) -> &'static str {
        "UPDATE {table} SET value_json = ?, version = version + 1, updated_at = ? \
         WHERE namespace_path = ? AND item_key = ? AND version = ?"
    }
}

/// H2 dialect, mirroring `H2JdbcStoreDialect`.
#[derive(Debug, Clone, Copy, Default)]
pub struct H2StoreDialect;

impl StoreDialect for H2StoreDialect {
    fn name(&self) -> &str {
        "h2"
    }

    fn create_table_sql(&self) -> &'static str {
        "CREATE TABLE IF NOT EXISTS {table} (\
           namespace_path VARCHAR(2048) NOT NULL,\
           item_key       VARCHAR(255) NOT NULL,\
           value_json     CLOB         NOT NULL,\
           version        BIGINT       NOT NULL,\
           updated_at     BIGINT       NOT NULL,\
           PRIMARY KEY (namespace_path, item_key)\
         )"
    }

    fn upsert_sql(&self) -> &'static str {
        "MERGE INTO {table} AS t \
         USING (VALUES (?, ?, ?, 1, ?)) AS s(namespace_path, item_key, value_json, version, updated_at) \
         ON t.namespace_path = s.namespace_path AND t.item_key = s.item_key \
         WHEN MATCHED THEN UPDATE SET \
           t.value_json = s.value_json, t.version = t.version + 1, t.updated_at = s.updated_at \
         WHEN NOT MATCHED THEN INSERT (namespace_path, item_key, value_json, version, updated_at) \
           VALUES (s.namespace_path, s.item_key, s.value_json, s.version, s.updated_at)"
    }

    fn insert_sql(&self) -> &'static str {
        "INSERT INTO {table} (namespace_path, item_key, value_json, version, updated_at) \
         VALUES (?, ?, ?, 1, ?)"
    }

    fn cas_update_sql(&self) -> &'static str {
        "UPDATE {table} SET value_json = ?, version = version + 1, updated_at = ? \
         WHERE namespace_path = ? AND item_key = ? AND version = ?"
    }
}

/// Joins namespace segments with [`NS_SEPARATOR`], always trailing the result with one so
/// prefix queries can distinguish `"a/b"` from `"a/bc"`.
///
/// Mirrors `JdbcStore.namespacePath`.
pub fn namespace_path(namespace: &[String]) -> Result<String, MysqlError> {
    let mut sb = String::new();
    for segment in namespace {
        if segment.contains(NS_SEPARATOR) {
            return Err(MysqlError::InvalidArgument(format!(
                "namespace segment must not contain the unit separator (0x1F): {segment:?}"
            )));
        }
        sb.push_str(segment);
        sb.push(NS_SEPARATOR);
    }
    Ok(sb)
}

/// Builds a `LIKE` prefix pattern for the given namespace, escaping the dialect's escape
/// character along with `%` and `_`. Mirrors `JdbcStore.likePrefixPattern`.
pub fn like_prefix_pattern(namespace: &[String], escape: char) -> Result<String, MysqlError> {
    let path = namespace_path(namespace)?;
    let mut sb = String::new();
    for ch in path.chars() {
        if ch == escape || ch == '%' || ch == '_' {
            sb.push(escape);
        }
        sb.push(ch);
    }
    sb.push('%');
    Ok(sb)
}

/// Validates that a key is non-empty. Mirrors `JdbcStore.validateKey`.
pub fn validate_key(key: &str) -> Result<(), MysqlError> {
    if key.is_empty() {
        return Err(MysqlError::InvalidArgument(
            "key must not be null or empty".into(),
        ));
    }
    Ok(())
}

use crate::error::MysqlError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_path_trails_separator() {
        let ns = vec!["a".to_string(), "b".to_string()];
        let path = namespace_path(&ns).unwrap();
        assert_eq!(path, format!("a{NS_SEPARATOR}b{NS_SEPARATOR}"));
    }

    #[test]
    fn namespace_path_rejects_separator() {
        let ns = vec![format!("a{NS_SEPARATOR}b")];
        assert!(namespace_path(&ns).is_err());
    }

    #[test]
    fn like_prefix_escapes_wildcards() {
        let ns = vec!["a%b".to_string()];
        let pat = like_prefix_pattern(&ns, '!').unwrap();
        assert_eq!(pat, format!("a!%b{NS_SEPARATOR}%"));
    }

    #[test]
    fn mysql_dialect_renders_table() {
        let sql = MysqlStoreDialect.render(MysqlStoreDialect.cas_update_sql(), "agentscope_store");
        assert!(sql.contains("agentscope_store"));
        assert!(sql.contains("version = version + 1"));
    }

    #[test]
    fn all_dialects_render_without_placeholder() {
        for d in [
            &MysqlStoreDialect as &dyn StoreDialect,
            &PostgresStoreDialect,
            &SqliteStoreDialect,
            &H2StoreDialect,
        ] {
            for tmpl in [
                d.create_table_sql(),
                d.upsert_sql(),
                d.insert_sql(),
                d.cas_update_sql(),
            ] {
                let rendered = d.render(tmpl, "t");
                assert!(
                    !rendered.contains("{table}"),
                    "dialect {} left placeholder",
                    d.name()
                );
            }
        }
    }

    #[test]
    fn validate_key_rejects_empty() {
        assert!(validate_key("").is_err());
        assert!(validate_key("k").is_ok());
    }

    // Java: DialectSqlTests#mysqlKeyColumnsUseBinaryCollation
    #[test]
    fn mysql_key_columns_use_binary_collation() {
        let ddl = MysqlStoreDialect.create_table_sql();
        assert!(ddl.contains("namespace_path VARCHAR(512) COLLATE utf8mb4_bin NOT NULL"));
        assert!(ddl.contains("item_key       VARCHAR(255) COLLATE utf8mb4_bin NOT NULL"));
        // Only the two key columns pin the collation; the payload column keeps the default.
        assert_eq!(ddl.matches("COLLATE utf8mb4_bin").count(), 2);
        assert!(ddl.contains("value_json     LONGTEXT     NOT NULL"));
        assert!(!ddl.contains("value_json     LONGTEXT     COLLATE"));
    }

    // Java: DialectSqlTests#mysqlPayloadColumnsKeepDefaultCollation — PostgreSQL, H2 and
    // SQLite already compare these columns case-sensitively, so no COLLATE clause leaks in.
    #[test]
    fn non_mysql_dialects_have_no_collation_clause() {
        for d in [
            &PostgresStoreDialect as &dyn StoreDialect,
            &SqliteStoreDialect,
            &H2StoreDialect,
        ] {
            assert!(
                !d.create_table_sql().contains("COLLATE"),
                "dialect {} leaked a collation clause",
                d.name()
            );
        }
    }

    /// `namespace_path` lengths follow each vendor class: MySQL is capped at 512 because the
    /// composite primary key must stay under InnoDB's 3072-byte utf8mb4 limit, while
    /// PostgreSQL and H2 use 2048 exactly like the Java dialects (SQLite has no limit).
    #[test]
    fn namespace_path_length_matches_each_java_dialect() {
        let ddl_of = |d: &dyn StoreDialect| d.create_table_sql().to_string();
        assert!(ddl_of(&MysqlStoreDialect).contains("namespace_path VARCHAR(512)"));
        assert!(ddl_of(&PostgresStoreDialect).contains("namespace_path VARCHAR(2048)"));
        assert!(ddl_of(&H2StoreDialect).contains("namespace_path VARCHAR(2048)"));
        assert!(ddl_of(&SqliteStoreDialect).contains("namespace_path TEXT"));
    }
}
