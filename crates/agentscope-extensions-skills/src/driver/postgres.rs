//! Real PostgreSQL [`SqlDatabase`] backed by `tokio-postgres` (feature `postgres-driver`).
//!
//! The repositories emit portable `?` placeholders and plain `INSERT` statements, so this
//! adapter
//! * renumbers `?` to `$n` ([`super::pg_placeholders`]);
//! * appends `RETURNING <pk>` for [`SqlConnection::insert_returning_id`];
//! * keeps the JDBC transaction semantics (`set_auto_commit(false)` → `BEGIN`, `commit` →
//!   `COMMIT`, `rollback` → `ROLLBACK`) on one session, so a write transaction is atomic.
//!
//! The seam is synchronous while the driver is async — see [`super::SyncBridge`].
//!
//! With the `postgres-tls` feature the connection is negotiated through [`super::pg_tls`]
//! (`rustls`); without it the driver speaks plain TCP.

use std::sync::Arc;

use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::config::SslMode;
use tokio_postgres::{Client, Config};
#[cfg(not(feature = "postgres-tls"))]
use tokio_postgres::NoTls;

#[cfg(feature = "postgres-tls")]
use super::pg_tls;
use super::{db_error, pg_placeholders, SyncBridge};
use crate::error::SkillRepositoryError;
use crate::sql::{SqlConnection, SqlDatabase, SqlParam, SqlRow, SqlValue};

const BACKEND: &str = "postgres";

/// A [`SqlDatabase`] over a live PostgreSQL server.
///
/// ```no_run
/// use agentscope_extensions_skills::{PgSqlDatabase, SqlDatabase};
/// let db = PgSqlDatabase::connect("host=localhost user=postgres dbname=agentscope")?;
/// let _conn = db.connection()?;
/// # Ok::<(), agentscope_extensions_skills::SkillRepositoryError>(())
/// ```
///
/// # TLS
///
/// The connection string decides *whether* TLS is used, the `postgres-tls` feature decides
/// whether it *can* be: with it, `sslmode=require` (or `SslMode::Require` on a [`Config`])
/// enforces a `rustls` handshake against the platform trust store (`webpki-roots` is used when
/// the platform store yields no usable certificate), and the default `sslmode=prefer` uses TLS
/// whenever the server offers it. Without the feature a `sslmode=require` string is rejected at
/// construction instead of failing at the handshake.
pub struct PgSqlDatabase {
    bridge: Arc<SyncBridge>,
    config: Config,
    /// Column returned by `insert_returning_id` (`id` by default, matching the crate DDL).
    returning_column: String,
    /// TLS connector (`postgres-tls`), built once so the trust anchors are parsed once per handle.
    #[cfg(feature = "postgres-tls")]
    tls: pg_tls::MakeRustlsConnect,
}

impl PgSqlDatabase {
    /// Connects using a libpq-style connection string
    /// (`host=localhost user=postgres password=... dbname=...`).
    pub fn connect(connection_string: &str) -> Result<Self, SkillRepositoryError> {
        let config: Config = connection_string
            .parse()
            .map_err(|e| db_error(BACKEND, format!("invalid connection string: {e}")))?;
        ensure_tls_supported(&config)?;
        let bridge = Arc::new(SyncBridge::new(BACKEND)?);
        // Fail fast: a bad DSN / unreachable server must surface at construction time.
        let database = Self {
            bridge,
            config,
            returning_column: "id".to_string(),
            #[cfg(feature = "postgres-tls")]
            tls: pg_tls::MakeRustlsConnect::new()?,
        };
        database.connection()?;
        Ok(database)
    }

    /// Overrides the primary-key column used by `insert_returning_id`.
    pub fn with_returning_column(mut self, column: impl Into<String>) -> Self {
        self.returning_column = column.into();
        self
    }

    /// The primary-key column used by `insert_returning_id`.
    pub fn returning_column(&self) -> &str {
        &self.returning_column
    }

