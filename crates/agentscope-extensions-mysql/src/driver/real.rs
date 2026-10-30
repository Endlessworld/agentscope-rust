//! `mysql_async`-backed implementation of the four backend seams.
//!
//! The crate's seams are synchronous because they serve the synchronous `BaseStore` and
//! guard traits. This adapter therefore owns a dedicated single-worker Tokio runtime and
//! drives the async driver with `Runtime::block_on` — the same bridge
//! `agentscope-extensions-postgresql` uses for `tokio-postgres`. Async callers (the
//! `AgentStateStore` / `RemoteSnapshotClient` impls) are expected to run the synchronous
//! call on a blocking thread; the trait surface never requires it, but blocking a runtime
//! worker is bad practice.
//!
//! # Session-scoped locks
//!
//! MySQL `GET_LOCK()` is **session scoped**: the same connection can re-acquire its own
//! lock (it succeeds), and releasing from another connection does nothing. Each acquired
//! lock therefore keeps its own [`mysql_async::Conn`]; `RELEASE_LOCK` runs on that same
//! connection and the connection is then closed, matching Java's
//! `SandboxLease.close()` (`RELEASE_LOCK()` followed by `Connection#close()`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentscope_core::error::{Error as CoreError, Result as CoreResult};
use mysql_async::prelude::Queryable;
use mysql_async::{Opts, Params, Pool, Value};
use tokio::runtime::Runtime;

use super::MysqlAsyncConfig;
use crate::error::{MysqlError, MysqlResult};
use crate::guard::LockBackend;
use crate::identifier::validate_identifier;
use crate::snapshot::SnapshotBackend;
use crate::state::{CasOutcome, StateBackend};
use crate::store::SqlBackend;

/// Separator between the user namespace and the session id inside the `session_id` column.
///
/// Java's table has no user column (its store is per-tenant); the Rust core trait carries a
/// user id, so the adapter folds it into the primary key with a control character that
/// cannot appear in a validated identifier.
const SESSION_USER_SEPARATOR: char = '\u{1F}';

/// Namespace used when the caller passes no user id (`IsolationScope::Global`).
const ANON_USER: &str = "__anon__";

/// A `mysql_async` pool plus the runtime that drives it, exposed through the crate's
/// synchronous backend traits.
pub struct MysqlAsyncBackend {
    runtime: Arc<Runtime>,
    pool: Pool,
    config: MysqlAsyncConfig,
    /// Connections holding an advisory lock, keyed by lock name.
    lock_conns: Mutex<HashMap<String, mysql_async::Conn>>,
}

impl std::fmt::Debug for MysqlAsyncBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MysqlAsyncBackend")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl MysqlAsyncBackend {
    /// Connects with the default table names and pings the server.
    pub fn connect(url: &str) -> MysqlResult<Arc<Self>> {
        Self::connect_with_config(url, MysqlAsyncConfig::default())
    }

    /// Connects with explicit table names and pings the server.
    ///
    /// Every failure mode (bad URL, unbuildable runtime, unreachable server, unauthenticated
    /// user) is reported as [`MysqlError::DriverUnavailable`] right here, so a caller never
    /// receives a backend whose first real statement silently fails.
    pub fn connect_with_config(url: &str, config: MysqlAsyncConfig) -> MysqlResult<Arc<Self>> {
        validate_identifier(&config.state_table, "State table")?;
        validate_identifier(&config.store_table, "Store table")?;
        validate_identifier(&config.snapshot_table, "Snapshot table")?;

        let opts = Opts::from_url(url)
            .map_err(|e| MysqlError::DriverUnavailable(format!("invalid MySQL URL: {e}")))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| {
                MysqlError::DriverUnavailable(format!("cannot build the MySQL runtime: {e}"))
            })?;
        let pool = Pool::new(opts);
        runtime
            .block_on(async { pool.get_conn().await.map(|_| ()) })
            .map_err(|e| MysqlError::DriverUnavailable(format!("cannot connect to MySQL: {e}")))?;
        Ok(Arc::new(Self {
            runtime: Arc::new(runtime),
            pool,
            config,
            lock_conns: Mutex::new(HashMap::new()),
        }))
    }

    /// The table names this adapter uses.
    pub fn config(&self) -> &MysqlAsyncConfig {
        &self.config
    }

    fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    /// Checks out a pooled connection, mapping the driver error onto [`MysqlError`].
    fn conn(&self) -> MysqlResult<mysql_async::Conn> {
        self.block_on(async { self.pool.get_conn().await }).map_err(driver_error)
    }

    /// Folds the user namespace into the `session_id` column.
    fn session_key(&self, user_id: Option<&str>, session_id: &str) -> String {
        let user = match user_id {
            Some(u) if !u.trim().is_empty() => u,
            _ => ANON_USER,
        };
        format!("{user}{SESSION_USER_SEPARATOR}{session_id}")
    }

    /// `LIKE` prefix covering every session of one user.
    fn session_key_prefix(&self, user_id: Option<&str>) -> String {
        let user = match user_id {
            Some(u) if !u.trim().is_empty() => u,
            _ => ANON_USER,
        };
        format!("{user}{SESSION_USER_SEPARATOR}")
    }

    fn state_table(&self) -> &str {
        &self.config.state_table
    }
}

