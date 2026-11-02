//! Real `SqlDatabase` drivers (feature-gated).
//!
//! [`crate::sql`] defines a *synchronous* seam (`SqlDatabase` / `SqlConnection`) because the
//! backend contract it serves is synchronous (see [`crate::AgentSkillRepository`]). Both
//! drivers offered by the Rust ecosystem are **async**, so this module provides the missing
//! bridge plus one adapter per backend:
//!
//! | backend | driver crate | feature | TLS feature |
//! |---|---|---|---|
//! | PostgreSQL | `tokio-postgres` 0.7 | `postgres-driver` | `postgres-tls` (`rustls`) |
//! | MySQL | `mysql_async` 0.37 | `mysql-driver` | `mysql-tls` (`rustls`) |
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
//!   TLS-requiring server needs the opt-in `mysql-tls` feature (`rustls` + `ring`, still no
//!   `aws-lc-sys`) or a plain-TCP proxy.
//!
//! ## TLS is opt-in
//!
//! `postgres-tls` / `mysql-tls` add the `rustls` stack (plus `tokio-rustls`,
//! `rustls-native-certs` and `webpki-roots` for PostgreSQL) to the build. Neither the default
//! build nor a driver-only build compiles any of it: TLS is a deliberate, explicit choice, and
//! the connection string (`sslmode=require` / `?require_ssl=true`) then decides whether it is
//! used.
//!
//! ## Default build stays dependency-free
//!
//! `default = []`: enabling neither feature leaves `tokio` / `tokio-postgres` / `mysql_async`
//! completely out of the build, and [`crate::sql::InMemorySqlDatabase`] remains the only
//! implementation.

use std::fmt::Display;
use std::future::Future;

use crate::error::SkillRepositoryError;

#[cfg(feature = "mysql-driver")]
mod mysql;
#[cfg(all(feature = "postgres-driver", feature = "postgres-tls"))]
mod pg_tls;
#[cfg(feature = "postgres-driver")]
mod postgres;

#[cfg(feature = "mysql-driver")]
pub use mysql::{MysqlSqlConnection, MysqlSqlDatabase};
#[cfg(feature = "postgres-driver")]
pub use postgres::{PgSqlConnection, PgSqlDatabase};

/// Wraps a driver failure in [`SkillRepositoryError::Database`].
fn db_error(context: &str, error: impl Display) -> SkillRepositoryError {
    SkillRepositoryError::Database(format!("{context}: {error}"))
}

/// A single-threaded Tokio runtime used to drive an async driver from the sync seam.
///
/// `SqlConnection` methods take `&mut self` and return owned values, so there is no cursor
/// lifetime crossing the bridge — every call is a self-contained `block_on`.
///
/// # Running inside another runtime
///
/// `block_on` cannot be nested inside another runtime, and the harness calls the synchronous
/// `SqlDatabase` seam from async code (`SkillRepository::load()` runs on a Tokio worker).
/// [`SyncBridge::run`] therefore hands the future to a dedicated OS thread when it detects an
/// ambient runtime; see its docs for why `block_in_place` is not an option.
pub(crate) struct SyncBridge {
    runtime: Option<tokio::runtime::Runtime>,
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
        Ok(Self {
            runtime: Some(runtime),
            backend,
        })
    }

    /// The bridge runtime; only absent once the bridge is being dropped.
    fn runtime(&self) -> &tokio::runtime::Runtime {
        self.runtime
            .as_ref()
            .expect("runtime is present until drop")
    }

    /// Runs `future` to completion on the bridge runtime.
    ///
    /// `block_on` must not be nested in another runtime (Tokio panics with "Cannot start a
    /// runtime from within a runtime"), yet the harness drives this *synchronous* seam from async
    /// code: `SkillRepository::load()` is called straight from a Tokio worker
    /// (`harness/agent/skill/manage.rs`, `harness_agent.rs`). Reporting an error there made every
    /// MySQL / PostgreSQL skill repository useless for a real agent, so when an ambient runtime is
    /// detected the future is moved to a **dedicated OS thread** and `block_on` runs there: a fresh
    /// thread starts with no Tokio context, which is exactly the condition `block_on` requires.
    ///
    /// `tokio::task::block_in_place` is deliberately *not* used: it panics on a current-thread
    /// runtime (the harness tests and `#[tokio::test]` default to that flavour) and it would block
    /// a worker of the caller's runtime instead of a thread we own.
    ///
    /// The driver futures are `Send` (each one owns or `&mut`-borrows its connection), so the
    /// scoped thread only needs a `Send` future and a `Send` output.
    pub(crate) fn run<F>(&self, future: F) -> Result<F::Output, SkillRepositoryError>
    where
        F: Future + Send,
        F::Output: Send,
    {
        if tokio::runtime::Handle::try_current().is_ok() {
            // `std::thread::scope` keeps the borrows (`&self.runtime`, the future) valid without
            // requiring `'static`; the join propagates a driver panic as an error instead of
            // unwinding through the caller's runtime.
            return std::thread::scope(|scope| {
                scope
                    .spawn(|| self.runtime().block_on(future))
                    .join()
                    .map_err(|_| {
                        db_error(
                            self.backend,
                            "the driver thread panicked while running a query",
                        )
                    })
            });
        }
        Ok(self.runtime().block_on(future))
    }

    /// Spawns a driver's connection task (PostgreSQL needs its `Connection` polled).
    #[cfg(feature = "postgres-driver")]
    pub(crate) fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.runtime().spawn(future);
    }
}

