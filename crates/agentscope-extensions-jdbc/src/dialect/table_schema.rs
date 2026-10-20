//! Assemble-time table schema validation, mirroring Java `TableSchemaValidator`.
//!
//! Compares the columns a dialect's `CREATE TABLE` DDL declares against the columns the
//! database actually has. Comparison is by column name only and case-insensitive; extra
//! columns and type differences are not errors — they belong to the user's own
//! migrations. Unparseable DDL skips validation (a safety net, not a gate).
//!
//! Rust equivalent of the Java probe: Java runs `SELECT * FROM <table> WHERE 1=0` and
//! reads the result-set metadata. [`JdbcExecutor`] exposes no result-set metadata, so
//! each dialect answers a small `information_schema` / `pragma_table_info` query
//! instead ([`AbstractJdbcDialect::table_columns_query`]).

use crate::backend::{JdbcError, JdbcExecutor, JdbcResult};
use crate::dialect::{AbstractJdbcDialect, BoundSql};

/// Whether `name` is a plain SQL identifier.
///
/// Table names reach SQL by concatenation (identifiers cannot be bound), so this is
/// the guard before the probe query.
fn is_valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Returns the column names declared by the first parseable `CREATE TABLE` in `ddl`.
///
/// `None` means "not parseable" — the caller skips validation rather than guessing.
pub fn parse_create_table_columns(ddl: &str) -> Option<Vec<String>> {
    let lower = ddl.to_ascii_lowercase();
    let create_at = lower.find("create table")?;
    let open_rel = ddl[create_at..].find('(')?;
    let open = create_at + open_rel;

    // Match the closing paren of the column list.
    let mut depth = 0usize;
    let mut close = None;
    for (offset, ch) in ddl[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + offset);
                    break;
                }
            }
            _ => {}
        }
    }
    let body = &ddl[open + 1..close?];

    // Split on top-level commas only (type declarations such as DECIMAL(10, 2) nest).
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (offset, ch) in body.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&body[start..offset]);
                start = offset + 1;
            }
            _ => {}
        }
    }
    parts.push(&body[start..]);

    let mut columns = Vec::new();
    for part in parts {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let first = part.split_whitespace().next().unwrap_or_default();
        let keyword = first.to_ascii_lowercase();
        // Table-level constraints are not columns.
        if matches!(
            keyword.as_str(),
            "primary" | "unique" | "constraint" | "foreign" | "check" | "key" | "index" | "exclude"
        ) {
            continue;
        }
        let name = first
            .trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']')
            .to_string();
        if !name.is_empty() {
            columns.push(name);
        }
    }

    if columns.is_empty() {
        None
    } else {
        Some(columns)
    }
}

/// Validates one table's columns against the live database.
///
/// Mirrors Java `TableSchemaValidator.validate`: a missing table or a missing expected
/// column fails with the table name, the missing columns, the actual column set and the
/// reference DDL; the framework never alters existing tables.
pub fn validate_table_schema(
    executor: &dyn JdbcExecutor,
    dialect: &dyn AbstractJdbcDialect,
    table_name: &str,
    ddls: &[String],
) -> JdbcResult<()> {
    if !is_valid_identifier(table_name) {
        return Err(JdbcError::Unsupported(format!(
            "invalid JDBC table name '{table_name}': expected a plain identifier"
        )));
    }

    let expected = match ddls.iter().find_map(|ddl| parse_create_table_columns(ddl)) {
        Some(columns) => columns,
        // Unparseable DDL: nothing to check against.
        None => return Ok(()),
    };

    let rows = executor.query(&dialect.table_columns_query(table_name))?;
    if rows.is_empty() {
        return Err(JdbcError::Database(format!(
            "table '{table_name}' is missing although its DDL was executed; reference DDL: {}. \
             The framework never alters existing tables — check that the configured table name \
             matches the deployed schema.",
            ddls.join("; ")
        )));
    }

    let actual: Vec<String> = rows
        .iter()
        .filter_map(|row| {
            row.first()
                .and_then(|value| value.as_text())
                .map(str::to_string)
        })
        .collect();
    if actual.is_empty() {
        // The probe returned rows without a usable name column: treat as unparseable
        // rather than reporting a bogus mismatch.
        return Ok(());
    }

    let actual_lower: Vec<String> = actual.iter().map(|c| c.to_ascii_lowercase()).collect();
    let missing: Vec<&str> = expected
        .iter()
        .filter(|column| !actual_lower.contains(&column.to_ascii_lowercase()))
        .map(String::as_str)
        .collect();
    if missing.is_empty() {
        return Ok(());
    }

    Err(JdbcError::Database(format!(
        "table '{table_name}' is missing column(s) [{}]; actual columns: [{}]; reference DDL: {}. \
         The framework never alters existing tables — apply your own migration.",
        missing.join(", "),
        actual.join(", "),
        ddls.join("; ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_column_list() {
        let ddl = "CREATE TABLE IF NOT EXISTS t (a TEXT NOT NULL, b INTEGER NOT NULL, \
                   PRIMARY KEY (a, b))";
        assert_eq!(
            parse_create_table_columns(ddl),
            Some(vec!["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn nested_type_parens_do_not_split_columns() {
        let ddl = "CREATE TABLE t (amount DECIMAL(10, 2) NOT NULL, name VARCHAR(255), CHECK (amount > 0))";
        assert_eq!(
            parse_create_table_columns(ddl),
            Some(vec!["amount".to_string(), "name".to_string()])
        );
    }

    #[test]
    fn quoted_identifiers_are_unquoted() {
        let ddl = "CREATE TABLE t (`key` VARCHAR(64), \"value\" TEXT)";
        assert_eq!(
            parse_create_table_columns(ddl),
            Some(vec!["key".to_string(), "value".to_string()])
        );
    }

    #[test]
    fn non_create_statements_are_not_parseable() {
        assert_eq!(parse_create_table_columns("CREATE INDEX i ON t (a)"), None);
        assert_eq!(parse_create_table_columns("not sql at all"), None);
    }

    #[test]
    fn rejects_unsafe_table_names() {
        assert!(is_valid_identifier("agentscope_store"));
        assert!(is_valid_identifier("_t1"));
        assert!(!is_valid_identifier("t; DROP TABLE x"));
        assert!(!is_valid_identifier("1t"));
        assert!(!is_valid_identifier(""));
    }
}
