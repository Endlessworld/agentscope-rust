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
    detect_dialect, validate_identifier, AbstractJdbcDialect, BoundSql, DialectKind,
    JdbcDialectBuilder, SessionStateDialect, SkillDialect, SkillResourcesDialect, SnapshotDialect,
    StoreDialect, TableNameConfigurable, TableNames,
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
        .build()
        .expect("dialect");
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
        .build()
        .expect("dialect");
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
        .build()
        .expect("dialect");
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

// ---------------------------------------------------------------------------
//  GAP-03: every MySQL key column must be binary-collated
//  (same defect class as Java 2349533a1 / #3288, which only fixed the store table)
// ---------------------------------------------------------------------------

/// Extracts the column definition for `column` from a `CREATE TABLE` statement.
fn column_def<'a>(ddl: &'a str, column: &str) -> &'a str {
    let lower = ddl.to_ascii_lowercase();
    let needle = format!("{} ", column.to_ascii_lowercase());
    let start = lower
        .find(&needle)
        .unwrap_or_else(|| panic!("missing column `{column}` in DDL: {ddl}"));
    let rest = &ddl[start..];
    let end = rest
        .find(',')
        .or_else(|| rest.find(')'))
        .unwrap_or(rest.len());
    &rest[..end]
}

fn assert_binary_key_column(ddl: &str, column: &str) {
    let def = column_def(ddl, column);
    assert!(
        def.contains("COLLATE utf8mb4_bin"),
        "key column `{column}` must pin a binary collation, got: {def}"
    );
    assert!(
        def.contains("CHARACTER SET utf8mb4"),
        "key column `{column}` must name its charset so COLLATE matches, got: {def}"
    );
}

#[test]
fn mysql_store_key_columns_use_binary_collation() {
    // Cases differing only in letter case must not collide on a primary key. A
    // case-insensitive collation makes `put("README.md")` then `put("readme.md")` a PK
    // conflict, so the UPSERT silently overwrites the first value.
    let store = MysqlDialect::new().store_create_table_ddls().remove(0);
    assert_binary_key_column(&store, "namespace_path");
    assert_binary_key_column(&store, "item_key");
    // The payload column is deliberately left on the table default.
    assert!(
        !column_def(&store, "value_json").contains("COLLATE"),
        "value_json must keep the table collation: {store}"
    );
}

#[test]
fn mysql_session_state_key_columns_use_binary_collation() {
    // The session-state PK is `(session_id, state_key, item_index)`. The table declares
    // `DEFAULT ... COLLATE utf8mb4_unicode_ci`, so without a column-level override both key
    // columns inherit a case-insensitive collation and two sessions/keys differing only in
    // letter case overwrite each other.
    let session = MysqlDialect::new()
        .session_state_create_table_ddls()
        .remove(0);
    assert_binary_key_column(&session, "session_id");
    assert_binary_key_column(&session, "state_key");
    assert!(
        !column_def(&session, "state_data").contains("COLLATE"),
        "state_data must keep the table collation: {session}"
    );
}

#[test]
fn mysql_snapshot_key_column_uses_binary_collation() {
    // `snapshot_id` is the snapshots-table primary key; the table declared no collation at
    // all, so the column inherited the server/database default (case-insensitive).
    let snapshot = MysqlDialect::new().snapshot_create_table_ddls().remove(0);
    assert_binary_key_column(&snapshot, "snapshot_id");
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
    assert!(bs
        .sql
        .contains("(session_id, state_key, item_index, state_data, version)"));
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
    let ddl = PostgresDialect::new()
        .snapshot_create_table_ddls()
        .remove(0);
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
    assert_eq!(
        PostgresDialect::new().store_insert("a", "b", "c", 1).sql,
        expected
    );
    assert_eq!(
        MysqlDialect::new().store_insert("a", "b", "c", 1).sql,
        expected
    );
    assert_eq!(
        AnsiDialect::new().store_insert("a", "b", "c", 1).sql,
        expected
    );
    assert_eq!(
        SqliteDialect::new().store_insert("a", "b", "c", 1).sql,
        expected
    );
}

