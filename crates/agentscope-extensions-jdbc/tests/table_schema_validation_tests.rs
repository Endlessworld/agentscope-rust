//! Assemble-time table schema validation, mirroring Java `TableSchemaValidator`.
//!
//! Backend: in-memory SQLite, the same pairing the other JDBC offline tests use.

use std::sync::Arc;

use agentscope_extensions_jdbc::dialect::vendor::SqliteDialect;
use agentscope_extensions_jdbc::dialect::{
    validate_table_schema, AbstractJdbcDialect, BoundSql, JdbcDialectBuilder, SessionStateDialect,
    StoreDialect,
};
use agentscope_extensions_jdbc::{JdbcAgentStateStore, JdbcExecutor, JdbcStore, RusqliteExecutor};

fn dialect() -> Arc<dyn AbstractJdbcDialect> {
    Arc::new(SqliteDialect::new())
}

#[test]
fn validation_passes_right_after_auto_creation() {
    // The happy path is the one every other JDBC test exercises implicitly; assert it
    // directly so a broken probe cannot hide behind unrelated assertions.
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect = dialect();
    let ddls = dialect.store_create_table_ddls();
    for ddl in &ddls {
        executor.execute(&BoundSql::no_params(ddl.clone())).unwrap();
    }
    validate_table_schema(
        executor.as_ref(),
        dialect.as_ref(),
        &dialect.resolved_store_table_name(),
        &ddls,
    )
    .expect("a freshly created table must validate");
}

#[test]
fn missing_table_is_reported_not_silently_accepted() {
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect = dialect();
    let ddls = dialect.store_create_table_ddls();
    let table = dialect.resolved_store_table_name();

    let err = validate_table_schema(executor.as_ref(), dialect.as_ref(), &table, &ddls)
        .expect_err("an absent table must fail");
    let message = err.to_string();
    assert!(message.contains("is missing"), "unexpected: {message}");
    assert!(
        message.contains(&table),
        "table name must be named: {message}"
    );
    assert!(
        message.contains("never alters existing tables"),
        "the error must explain the boundary: {message}"
    );
}

#[test]
fn stale_table_with_a_missing_column_fails_auto_creation() {
    // Simulates a deployed table that predates a column: the DDL uses IF NOT EXISTS,
    // so it does not add the column — exactly the case the validator exists for.
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect = dialect();
    let table = dialect.resolved_store_table_name();
    executor
        .execute(&BoundSql::no_params(format!(
            "CREATE TABLE {table} (namespace_path TEXT NOT NULL, item_key TEXT NOT NULL, \
             PRIMARY KEY (namespace_path, item_key))"
        )))
        .unwrap();

    let err = JdbcStore::new(executor, Arc::clone(&dialect), true)
        .expect_err("a stale table must fail schema initialization");
    let message = err.to_string();
    assert!(
        message.contains("missing column(s)"),
        "unexpected: {message}"
    );
    assert!(
        message.contains("value_json"),
        "must list the missing column: {message}"
    );
    assert!(
        message.contains("version"),
        "must list every missing column: {message}"
    );
}

#[test]
fn stale_session_state_table_fails_auto_creation() {
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect = dialect();
    let table = dialect.resolved_session_state_table_name();
    executor
        .execute(&BoundSql::no_params(format!(
            "CREATE TABLE {table} (session_id TEXT NOT NULL, state_key TEXT NOT NULL)"
        )))
        .unwrap();

    let err = JdbcAgentStateStore::new(executor, Arc::clone(&dialect), true)
        .expect_err("a stale session-state table must fail schema initialization");
    assert!(
        err.to_string().contains("missing column(s)"),
        "unexpected: {err}"
    );
}

#[test]
fn non_identifier_table_name_is_rejected_before_sql() {
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect = dialect();
    let ddls = dialect.store_create_table_ddls();
    let err = validate_table_schema(
        executor.as_ref(),
        dialect.as_ref(),
        "t; DROP TABLE x",
        &ddls,
    )
    .expect_err("an unsafe table name must be rejected");
    assert!(
        err.to_string().contains("invalid JDBC table name"),
        "unexpected: {err}"
    );
}

#[test]
fn unparseable_ddl_skips_validation() {
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect = dialect();
    // An empty DDL list means the dialect contributes nothing to validate.
    validate_table_schema(executor.as_ref(), dialect.as_ref(), "agentscope_store", &[])
        .expect("unparseable/absent DDL must skip validation, not fail");
}

#[test]
fn custom_table_names_are_validated_under_their_configured_name() {
    let executor = Arc::new(RusqliteExecutor::in_memory().unwrap());
    let dialect: Arc<dyn AbstractJdbcDialect> =
        JdbcDialectBuilder::new(Arc::new(SqliteDialect::new()))
            .store_table_name("custom_store")
            .expect("valid identifier")
            .build();
    // Auto-creation under the custom name must pass its own validation.
    JdbcStore::new(executor, dialect, true).expect("custom table must be created and validated");
}