/// Maps a driver error, preserving the server message so
/// [`MysqlError::is_duplicate_key`] can recognise error 1062.
fn driver_error(e: mysql_async::Error) -> MysqlError {
    MysqlError::Sql(e.to_string())
}

/// Maps a driver error onto the core error type used by [`SnapshotBackend`].
fn core_error(e: mysql_async::Error) -> CoreError {
    CoreError::Tool(format!("mysql snapshot backend: {e}"))
}

// ---------------------------------------------------------------------------------------
// StateBackend
// ---------------------------------------------------------------------------------------

impl StateBackend for MysqlAsyncBackend {
    fn execute_ddl(&self, sql: &str) -> MysqlResult<()> {
        let mut conn = self.conn()?;
        self.block_on(async { conn.query_drop(sql).await })
            .map_err(driver_error)
    }

    fn database_exists(&self, database_name: &str) -> MysqlResult<bool> {
        let mut conn = self.conn()?;
        let found = self.block_on(async {
            conn.exec_first::<i64, _, _>(
                "SELECT COUNT(*) FROM INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME = ?",
                (database_name.to_string(),),
            )
            .await
        })
        .map_err(driver_error)?;
        Ok(found.unwrap_or(0) > 0)
    }

    fn table_exists(&self, database_name: &str, table_name: &str) -> MysqlResult<bool> {
        let mut conn = self.conn()?;
        let found = self.block_on(async {
            conn.exec_first::<i64, _, _>(
                "SELECT COUNT(*) FROM INFORMATION_SCHEMA.TABLES \
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
                (database_name.to_string(), table_name.to_string()),
            )
            .await
        })
        .map_err(driver_error)?;
        Ok(found.unwrap_or(0) > 0)
    }

    fn table_columns(&self, database_name: &str, table_name: &str) -> MysqlResult<Vec<String>> {
        let mut conn = self.conn()?;
        let rows = self.block_on(async {
            conn.exec::<String, _, _>(
                "SELECT COLUMN_NAME FROM INFORMATION_SCHEMA.COLUMNS \
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
                (database_name.to_string(), table_name.to_string()),
            )
            .await
        })
        .map_err(driver_error)?;
        Ok(rows)
    }

    fn save_single(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        value_json: &str,
    ) -> MysqlResult<()> {
        let sql = format!(
            "INSERT INTO {} (session_id, state_key, item_index, state_data) VALUES (?, ?, 0, ?) \
             ON DUPLICATE KEY UPDATE state_data = VALUES(state_data)",
            self.state_table()
        );
        let sid = self.session_key(user_id, session_id);
        let mut conn = self.conn()?;
        self.block_on(async {
            conn.exec_drop(
                &sql,
                (sid.clone(), state_key.to_string(), value_json.to_string()),
            )
            .await
        })
        .map_err(driver_error)
    }

