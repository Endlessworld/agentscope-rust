//! `tokio-postgres`-backed [`SqlClient`] (feature `postgres-executor`).
//!
//! This is the real PostgreSQL backend of the crate: it runs on the dedicated driver thread
//! owned by [`DriverExecutor`], so no statement ever blocks a Tokio worker, and it opens one
//! extra connection per native advisory-lock session.
//!
//! The default build does **not** compile this module (and pulls in no driver crate): enable
//! the `postgres-executor` feature to use it.

use std::str::FromStr;

use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{Client, Config, NoTls, Row};

use super::{to_dollar_placeholders, DriverExecutor, SqlClient};
use crate::backend::{ColumnValue, JdbcError, JdbcResult};
use crate::dialect::SqlParam;
use crate::sandbox::NativeLockFlavor;

/// Environment variable read by [`postgres_executor_from_env`] and the env-gated
/// integration tests. When unset, the driver stays disabled and reports
/// [`JdbcError::Configuration`].
pub const POSTGRES_URL_ENV: &str = "AGENTSCOPE_TEST_POSTGRES_URL";

/// The driver-generic executor bound to the `tokio-postgres` client.
pub type PostgresExecutor = DriverExecutor<PostgresSqlClient>;

/// Connects to PostgreSQL with native advisory locks (`pg_try_advisory_lock`) enabled.
///
/// The configuration string is a libpq-style keyword/value list or a `postgres://` URL, e.g.
/// `host=localhost user=postgres password=secret dbname=agentscope`.
pub fn connect_postgres(config: &str) -> JdbcResult<PostgresExecutor> {
    DriverExecutor::connect(config, Some(NativeLockFlavor::Postgres))
}

/// Connects using [`POSTGRES_URL_ENV`].
///
/// An unset/blank variable is a [`JdbcError::Configuration`] — the integration tests use exactly
/// that to skip instead of failing (`#[ignore]` is never used).
pub fn postgres_executor_from_env() -> JdbcResult<PostgresExecutor> {
    DriverExecutor::from_env(POSTGRES_URL_ENV, Some(NativeLockFlavor::Postgres))
}

/// A [`SqlClient`] over one `tokio-postgres` server connection.
pub struct PostgresSqlClient {
    client: Client,
}

impl std::fmt::Debug for PostgresSqlClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresSqlClient").finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl SqlClient for PostgresSqlClient {
    async fn connect(config: &str) -> JdbcResult<Self> {
        let parsed = Config::from_str(config).map_err(|error| {
            JdbcError::Configuration(format!("invalid PostgreSQL configuration: {error}"))
        })?;
        let (client, connection) = parsed.connect(NoTls).await.map_err(|error| {
            JdbcError::Database(format!("PostgreSQL connection failed: {error}"))
        })?;
        // The connection future drives the socket; the driver thread polls it whenever a
        // statement is in flight (it is spawned on that thread's runtime).
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::warn!("[jdbc-postgres] connection closed with an error: {error}");
            }
        });
        Ok(Self { client })
    }

    async fn execute(&mut self, sql: &str, params: &[SqlParam]) -> JdbcResult<usize> {
        let sql = to_dollar_placeholders(sql);
        let owned = to_postgres_params(params);
        let borrowed = borrow_params(&owned);
        self.client
            .execute(&sql, &borrowed)
            .await
            .map(|affected| affected as usize)
            .map_err(map_error)
    }

    async fn query(&mut self, sql: &str, params: &[SqlParam]) -> JdbcResult<Vec<Vec<ColumnValue>>> {
        let sql = to_dollar_placeholders(sql);
        let owned = to_postgres_params(params);
        let borrowed = borrow_params(&owned);
        let rows = self
            .client
            .query(&sql, &borrowed)
            .await
            .map_err(map_error)?;
        rows.iter().map(decode_row).collect()
    }

    /// Uses the **simple** query protocol: transaction control (`BEGIN` / `COMMIT` /
    /// `ROLLBACK`) is rejected as a prepared statement by PostgreSQL.
    async fn batch_execute(&mut self, sql: &str) -> JdbcResult<()> {
        self.client.batch_execute(sql).await.map_err(map_error)
    }
}

