//! Dialect SQL-generation tests, mirroring the Java `DialectSqlTests` and
//! `AbstractJdbcDialectFromTest`.
//!
//! All assertions inspect the generated SQL templates — no database connection is made.
//! Rust has no `H2Dialect`: per `JDBC__GAP_TRACKER.md` §3.4 the H2 semantics are carried by
//! [`AnsiDialect`], so Java's `H2Dialect` assertions map onto `AnsiDialect` here.

use std::sync::Arc;

use agentscope_extensions_jdbc::dialect::vendor::{
    AnsiDialect, MysqlDialect, PostgresDialect, SqliteDialect,
};
use agentscope_extensions_jdbc::dialect::{
    detect_dialect, validate_identifier, AbstractJdbcDialect, DialectKind, JdbcDialectBuilder,
    SessionStateDialect, SnapshotDialect, StoreDialect,
};

// ---------------------------------------------------------------------------
//  Table-name resolution (prefix + base, override)
//  Java: defaultPrefixTableNames / customPrefixTableNames / perTableOverride
// ---------------------------------------------------------------------------

#[test]
fn default_prefix_table_names() {
    // Java: DialectSqlTests.defaultPrefixTableNames
    let d = PostgresDialect::new();
    assert_eq!(d.resolved_store_table_name(), "agentscope_store");
    assert_eq!(d.resolved_session_state_table_name(), "agentscope_sessions");
    assert_eq!(d.resolved_snapshot_table_name(), "agentscope_snapshots");
}

#[test]
fn default_prefix_lock_table_name() {
    // Java: AbstractJdbcDialect.lockTableName() = tablePrefix + "distributed_locks"
    let d = PostgresDialect::new();
    assert_eq!(d.resolved_lock_table_name(), "agentscope_distributed_locks");
    let ddl = d.lock_create_table_ddl();
    assert!(ddl.contains("agentscope_distributed_locks"));
    assert!(ddl.contains("lock_name VARCHAR(255) NOT NULL PRIMARY KEY"));
}

#[test]
fn custom_prefix_table_names() {
    // Java: DialectSqlTests.customPrefixTableNames
    let d = JdbcDialectBuilder::from_product_name("H2")
        .table_prefix("custom_")
        .unwrap()
        .auto_create_table(false)
        .build();
    assert_eq!(d.resolved_store_table_name(), "custom_store");
    assert_eq!(d.resolved_session_state_table_name(), "custom_sessions");
}

#[test]
fn per_table_override_wins_over_prefix() {
    // Java: DialectSqlTests.perTableOverride
    let d = JdbcDialectBuilder::from_product_name("H2")
        .table_prefix("custom_")
        .unwrap()
        .store_table_name("my_kv_table")
        .unwrap()
        .auto_create_table(false)
        .build();
    assert_eq!(d.resolved_store_table_name(), "my_kv_table");
    assert_eq!(d.resolved_session_state_table_name(), "custom_sessions");
}

#[test]
fn snapshot_table_override_is_honoured() {
    // Java: AbstractJdbcDialect.snapshotTableName() override > prefix + base
    let d = JdbcDialectBuilder::from_product_name("SQLite")
        .snapshot_table_name("my_snaps")
        .unwrap()
        .auto_create_table(false)
        .build();
    assert_eq!(d.resolved_snapshot_table_name(), "my_snaps");
    assert_eq!(d.resolved_store_table_name(), "agentscope_store");
}

// ---------------------------------------------------------------------------
//  StoreDialect — UPSERT syntax differences
//  Java: postgresStoreUpsertUsesOnConflict / mysqlStoreUpsertUsesOnDuplicateKey /
//        h2StoreUpsertUsesMergeInto / sqliteStoreUpsertUsesOnConflict
// ---------------------------------------------------------------------------