    fn save_single_versioned(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        value_json: &str,
    ) -> MysqlResult<i64> {
        let table = self.state_table();
        let upsert = format!(
            "INSERT INTO {table} (session_id, state_key, item_index, state_data, version) \
             VALUES (?, ?, 0, ?, 1) \
             ON DUPLICATE KEY UPDATE state_data = VALUES(state_data), version = version + 1"
        );
        let count = format!(
            "SELECT version FROM {table} \
             WHERE session_id = ? AND state_key = ? AND item_index = 0"
        );
        let sid = self.session_key(user_id, session_id);
        let mut conn = self.conn()?;
        self.block_on(async {
            conn.exec_drop(
                &upsert,
                (sid.clone(), state_key.to_string(), value_json.to_string()),
            )
            .await?;
            conn.exec_first::<i64, _, _>(&count, (sid, state_key.to_string()))
                .await
        })
        .map_err(driver_error)?
        .ok_or_else(|| MysqlError::Internal("version row vanished after upsert".to_string()))
    }

    fn insert_single_if_absent(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        value_json: &str,
    ) -> MysqlResult<CasOutcome> {
        let sql = format!(
            "INSERT INTO {} (session_id, state_key, item_index, state_data, version) \
             VALUES (?, ?, 0, ?, 1)",
            self.state_table()
        );
        let sid = self.session_key(user_id, session_id);
        let mut conn = self.conn()?;
        match self.block_on(async {
            conn.exec_drop(
                &sql,
                (sid, state_key.to_string(), value_json.to_string()),
            )
            .await
        }) {
            Ok(()) => Ok(CasOutcome::Applied(1)),
            Err(e) => {
                let mapped = driver_error(e);
                if mapped.is_duplicate_key() {
                    Ok(CasOutcome::Conflict)
                } else {
                    Err(mapped)
                }
            }
        }
    }

