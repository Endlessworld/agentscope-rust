//! Whitelist validation for table / column identifiers.
//!
//! Identifiers are the only user-supplied strings this crate splices into DDL/DML
//! verbatim (MySQL cannot bind an identifier as a parameter), so they are validated
//! against a strict whitelist before they can reach a statement. Mirrors Java
//! `JdbcStore.Builder#tableName` and `MysqlAgentStateStore#IDENTIFIER_PATTERN`
//! (`^[a-zA-Z_][a-zA-Z0-9_-]*$`, 64-char limit).
//!
//! Anything else — quotes, backticks, comment markers, statement separators, dots,
//! whitespace inside the name, over-long names — is rejected with
//! [`MysqlError::InvalidArgument`] instead of being concatenated into SQL.
//!
//! Java allows `-` in identifiers (common in production names such as
//! `agentscope-prod`). MySQL only accepts such a name when it is backtick-quoted, so every
//! splice point quotes through [`quote_identifier`] — validated first, which rules out a
//! backtick inside the name.

use crate::error::{MysqlError, MysqlResult};

/// Maximum identifier length, matching MySQL's table/column name limit.
pub const MAX_IDENTIFIER_LENGTH: usize = 64;

/// Validates `name` as a table/column identifier, returning it without surrounding
/// whitespace.
pub fn validate_identifier(name: &str, label: &str) -> MysqlResult<String> {
    let trimmed = name.trim();
    let mut chars = trimmed.chars();
    let head_ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
    if !head_ok || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(MysqlError::InvalidArgument(format!(
            "{label} must match [A-Za-z_][A-Za-z0-9_-]*, got: {name:?}"
        )));
    }
    if trimmed.len() > MAX_IDENTIFIER_LENGTH {
        return Err(MysqlError::InvalidArgument(format!(
            "{label} exceeds {MAX_IDENTIFIER_LENGTH} characters: {name:?}"
        )));
    }
    Ok(trimmed.to_string())
}

/// Wraps `name` in MySQL backticks.
///
/// Callers validate first ([`validate_identifier`]), which rules out a backtick inside the
/// name, so quoting cannot be escaped out of. MySQL only accepts an identifier containing
/// `-` when it is quoted.
pub fn quote_identifier(name: &str) -> String {
    format!("`{name}`")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_identifiers() {
        assert_eq!(
            validate_identifier("agentscope_sessions", "t").unwrap(),
            "agentscope_sessions"
        );
        assert_eq!(validate_identifier("_ok", "t").unwrap(), "_ok");
        assert_eq!(validate_identifier("a_1", "t").unwrap(), "a_1");
        assert_eq!(validate_identifier("A9", "t").unwrap(), "A9");
    }

    #[test]
    fn trims_surrounding_whitespace() {
        assert_eq!(validate_identifier("  users  ", "t").unwrap(), "users");
    }

    #[test]
    fn rejects_injection_payloads() {
        for bad in [
            "",
            "   ",
            "1tbl",
            "tbl name",
            "`tbl`",
            "tbl'",
            "tbl\"",
            "tbl; DROP TABLE agentscope_sessions",
            "tbl/*c*/",
            "agent.scope",
            "tbl-name;",
            "tbl)",
        ] {
            assert!(
                validate_identifier(bad, "Table name").is_err(),
                "expected rejection of {bad:?}"
            );
        }
    }

    #[test]
    fn quotes_with_backticks() {
        assert_eq!(quote_identifier("agentscope-prod"), "`agentscope-prod`");
    }

    #[test]
    fn enforces_length_limit() {
        assert!(validate_identifier(&"a".repeat(MAX_IDENTIFIER_LENGTH), "t").is_ok());
        let err = validate_identifier(&"a".repeat(MAX_IDENTIFIER_LENGTH + 1), "t").unwrap_err();
        assert!(
            err.to_string().contains("exceeds"),
            "unexpected error: {err}"
        );
    }

    /// Java's pattern allows `-` (the name is backtick-quoted at every splice point).
    #[test]
    fn accepts_hyphenated_identifiers() {
        assert_eq!(
            validate_identifier("agentscope-prod", "t").unwrap(),
            "agentscope-prod"
        );
        assert_eq!(validate_identifier("a-b_c", "t").unwrap(), "a-b_c");
        // Still no leading `-`, no dot, no whitespace.
        for bad in ["-lead", "a.b", "a b"] {
            assert!(
                validate_identifier(bad, "t").is_err(),
                "expected rejection of {bad:?}"
            );
        }
    }

    #[test]
    fn error_message_names_the_label() {
        let err = validate_identifier("bad name", "Snapshot table name").unwrap_err();
        assert!(
            err.to_string().contains("Snapshot table name"),
            "unexpected error: {err}"
        );
    }
}