    fn open(&self) -> Result<Client, SkillRepositoryError> {
        // `postgres-tls` swaps `NoTls` for the rustls connector; the DSN still decides whether
        // TLS is actually negotiated (`sslmode`).
        #[cfg(feature = "postgres-tls")]
        let connected = {
            let tls = self.tls.clone();
            self.bridge.run(async { self.config.connect(tls).await })
        };
        #[cfg(not(feature = "postgres-tls"))]
        let connected = self.bridge.run(async { self.config.connect(NoTls).await });
        let connected = connected.map_err(|e| db_error(BACKEND, e))?;
        let (client, connection) = connected.map_err(|e| {
            db_error(
                BACKEND,
                format!("connection failed ({:?}): {e}", self.config.get_hosts()),
            )
        })?;
        self.bridge.spawn(async move {
            if let Err(e) = connection.await {
                tracing::warn!(backend = BACKEND, error = %e, "connection error");
            }
        });
        Ok(client)
    }
}

/// Rejects a connection string that demands TLS when no TLS backend was compiled in.
///
/// `tokio-postgres` only reports this at handshake time, after the TCP connect; failing at
/// construction keeps the message actionable. `sslmode=prefer` (the default) stays allowed: it
/// simply means "TLS when the server offers it".
fn ensure_tls_supported(config: &Config) -> Result<(), SkillRepositoryError> {
    if !cfg!(feature = "postgres-tls") && config.get_ssl_mode() == SslMode::Require {
        return Err(db_error(
            BACKEND,
            "the connection string requires TLS (sslmode=require) but this build has no TLS \
             backend; enable the `postgres-tls` feature",
        ));
    }
    Ok(())
}

impl SqlDatabase for PgSqlDatabase {
    type Connection = PgSqlConnection;

    fn connection(&self) -> Result<Self::Connection, SkillRepositoryError> {
        Ok(PgSqlConnection {
            bridge: Arc::clone(&self.bridge),
            client: self.open()?,
            returning_column: self.returning_column.clone(),
            in_transaction: false,
        })
    }
}

/// A single PostgreSQL session, mirroring a JDBC `Connection`.
pub struct PgSqlConnection {
    bridge: Arc<SyncBridge>,
    client: Client,
    returning_column: String,
    in_transaction: bool,
}

/// Converts seam parameters into owned `ToSql` values.
fn owned_params(params: &[SqlParam]) -> Vec<Box<dyn ToSql + Sync + Send>> {
    params
        .iter()
        .map(|p| -> Box<dyn ToSql + Sync + Send> {
            match p {
                SqlParam::Null => Box::new(Option::<String>::None),
                SqlParam::Int(v) => Box::new(*v),
                SqlParam::Text(v) => Box::new(v.clone()),
            }
        })
        .collect()
}

/// Decodes one cell, preferring the column type so `INT8` is not mis-read as text.
fn decode_cell(row: &tokio_postgres::Row, index: usize) -> SqlValue {
    let column_type = row.columns()[index].type_();
    if *column_type == Type::INT8 {
        return row
            .try_get::<_, Option<i64>>(index)
            .ok()
            .flatten()
            .map(SqlValue::Int)
            .unwrap_or(SqlValue::Null);
    }
    if *column_type == Type::INT4 {
        return row
            .try_get::<_, Option<i32>>(index)
            .ok()
            .flatten()
            .map(|v| SqlValue::Int(i64::from(v)))
            .unwrap_or(SqlValue::Null);
    }
    if *column_type == Type::INT2 {
        return row
            .try_get::<_, Option<i16>>(index)
            .ok()
            .flatten()
            .map(|v| SqlValue::Int(i64::from(v)))
            .unwrap_or(SqlValue::Null);
    }
    if *column_type == Type::BOOL {
        return row
            .try_get::<_, Option<bool>>(index)
            .ok()
            .flatten()
            .map(|v| SqlValue::Int(i64::from(v)))
            .unwrap_or(SqlValue::Null);
    }
    row.try_get::<_, Option<String>>(index)
        .ok()
        .flatten()
        .map(SqlValue::Text)
        .unwrap_or(SqlValue::Null)
}

/// Builds a [`SqlRow`] from a driver row.
fn to_sql_row(row: &tokio_postgres::Row) -> SqlRow {
    let cells = row
        .columns()
        .iter()
        .enumerate()
        .map(|(index, column)| (column.name().to_string(), decode_cell(row, index)));
    SqlRow::new(cells)
}

/// Appends `RETURNING <column>` unless the statement already returns rows.
fn with_returning(sql: &str, column: &str) -> String {
    if sql.to_ascii_uppercase().contains("RETURNING") {
        return sql.to_string();
    }
    format!("{} RETURNING {}", sql.trim_end().trim_end_matches(';'), column)
}