    fn update_single_if_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        value_json: &str,
        expected_version: i64,
    ) -> MysqlResult<CasOutcome> {
        let sql = format!(
            "UPDATE {} SET state_data = ?, version = version + 1 \
             WHERE session_id = ? AND state_key = ? AND item_index = 0 AND version = ?",
            self.state_table()
        );
        let sid = self.session_key(user_id, session_id);
        let mut conn = self.conn()?;
        let affected = self
            .block_on(async {
                conn.exec_iter(
                    &sql,
                    (
                        value_json.to_string(),
                        sid,
                        state_key.to_string(),
                        expected_version,
                    ),
                )
                .await
                .map(|result| result.affected_rows())
            })
            .map_err(driver_error)?;
        if affected == 1 {
            Ok(CasOutcome::Applied(expected_version + 1))
        } else {
            Ok(CasOutcome::Conflict)
        }
    }

    fn get_single(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
    ) -> MysqlResult<Option<String>> {
        let sql = format!(
            "SELECT state_data FROM {} WHERE session_id = ? AND state_key = ? AND item_index = 0",
            self.state_table()
        );
        let sid = self.session_key(user_id, session_id);
        let mut conn = self.conn()?;
        self.block_on(async {
            conn.exec_first::<String, _, _>(&sql, (sid, state_key.to_string()))
                .await
        })
        .map_err(driver_error)
    }

    fn save_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
        values_json: &[String],
        from_index: usize,
    ) -> MysqlResult<()> {
        let table = self.state_table();
        let sid = self.session_key(user_id, session_id);
        let start = from_index.min(values_json.len());
        let mut conn = self.conn()?;

        if start < values_json.len() {
            // One multi-row upsert for the written slice: `from_index == 0` rewrites the
            // whole list, a higher value only touches the appended tail.
            let mut tuples: Vec<&str> = Vec::with_capacity(values_json.len() - start);
            let mut params: Vec<Value> = Vec::with_capacity((values_json.len() - start) * 4);
            for (index, json) in values_json.iter().enumerate().skip(start) {
                tuples.push("(?, ?, ?, ?, 1)");
                params.push(Value::from(sid.clone()));
                params.push(Value::from(state_key.to_string()));
                params.push(Value::from(index as i64));
                params.push(Value::from(json.clone()));
            }
            let sql = format!(
                "INSERT INTO {table} (session_id, state_key, item_index, state_data, version) \
                 VALUES {} ON DUPLICATE KEY UPDATE state_data = VALUES(state_data)",
                tuples.join(", ")
            );
            self.block_on(async {
                conn.exec_drop(&sql, Params::Positional(params)).await
            })
            .map_err(driver_error)?;
        }

        // Drop rows past the new end so shrinking a list removes stale items.
        let trim = format!(
            "DELETE FROM {table} WHERE session_id = ? AND state_key = ? AND item_index >= ?"
        );
        let total = values_json.len() as i64;
        self.block_on(async {
            conn.exec_drop(&trim, (sid, state_key.to_string(), total))
                .await
        })
        .map_err(driver_error)
    }

    fn get_list(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
    ) -> MysqlResult<Vec<String>> {
        let sql = format!(
            "SELECT state_data FROM {} WHERE session_id = ? AND state_key = ? \
             ORDER BY item_index ASC",
            self.state_table()
        );
        let sid = self.session_key(user_id, session_id);
        let mut conn = self.conn()?;
        self.block_on(async {
            conn.exec::<String, _, _>(&sql, (sid, state_key.to_string()))
                .await
        })
        .map_err(driver_error)
    }

    fn list_len(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
    ) -> MysqlResult<usize> {
        let sql = format!(
            "SELECT COUNT(*) FROM {} WHERE session_id = ? AND state_key = ?",
            self.state_table()
        );
        let sid = self.session_key(user_id, session_id);
        let mut conn = self.conn()?;
        let count = self
            .block_on(async {
                conn.exec_first::<i64, _, _>(&sql, (sid, state_key.to_string()))
                    .await
            })
            .map_err(driver_error)?
            .unwrap_or(0);
        Ok(count.max(0) as usize)
    }

    fn exists(&self, user_id: Option<&str>, session_id: &str) -> MysqlResult<bool> {
        let sql = format!(
            "SELECT COUNT(*) FROM {} WHERE session_id = ?",
            self.state_table()
        );
        let sid = self.session_key(user_id, session_id);
        let mut conn = self.conn()?;
        let count = self
            .block_on(async { conn.exec_first::<i64, _, _>(&sql, (sid,)).await })
            .map_err(driver_error)?
            .unwrap_or(0);
        Ok(count > 0)
    }

    fn delete(&self, user_id: Option<&str>, session_id: &str) -> MysqlResult<()> {
        let sql = format!("DELETE FROM {} WHERE session_id = ?", self.state_table());
        let sid = self.session_key(user_id, session_id);
        let mut conn = self.conn()?;
        self.block_on(async { conn.exec_drop(&sql, (sid,)).await })
            .map_err(driver_error)
    }

    fn list_session_ids(&self, user_id: Option<&str>) -> MysqlResult<Vec<String>> {
        let sql = format!(
            "SELECT DISTINCT session_id FROM {} WHERE session_id LIKE ? ORDER BY session_id ASC",
            self.state_table()
        );
        let prefix = self.session_key_prefix(user_id);
        let pattern = format!("{prefix}%");
        let mut conn = self.conn()?;
        let rows = self
            .block_on(async { conn.exec::<String, _, _>(&sql, (pattern,)).await })
            .map_err(driver_error)?;
        Ok(rows
            .into_iter()
            .filter_map(|row| row.strip_prefix(prefix.as_str()).map(str::to_string))
            .collect())
    }

    fn read_version(
        &self,
        user_id: Option<&str>,
        session_id: &str,
        state_key: &str,
    ) -> MysqlResult<i64> {
        let sql = format!(
            "SELECT version FROM {} WHERE session_id = ? AND state_key = ? AND item_index = 0",
            self.state_table()
        );
        let sid = self.session_key(user_id, session_id);
        let mut conn = self.conn()?;
        Ok(self
            .block_on(async {
                conn.exec_first::<i64, _, _>(&sql, (sid, state_key.to_string()))
                    .await
            })
            .map_err(driver_error)?
            .unwrap_or(0))
    }
}

