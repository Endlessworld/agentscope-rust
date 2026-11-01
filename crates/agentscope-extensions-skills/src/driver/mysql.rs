//! Real MySQL [`SqlDatabase`] backed by `mysql_async` (feature `mysql-driver`).
//!
//! The repositories already emit `?` placeholders, which is MySQL's own syntax, so no
//! statement rewriting is needed here. `insert_returning_id` uses the connection's
//! `LAST_INSERT_ID()` (MySQL has no `RETURNING`), and the transaction methods mirror JDBC:
//! `set_auto_commit(false)` → `START TRANSACTION`, `commit` → `COMMIT`, `rollback` →
//! `ROLLBACK`.
//!
//! Built with `mysql_async`'s `minimal` feature set by default (no TLS backend); the opt-in
//! `mysql-tls` feature adds its `rustls` backend, after which the connection URL decides whether
//! TLS is used (`?require_ssl=true`).

use std::sync::Arc;

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, Params, Value};

use super::{db_error, SyncBridge};
use crate::error::SkillRepositoryError;
use crate::sql::{SqlConnection, SqlDatabase, SqlParam, SqlRow, SqlValue};

const BACKEND: &str = "mysql";

/// A [`SqlDatabase`] over a live MySQL / MariaDB server.
///
/// ```no_run
/// use agentscope_extensions_skills::{MysqlSqlDatabase, SqlDatabase};
/// let db = MysqlSqlDatabase::connect("mysql://root:secret@127.0.0.1:3306/agentscope")?;
/// let _conn = db.connection()?;
/// # Ok::<(), agentscope_extensions_skills::SkillRepositoryError>(())
/// ```
///
/// # TLS
///
/// TLS is enabled with the `mysql-tls` feature and requested by the URL: `?require_ssl=true`
/// makes `mysql_async` wrap the connection in `rustls` (`SslOpts::default()`: the bundled
/// `webpki-roots` as trust anchors, server name validated), `verify_identity=false` drops the
/// name check and `verify_ca=false` accepts any certificate. Without the feature a URL carrying
/// `require_ssl=true` is rejected at construction instead of failing at the handshake.
pub struct MysqlSqlDatabase {
    bridge: Arc<SyncBridge>,
    url: String,
}

impl MysqlSqlDatabase {
    /// Connects using a MySQL connection URL
    /// (`mysql://user:password@host:3306/database`).
    ///
    /// The URL is validated eagerly (and one connection is opened and closed) so a malformed
    /// DSN or an unreachable server surfaces here instead of on first use.
    pub fn connect(url: &str) -> Result<Self, SkillRepositoryError> {
        let bridge = Arc::new(SyncBridge::new(BACKEND)?);
        let database = Self {
            bridge,
            url: url.to_string(),
        };
        let connection = database.open()?;
        database.close(connection)?;
        Ok(database)
    }

    /// The connection URL this handle was built from.
    pub fn url(&self) -> &str {
        &self.url
    }

    fn open(&self) -> Result<Conn, SkillRepositoryError> {
        self.bridge
            .run(Conn::new(assemble(&self.url)?))
            .map_err(|e| db_error(BACKEND, e))?
            .map_err(|e| db_error(BACKEND, format!("connection failed: {e}")))
    }

    fn close(&self, connection: Conn) -> Result<(), SkillRepositoryError> {
        self.bridge
            .run(connection.disconnect())
            .map_err(|e| db_error(BACKEND, e))?
            .map_err(|e| db_error(BACKEND, format!("disconnect failed: {e}")))
    }
}

/// Parses the connection URL into driver options and enforces the TLS contract.
///
/// `mysql_async` turns `?require_ssl=true` into `SslOpts::default()`, which only works when a TLS
/// backend is compiled in (`mysql-tls`); without one the handshake would fail on first use.
fn assemble(url: &str) -> Result<Opts, SkillRepositoryError> {
    let opts = Opts::from_url(url)
        .map_err(|e| db_error(BACKEND, format!("invalid connection URL: {e}")))?;
    if !cfg!(feature = "mysql-tls") && opts.ssl_opts().is_some() {
        return Err(db_error(
            BACKEND,
            "the connection URL requires TLS (`require_ssl=true`) but this build has no TLS \
             backend; enable the `mysql-tls` feature",
        ));
    }
    Ok(opts)
}

