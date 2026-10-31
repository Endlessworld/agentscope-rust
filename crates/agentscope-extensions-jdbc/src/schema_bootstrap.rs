//! Serialized schema bootstrap.
//!
//! Exposed defect (found by the environment-gated PostgreSQL integration test): two
//! bootstraps running concurrently against the same PostgreSQL database both execute the
//! dialect's `CREATE TABLE IF NOT EXISTS` DDL, and PostgreSQL is **not** atomic there — the
//! second one fails with SQLSTATE `23505`
//! (`duplicate key value violates unique constraint "pg_type_typname_nsp_index"`), because the
//! table's implicit composite type is registered before the *relation* is visible to the
//! concurrent `IF NOT EXISTS` probe. The losing store then cannot be used at all.
//!
//! MySQL (metadata lock) and SQLite (single writer) do not have this race, so only PostgreSQL
//! takes the extra `pg_advisory_lock` round trip. The lock is held for the duration of the DDL
//! (a few milliseconds) and released even when a statement fails, so a broken dialect can
//! never keep it for the lifetime of the session.

use crate::backend::{JdbcExecutor, JdbcResult};
use crate::dialect::{AbstractJdbcDialect, BoundSql, DialectKind};

/// Advisory-lock key for schema bootstrap. `AGENTSCOPE` as ASCII (`0x4147454E5453434F`).
///
/// Deliberately a single constant for the whole schema: bootstrapping is rare and short, and
/// one key also serializes the *store*, *session*, *snapshot* and *skill* tables that a
/// `JdbcDistributedStore` creates back to back.
pub(crate) const SCHEMA_BOOTSTRAP_LOCK_KEY: i64 = 0x4147_454E_5453_434F;

/// Runs the dialect's idempotent DDL, serialized against concurrent bootstraps.
pub(crate) fn run_ddl(
    executor: &dyn JdbcExecutor,
    dialect: &dyn AbstractJdbcDialect,
    ddls: &[String],
) -> JdbcResult<()> {
    let serialized = dialect.kind() == DialectKind::Postgres;
    if serialized {
        // `execute`, not `query`: `pg_advisory_lock` returns `void`, and a `void` result row
        // has no decodable value.
        executor.execute(&BoundSql::no_params(format!(
            "SELECT pg_advisory_lock({SCHEMA_BOOTSTRAP_LOCK_KEY})"
        )))?;
    }

    let mut outcome = Ok(());
    for ddl in ddls {
        if let Err(error) = executor.execute(&BoundSql::no_params(ddl.clone())) {
            outcome = Err(error);
            break;
        }
    }

    if serialized {
        // Best effort: the DDL result is what the caller has to see, and failing to unlock
        // can only happen if the connection itself is gone.
        let _ = executor.execute(&BoundSql::no_params(format!(
            "SELECT pg_advisory_unlock({SCHEMA_BOOTSTRAP_LOCK_KEY})"
        )));
    }

    outcome
}
