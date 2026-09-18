//! A minimal, synchronous SQL client seam for PostgreSQL.
//!
//! The harness [`BaseStore`](agentscope_harness::filesystem::BaseStore) trait is
//! **synchronous**, while [`AgentStateStore`](agentscope_core::state::AgentStateStore)
//! is **async**. Rather than duplicating every statement across two executor models,
//! this crate routes all SQL through the synchronous [`PostgresClient`] trait and
//! bridges the async traits onto it with `tokio::task::spawn_blocking` (see
//! [`crate::blocking`]).
//!
//! Two implementations ship in this crate:
//! * [`crate::memory::MemoryClient`] — a pure in-memory SQL *simulator* used for the
//!   crate's unit tests (no database required).
//! * `PgSimpleClient` (feature `postgres-driver`) — wraps `tokio-postgres`.
//!
//! Keeping SQL as `(text, params)` lets the concrete stores own the schema and all
//! statement semantics, exactly like the Java module.

use crate::error::PgResult;

/// A value bound into a SQL statement.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    Null,
    Text(String),
    Int(i32),
    BigInt(i64),
    Bytes(Vec<u8>),
    Timestamp(i64),
}

impl SqlValue {
    pub fn text(v: impl Into<String>) -> Self {
        SqlValue::Text(v.into())
    }
    pub fn int(v: i32) -> Self {
        SqlValue::Int(v)
    }
    pub fn bigint(v: i64) -> Self {
        SqlValue::BigInt(v)
    }
}

impl From<&str> for SqlValue {
    fn from(v: &str) -> Self {
        SqlValue::Text(v.to_string())
    }
}

impl From<String> for SqlValue {
    fn from(v: String) -> Self {
        SqlValue::Text(v)
    }
}

impl From<i32> for SqlValue {
    fn from(v: i32) -> Self {
        SqlValue::Int(v)
    }
}

impl From<i64> for SqlValue {
    fn from(v: i64) -> Self {
        SqlValue::BigInt(v)
    }
}

/// A single result cell decoded from a query.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    Text(String),
    Int(i32),
    BigInt(i64),
    Bytes(Vec<u8>),
    Bool(bool),
}

impl Cell {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Cell::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Cell::BigInt(v) => Some(*v),
            Cell::Int(v) => Some(*v as i64),
            _ => None,
        }
    }
    pub fn as_i32(&self) -> Option<i32> {
        match self {
            Cell::Int(v) => Some(*v),
            Cell::BigInt(v) => Some(*v as i32),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Cell::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Cell::Bytes(b) => Some(b.as_slice()),
            _ => None,
        }
    }
}

/// A decoded result row (column values in select order).
pub type Row = Vec<Cell>;

/// A statement result: either an affected-row count or a set of rows.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryOutcome {
    /// A statement that mutated `n` rows.
    Affected(u64),
    /// A `SELECT` returning rows.
    Rows(Vec<Row>),
}

impl QueryOutcome {
    pub fn affected(&self) -> u64 {
        match self {
            QueryOutcome::Affected(n) => *n,
            QueryOutcome::Rows(rows) => rows.len() as u64,
        }
    }
}

/// A synchronous PostgreSQL client seam.
///
/// Implementations must be cheaply cloneable (typically `Arc` inside) and safe to
/// share across threads, since `BaseStore` methods take `&self`.
pub trait PostgresClient: Send + Sync + std::fmt::Debug {
    /// Executes a statement with bound parameters and returns the outcome.
    fn query(&self, sql: &str, params: &[SqlValue]) -> PgResult<QueryOutcome>;

    /// Executes a multi-statement batch, returning the count of statements.
    ///
    /// Used for schema initialization DDL.
    fn execute_batch(&self, sql: &str) -> PgResult<()>;
}