#[test]
fn postgres_store_upsert_uses_on_conflict() {
    // Java: DialectSqlTests.postgresStoreUpsertUsesOnConflict
    let bs = PostgresDialect::new().store_upsert("ns", "k", "{}", 1);
    assert!(bs.sql.contains("ON CONFLICT"));
    assert!(bs.sql.contains("EXCLUDED.value_json"));
    assert_eq!(bs.params.len(), 4);
}

#[test]
fn mysql_store_upsert_uses_on_duplicate_key() {
    // Java: DialectSqlTests.mysqlStoreUpsertUsesOnDuplicateKey
    let bs = MysqlDialect::new().store_upsert("ns", "k", "{}", 1);
    assert!(bs.sql.contains("ON DUPLICATE KEY UPDATE"));
    assert!(bs.sql.contains("VALUES(value_json)"));
}

#[test]
fn h2_store_upsert_uses_merge_into() {
    // Java: DialectSqlTests.h2StoreUpsertUsesMergeInto (AnsiDialect carries H2 semantics)
    let bs = AnsiDialect::new().store_upsert("ns", "k", "{}", 1);
    assert!(bs.sql.contains("MERGE INTO"));
}

#[test]
fn sqlite_store_upsert_uses_on_conflict() {
    // Java: DialectSqlTests.sqliteStoreUpsertUsesOnConflict
    let bs = SqliteDialect::new().store_upsert("ns", "k", "{}", 1);
    assert!(bs.sql.contains("ON CONFLICT"));
}

// ---------------------------------------------------------------------------
//  StoreDialect — DDL type differences
//  Java: mysqlStoreDdlHasInnoDB / sqliteStoreDdlUsesTextInteger / h2StoreDdlUsesClob
// ---------------------------------------------------------------------------

#[test]
fn mysql_store_ddl_has_innodb_utf8mb4_longtext() {
    // Java: DialectSqlTests.mysqlStoreDdlHasInnoDB
    let ddl = MysqlDialect::new().store_create_table_ddls().remove(0);
    assert!(ddl.contains("LONGTEXT"));
    assert!(ddl.contains("ENGINE=InnoDB"));
    assert!(ddl.contains("utf8mb4"));
}

#[test]
fn sqlite_store_ddl_uses_text_and_integer() {
    // Java: DialectSqlTests.sqliteStoreDdlUsesTextInteger
    let ddl = SqliteDialect::new().store_create_table_ddls().remove(0);
    assert!(ddl.contains("TEXT"));
    assert!(ddl.contains("INTEGER"));
}

#[test]
fn h2_store_ddl_uses_clob() {
    // Java: DialectSqlTests.h2StoreDdlUsesClob (AnsiDialect carries H2 semantics)
    let ddl = AnsiDialect::new().store_create_table_ddls().remove(0);
    assert!(ddl.contains("CLOB"));
}

// ---------------------------------------------------------------------------
//  Secondary indexes
//  Java: mysqlDdlInlinesIndexes / postgresIndexDdl / h2AndSqliteIndexDdl
// ---------------------------------------------------------------------------

#[test]
fn mysql_ddl_inlines_indexes_and_keeps_three_ddls() {
    // Java: DialectSqlTests.mysqlDdlInlinesIndexes
    let d = MysqlDialect::new();
    let store = d.store_create_table_ddls();
    assert_eq!(store.len(), 1);
    assert!(store[0].contains("INDEX idx_namespace (namespace_path)"));

    let session = d.session_state_create_table_ddls();
    assert_eq!(session.len(), 1);
    assert!(session[0].contains("INDEX idx_session (session_id)"));

    // MySQL has no CREATE INDEX IF NOT EXISTS, so the three tables stay three DDLs.
    assert_eq!(d.create_table_ddls().len(), 3);
}

#[test]
fn postgres_emits_separate_idempotent_index_ddl() {
    // Java: DialectSqlTests.postgresIndexDdl
    assert_store_and_session_indexes(&PostgresDialect::new());
}