#[test]
fn all_dialects_inherit_identical_store_select_and_delete() {
    // Java: StoreDialect default storeSelect / storeDelete
    let expected_select =
        "SELECT value_json, version FROM agentscope_store WHERE namespace_path = ? AND item_key = ?";
    let expected_delete = "DELETE FROM agentscope_store WHERE namespace_path = ? AND item_key = ?";
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
    // Rust does not implement `SandboxLockStrategy` on the MySQL dialect: the native lock is
    // session-scoped and the dialect holds no connection, so it lives in
    // `dialect::native_lock::MysqlNativeLockStrategy` instead. This asserts the dialect
    // contract the dialect *does* expose.
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
    let composite_pk_bytes = (namespace_path_len + item_key_len) * UTF8MB4_MAX_BYTES_PER_CHAR;
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
        .build()
        .expect("dialect");
    assert_eq!(d.resolved_store_table_name(), "my_store");
    assert_eq!(d.resolved_session_state_table_name(), "my_app_sessions");
}

// ---------------------------------------------------------------------------
//  Table-group switches — Java: DialectBuilderTableGroupsTest
// ---------------------------------------------------------------------------

#[test]
fn default_table_groups_cover_the_base_tables_only() {
    // Java: DialectBuilderTableGroupsTest.defaultGroups
    let d = JdbcDialectBuilder::from_product_name("H2")
        .build()
        .expect("dialect");
    assert!(d.base_tables_enabled());
    assert!(!d.skill_tables_enabled());
    assert_eq!(
        vec!["agentscope_store", "agentscope_sessions", "agentscope_snapshots"],
        d.enabled_table_names()
    );
    let joined = d.create_table_ddls().join("\n").to_lowercase();
    assert!(joined.contains("agentscope_store"));
    assert!(joined.contains("agentscope_sessions"));
    assert!(joined.contains("agentscope_snapshots"));
    assert!(!joined.contains("skill"));
}

#[test]
fn skill_only_group_never_creates_the_base_tables() {
    // Java: DialectBuilderTableGroupsTest.skillOnly
    let d = JdbcDialectBuilder::from_product_name("H2")
        .enable_base_tables(false)
        .enable_skill_tables(true)
        .build()
        .expect("dialect");
    assert!(!d.base_tables_enabled());
    assert!(d.skill_tables_enabled());
    assert_eq!(
        vec!["agentscope_skills", "agentscope_skill_resources"],
        d.enabled_table_names()
    );
    let joined = d.create_table_ddls().join("\n").to_lowercase();
    assert!(joined.contains("agentscope_skills"));
    assert!(joined.contains("agentscope_skill_resources"));
    assert!(!joined.contains("agentscope_store"));
    assert!(!joined.contains("agentscope_sessions"));
    assert!(!joined.contains("agentscope_snapshots"));
}

#[test]
fn both_groups_create_all_five_tables() {
    // Java: DialectBuilderTableGroupsTest.bothGroups
    let d = JdbcDialectBuilder::from_product_name("H2")
        .enable_skill_tables(true)
        .build()
        .expect("dialect");
    assert_eq!(
        vec![
            "agentscope_store",
            "agentscope_sessions",
            "agentscope_snapshots",
            "agentscope_skills",
            "agentscope_skill_resources"
        ],
        d.enabled_table_names()
    );
    let joined = d.create_table_ddls().join("\n").to_lowercase();
    for table in [
        "agentscope_store",
        "agentscope_sessions",
        "agentscope_snapshots",
        "agentscope_skills",
        "agentscope_skill_resources",
    ] {
        assert!(joined.contains(table), "{table} must be created");
    }
}

#[test]
fn disabling_both_table_groups_is_rejected_at_build() {
    // Java: DialectBuilderTableGroupsTest.noGroupsEnabled
    let error = JdbcDialectBuilder::from_product_name("H2")
        .enable_base_tables(false)
        .enable_skill_tables(false)
        .build()
        .err()
        .expect("nothing left to create or validate");
    assert!(error.contains("enable_skill_tables(false)"), "{error}");
    assert!(error.contains("enable_base_tables(false)"), "{error}");

    // The switches are "last call wins": enabling the skill group and disabling it again is a
    // rejectable build, exactly like never enabling it.
    let error = JdbcDialectBuilder::from_product_name("H2")
        .enable_skill_tables(true)
        .enable_skill_tables(false)
        .enable_skill_tables(false)
        .enable_base_tables(false)
        .build()
        .err()
        .expect("last call wins");
    assert!(error.contains("No table groups enabled"), "{error}");
}

#[test]
fn auto_create_table_is_orthogonal_to_the_table_groups() {
    // Java: DialectBuilderTableGroupsTest.autoCreateFalseValidatesSkillGroupOnly /
    //       reopenWithAutoCreateFalse. Rust's builder records create-vs-validate only (the
    //       component constructors own the schema work), so the check is that the plan and the
    //       flag stay independent.
    let builder = JdbcDialectBuilder::from_product_name("H2")
        .enable_base_tables(false)
        .enable_skill_tables(true)
        .auto_create_table(false);
    assert!(!builder.should_auto_create_table());
    let d = builder.build().expect("dialect");
    assert!(d.skill_tables_enabled());
    assert_eq!(2, d.enabled_table_names().len());
}