// ---------------------------------------------------------------------------------------
// SqlBackend
// ---------------------------------------------------------------------------------------

impl SqlBackend for MysqlAsyncBackend {
    fn execute_ddl(&self, sql: &str) -> MysqlResult<()> {
        <Self as StateBackend>::execute_ddl(self, sql)
    }

    fn upsert(
        &self,
        sql: &str,
        ns_path: &str,
        key: &str,
        value_json: &str,
        now_ms: i64,
    ) -> MysqlResult<()> {
        let mut conn = self.conn()?;
        self.block_on(async {
            conn.exec_drop(
                sql,
                (
                    ns_path.to_string(),
                    key.to_string(),
                    value_json.to_string(),
                    now_ms,
                ),
            )
            .await
        })
        .map_err(driver_error)
    }

    fn insert_create(
        &self,
        sql: &str,
        ns_path: &str,
        key: &str,
        value_json: &str,
        now_ms: i64,
    ) -> MysqlResult<()> {
        <Self as SqlBackend>::upsert(self, sql, ns_path, key, value_json, now_ms)
    }

    fn cas_update(
        &self,
        sql: &str,
        value_json: &str,
        now_ms: i64,
        ns_path: &str,
        key: &str,
        expected_version: i64,
    ) -> MysqlResult<u64> {
        let mut conn = self.conn()?;
        let affected = self
            .block_on(async {
                conn.exec_iter(
                    sql,
                    (
                        value_json.to_string(),
                        now_ms,
                        ns_path.to_string(),
                        key.to_string(),
                        expected_version,
                    ),
                )
                .await
                .map(|result| result.affected_rows())
            })
            .map_err(driver_error)?;
        Ok(affected)
    }

    fn select(&self, sql: &str, ns_path: &str, key: &str) -> MysqlResult<Option<(String, i64)>> {
        let mut conn = self.conn()?;
        self.block_on(async {
            conn.exec_first::<(String, i64), _, _>(sql, (ns_path.to_string(), key.to_string()))
                .await
        })
        .map_err(driver_error)
    }

    fn search(
        &self,
        sql: &str,
        like_pattern: &str,
        limit: usize,
        offset: usize,
    ) -> MysqlResult<Vec<(String, String, i64)>> {
        let mut conn = self.conn()?;
        self.block_on(async {
            conn.exec::<(String, String, i64), _, _>(
                sql,
                (
                    like_pattern.to_string(),
                    limit as i64,
                    offset as i64,
                ),
            )
            .await
        })
        .map_err(driver_error)
    }

    fn delete(&self, sql: &str, ns_path: &str, key: &str) -> MysqlResult<()> {
        let mut conn = self.conn()?;
        self.block_on(async {
            conn.exec_drop(sql, (ns_path.to_string(), key.to_string()))
                .await
        })
        .map_err(driver_error)
    }
}

// ---------------------------------------------------------------------------------------
// SnapshotBackend
// ---------------------------------------------------------------------------------------

impl SnapshotBackend for MysqlAsyncBackend {
    fn execute_ddl(&self, sql: &str) -> CoreResult<()> {
        let mut conn = self.conn().map_err(|e| CoreError::Tool(e.to_string()))?;
        self.block_on(async { conn.query_drop(sql).await })
            .map_err(core_error)
    }

