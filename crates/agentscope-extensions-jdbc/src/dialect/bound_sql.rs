//! An assembled SQL statement with its bind parameters, mirroring `BoundSql`.
//!
//! The unified return type for all dialect business-SQL methods. Consumers execute
//! [`BoundSql::sql`] and bind [`BoundSql::params`] in order.

/// A SQL statement string together with its positional bind parameters.
///
/// Mirrors Java `io.agentscope.extensions.jdbc.dialect.BoundSql`. The SQL uses the
/// database's native placeholder style (question marks for SQLite/H2/MySQL, and
/// `?`/`$n` for PostgreSQL is normalized to `?` for the generic binding loop).
#[derive(Debug, Clone, PartialEq)]
pub struct BoundSql {
    /// The SQL statement with `?` placeholders.
    pub sql: String,
    /// Bind parameters, in placeholder order.
    pub params: Vec<SqlParam>,
}

impl BoundSql {
    /// Creates a new `BoundSql` with the given SQL and params.
    pub fn new(sql: impl Into<String>, params: Vec<SqlParam>) -> Self {
        Self { sql: sql.into(), params }
    }

    /// Creates a `BoundSql` with no parameters.
    pub fn no_params(sql: impl Into<String>) -> Self {
        Self { sql: sql.into(), params: Vec::new() }
    }

    /// Creates a `BoundSql` from a list of text parameters.
    pub fn text(sql: impl Into<String>, params: Vec<String>) -> Self {
        Self {
            sql: sql.into(),
            params: params.into_iter().map(SqlParam::Text).collect(),
        }
    }
}

/// A typed bind parameter for a SQL statement.
///
/// Mirrors the heterogeneous `List<Object>` params of the Java `BoundSql`; each variant
/// maps onto the corresponding JDBC `PreparedStatement#setObject` call.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlParam {
    /// A UTF-8 text value.
    Text(String),
    /// A 64-bit signed integer (versions, timestamps, indices).
    Int(i64),
    /// A byte-blob value (snapshot archives bound as binary).
    Blob(Vec<u8>),
    /// An explicit SQL NULL.
    Null,
}

impl SqlParam {
    /// Returns the value as text, if it is a [`SqlParam::Text`].
    pub fn as_text(&self) -> Option<&str> {
        match self {
            SqlParam::Text(s) => Some(s),
            _ => None,
        }
    }

    /// Returns the value as an integer, if it is a [`SqlParam::Int`].
    pub fn as_int(&self) -> Option<i64> {
        match self {
            SqlParam::Int(i) => Some(*i),
            _ => None,
        }
    }
}

impl From<String> for SqlParam {
    fn from(value: String) -> Self {
        SqlParam::Text(value)
    }
}

impl From<&str> for SqlParam {
    fn from(value: &str) -> Self {
        SqlParam::Text(value.to_string())
    }
}

impl From<i64> for SqlParam {
    fn from(value: i64) -> Self {
        SqlParam::Int(value)
    }
}

impl From<Vec<u8>> for SqlParam {
    fn from(value: Vec<u8>) -> Self {
        SqlParam::Blob(value)
    }
}
