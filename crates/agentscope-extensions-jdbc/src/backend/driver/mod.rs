//! Driver-backed executors: run the components against a **real** server.
//!
//! [`JdbcExecutor`] is synchronous (the components are), while every modern Rust database
//! driver is asynchronous, and the components themselves are used from async code
//! (`BaseStore` / `AgentStateStore`). Blocking the caller with `Runtime::block_on` therefore
//! panics — "cannot start a runtime from within a runtime" — the moment a store method runs
//! on a Tokio worker. So a driver executor owns a **dedicated driver thread**:
//!
//! ```text
//! component (any thread)  --job-->  driver thread (own current-thread runtime)
//!         ^                              |
//!         +---------- reply -------------+
//! ```
//!
//! The thread owns the connection, so statements are serialised naturally and a transaction
//! is simply `BEGIN` … `COMMIT` on that one connection. A driver only implements
//! [`SqlClient`]; [`DriverExecutor`] adds the threading, the transaction gate and the
//! server-scoped advisory-lock sessions.
//!
//! ## Shipped drivers
//!
//! * **PostgreSQL** — [`PostgresSqlClient`] over `tokio-postgres`, feature
//!   `postgres-executor` (off by default: the default build pulls in no driver crate).
//!   Native advisory locks (`pg_try_advisory_lock` / `pg_advisory_unlock`) are enabled.
//! * **MySQL** — [`MysqlSqlClient`] over `mysql_async`, feature `mysql-executor` (off by
//!   default). Native advisory locks (`GET_LOCK` / `RELEASE_LOCK` / `IS_USED_LOCK`) are
//!   enabled, and every lease opens its own connection: the lock session calls
//!   [`SqlClient::connect`] again, which is the exact `getConnection()` … `close()` pair the
//!   Java dialect uses.
//!
//! Both drivers are opt-in, so the default build pulls in no driver crate.
//!
//! ## Fail-closed configuration
//!
//! No driver backend ever falls back to an implicit `localhost` connection: an empty
//! configuration is [`JdbcError::Configuration`], and [`DriverExecutor::from_env`] reports a
//! clear configuration error when its environment variable is unset. The container
//! integration tests use exactly that to *skip* instead of failing.

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use super::{ColumnValue, JdbcError, JdbcExecutor, JdbcResult, JdbcSession};
use crate::dialect::{
    BoundSql, NativeLockError, NativeLockSession, SqlParam, MYSQL_GET_LOCK_SQL,
    MYSQL_RELEASE_LOCK_SQL,
};
use crate::sandbox::NativeLockFlavor;

mod worker;

use worker::DriverWorker;

#[cfg(feature = "postgres-executor")]
mod postgres;
#[cfg(feature = "postgres-executor")]
pub use postgres::{
    connect_postgres, postgres_executor_from_env, PostgresExecutor, PostgresSqlClient,
    POSTGRES_URL_ENV,
};

#[cfg(feature = "mysql-executor")]
mod mysql;
#[cfg(feature = "mysql-executor")]
pub use mysql::{
    connect_mysql, mysql_executor_from_env, MysqlExecutor, MysqlSqlClient, MYSQL_URL_ENV,
};

/// MySQL probe: session id currently holding the lock, `NULL` when free.
pub const MYSQL_IS_USED_LOCK_SQL: &str = "SELECT IS_USED_LOCK(?)";
/// PostgreSQL acquire probe, folded to `1`/`0` so every driver can decode it as an integer.
pub const PG_TRY_ADVISORY_LOCK_INT_SQL: &str =
    "SELECT CASE WHEN pg_try_advisory_lock(?) THEN 1 ELSE 0 END";
/// PostgreSQL release probe, folded to `1`/`0`.
pub const PG_ADVISORY_UNLOCK_INT_SQL: &str =
    "SELECT CASE WHEN pg_advisory_unlock(?) THEN 1 ELSE 0 END";
/// PostgreSQL diagnostics: does **any** session hold this advisory key?
pub const PG_ADVISORY_LOCK_IS_HELD_SQL: &str = "SELECT CASE WHEN EXISTS (SELECT 1 FROM \
     pg_locks WHERE locktype = 'advisory' \
     AND ((classid::bigint << 32) | objid::bigint) = ?) THEN 1 ELSE 0 END";

