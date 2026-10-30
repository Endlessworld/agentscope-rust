//! Real `SqlDatabase` drivers (feature-gated).
//!
//! [`crate::sql`] defines a *synchronous* seam (`SqlDatabase` / `SqlConnection`) because the
//! backend contract it serves is synchronous (see [`crate::AgentSkillRepository`]). Both
//! drivers offered by the Rust ecosystem are **async**, so this module provides the missing
//! bridge plus one adapter per backend:
//!
//! | backend | driver crate | feature |
//! |---|---|---|
//! | PostgreSQL | `tokio-postgres` 0.7 | `postgres-driver` |
//! | MySQL | `mysql_async` 0.37 | `mysql-driver` |
//!
//! ## Why these two (selection rationale)
//!
//! * **No new dependency family.** Both crates are already pinned in the workspace
//!   `[workspace.dependencies]` for exactly this purpose (`存储/技能模块 P1`), so this crate
//!   only adds `optional = true` references — it does not widen the dependency surface.
//! * **`tokio-postgres` is the driver the workspace already builds.**
//!   `agentscope-extensions-postgresql` ships `PgSimpleClient` on top of it behind its own
//!   `postgres-driver` feature, so the PostgreSQL adapter here reuses a crate that is already
//!   compiled and audited rather than introducing a second PostgreSQL stack.
//! * **`sqlx` was rejected** as the shared driver: it would pull the whole `sqlx-core` /
//!   macro stack (plus TLS backends) into a crate that only needs `(sql, params) -> rows`, and
//!   its `Any` backend erases the typed decoding these repositories rely on.
//! * **`mysql_async` uses the `minimal` feature set** (no TLS backend), which keeps the build
//!   free of `aws-lc-sys` / OpenSSL. Plain-TCP connections are the common in-cluster case; a
//!   TLS-requiring server must be fronted by a proxy (documented in the tracker).
//!
//! ## Default build stays dependency-free
//!
//! `default = []`: enabling neither feature leaves `tokio` / `tokio-postgres` / `mysql_async`
//! completely out of the build, and [`crate::sql::InMemorySqlDatabase`] remains the only
//! implementation.

use std::fmt::Display;
use std::future::Future;
use std::sync::Arc;

use crate::error::SkillRepositoryError;

#[cfg(feature = "mysql-driver")]
mod mysql;
#[cfg(feature = "postgres-driver")]
mod postgres;

#[cfg(feature = "mysql-driver")]
pub use mysql::{MysqlSqlDatabase, MysqlSqlConnection};
#[cfg(feature = "postgres-driver")]
pub use postgres::{PgSqlDatabase, PgSqlConnection};

/// Wraps a driver failure in [`SkillRepositoryError::Database`].
fn db_error(context: &str, error: impl Display) -> SkillRepositoryError {
    SkillRepositoryError::Database(format!("{context}: {error}"))
}

/// A single-threaded Tokio runtime used to drive an async driver from the sync seam.
///
/// `SqlConnection` methods take `&mut self` and return owned values, so there is no cursor
/// lifetime crossing the bridge — every call is a self-contained `block_on`.
///
/// # Panics / errors
///
/// `block_on` cannot be nested inside another runtime: calling any driver method from a Tokio
/// worker thread would panic, so [`SyncBridge::new`] is used with [`SyncBridge::run`] which
/// reports the situation as [`SkillRepositoryError::Database`] instead of panicking.
pub(crate) struct SyncBridge {
    runtime: tokio::runtime::Runtime,
    backend: &'static str,
}

impl SyncBridge {
    /// Builds the (single-worker, IO-enabled) runtime used by one database handle.
    pub(crate) fn new(backend: &'static str) -> Result<Self, SkillRepositoryError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| db_error(backend, format!("failed to build runtime: {e}")))?;
        Ok(Self { runtime, backend })
    }

    /// Runs `future` to completion on the bridge runtime.
    pub(crate) fn run<F>(&self, future: F) -> Result<F::Output, SkillRepositoryError>
    where
        F: Future,
    {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(db_error(
                self.backend,
                "the synchronous `SqlDatabase` seam must be driven from a blocking thread",
            ));
        }
        Ok(self.runtime.block_on(future))
    }

    /// Spawns a driver's connection task (PostgreSQL needs its `Connection` polled).
    #[cfg(feature = "postgres-driver")]
    pub(crate) fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.runtime.spawn(future);
    }
}

/// Rewrites JDBC-style `?` placeholders into PostgreSQL `$n` placeholders.
///
/// The repositories are driver-agnostic and always emit `?` (that is what JDBC prepared
/// statements take); PostgreSQL only understands `$1..$n`. Quoted literals and quoted
/// identifiers are copied verbatim so a `?` inside `'...'` or `"..."` is never renumbered.
#[cfg(feature = "postgres-driver")]
fn pg_placeholders(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len() + 8);
    let mut next = 1usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                out.push(c);
            }
            '"' if !in_single => {
                in_double = !in_double;
                out.push(c);
            }
            '?' if !in_single && !in_double => {
                out.push('$');
                out.push_str(&next.to_string());
                next += 1;
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(feature = "postgres-driver")]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_numbered_in_order() {
        assert_eq!(
            pg_placeholders("INSERT INTO t (a, b, c) VALUES (?, ?, ?)"),
            "INSERT INTO t (a, b, c) VALUES ($1, $2, $3)"
        );
        assert_eq!(pg_placeholders("SELECT 1"), "SELECT 1");
    }

    #[test]
    fn placeholders_inside_quoted_text_are_left_alone() {
        assert_eq!(
            pg_placeholders("SELECT '?' AS q, \"a?b\" FROM t WHERE name = ?"),
            "SELECT '?' AS q, \"a?b\" FROM t WHERE name = $1"
        );
    }

    #[test]
    fn placeholders_are_renumbered_from_one_per_statement() {
        assert_eq!(pg_placeholders("SELECT ?"), "SELECT $1");
        assert_eq!(pg_placeholders("DELETE FROM t WHERE id = ?"), "DELETE FROM t WHERE id = $1");
    }
}
