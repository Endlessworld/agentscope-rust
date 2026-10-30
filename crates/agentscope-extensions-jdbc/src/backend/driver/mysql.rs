//! `mysql_async`-backed [`SqlClient`] (feature `mysql-executor`).
//!
//! This is the real MySQL backend of the crate, the MySQL twin of the PostgreSQL backend.
//! It runs on the dedicated driver thread owned by [`DriverExecutor`], so no statement ever
//! blocks a Tokio worker, and — because the driver executor opens a **new** connection for
//! every lock session (it calls [`SqlClient::connect`] again) — each native-lock lease gets
//! its **own** connection, which is exactly what MySQL `GET_LOCK` / `RELEASE_LOCK` require
//! (they are session scoped and die with the connection).
//!
//! The default build does **not** compile this module (and pulls in no driver crate): enable
//! the `mysql-executor` feature to use it.
//!
//! # Connection string
//!
//! `DriverExecutor::connect` takes a MySQL URL, e.g.
//! `mysql://root:secret@127.0.0.1:3306/agentscope`. The path segment (schema) matters:
//! the MySQL dialect resolves the session/snapshot/skill tables through `DATABASE()`,
//! so connecting without a database makes the schema probes return nothing.
//!
//! # Text vs binary columns
//!
//! MySQL reports `TEXT` and `BLOB` columns with the **same** wire type (`MYSQL_TYPE_BLOB`),
//! so the declared type alone cannot decide whether a cell is text or bytes. The
//! discriminator is the column's character set: `63` (the `binary` charset) means a BLOB,
//! anything else means text. Decoding purely by "does the payload happen to be UTF-8?"
//! would hand a BLOB back as [`crate::backend::ColumnValue::Text`] whenever the archive is
//! ASCII, and `JdbcRemoteSnapshotClient::download` only accepts a
//! [`crate::backend::ColumnValue::Blob`] — the snapshot round trip would break.

use mysql_async::consts::ColumnType;
use mysql_async::prelude::Queryable;
use mysql_async::{Column, Conn, Opts, Params, Value};

use super::{DriverExecutor, SqlClient};
use crate::backend::{ColumnValue, JdbcError, JdbcResult};
use crate::dialect::SqlParam;
use crate::sandbox::NativeLockFlavor;

/// Environment variable read by [`mysql_executor_from_env`] and the env-gated integration
/// tests. When unset, the driver stays disabled and reports
/// [`JdbcError::Configuration`].
pub const MYSQL_URL_ENV: &str = "AGENTSCOPE_TEST_MYSQL_URL";

/// MySQL's character-set id for `binary` (BLOB/BINARY columns).
const BINARY_CHARSET_ID: u16 = 63;

/// The driver-generic executor bound to the `mysql_async` client.
pub type MysqlExecutor = DriverExecutor<MysqlSqlClient>;

/// Connects to MySQL with native advisory locks (`GET_LOCK` / `RELEASE_LOCK`) enabled.
///
/// The configuration string is a MySQL URL, e.g.
/// `mysql://root:secret@127.0.0.1:3306/agentscope`.
pub fn connect_mysql(url: &str) -> JdbcResult<MysqlExecutor> {
    DriverExecutor::connect(url, Some(NativeLockFlavor::Mysql))
}

/// Connects using [`MYSQL_URL_ENV`].
///
/// An unset/blank variable is a [`JdbcError::Configuration`] — the integration tests use
/// exactly that to skip instead of failing (`#[ignore]` is never used).
pub fn mysql_executor_from_env() -> JdbcResult<MysqlExecutor> {
    DriverExecutor::from_env(MYSQL_URL_ENV, Some(NativeLockFlavor::Mysql))
}

/// A [`SqlClient`] over one `mysql_async` server connection.
pub struct MysqlSqlClient {
    conn: Conn,
}