/// Rewrites `?` placeholders to PostgreSQL's `$1, $2, …` form.
///
/// [`BoundSql`] normalises every dialect to `?` so one generic binding loop serves them all;
/// `tokio-postgres` accepts only `$n` numbering. The rewrite is positional, so the parameter
/// vector stays valid unchanged. A `?` inside a string literal would be rewritten too — none
/// of this crate's SQL templates embed one.
pub fn to_dollar_placeholders(sql: &str) -> String {
    let mut rewritten = String::with_capacity(sql.len() + 8);
    let mut index = 0usize;
    for character in sql.chars() {
        if character == '?' {
            index += 1;
            rewritten.push('$');
            rewritten.push_str(&index.to_string());
        } else {
            rewritten.push(character);
        }
    }
    rewritten
}

/// The minimal asynchronous SQL surface a driver has to provide.
///
/// Implementations own **one** connection: [`DriverExecutor`] runs them on a dedicated thread
/// and never calls two methods concurrently, so `&mut self` is enough and no pooling is
/// required. Parameters use the crate-wide [`SqlParam`] so the components stay
/// driver-agnostic.
#[async_trait::async_trait]
pub trait SqlClient: Send + 'static {
    /// Opens a connection from a driver-specific configuration string.
    async fn connect(config: &str) -> JdbcResult<Self>
    where
        Self: Sized;

    /// Runs a mutating statement and returns the affected row count.
    async fn execute(&mut self, sql: &str, params: &[SqlParam]) -> JdbcResult<usize>;

    /// Runs a SELECT and decodes every row into column values.
    async fn query(&mut self, sql: &str, params: &[SqlParam]) -> JdbcResult<Vec<Vec<ColumnValue>>>;

    /// Runs a statement without bind parameters (DDL, `BEGIN`, `COMMIT`, `ROLLBACK`).
    ///
    /// Transaction-control statements must go through the *simple* query protocol, which is
    /// why this is a separate seam from [`Self::execute`].
    async fn batch_execute(&mut self, sql: &str) -> JdbcResult<()> {
        self.execute(sql, &[]).await.map(|_| ())
    }

    /// Opens a transaction on the connection.
    async fn begin(&mut self) -> JdbcResult<()> {
        self.batch_execute("BEGIN").await
    }

    /// Commits the open transaction.
    async fn commit(&mut self) -> JdbcResult<()> {
        self.batch_execute("COMMIT").await
    }

    /// Rolls the open transaction back.
    async fn rollback(&mut self) -> JdbcResult<()> {
        self.batch_execute("ROLLBACK").await
    }
}

/// A [`JdbcExecutor`] that drives an asynchronous [`SqlClient`] on a dedicated thread.
///
/// * `execute` / `query` — one job each, serialised by the driver thread.
/// * `in_transaction` — real `BEGIN` … `COMMIT` / `ROLLBACK` on that connection, guarded by
///   [`Self::tx_gate`] so a statement from another thread can never slip into an open
///   transaction (and therefore never get rolled back with it).
/// * `execute_batch` — atomic (runs inside `in_transaction`).
/// * `open_native_lock_session` — opens a **second** connection per call and speaks the
///   configured flavour's advisory-lock SQL, which is what `GET_LOCK` /
///   `pg_try_advisory_lock` require (the lock dies with its session).
///
/// ```text
/// // feature `postgres-executor`
/// let executor = connect_postgres("host=localhost user=postgres dbname=agentscope")?;
/// let store = JdbcDistributedStore::create(Arc::new(executor), "PostgreSQL")?;
///
/// // feature `mysql-executor`
/// let executor = connect_mysql("mysql://root:secret@127.0.0.1:3306/agentscope")?;
/// let store = JdbcDistributedStore::create(Arc::new(executor), "MySQL")?;
/// ```
pub struct DriverExecutor<C: SqlClient> {
    worker: DriverWorker<C>,
    config: String,
    native_lock_flavor: Option<NativeLockFlavor>,
    /// Serialises transactions and keeps plain statements out of them.
    tx_gate: Mutex<()>,
}

impl<C: SqlClient> std::fmt::Debug for DriverExecutor<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The configuration string may carry a password: never print it.
        f.debug_struct("DriverExecutor")
            .field("native_lock_flavor", &self.native_lock_flavor)
            .finish_non_exhaustive()
    }
}

impl<C: SqlClient> DriverExecutor<C> {
    /// Connects through `config`, enabling native advisory locks when `flavor` is `Some`.
    ///
    /// An empty configuration is rejected with [`JdbcError::Configuration`] instead of
    /// silently dialling a default server.
    pub fn connect(
        config: impl Into<String>,
        native_lock_flavor: Option<NativeLockFlavor>,
    ) -> JdbcResult<Self> {
        let config = config.into();
        if config.trim().is_empty() {
            return Err(JdbcError::Configuration(
                "driver configuration is empty: pass a connection string such as \
                 `host=localhost user=postgres dbname=agentscope`"
                    .to_string(),
            ));
        }
        let worker = {
            let config = config.clone();
            DriverWorker::spawn(move |runtime| runtime.block_on(C::connect(&config)))?
        };
        Ok(Self {
            worker,
            config,
            native_lock_flavor,
            tx_gate: Mutex::new(()),
        })
    }