impl SqlDatabase for MysqlSqlDatabase {
    type Connection = MysqlSqlConnection;

    fn connection(&self) -> Result<Self::Connection, SkillRepositoryError> {
        Ok(MysqlSqlConnection {
            bridge: Arc::clone(&self.bridge),
            connection: self.open()?,
            in_transaction: false,
        })
    }
}

/// A single MySQL session, mirroring a JDBC `Connection`.
pub struct MysqlSqlConnection {
    bridge: Arc<SyncBridge>,
    connection: Conn,
    in_transaction: bool,
}

/// Converts seam parameters into driver values.
fn positional(params: &[SqlParam]) -> Params {
    Params::Positional(
        params
            .iter()
            .map(|p| match p {
                SqlParam::Null => Value::NULL,
                SqlParam::Int(v) => Value::from(*v),
                SqlParam::Text(v) => Value::from(v.clone()),
            })
            .collect(),
    )
}

/// Maps a driver value onto the seam's three-value model.
fn decode_value(value: &Value) -> SqlValue {
    match value {
        Value::NULL => SqlValue::Null,
        Value::Int(v) => SqlValue::Int(*v),
        Value::UInt(v) => SqlValue::Int(*v as i64),
        Value::Bytes(bytes) => SqlValue::Text(String::from_utf8_lossy(bytes).into_owned()),
        // Every other driver value (floats, dates, …) is outside the seam's three-value
        // model; the repositories only read text and integer columns.
        _ => SqlValue::Null,
    }
}

/// Builds a [`SqlRow`] from a driver row (column names come from the result metadata).
fn to_sql_row(row: &mysql_async::Row) -> SqlRow {
    let cells = row
        .columns_ref()
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let name = column.name_str().to_string();
            let value = row.as_ref(index).map(decode_value).unwrap_or(SqlValue::Null);
            (name, value)
        });
    SqlRow::new(cells)
}

impl MysqlSqlConnection {
    fn simple(&mut self, sql: &'static str) -> Result<(), SkillRepositoryError> {
        let bridge = Arc::clone(&self.bridge);
        let connection = &mut self.connection;
        bridge
            .run(async move { connection.query_drop(sql).await })
            .map_err(|e| db_error(BACKEND, e))?
            .map_err(|e| db_error(BACKEND, format!("{sql} failed: {e}")))
    }
}

impl SqlConnection for MysqlSqlConnection {
    fn execute(&mut self, sql: &str, params: &[SqlParam]) -> Result<usize, SkillRepositoryError> {
        let bridge = Arc::clone(&self.bridge);
        let connection = &mut self.connection;
        let bound = positional(params);
        let statement = sql.to_string();
        bridge
            .run(async move { connection.exec_drop(statement, bound).await })
            .map_err(|e| db_error(BACKEND, e))?
            .map_err(|e| db_error(BACKEND, format!("execute failed: {e}")))?;
        Ok(self.connection.affected_rows() as usize)
    }