#[test]
fn a_table_name_claimed_by_two_domains_is_rejected() {
    // Java: AbstractJdbcDialect.putDdls — a collision would silently drop one domain's
    // creation *and* its validation.
    let error = JdbcDialectBuilder::from_product_name("H2")
        .table_prefix("app_")
        .unwrap()
        .store_table_name("app_sessions")
        .unwrap()
        .build()
        .err()
        .expect("the store table took the sessions table name");
    assert!(
        error.contains("Duplicate JDBC table name 'app_sessions'"),
        "{error}"
    );

    // Same across groups: a skill table named like the store table.
    let error = JdbcDialectBuilder::from_product_name("H2")
        .enable_skill_tables(true)
        .skill_table_name("agentscope_store")
        .unwrap()
        .build()
        .err()
        .expect("the skill table took the store table name");
    assert!(
        error.contains("Duplicate JDBC table name 'agentscope_store'"),
        "{error}"
    );
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

// ---------------------------------------------------------------------------
//  Versioned session-state UPSERT (AD-2 / `save_if_version`)
//  Java has no dialect-level assertion for this; pinned here because the MySQL
//  `version ... DEFAULT 0` ghost-CAS defect (JDBC__GAP_TRACKER §9.5) came from exactly this
//  template drifting apart between vendors.
// ---------------------------------------------------------------------------

#[test]
fn session_state_upsert_writes_version_one_and_increments_on_conflict() {
    // A fresh row must start at version 1 through the *statement*, never through the DDL
    // default: the JDBC agent-state store relies on `expected == 1` after the first write.
    let postgres = PostgresDialect::new();
    let sqlite = SqliteDialect::new();
    let dialects: [&dyn SessionStateDialect; 2] = [&postgres, &sqlite];
    for d in dialects {
        let bs = d.session_state_upsert("slot", "key", 2, "{}");
        assert!(
            bs.sql.contains("VALUES (?, ?, ?, ?, 1)"),
            "explicit version = 1 on insert: {}",
            bs.sql
        );
        assert!(
            bs.sql.contains("version = agentscope_sessions.version + 1"),
            "conflict path must increment: {}",
            bs.sql
        );
    }
}

#[test]
fn postgres_upsert_can_return_the_new_version_atomically() {
    // Only Postgres/H2 offer a returning upsert; MySQL must degrade (documented, non-atomic).
    let returning = PostgresDialect::new()
        .session_state_upsert_returning_version("slot", "key", 2, "{}")
        .expect("postgres can return the version in the same statement");
    assert!(
        returning.sql.trim_end().ends_with("RETURNING version"),
        "{}",
        returning.sql
    );
    assert!(
        MysqlDialect::new()
            .session_state_upsert_returning_version("slot", "key", 2, "{}")
            .is_none(),
        "MySQL has no RETURNING: callers must take the documented SELECT fallback"
    );
}

#[test]
fn create_table_ddls_covers_the_three_store_owned_tables_only() {
    // Java: `AbstractJdbcDialect.createTableDdls()` — the default configuration enables the base
    // group (store → sessions → snapshots) and leaves the skill group off, so a store-only
    // deployment never creates the skill tables (see the group-switch tests below).
    let ddls = PostgresDialect::new().create_table_ddls();
    let joined = ddls.join("\n");
    for table in [
        "agentscope_store",
        "agentscope_sessions",
        "agentscope_snapshots",
    ] {
        assert!(
            joined.contains(table),
            "{table} must be created by create_table_ddls()"
        );
    }
    assert!(
        !joined.to_lowercase().contains("skill"),
        "skill tables belong to the skill repository, not to create_table_ddls()"
    );
    let sqlite = SqliteDialect::new().create_table_ddls().join("\n");
    assert!(!sqlite.to_lowercase().contains("skill"));
}

/// A dialect that implements only the two required statements, so the **default**
/// `session_state_check_table_exists` is reachable (every shipped vendor overrides it).
struct BareDialect {
    names: TableNames,
}

impl TableNameConfigurable for BareDialect {
    fn table_names(&self) -> &TableNames {
        &self.names
    }
}

impl SessionStateDialect for BareDialect {
    fn session_state_create_table_ddls(&self) -> Vec<String> {
        Vec::new()
    }

    fn session_state_upsert(
        &self,
        _slot_id: &str,
        _key: &str,
        _index: i32,
        _json: &str,
    ) -> BoundSql {
        BoundSql::no_params("INSERT INTO nowhere VALUES (1)")
    }
}

#[test]
fn default_session_state_probe_is_fail_closed() {
    // A vendor dialect that forgets to override the probe must not silently "confirm" a table:
    // the default returns no rows *even for an existing table*, and
    // `JdbcAgentStateStore::verify_table_exists` treats an empty result as "missing".
    use agentscope_extensions_jdbc::backend::JdbcExecutor;
    use agentscope_extensions_jdbc::RusqliteExecutor;

    let dialect = BareDialect {
        names: TableNames::new(),
    };
    let probe = dialect.session_state_check_table_exists("agentscope_sessions");
    assert_eq!(probe.sql, "SELECT 1 FROM agentscope_sessions WHERE 1 = 0");

    let executor = RusqliteExecutor::in_memory().unwrap();
    executor
        .execute(&BoundSql::no_params(
            "CREATE TABLE agentscope_sessions (id TEXT)",
        ))
        .unwrap();
    assert_eq!(
        executor.query(&probe).unwrap().len(),
        0,
        "the default probe can never confirm that a table exists"
    );

    // The shipped vendors answer for real (PostgreSQL asks `information_schema`).
    let postgres = PostgresDialect::new().session_state_check_table_exists("agentscope_sessions");
    assert!(postgres.sql.contains("information_schema.tables"), "{}", postgres.sql);
    assert_eq!(postgres.params.len(), 1);
}

// ---------------------------------------------------------------------------
//  Skill table identity
//  Java: DialectSqlTests.skillDdlsIndexAndCollateNamespace /
//        DialectSqlTests.skillStatementsNeverUpdateNamespace
// ---------------------------------------------------------------------------

#[test]
fn skill_ddl_keeps_the_namespace_unique_key_and_mysql_collation() {
    // The namespace is part of a skill's identity, so the unique key must cover
    // (namespace, name) on every vendor, and MySQL must pin the namespace to a binary
    // collation (`Team-A` and `team-a` stay different skills).
    let postgres = PostgresDialect::new();
    let mysql = MysqlDialect::new();
    let sqlite = SqliteDialect::new();
    let ansi = AnsiDialect::new();
    let vendors: [&dyn AbstractJdbcDialect; 4] = [&postgres, &mysql, &sqlite, &ansi];

    for dialect in vendors {
        let skills = dialect.skill_create_table_ddls().join("\n");
        assert!(skills.contains("uk_namespace_name"), "{skills}");
        assert!(skills.contains("(namespace, name)"), "{skills}");
        let resources = dialect.skill_resources_create_table_ddls().join("\n");
        assert!(
            resources.contains("namespace"),
            "resources must be namespaced: {resources}"
        );
    }

    let mysql_skills = mysql.skill_create_table_ddls().join("\n");
    assert!(
        mysql_skills.contains("COLLATE utf8mb4_bin"),
        "MySQL namespace must be binary-collated: {mysql_skills}"
    );
    let mysql_resources = mysql.skill_resources_create_table_ddls().join("\n");
    assert!(
        mysql_resources.contains("COLLATE utf8mb4_bin"),
        "{mysql_resources}"
    );
}

#[test]
fn skill_statements_never_update_the_namespace() {
    // No statement may move an existing row between namespaces: the namespace is always a
    // predicate (or an INSERT column), never an UPDATE target.
    let dialect = PostgresDialect::new();
    let statements = [
        dialect.skill_select_by_name("team-a", "n"),
        dialect.skill_select_all("team-a"),
        dialect.skill_select_all_names("team-a"),
        dialect.skill_exists("team-a", "n"),
        dialect.skill_select_id_by_name("team-a", "n"),
        dialect.skill_delete_by_name("team-a", "n"),
        dialect.skill_delete_all("team-a"),
        dialect.skill_resources_select_all("team-a"),
        dialect.skill_resources_delete_all("team-a"),
    ];

    for bound in statements {
        assert!(
            !bound.sql.to_ascii_uppercase().contains("SET NAMESPACE"),
            "namespace must never be updated: {}",
            bound.sql
        );
        assert!(
            !bound.sql.trim_start().to_ascii_uppercase().starts_with("UPDATE"),
            "skill statements never update rows: {}",
            bound.sql
        );
        assert!(
            bound.params.iter().any(|param| param.as_text() == Some("team-a")),
            "the namespace must be bound, not inlined: {}",
            bound.sql
        );
    }
}