    /// Connects using the environment variable `variable`.
    ///
    /// Unset/blank yields [`JdbcError::Configuration`] — the usual way for integration tests
    /// to detect "no server available" and skip.
    pub fn from_env(
        variable: &str,
        native_lock_flavor: Option<NativeLockFlavor>,
    ) -> JdbcResult<Self> {
        match std::env::var(variable) {
            Ok(value) if !value.trim().is_empty() => Self::connect(value, native_lock_flavor),
            _ => Err(JdbcError::Configuration(format!(
                "{variable} is not set: driver backends stay disabled without a server URL"
            ))),
        }
    }

    /// The configuration string this executor was built with.
    pub fn config(&self) -> &str {
        &self.config
    }

    /// The native advisory-lock flavour, or `None` when the backend has none.
    pub fn native_lock_flavor(&self) -> Option<NativeLockFlavor> {
        self.native_lock_flavor
    }

    fn gate(&self) -> MutexGuard<'_, ()> {
        self.tx_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn execute_statement(&self, bound: &BoundSql) -> JdbcResult<usize> {
        let sql = bound.sql.clone();
        let params = bound.params.clone();
        self.worker
            .call(move |client, runtime| runtime.block_on(client.execute(&sql, &params)))
    }

    fn query_statement(&self, bound: &BoundSql) -> JdbcResult<Vec<Vec<ColumnValue>>> {
        let sql = bound.sql.clone();
        let params = bound.params.clone();
        self.worker
            .call(move |client, runtime| runtime.block_on(client.query(&sql, &params)))
    }

    /// Runs one transaction-control statement (`BEGIN` / `COMMIT` / `ROLLBACK`).
    fn transaction_op<F>(&self, op: F) -> JdbcResult<()>
    where
        F: FnOnce(&mut C, &tokio::runtime::Runtime) -> JdbcResult<()> + Send + 'static,
    {
        self.worker.call(op)
    }
}

impl<C: SqlClient> JdbcExecutor for DriverExecutor<C> {
    fn execute(&self, bound: &BoundSql) -> JdbcResult<usize> {
        let _gate = self.gate();
        self.execute_statement(bound)
    }

    fn query(&self, bound: &BoundSql) -> JdbcResult<Vec<Vec<ColumnValue>>> {
        let _gate = self.gate();
        self.query_statement(bound)
    }

    /// Atomic, like the SQLite backend: the whole batch runs in one transaction.
    fn execute_batch(&self, bounds: &[BoundSql]) -> JdbcResult<()> {
        let bounds = bounds.to_vec();
        self.in_transaction(&mut |session| {
            for bound in &bounds {
                session.execute(bound)?;
            }
            Ok(())
        })
    }

    fn in_transaction(
        &self,
        work: &mut dyn FnMut(&mut dyn JdbcSession) -> JdbcResult<()>,
    ) -> JdbcResult<()> {
        // Held for the whole transaction: statements from other threads must not join (and be
        // rolled back with) this transaction. The `work` closure goes through
        // `DriverSession`, which deliberately does not take the gate again.
        let _gate = self.gate();
        self.transaction_op(|client, runtime| runtime.block_on(client.begin()))?;
        let mut session = DriverSession { executor: self };
        match work(&mut session) {
            Ok(()) => match self.transaction_op(|client, runtime| runtime.block_on(client.commit()))
            {
                Ok(()) => Ok(()),
                Err(error) => {
                    let _ = self.transaction_op(|client, runtime| runtime.block_on(client.rollback()));
                    Err(error)
                }
            },
            Err(error) => {
                let _ = self.transaction_op(|client, runtime| runtime.block_on(client.rollback()));
                Err(error)
            }
        }
    }

    fn open_native_lock_session(&self) -> JdbcResult<Box<dyn NativeLockSession>> {
        let flavor = self.native_lock_flavor.ok_or_else(|| {
            JdbcError::Unsupported(format!(
                "{} is not configured for native advisory locks: pass a flavor to connect()",
                std::any::type_name::<Self>()
            ))
        })?;
        let config = self.config.clone();
        let worker = DriverWorker::spawn(move |runtime| runtime.block_on(C::connect(&config)))?;
        Ok(Box::new(DriverNativeLockSession { worker, flavor }))
    }
}