#[test]
fn ansi_and_sqlite_emit_separate_idempotent_index_ddl() {
    // Java: DialectSqlTests.h2AndSqliteIndexDdl (H2 -> AnsiDialect)
    assert_store_and_session_indexes(&AnsiDialect::new());
    assert_store_and_session_indexes(&SqliteDialect::new());
}

fn assert_store_and_session_indexes(d: &dyn AbstractJdbcDialect) {
    let store = d.store_create_table_ddls();
    assert_eq!(store.len(), 2);
    assert!(store[0].starts_with("CREATE TABLE IF NOT EXISTS"));
    assert!(store[1].starts_with("CREATE INDEX IF NOT EXISTS"));
    assert!(store[1].contains("namespace_path"));

    let session = d.session_state_create_table_ddls();
    assert_eq!(session.len(), 2);
    assert!(session[0].starts_with("CREATE TABLE IF NOT EXISTS"));
    assert!(session[1].starts_with("CREATE INDEX IF NOT EXISTS"));
    assert!(session[1].contains("session_id"));
}

// ---------------------------------------------------------------------------
//  SessionStateDialect — UPSERT + table existence check
//  Java: mysqlStateUpsertAndCheck / h2StateUpsertAndCheck / sqliteStateCheckUsesSqliteMaster
// ---------------------------------------------------------------------------

#[test]
fn mysql_state_upsert_and_check() {
    // Java: DialectSqlTests.mysqlStateUpsertAndCheck
    let d = MysqlDialect::new();
    let upsert = d.session_state_upsert("sid", "key", 0, "data");
    assert!(upsert.sql.contains("ON DUPLICATE KEY UPDATE"));

    let check = d.session_state_check_table_exists("my_table");
    assert!(check.sql.contains("DATABASE()"));
    assert!(check.sql.contains("INFORMATION_SCHEMA"));
    assert_eq!(check.params.len(), 1);
}

#[test]
fn h2_state_upsert_and_check() {
    // Java: DialectSqlTests.h2StateUpsertAndCheck (H2 -> AnsiDialect)
    let d = AnsiDialect::new();
    let upsert = d.session_state_upsert("sid", "key", 0, "data");
    assert!(upsert.sql.contains("MERGE INTO"));

    let check = d.session_state_check_table_exists("my_table");
    assert!(check.sql.contains("INFORMATION_SCHEMA"));
}

#[test]
fn sqlite_state_check_uses_sqlite_master() {
    // Java: DialectSqlTests.sqliteStateCheckUsesSqliteMaster
    let check = SqliteDialect::new().session_state_check_table_exists("my_table");
    assert!(check.sql.contains("sqlite_master"));
}

#[test]
fn postgres_state_check_uses_information_schema() {
    // Java: PostgresDialect.sessionStateCheckTableExists
    let check = PostgresDialect::new().session_state_check_table_exists("my_table");
    assert!(check.sql.contains("information_schema.tables"));
    assert!(check.sql.contains("current_schema()"));
}

#[test]
fn session_state_update_if_version_writes_expected_plus_one() {
    // Java: SessionStateDialect.sessionStateUpdateIfVersion — binds expectedVersion + 1
    let bs = SqliteDialect::new().session_state_update_if_version("sid", "key", 0, "json", 7);
    assert!(bs.sql.contains("version = ?"));
    assert!(!bs.sql.contains("version = version + 1"));
    // params: state_data, version(=8), session_id, state_key, item_index, expected_version(=7)
    assert_eq!(bs.params.len(), 6);
    assert_eq!(bs.params[1].as_int(), Some(8));
    assert_eq!(bs.params[5].as_int(), Some(7));
}

#[test]
fn session_state_insert_if_absent_pins_version_one() {
    // Java: SessionStateDialect.sessionStateInsertIfAbsent
    let bs = SqliteDialect::new().session_state_insert_if_absent("sid", "key", 0, "json");
    assert!(bs.sql.contains("(session_id, state_key, item_index, state_data, version)"));
    assert!(bs.sql.contains("VALUES (?, ?, ?, ?, 1)"));
    assert_eq!(bs.params.len(), 4);
}