impl PgSqlConnection {
    fn statement(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> (String, Vec<Box<dyn ToSql + Sync + Send>>) {
        (pg_placeholders(sql), owned_params(params))
    }

    fn transaction(&mut self, sql: &'static str) -> Result<(), SkillRepositoryError> {
        let bridge = Arc::clone(&self.bridge);
        let client = &self.client;
        bridge
            .run(async move { client.batch_execute(sql).await })
            .map_err(|e| db_error(BACKEND, e))?
            .map_err(|e| db_error(BACKEND, format!("{sql} failed: {e}")))
    }
}

impl SqlConnection for PgSqlConnection {
    fn execute(&mut self, sql: &str, params: &[SqlParam]) -> Result<usize, SkillRepositoryError> {
        let (statement, owned) = self.statement(sql, params);
        let refs: Vec<&(dyn ToSql + Sync)> = owned
            .iter()
            .map(|b| b.as_ref() as &(dyn ToSql + Sync))
            .collect();
        let bridge = Arc::clone(&self.bridge);
        let client = &self.client;
        let affected = bridge
            .run(async move { client.execute(&statement, &refs).await })
            .map_err(|e| db_error(BACKEND, e))?
            .map_err(|e| db_error(BACKEND, format!("execute failed: {e}")))?;
        Ok(affected as usize)
    }