/// Transaction-scoped view handed to [`JdbcExecutor::in_transaction`] callbacks.
struct DriverSession<'a, C: SqlClient> {
    executor: &'a DriverExecutor<C>,
}

impl<C: SqlClient> JdbcSession for DriverSession<'_, C> {
    fn execute(&self, bound: &BoundSql) -> JdbcResult<usize> {
        self.executor.execute_statement(bound)
    }

    fn query(&self, bound: &BoundSql) -> JdbcResult<Vec<Vec<ColumnValue>>> {
        self.executor.query_statement(bound)
    }
}

/// A dedicated connection able to hold a server-scoped advisory lock.
///
/// Opening one is a *new* connection per acquisition (the lock dies with the session), which
/// is exactly the Java `getDataSource().getConnection()` … `close()` pair. Dropping it closes
/// the connection and therefore releases any lock still held.
struct DriverNativeLockSession<C: SqlClient> {
    worker: DriverWorker<C>,
    flavor: NativeLockFlavor,
}

impl<C: SqlClient> std::fmt::Debug for DriverNativeLockSession<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverNativeLockSession")
            .field("flavor", &self.flavor)
            .finish_non_exhaustive()
    }
}

impl<C: SqlClient> DriverNativeLockSession<C> {
    /// Runs `sql` on the dedicated connection and reads the first integer cell.
    ///
    /// `Ok(None)` means SQL `NULL` (MySQL reports "no such lock" that way).
    fn probe_int(
        &self,
        sql: &'static str,
        params: Vec<SqlParam>,
    ) -> Result<Option<i64>, NativeLockError> {
        let rows = self
            .worker
            .call(move |client, runtime| runtime.block_on(client.query(sql, &params)))
            .map_err(|error| NativeLockError::Failed(error.to_string()))?;
        Ok(rows
            .first()
            .and_then(|row| row.first())
            .and_then(|value| value.as_int()))
    }
}

/// Parses the strategy-supplied PostgreSQL advisory key (a decimal `i64`).
fn postgres_advisory_key(lock_key: &str) -> Result<i64, NativeLockError> {
    lock_key.parse::<i64>().map_err(|_| {
        NativeLockError::Failed(format!(
            "'{lock_key}' is not a PostgreSQL advisory-lock key (expected a decimal i64)"
        ))
    })
}

impl<C: SqlClient> NativeLockSession for DriverNativeLockSession<C> {
    fn try_acquire(&self, lock_key: &str, timeout: Duration) -> Result<bool, NativeLockError> {
        match self.flavor {
            NativeLockFlavor::Mysql => {
                let params = vec![
                    SqlParam::Text(lock_key.to_string()),
                    SqlParam::Int(timeout.as_secs() as i64),
                ];
                Ok(self
                    .probe_int(MYSQL_GET_LOCK_SQL, params)?
                    .map(|value| value == 1)
                    .unwrap_or(false))
            }
            NativeLockFlavor::Postgres => {
                let key = postgres_advisory_key(lock_key)?;
                Ok(self
                    .probe_int(PG_TRY_ADVISORY_LOCK_INT_SQL, vec![SqlParam::Int(key)])?
                    .map(|value| value == 1)
                    .unwrap_or(false))
            }
        }
    }

    fn release(&self, lock_key: &str) -> Result<(), NativeLockError> {
        match self.flavor {
            NativeLockFlavor::Mysql => self
                .probe_int(MYSQL_RELEASE_LOCK_SQL, vec![SqlParam::Text(lock_key.to_string())])
                .map(|_| ()),
            NativeLockFlavor::Postgres => {
                let key = postgres_advisory_key(lock_key)?;
                self.probe_int(PG_ADVISORY_UNLOCK_INT_SQL, vec![SqlParam::Int(key)])
                    .map(|_| ())
            }
        }
    }

    fn is_held(&self, lock_key: &str) -> Result<bool, NativeLockError> {
        match self.flavor {
            NativeLockFlavor::Mysql => Ok(self
                .probe_int(MYSQL_IS_USED_LOCK_SQL, vec![SqlParam::Text(lock_key.to_string())])?
                .is_some()),
            NativeLockFlavor::Postgres => {
                let key = postgres_advisory_key(lock_key)?;
                Ok(self
                    .probe_int(PG_ADVISORY_LOCK_IS_HELD_SQL, vec![SqlParam::Int(key)])?
                    .map(|value| value == 1)
                    .unwrap_or(false))
            }
        }
    }
}