/// Maps the crate-wide [`SqlParam`] onto owned `tokio-postgres` values.
fn to_postgres_params(params: &[SqlParam]) -> Vec<Box<dyn ToSql + Sync + Send>> {
    params
        .iter()
        .map(|param| -> Box<dyn ToSql + Sync + Send> {
            match param {
                SqlParam::Text(value) => Box::new(value.clone()),
                SqlParam::Int(value) => Box::new(*value),
                SqlParam::Blob(value) => Box::new(value.clone()),
                // A typed NULL: `Option<String>` binds SQL NULL for any inferred column type.
                SqlParam::Null => Box::new(Option::<String>::None),
            }
        })
        .collect()
}

/// Borrows the owned parameters for the driver call; the `Send` bound of the owned boxes
/// is not part of the borrowed view the driver asks for.
fn borrow_params(params: &[Box<dyn ToSql + Sync + Send>]) -> Vec<&(dyn ToSql + Sync)> {
    params
        .iter()
        .map(|value| value.as_ref() as &(dyn ToSql + Sync))
        .collect()
}

/// Decodes one row, using the result columns' declared types.
fn decode_row(row: &Row) -> JdbcResult<Vec<ColumnValue>> {
    let mut values = Vec::with_capacity(row.len());
    for (index, column) in row.columns().iter().enumerate() {
        values.push(decode_column(row, index, column.type_())?);
    }
    Ok(values)
}

fn decode_column(row: &Row, index: usize, column_type: &Type) -> JdbcResult<ColumnValue> {
    let context = |error: tokio_postgres::Error| {
        JdbcError::Database(format!(
            "failed to decode column {index} ({}): {error}",
            column_type.name()
        ))
    };
    // Match on the declared type *name* rather than comparing `Type` values: `Type` is not
    // `Copy`, and the name is what the wire protocol reports for built-in and custom types.
    if column_type.name() == "bytea" {
        let value: Option<Vec<u8>> = row.try_get(index).map_err(context)?;
        return Ok(value
            .map(ColumnValue::Blob)
            .unwrap_or(ColumnValue::Null));
    }
    if column_type.name() == "int2" {
        let value: Option<i16> = row.try_get(index).map_err(context)?;
        return Ok(value
            .map(|value| ColumnValue::Int(i64::from(value)))
            .unwrap_or(ColumnValue::Null));
    }
    if column_type.name() == "int4" {
        let value: Option<i32> = row.try_get(index).map_err(context)?;
        return Ok(value
            .map(|value| ColumnValue::Int(i64::from(value)))
            .unwrap_or(ColumnValue::Null));
    }
    if column_type.name() == "oid" {
        let value: Option<u32> = row.try_get(index).map_err(context)?;
        return Ok(value
            .map(|value| ColumnValue::Int(i64::from(value)))
            .unwrap_or(ColumnValue::Null));
    }
    if column_type.name() == "int8" {
        let value: Option<i64> = row.try_get(index).map_err(context)?;
        return Ok(value.map(ColumnValue::Int).unwrap_or(ColumnValue::Null));
    }
    if column_type.name() == "bool" {
        // The crate's column model has no boolean: fold it to 0/1 (the advisory-lock probes
        // already return integers, this is for ad-hoc queries).
        let value: Option<bool> = row.try_get(index).map_err(context)?;
        return Ok(value
            .map(|value| ColumnValue::Int(i64::from(u8::from(value))))
            .unwrap_or(ColumnValue::Null));
    }
    // TEXT / VARCHAR / BPCHAR / NAME / json / enum / anything else text-shaped.
    let value: Option<String> = row.try_get(index).map_err(context)?;
    Ok(value.map(ColumnValue::Text).unwrap_or(ColumnValue::Null))
}

/// Maps a driver error, keeping the SQLSTATE code in the message.
///
/// The code is what [`crate::backend::is_unique_violation`] keys on (`23505` = unique
/// violation); `tokio-postgres`'s `Display` alone is not guaranteed to carry it.
fn map_error(error: tokio_postgres::Error) -> JdbcError {
    if let Some(details) = error.as_db_error() {
        let detail = details
            .detail()
            .map(|detail| format!(" ({detail})"))
            .unwrap_or_default();
        return JdbcError::Database(format!(
            "{}: {}{detail}",
            details.code().code(),
            details.message()
        ));
    }
    JdbcError::Database(error.to_string())
}