// ---------------------------------------------------------------------------
//  SnapshotDialect — UPSERT + DDL differences
//  Java: postgresSnapshotDdlUsesBytea / mysqlSnapshotDdlUsesLongBlob /
//        h2SnapshotUpsertUpdatesCreatedAt
// ---------------------------------------------------------------------------

#[test]
fn postgres_snapshot_ddl_uses_bytea() {
    // Java: DialectSqlTests.postgresSnapshotDdlUsesBytea
    let ddl = PostgresDialect::new().snapshot_create_table_ddls().remove(0);
    assert!(ddl.contains("BYTEA"));
}

#[test]
fn mysql_snapshot_ddl_uses_longblob() {
    // Java: DialectSqlTests.mysqlSnapshotDdlUsesLongBlob
    let ddl = MysqlDialect::new().snapshot_create_table_ddls().remove(0);
    assert!(ddl.contains("LONGBLOB"));
}

#[test]
fn h2_snapshot_upsert_updates_created_at() {
    // Java: DialectSqlTests.h2SnapshotUpsertUpdatesCreatedAt (H2 -> AnsiDialect)
    let bs = AnsiDialect::new().snapshot_upsert("snap", &[1, 2]);
    assert!(bs.sql.contains("MERGE INTO"));
    assert!(bs.sql.contains("WHEN MATCHED THEN UPDATE SET"));
    assert!(bs.sql.contains("created_at = CURRENT_TIMESTAMP"));
}

#[test]
fn sqlite_snapshot_upsert_uses_on_conflict() {
    // Java: SqliteDialect.snapshotUpsert
    let bs = SqliteDialect::new().snapshot_upsert("snap", &[1, 2]);
    assert!(bs.sql.contains("ON CONFLICT(snapshot_id) DO UPDATE"));
    assert!(bs.sql.contains("excluded.data"));
    assert_eq!(bs.params.len(), 2);
}

// ---------------------------------------------------------------------------
//  ANSI defaults inherited identically
//  Java: allInheritStoreInsert
// ---------------------------------------------------------------------------

#[test]
fn all_dialects_inherit_identical_store_insert() {
    // Java: DialectSqlTests.allInheritStoreInsert
    let expected = "INSERT INTO agentscope_store (namespace_path, item_key, value_json, version, updated_at) VALUES (?, ?, ?, 1, ?)";
    assert_eq!(PostgresDialect::new().store_insert("a", "b", "c", 1).sql, expected);
    assert_eq!(MysqlDialect::new().store_insert("a", "b", "c", 1).sql, expected);
    assert_eq!(AnsiDialect::new().store_insert("a", "b", "c", 1).sql, expected);
    assert_eq!(SqliteDialect::new().store_insert("a", "b", "c", 1).sql, expected);
}

#[test]
fn all_dialects_inherit_identical_store_select_and_delete() {
    // Java: StoreDialect default storeSelect / storeDelete
    let expected_select =
        "SELECT value_json, version FROM agentscope_store WHERE namespace_path = ? AND item_key = ?";
    let expected_delete =
        "DELETE FROM agentscope_store WHERE namespace_path = ? AND item_key = ?";
    for d in [
        Arc::new(PostgresDialect::new()) as Arc<dyn AbstractJdbcDialect>,
        Arc::new(MysqlDialect::new()),
        Arc::new(AnsiDialect::new()),
        Arc::new(SqliteDialect::new()),
    ] {
        assert_eq!(d.store_select("ns", "k").sql, expected_select);
        assert_eq!(d.store_delete("ns", "k").sql, expected_delete);
    }
}

#[test]
fn store_search_uses_dialect_escape_char_and_pagination() {
    // Java: StoreDialect.storeSearch / storeLikeEscapeChar (default '!')
    let bs = SqliteDialect::new().store_search("ns%", 10, 0);
    assert!(bs.sql.contains("LIKE ? ESCAPE '!'"));
    assert!(bs.sql.contains("ORDER BY item_key LIMIT ? OFFSET ?"));
    assert_eq!(bs.params.len(), 3);
    assert_eq!(SqliteDialect::new().store_like_escape_char(), '!');
    assert_eq!(SqliteDialect::new().session_state_like_escape_char(), '!');
}

