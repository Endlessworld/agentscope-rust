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
//! * `crate::memory::MemoryClient` — a pure in-memory SQL *simulator* used for the
//!   crate's unit tests (no database required).
//! * `PgSimpleClient` (feature `postgres-driver`) — wraps `tokio-postgres`.
//!
//! Keeping SQL as `(text, params)` lets the concrete stores own the schema and all
//! statement semantics, exactly like the Java module.

use crate::error::{PgResult, PostgresError};

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

/// A single `(sql, params)` statement, used by
/// [`PostgresClient::execute_in_transaction`].
#[derive(Debug, Clone, PartialEq)]
pub struct SqlStatement {
    pub sql: String,
    pub params: Vec<SqlValue>,
}

impl SqlStatement {
    pub fn new(sql: impl Into<String>, params: Vec<SqlValue>) -> Self {
        Self {
            sql: sql.into(),
            params,
        }
    }
}

/// A **dedicated** PostgreSQL session (exactly one server connection) that can hold
/// advisory locks.
///
/// PostgreSQL advisory locks are **session-scoped**: a lock is owned by the session that
/// acquired it, is re-entrant *within* that session, keeps other sessions out, and is
/// released when the session ends. Servicing `pg_try_advisory_lock` / `pg_advisory_unlock`
/// over a client that arbitrarily multiplexes statements across connections (or reuses one
/// shared connection for several logical leases) therefore silently breaks mutual exclusion
/// — two "different" leases can end up in the same session and both "succeed".
///
/// Java obtains a fresh `Connection` per lease and closes it in `PostgresLease#close()`
/// (`PostgresSandboxExecutionGuard`); this trait is the Rust equivalent.
pub trait PostgresLockSession: Send + Sync + std::fmt::Debug {
    /// `SELECT pg_try_advisory_lock($1)` on this session.
    fn try_lock(&self, lock_name: i64) -> PgResult<bool>;

    /// `SELECT pg_advisory_unlock($1)` on this session.
    fn unlock(&self, lock_name: i64) -> PgResult<bool>;

    /// Ends the session; the server releases every advisory lock it still holds.
    ///
    /// Mirrors Java `Connection#close()`. Must be idempotent.
    fn close(&mut self);
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

    /// Opens a **dedicated** session for advisory locks (see [`PostgresLockSession`]).
    ///
    /// The default implementation **fails explicitly**: a client that cannot hand out
    /// independent sessions would make `pg_try_advisory_lock` report success for concurrent
    /// leases in the same process, i.e. it would pretend to be a distributed lock. Callers
    /// (`PostgresSandboxExecutionGuard`) propagate this error instead of degrading.
    fn open_lock_session(&self) -> PgResult<Box<dyn PostgresLockSession>> {
        Err(PostgresError::DriverUnavailable(format!(
            "{self:?} does not support dedicated advisory-lock sessions; \
             refusing to report a lock as acquired"
        )))
    }

    /// Runs `statements` atomically on a **dedicated** connection (`BEGIN` … `COMMIT`),
    /// rolling back on the first failure, and returns one outcome per statement in order.
    ///
    /// Mirrors Java `PostgresAgentStateStore#executeInWriteTransaction`. This is a
    /// capability-style batch (not a closure) so the seam stays object-safe and free of
    /// async-lifetime plumbing. The default implementation **fails explicitly** rather than
    /// running the statements non-atomically behind the caller's back.
    fn execute_in_transaction(&self, statements: &[SqlStatement]) -> PgResult<Vec<QueryOutcome>> {
        let _ = statements;
        Err(PostgresError::DriverUnavailable(format!(
            "{self:?} does not support write transactions; refusing to run a multi-statement \
             write non-atomically"
        )))
    }
}