    fn upload(&self, snapshot_id: &str, data: &[u8]) -> CoreResult<()> {
        let sql = format!(
            "INSERT INTO {} (snapshot_id, data) VALUES (?, ?) \
             ON DUPLICATE KEY UPDATE data = VALUES(data)",
            self.config.snapshot_table
        );
        let mut conn = self.conn().map_err(|e| CoreError::Tool(e.to_string()))?;
        self.block_on(async {
            conn.exec_drop(&sql, (snapshot_id.to_string(), data.to_vec()))
                .await
        })
        .map_err(core_error)
    }

    fn download(&self, snapshot_id: &str) -> CoreResult<Vec<u8>> {
        let sql = format!(
            "SELECT data FROM {} WHERE snapshot_id = ?",
            self.config.snapshot_table
        );
        let mut conn = self.conn().map_err(|e| CoreError::Tool(e.to_string()))?;
        self.block_on(async {
            conn.exec_first::<Vec<u8>, _, _>(&sql, (snapshot_id.to_string(),))
                .await
        })
        .map_err(core_error)?
        .ok_or_else(|| CoreError::Tool(format!("snapshot not found: {snapshot_id}")))
    }

    fn exists(&self, snapshot_id: &str) -> CoreResult<bool> {
        let sql = format!(
            "SELECT COUNT(*) FROM {} WHERE snapshot_id = ?",
            self.config.snapshot_table
        );
        let mut conn = self.conn().map_err(|e| CoreError::Tool(e.to_string()))?;
        let count = self
            .block_on(async {
                conn.exec_first::<i64, _, _>(&sql, (snapshot_id.to_string(),))
                    .await
            })
            .map_err(core_error)?
            .unwrap_or(0);
        Ok(count > 0)
    }
}

// ---------------------------------------------------------------------------------------
// LockBackend
// ---------------------------------------------------------------------------------------

impl LockBackend for MysqlAsyncBackend {
    fn try_acquire(&self, lock_name: &str, timeout: Duration) -> MysqlResult<bool> {
        // Re-acquiring a lock this adapter already holds is a local no-op: MySQL
        // `GET_LOCK()` on the *same* session succeeds again, but it would leak the first
        // lease (one release would drop both).
        if self
            .lock_conns
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(lock_name)
        {
            return Ok(true);
        }

        let mut conn = self.conn()?;
        let acquired = self
            .block_on(async {
                conn.exec_first::<i64, _, _>(
                    "SELECT GET_LOCK(?, ?)",
                    (lock_name.to_string(), timeout.as_secs() as i64),
                )
                .await
            })
            .map_err(driver_error)?;

        if acquired == Some(1) {
            self.lock_conns
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(lock_name.to_string(), conn);
            Ok(true)
        } else {
            // Timed out (0) or errored (NULL): give the connection back instead of
            // keeping a session that holds nothing.
            self.block_on(async { conn.disconnect().await }).ok();
            Ok(false)
        }
    }

    fn release_lock(&self, lock_name: &str) -> MysqlResult<bool> {
        let mut conn = {
            let mut held = self.lock_conns.lock().unwrap_or_else(|p| p.into_inner());
            match held.remove(lock_name) {
                Some(conn) => conn,
                None => return Ok(false),
            }
        };
        let released = self
            .block_on(async {
                conn.exec_first::<i64, _, _>("SELECT RELEASE_LOCK(?)", (lock_name.to_string(),))
                    .await
            })
            .map_err(driver_error)?;
        // Closing the session is what Java's `Connection#close()` does in
        // `SandboxLease.close()`; it also releases anything still held by this session.
        self.block_on(async { conn.disconnect().await }).ok();
        Ok(released.unwrap_or(0) != 0)
    }

    fn is_held(&self, lock_name: &str) -> MysqlResult<bool> {
        let mut conn = self.conn()?;
        let holder = self
            .block_on(async {
                conn.exec_first::<i64, _, _>(
                    "SELECT IS_USED_LOCK(?)",
                    (lock_name.to_string(),),
                )
                .await
            })
            .map_err(driver_error)?;
        Ok(holder.is_some())
    }
}