impl std::fmt::Debug for MysqlSqlClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MysqlSqlClient")
            .field("connection_id", &self.conn.id())
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl SqlClient for MysqlSqlClient {
    async fn connect(url: &str) -> JdbcResult<Self> {
        let opts = Opts::from_url(url).map_err(|error| {
            JdbcError::Configuration(format!(
                "invalid MySQL URL (expected mysql://user:password@host:port/database): {error}"
            ))
        })?;
        let conn = Conn::new(opts)
            .await
            .map_err(|error| JdbcError::Database(format!("MySQL connection failed: {error}")))?;
        Ok(Self { conn })
    }

    /// Statements **without** bind parameters go through the text protocol.
    ///
    /// MySQL refuses to prepare several statements this crate issues — `BEGIN`,
    /// `COMMIT`, `ROLLBACK` and some DDL are not valid prepared statements — while the text
    /// protocol executes them verbatim. The affected-row count is read back from the same
    /// connection's last OK packet.
    async fn execute(&mut self, sql: &str, params: &[SqlParam]) -> JdbcResult<usize> {
        if params.is_empty() {
            self.conn.query_drop(sql).await.map_err(map_error)?;
            return Ok(self.conn.affected_rows() as usize);
        }
        let result = self
            .conn
            .exec_iter(sql, to_params(params))
            .await
            .map_err(map_error)?;
        Ok(result.affected_rows() as usize)
    }

    async fn query(&mut self, sql: &str, params: &[SqlParam]) -> JdbcResult<Vec<Vec<ColumnValue>>> {
        let mut result = self
            .conn
            .exec_iter(sql, to_params(params))
            .await
            .map_err(map_error)?;
        // Column metadata comes with the result set; copy it before consuming the rows.
        let columns = result.columns_ref().to_vec();
        let rows = result.collect::<mysql_async::Row>().await.map_err(map_error)?;
        Ok(rows.iter().map(|row| decode_row(row, &columns)).collect())
    }

    /// Uses the **text** query protocol: `BEGIN` / `COMMIT` / `ROLLBACK` are not preparable.
    async fn batch_execute(&mut self, sql: &str) -> JdbcResult<()> {
        self.conn.query_drop(sql).await.map_err(map_error)
    }
}

/// Maps the crate-wide [`SqlParam`] onto owned `mysql_async` values.
fn to_values(params: &[SqlParam]) -> Vec<Value> {
    params
        .iter()
        .map(|param| match param {
            SqlParam::Text(value) => Value::from(value.clone()),
            SqlParam::Int(value) => Value::from(*value),
            SqlParam::Blob(value) => Value::from(value.clone()),
            SqlParam::Null => Value::NULL,
        })
        .collect()
}

/// Builds the driver parameter list; an empty slice becomes [`Params::Empty`].
fn to_params(params: &[SqlParam]) -> Params {
    if params.is_empty() {
        Params::Empty
    } else {
        Params::Positional(to_values(params))
    }
}

/// Whether the declared column is a binary (BLOB/BINARY/GEOMETRY) column.
///
/// MySQL shares one wire type for `TEXT` and `BLOB`; only the character set separates
/// them (`binary` = 63).
fn column_is_binary(column: &Column) -> bool {
    let blob_family = matches!(
        column.column_type(),
        ColumnType::MYSQL_TYPE_TINY_BLOB
            | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
            | ColumnType::MYSQL_TYPE_LONG_BLOB
            | ColumnType::MYSQL_TYPE_BLOB
            | ColumnType::MYSQL_TYPE_GEOMETRY
    );
    blob_family && column.character_set() == BINARY_CHARSET_ID
}

/// Decodes one cell. `binary` is the column's [`column_is_binary`] verdict.
fn decode_cell(value: Option<&Value>, binary: bool) -> ColumnValue {
    match value {
        None | Some(Value::NULL) => ColumnValue::Null,
        Some(Value::Int(value)) => ColumnValue::Int(*value),
        // `BIGINT UNSIGNED` values above `i64::MAX` saturate: the column model has no u64.
        Some(Value::UInt(value)) => ColumnValue::Int(i64::try_from(*value).unwrap_or(i64::MAX)),
        // Floats are not part of the component schema; keep them readable as text.
        Some(Value::Float(value)) => ColumnValue::Text(value.to_string()),
        Some(Value::Double(value)) => ColumnValue::Text(value.to_string()),
        Some(Value::Date(year, month, day, hour, minute, second, micros)) => ColumnValue::Text(
            format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{micros:06}"),
        ),
        Some(Value::Time(negative, days, hour, minute, second, micros)) => {
            ColumnValue::Text(format!(
                "{}{days} {hour:02}:{minute:02}:{second:02}.{micros:06}",
                if *negative { "-" } else { "" }
            ))
        }
        Some(Value::Bytes(bytes)) => {
            if binary {
                ColumnValue::Blob(bytes.clone())
            } else {
                // A TEXT column should be valid UTF-8; lossy conversion keeps a mis-encoded
                // value readable instead of failing the whole query.
                ColumnValue::Text(String::from_utf8_lossy(bytes).into_owned())
            }
        }
    }
}