// ---------------------------------------------------------------------------
//  Lock-strategy surface (Java: mysqlDialectIsLockStrategy)
// ---------------------------------------------------------------------------

#[test]
fn mysql_dialect_declares_native_lock_support() {
    // Java: DialectSqlTests.mysqlDialectIsLockStrategy.
    // Rust does not implement `SandboxLockStrategy` on the MySQL dialect (native GET_LOCK is
    // a tracked P1 gap, §3.2), so this asserts the dialect contract it *does* expose.
    let d: Arc<dyn AbstractJdbcDialect> = Arc::new(MysqlDialect::new());
    assert_eq!(d.kind(), DialectKind::Mysql);
    assert!(d.supports_native_locks());
}

#[test]
fn portable_dialects_do_not_report_native_locks() {
    // Java: AbstractJdbcDialect.supportsNativeLocks() default = false
    for d in [
        Arc::new(AnsiDialect::new()) as Arc<dyn AbstractJdbcDialect>,
        Arc::new(SqliteDialect::new()),
    ] {
        assert!(!d.supports_native_locks());
    }
    assert!(PostgresDialect::new().supports_native_locks());
}

// ---------------------------------------------------------------------------
//  InnoDB utf8mb4 index limit (ported from Java MysqlJdbcStoreDialectTest)
//  Java: mysqlStorePkFitsInnoDbUtf8mb4Limit
// ---------------------------------------------------------------------------

const INNODB_UTF8MB4_INDEX_LIMIT_BYTES: u64 = 3072;
const UTF8MB4_MAX_BYTES_PER_CHAR: u64 = 4;

#[test]
fn mysql_store_pk_fits_innodb_utf8mb4_limit() {
    // Java: DialectSqlTests.mysqlStorePkFitsInnoDbUtf8mb4Limit
    let ddl = MysqlDialect::new().store_create_table_ddls().remove(0);
    let namespace_path_len = varchar_length(&ddl, "namespace_path");
    let item_key_len = varchar_length(&ddl, "item_key");
    let composite_pk_bytes =
        (namespace_path_len + item_key_len) * UTF8MB4_MAX_BYTES_PER_CHAR;
    assert!(
        composite_pk_bytes <= INNODB_UTF8MB4_INDEX_LIMIT_BYTES,
        "Composite PK is {composite_pk_bytes} bytes, over the InnoDB utf8mb4 limit of {INNODB_UTF8MB4_INDEX_LIMIT_BYTES}"
    );
}

fn varchar_length(ddl: &str, column_name: &str) -> u64 {
    let lower = ddl.to_ascii_lowercase();
    let needle = format!("{} varchar(", column_name);
    let pos = lower
        .find(&needle)
        .unwrap_or_else(|| panic!("Missing VARCHAR definition for {column_name}"));
    let rest = &ddl[pos + needle.len()..];
    let end = rest.find(')').expect("malformed VARCHAR(...)");
    rest[..end].trim().parse().expect("numeric VARCHAR length")
}

// ---------------------------------------------------------------------------
//  Table-name validation (SQL-injection guard)
//  Java: builderRejectsInvalidPrefix / builderRejectsInvalidTableName / builderAcceptsValidNames
// ---------------------------------------------------------------------------

#[test]
fn builder_rejects_invalid_prefix() {
    // Java: DialectSqlTests.builderRejectsInvalidPrefix
    let result = JdbcDialectBuilder::from_product_name("H2").table_prefix("evil; DROP TABLE");
    assert!(result.is_err());
}

#[test]
fn builder_rejects_invalid_store_table_name() {
    // Java: DialectSqlTests.builderRejectsInvalidTableName
    let result =
        JdbcDialectBuilder::from_product_name("H2").store_table_name("t; DROP TABLE users");
    assert!(result.is_err());
}

