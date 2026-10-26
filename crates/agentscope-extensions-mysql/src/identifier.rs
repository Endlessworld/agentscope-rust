//! Whitelist validation for table / column identifiers.
//!
//! Identifiers are the only user-supplied strings this crate splices into DDL/DML
//! verbatim (MySQL cannot bind an identifier as a parameter), so they are validated
//! against a strict whitelist before they can reach a statement. Mirrors Java
//! `JdbcStore.Builder#tableName` (`[A-Za-z_][A-Za-z0-9_]*`) and
//! `MysqlAgentStateStore#validateIdentifier` (same shape, 64-char limit).
//!
//! Anything else — quotes, backticks, comment markers, statement separators, dots,
//! whitespace inside the name, over-long names — is rejected with
//! [`MysqlError::InvalidArgument`] instead of being concatenated into SQL.
//!
//! Java's state-store pattern additionally allows `-`
//! (`^[a-zA-Z_][a-zA-Z0-9_-]*$`). MySQL only accepts such a name when it is
//! backtick-quoted, and this crate never quotes, so `-` is rejected here as well.

use crate::error::{MysqlError, MysqlResult};

/// Maximum identifier length, matching MySQL's table/column name limit.
pub const MAX_IDENTIFIER_LENGTH: usize = 64;

/// Validates `name` as a table/column identifier, returning it without surrounding
/// whitespace.
pub fn validate_identifier(name: &str, label: &str) -> MysqlResult<String> {
    let trimmed = name.trim();
    let mut chars = trimmed.chars();
    let head_ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
    if !head_ok || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(MysqlError::InvalidArgument(format!(
            "{label} must match [A-Za-z_][A-Za-z0-9_]*, got: {name:?}"
        )));
    }
    if trimmed.len() > MAX_IDENTIFIER_LENGTH {
        return Err(MysqlError::InvalidArgument(format!(
            "{label} exceeds {MAX_IDENTIFIER_LENGTH} characters: {name:?}"
        )));
    }
    Ok(trimmed.to_string())
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
            "tbl--comment",
            "tbl/*c*/",
            "agent.scope",
            "tbl-name",
            "tbl)",
        ] {
            assert!(
                validate_identifier(bad, "Table name").is_err(),
                "expected rejection of {bad:?}"
            );
        }
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

    #[test]
    fn error_message_names_the_label() {
        let err = validate_identifier("bad name", "Snapshot table name").unwrap_err();
        assert!(
            err.to_string().contains("Snapshot table name"),
            "unexpected error: {err}"
        );
    }
}