impl Drop for SyncBridge {
    /// Tears the bridge runtime down without waiting for its workers.
    ///
    /// A repository is normally dropped on a Tokio worker (it lives inside an
    /// `Arc<dyn AgentSkillRepository>` owned by an async agent), and dropping a multi-thread
    /// `Runtime` there panics with "Cannot drop a runtime in a context where blocking is not
    /// allowed", because the default drop waits for the blocking pool. `shutdown_background`
    /// never waits, so the drop is safe both inside and outside async contexts.
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // SKILLS-01: the bridge must work from inside a Tokio runtime
    // ------------------------------------------------------------------

    /// The harness calls the synchronous `SqlDatabase` seam from async code
    /// (`SkillRepository::load()` runs on a Tokio worker), so the bridge must be able to drive a
    /// driver future while an ambient runtime is current. Before the fix `run` returned
    /// `Database(... must be driven from a blocking thread)`, which the repository adapter then
    /// turned into an empty skill table.
    ///
    /// The default `#[tokio::test]` flavour is a *current-thread* runtime - the case where
    /// `block_in_place` panics - so this also pins the no-`block_in_place` decision.
    #[tokio::test]
    async fn sync_bridge_runs_inside_a_tokio_runtime() {
        let bridge = SyncBridge::new("test").expect("bridge runtime");
        let answer = bridge
            .run(async { 42 })
            .expect("a driver future must be drivable from async code");
        assert_eq!(answer, 42);
    }

    /// A driver-level failure is still a *value*: the `Result` crosses the bridge untouched, so a
    /// failed query is never mistaken for a broken bridge.
    #[tokio::test]
    async fn sync_bridge_forwards_the_future_output_inside_a_tokio_runtime() {
        let bridge = SyncBridge::new("test").expect("bridge runtime");
        let outcome: Result<u8, &str> = bridge
            .run(async { Err("driver said no") })
            .expect("the bridge itself must not fail");
        assert_eq!(outcome, Err("driver said no"));
    }

    /// Outside a runtime the future is still driven on the caller's thread (no extra hop, and the
    /// pre-existing sync contract is unchanged).
    #[test]
    fn sync_bridge_runs_outside_a_tokio_runtime() {
        let bridge = SyncBridge::new("test").expect("bridge runtime");
        assert_eq!(bridge.run(async { 7 }).expect("run"), 7);
    }

    #[cfg(feature = "postgres-driver")]
    #[test]
    fn placeholders_are_numbered_in_order() {
        assert_eq!(
            pg_placeholders("INSERT INTO t (a, b, c) VALUES (?, ?, ?)"),
            "INSERT INTO t (a, b, c) VALUES ($1, $2, $3)"
        );
        assert_eq!(pg_placeholders("SELECT 1"), "SELECT 1");
    }

    #[cfg(feature = "postgres-driver")]
    #[test]
    fn placeholders_inside_quoted_text_are_left_alone() {
        assert_eq!(
            pg_placeholders("SELECT '?' AS q, \"a?b\" FROM t WHERE name = ?"),
            "SELECT '?' AS q, \"a?b\" FROM t WHERE name = $1"
        );
    }

    #[cfg(feature = "postgres-driver")]
    #[test]
    fn placeholders_are_renumbered_from_one_per_statement() {
        assert_eq!(pg_placeholders("SELECT ?"), "SELECT $1");
        assert_eq!(
            pg_placeholders("DELETE FROM t WHERE id = ?"),
            "DELETE FROM t WHERE id = $1"
        );
    }
}