/// Decodes one row against its result-set metadata.
fn decode_row(row: &mysql_async::Row, columns: &[Column]) -> Vec<ColumnValue> {
    (0..row.len())
        .map(|index| {
            let binary = columns.get(index).map(column_is_binary).unwrap_or(false);
            decode_cell(row.as_ref(index), binary)
        })
        .collect()
}

/// Maps a driver error, keeping the MySQL error code and SQLSTATE in the message.
///
/// [`crate::backend::is_unique_violation`] keys on the text (`duplicate`, `23000`);
/// `mysql_async`'s `Display` for a server error does not guarantee the SQLSTATE, so it is
/// appended explicitly.
fn map_error(error: mysql_async::Error) -> JdbcError {
    match &error {
        mysql_async::Error::Server(server) => JdbcError::Database(format!(
            "{}: {} (SQLSTATE {})",
            server.code, server.message, server.state
        )),
        _ => JdbcError::Database(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_map_onto_driver_values() {
        let params = vec![
            SqlParam::Text("name".to_string()),
            SqlParam::Int(-7),
            SqlParam::Blob(vec![0, 159, 146, 150]),
            SqlParam::Null,
        ];
        assert_eq!(
            to_values(&params),
            vec![
                Value::Bytes(b"name".to_vec()),
                Value::Int(-7),
                Value::Bytes(vec![0, 159, 146, 150]),
                Value::NULL,
            ]
        );
        assert_eq!(to_params(&params), Params::Positional(to_values(&params)));
    }

    #[test]
    fn empty_parameter_lists_become_params_empty() {
        assert_eq!(to_params(&[]), Params::Empty);
        assert!(to_values(&[]).is_empty());
    }

    #[test]
    fn blobs_and_text_share_a_wire_type_and_differ_by_charset() {
        let longblob =
            Column::new(ColumnType::MYSQL_TYPE_BLOB).with_character_set(BINARY_CHARSET_ID);
        let longtext = Column::new(ColumnType::MYSQL_TYPE_BLOB).with_character_set(255);
        assert!(column_is_binary(&longblob));
        assert!(!column_is_binary(&longtext));

        // An ASCII-safe archive must still come back as a BLOB: the snapshot client only
        // accepts `ColumnValue::Blob`.
        let ascii = Value::Bytes(b"plain ascii archive".to_vec());
        assert_eq!(
            decode_cell(Some(&ascii), column_is_binary(&longblob)),
            ColumnValue::Blob(b"plain ascii archive".to_vec())
        );
        assert_eq!(
            decode_cell(Some(&ascii), column_is_binary(&longtext)),
            ColumnValue::Text("plain ascii archive".to_string())
        );

        // A non-BLOB column is never binary, whatever its payload.
        let varchar =
            Column::new(ColumnType::MYSQL_TYPE_VAR_STRING).with_character_set(BINARY_CHARSET_ID);
        assert!(!column_is_binary(&varchar));
    }

    #[test]
    fn every_value_kind_decodes_to_a_column_value() {
        assert_eq!(decode_cell(None, false), ColumnValue::Null);
        assert_eq!(decode_cell(Some(&Value::NULL), true), ColumnValue::Null);
        assert_eq!(decode_cell(Some(&Value::Int(42)), false), ColumnValue::Int(42));
        assert_eq!(
            decode_cell(Some(&Value::UInt(9)), false),
            ColumnValue::Int(9)
        );
        assert_eq!(
            decode_cell(Some(&Value::UInt(u64::MAX)), false),
            ColumnValue::Int(i64::MAX),
            "BIGINT UNSIGNED saturates instead of wrapping negative"
        );
        assert_eq!(
            decode_cell(Some(&Value::Float(1.5)), false),
            ColumnValue::Text("1.5".to_string())
        );
        assert_eq!(
            decode_cell(Some(&Value::Double(2.25)), false),
            ColumnValue::Text("2.25".to_string())
        );
        assert_eq!(
            decode_cell(Some(&Value::Date(2026, 10, 31, 1, 2, 3, 4)), false),
            ColumnValue::Text("2026-10-31 01:02:03.000004".to_string())
        );
        assert_eq!(
            decode_cell(Some(&Value::Time(true, 1, 2, 3, 4, 5)), false),
            ColumnValue::Text("-1 02:03:04.000005".to_string())
        );
        // Invalid UTF-8 in a text column is replaced, never fatal.
        assert_eq!(
            decode_cell(Some(&Value::Bytes(vec![0xff])), false),
            ColumnValue::Text("\u{fffd}".to_string())
        );
    }
}