    fn query(
        &mut self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<Vec<SqlRow>, SkillRepositoryError> {
        let bridge = Arc::clone(&self.bridge);
        let connection = &mut self.connection;
        let bound = positional(params);
        let statement = sql.to_string();
        let rows = bridge
            .run(async move {
                let mut result = connection.exec_iter(statement, bound).await?;
                let mut rows = Vec::new();
                while let Some(row) = result.next().await? {
                    rows.push(row);
                }
                Ok::<_, mysql_async::Error>(rows)
            })
            .map_err(|e| db_error(BACKEND, e))?
            .map_err(|e| db_error(BACKEND, format!("query failed: {e}")))?;
        Ok(rows.iter().map(to_sql_row).collect())
    }

    fn insert_returning_id(
        &mut self,
        sql: &str,
        params: &[SqlParam],
    ) -> Result<i64, SkillRepositoryError> {
        self.execute(sql, params)?;
        Ok(self
            .connection
            .last_insert_id()
            .map(|id| id as i64)
            .unwrap_or(0))
    }

    fn set_auto_commit(&mut self, enabled: bool) -> Result<(), SkillRepositoryError> {
        if enabled {
            if self.in_transaction {
                self.simple("COMMIT")?;
                self.in_transaction = false;
            }
        } else if !self.in_transaction {
            self.simple("START TRANSACTION")?;
            self.in_transaction = true;
        }
        Ok(())
    }

    fn commit(&mut self) -> Result<(), SkillRepositoryError> {
        if self.in_transaction {
            self.simple("COMMIT")?;
            self.in_transaction = false;
        }
        Ok(())
    }

    fn rollback(&mut self) -> Result<(), SkillRepositoryError> {
        if self.in_transaction {
            self.simple("ROLLBACK")?;
            self.in_transaction = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_keep_their_types() {
        match positional(&[
            SqlParam::Null,
            SqlParam::Int(7),
            SqlParam::Text("x".to_string()),
        ]) {
            Params::Positional(values) => {
                assert_eq!(values.len(), 3);
                assert_eq!(decode_value(&values[0]), SqlValue::Null);
                assert_eq!(decode_value(&values[1]), SqlValue::Int(7));
                assert_eq!(decode_value(&values[2]).as_text(), Some("x"));
            }
            _ => panic!("expected positional parameters"),
        }
    }

    #[test]
    fn plain_urls_stay_plain_tcp() {
        let opts = assemble("mysql://root@127.0.0.1:3306/agentscope").expect("opts");
        assert!(opts.ssl_opts().is_none());
    }

    #[test]
    fn tls_urls_are_rejected_exactly_when_no_backend_is_compiled_in() {
        let assembled = assemble("mysql://root@127.0.0.1:3306/agentscope?require_ssl=true");
        assert_eq!(
            assembled.is_ok(),
            cfg!(feature = "mysql-tls"),
            "`require_ssl=true` must fail exactly when no TLS backend is compiled in"
        );
    }

    #[cfg(feature = "mysql-tls")]
    #[test]
    fn a_tls_url_assembles_the_default_ssl_opts() {
        let opts =
            assemble("mysql://root@127.0.0.1:3306/agentscope?require_ssl=true").expect("opts");
        assert_eq!(opts.ssl_opts(), Some(&mysql_async::SslOpts::default()));

        let relaxed = assemble(
            "mysql://root@127.0.0.1:3306/agentscope?require_ssl=true&verify_identity=false",
        )
        .expect("opts");
        assert!(relaxed.ssl_opts().expect("ssl opts").skip_domain_validation());
    }

    #[cfg(not(feature = "mysql-tls"))]
    #[test]
    fn connect_rejects_require_ssl_without_a_tls_backend() {
        // Rejected before any I/O (port 1 never accepts a TCP connection), so this stays offline.
        let error = match MysqlSqlDatabase::connect("mysql://root@127.0.0.1:1/none?require_ssl=true") {
            Ok(_) => panic!("a TLS-only URL must be rejected"),
            Err(e) => e,
        };
        assert!(error.to_string().contains("mysql-tls"), "{error}");
    }

    #[test]
    fn an_unreachable_server_is_an_explicit_error() {
        let error = match MysqlSqlDatabase::connect("mysql://nobody:nobody@127.0.0.1:1/none") {
            Ok(_) => panic!("connecting to a closed port must fail"),
            Err(e) => e,
        };
        assert!(
            matches!(error, SkillRepositoryError::Database(_)),
            "expected a database error, got {error:?}"
        );
        assert!(error.to_string().contains("connection failed"));
    }
}