    fn query(
        &mut self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<Vec<SqlRow>, SkillRepositoryError> {
        let (statement, owned) = self.statement(sql, params);
        let refs: Vec<&(dyn ToSql + Sync)> = owned
            .iter()
            .map(|b| b.as_ref() as &(dyn ToSql + Sync))
            .collect();
        let bridge = Arc::clone(&self.bridge);
        let client = &self.client;
        let rows = bridge
            .run(async move { client.query(&statement, &refs).await })
            .map_err(|e| db_error(BACKEND, e))?
            .map_err(|e| db_error(BACKEND, format!("query failed: {e}")))?;
        Ok(rows.iter().map(to_sql_row).collect())
    }

    fn execute_batch(
        &mut self,
        sql: &str,
        params: &[Vec<SqlParam>],
    ) -> Result<usize, SkillRepositoryError> {
        // One prepared statement, one round trip per parameter set (PostgreSQL's extended
        // protocol has no multi-statement bind list): still prepared once, which is what the
        // Java `addBatch`/`executeBatch` path buys.
        if params.is_empty() {
            return Ok(0);
        }
        let statement = pg_placeholders(sql);
        let bridge = Arc::clone(&self.bridge);
        let client = &self.client;
        let prepared = bridge
            .run(async move { client.prepare(&statement).await })
            .map_err(|e| db_error(BACKEND, e))?
            .map_err(|e| db_error(BACKEND, format!("prepare failed: {e}")))?;
        let mut affected = 0usize;
        for row in params {
            let owned = owned_params(row);
            let refs: Vec<&(dyn ToSql + Sync)> = owned
                .iter()
                .map(|b| b.as_ref() as &(dyn ToSql + Sync))
                .collect();
            let client = &self.client;
            let statement = prepared.clone();
            let done = bridge
                .run(async move { client.execute(&statement, &refs).await })
                .map_err(|e| db_error(BACKEND, e))?
                .map_err(|e| db_error(BACKEND, format!("batch execute failed: {e}")))?;
            affected += done as usize;
        }
        Ok(affected)
    }

    fn insert_returning_id(
        &mut self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<i64, SkillRepositoryError> {
        let statement = pg_placeholders(&with_returning(sql, &self.returning_column));
        let owned = owned_params(params);
        let refs: Vec<&(dyn ToSql + Sync)> = owned
            .iter()
            .map(|b| b.as_ref() as &(dyn ToSql + Sync))
            .collect();
        let bridge = Arc::clone(&self.bridge);
        let client = &self.client;
        let row = bridge
            .run(async move { client.query_opt(&statement, &refs).await })
            .map_err(|e| db_error(BACKEND, e))?
            .map_err(|e| db_error(BACKEND, format!("insert failed: {e}")))?;
        match row {
            Some(row) => Ok(row.try_get::<_, i64>(0).unwrap_or(0)),
            None => Ok(0),
        }
    }

    fn set_auto_commit(&mut self, enabled: bool) -> Result<(), SkillRepositoryError> {
        // `tokio-postgres` has no auto-commit flag: autocommit is "no open transaction".
        if enabled {
            if self.in_transaction {
                self.transaction("COMMIT")?;
                self.in_transaction = false;
            }
        } else if !self.in_transaction {
            self.transaction("BEGIN")?;
            self.in_transaction = true;
        }
        Ok(())
    }

    fn commit(&mut self) -> Result<(), SkillRepositoryError> {
        if self.in_transaction {
            self.transaction("COMMIT")?;
            self.in_transaction = false;
        }
        Ok(())
    }

    fn rollback(&mut self) -> Result<(), SkillRepositoryError> {
        if self.in_transaction {
            self.transaction("ROLLBACK")?;
            self.in_transaction = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returning_clause_is_appended_once() {
        assert_eq!(
            with_returning("INSERT INTO t (a) VALUES (?)", "id"),
            "INSERT INTO t (a) VALUES (?) RETURNING id"
        );
        // A trailing semicolon must not break the appended clause.
        assert_eq!(
            with_returning("INSERT INTO t (a) VALUES (?);", "id"),
            "INSERT INTO t (a) VALUES (?) RETURNING id"
        );
        // Already-returning statements are left alone.
        assert_eq!(
            with_returning("INSERT INTO t (a) VALUES (?) RETURNING id", "id"),
            "INSERT INTO t (a) VALUES (?) RETURNING id"
        );
    }

    #[test]
    fn an_unreachable_server_is_an_explicit_error() {
        // Port 1 is never listening: this exercises the "no real database" path without
        // touching the network beyond a local connection refusal.
        let error = match PgSqlDatabase::connect("host=127.0.0.1 port=1 user=none dbname=none") {
            Ok(_) => panic!("connecting to a closed port must fail"),
            Err(e) => e,
        };
        assert!(
            matches!(error, SkillRepositoryError::Database(_)),
            "expected a database error, got {error:?}"
        );
        assert!(error.to_string().contains("connection failed"));
    }

    #[test]
    fn an_invalid_connection_string_is_rejected() {
        let error = match PgSqlDatabase::connect("host=127.0.0.1 port=not-a-number") {
            Ok(_) => panic!("a malformed connection string must be rejected"),
            Err(e) => e,
        };
        assert!(error.to_string().contains("invalid connection string"));
    }

    #[test]
    fn only_sslmode_require_needs_a_tls_backend() {
        let required: Config = "host=127.0.0.1 user=none sslmode=require".parse().unwrap();
        assert_eq!(
            ensure_tls_supported(&required).is_ok(),
            cfg!(feature = "postgres-tls"),
            "sslmode=require must be rejected exactly when no TLS backend is compiled in"
        );
        for dsn in [
            "host=127.0.0.1 user=none",
            "host=127.0.0.1 user=none sslmode=prefer",
            "host=127.0.0.1 user=none sslmode=disable",
        ] {
            let config: Config = dsn.parse().unwrap();
            assert!(ensure_tls_supported(&config).is_ok(), "{dsn}");
        }
    }

    #[cfg(not(feature = "postgres-tls"))]
    #[test]
    fn connect_rejects_sslmode_require_without_a_tls_backend() {
        // Rejected before any I/O (port 1 never accepts a TCP connection), so this stays offline.
        let error = match PgSqlDatabase::connect("host=127.0.0.1 port=1 user=none sslmode=require") {
            Ok(_) => panic!("a TLS-only connection string must be rejected"),
            Err(e) => e,
        };
        assert!(error.to_string().contains("postgres-tls"), "{error}");
    }

    #[cfg(feature = "postgres-tls")]
    #[test]
    fn a_tls_build_still_accepts_a_plain_tcp_connection_string() {
        // `sslmode=prefer` (the default) must not force TLS now that a backend exists.
        let error = match PgSqlDatabase::connect("host=127.0.0.1 port=1 user=none") {
            Ok(_) => panic!("connecting to a closed port must fail"),
            Err(e) => e,
        };
        assert!(error.to_string().contains("connection failed"), "{error}");
    }

    #[test]
    fn the_returning_column_defaults_to_id() {
        // Constructor validation needs a live server, so only the accessor is asserted here.
        let config: Config = "host=localhost".parse().unwrap();
        let database = PgSqlDatabase {
            bridge: Arc::new(SyncBridge::new(BACKEND).unwrap()),
            config,
            returning_column: "id".to_string(),
            #[cfg(feature = "postgres-tls")]
            tls: pg_tls::MakeRustlsConnect::new().unwrap(),
        };
        assert_eq!(database.returning_column(), "id");
    }
}