#[test]
fn builder_accepts_valid_names() {
    // Java: DialectSqlTests.builderAcceptsValidNames
    let d = JdbcDialectBuilder::from_product_name("H2")
        .table_prefix("my_app_")
        .unwrap()
        .store_table_name("my_store")
        .unwrap()
        .auto_create_table(false)
        .build();
    assert_eq!(d.resolved_store_table_name(), "my_store");
    assert_eq!(d.resolved_session_state_table_name(), "my_app_sessions");
}

#[test]
fn validate_identifier_boundaries() {
    // Java: AbstractJdbcDialectBuilder.VALID_IDENTIFIER regex semantics
    assert!(validate_identifier("ok_1", "f").is_ok());
    assert!(validate_identifier("_lead", "f").is_ok());
    assert!(validate_identifier("My_Table1", "f").is_ok());

    assert!(validate_identifier("", "f").is_err());
    assert!(validate_identifier("1abc", "f").is_err());
    assert!(validate_identifier("abc-def", "f").is_err());
    assert!(validate_identifier("abc def", "f").is_err());
    assert!(validate_identifier("evil; DROP", "f").is_err());
}

// ---------------------------------------------------------------------------
//  Dialect auto-detection
//  Java: AbstractJdbcDialectFromTest
// ---------------------------------------------------------------------------

#[test]
fn detects_postgres() {
    // Java: AbstractJdbcDialectFromTest.detectsPostgres
    assert_eq!(detect_dialect("PostgreSQL").kind(), DialectKind::Postgres);
}

#[test]
fn detects_mysql() {
    // Java: AbstractJdbcDialectFromTest.detectsMysql
    assert_eq!(detect_dialect("MySQL").kind(), DialectKind::Mysql);
}

#[test]
fn detects_mariadb_as_mysql() {
    // Java: AbstractJdbcDialectFromTest.detectsMariaDb
    assert_eq!(detect_dialect("MariaDB").kind(), DialectKind::Mysql);
}

#[test]
fn detects_h2_as_ansi() {
    // Java: AbstractJdbcDialectFromTest.detectsH2 returns H2Dialect.
    // Rust has no H2Dialect (§3.4): H2 (and unknown databases) map to AnsiDialect.
    assert_eq!(detect_dialect("H2").kind(), DialectKind::Ansi);
}

#[test]
fn detects_sqlite() {
    // Java: AbstractJdbcDialectFromTest.detectsSqlite
    assert_eq!(detect_dialect("SQLite").kind(), DialectKind::Sqlite);
}

#[test]
fn detection_is_case_insensitive_and_defaults_to_ansi() {
    // Java: AbstractJdbcDialectFromTest.throwsForUnsupported (Oracle -> IllegalStateException).
    // Rust has no SPI failure mode: unsupported product names fall back to AnsiDialect and the
    // caller decides whether that is acceptable (documented in §3.3/§3.4).
    assert_eq!(detect_dialect("sqlite").kind(), DialectKind::Sqlite);
    assert_eq!(detect_dialect("postgresql").kind(), DialectKind::Postgres);
    assert_eq!(detect_dialect("Oracle").kind(), DialectKind::Ansi);
    assert_eq!(detect_dialect("").kind(), DialectKind::Ansi);
}

// ---------------------------------------------------------------------------
//  BoundSql / SqlParam plumbing
// ---------------------------------------------------------------------------

#[test]
fn bound_sql_exposes_text_and_int_params_in_order() {
    let bs = PostgresDialect::new().store_cas_update("json", 42, "ns", "k", 3);
    assert_eq!(bs.params[0].as_text(), Some("json"));
    assert_eq!(bs.params[1].as_int(), Some(42));
    assert_eq!(bs.params[2].as_text(), Some("ns"));
    assert_eq!(bs.params[3].as_text(), Some("k"));
    assert_eq!(bs.params[4].as_int(), Some(3));
}
