//! Aggregate dialect implementing all table-domain traits (store / session state / snapshot /
//! skill / skill resources), mirroring `AbstractJdbcDialect`.
//!
//! Lock strategies are **not** part of the dialect: advisory locks are session-scoped and a
//! dialect carries no connection (see `dialect::lock_strategy` and `dialect::native_lock`).
//! Holds a unified table prefix and per-table name overrides. Vendor dialects extend
//! this type and override only the methods where their SQL diverges from the ANSI
//! baseline. Table-name resolution priority: per-table override > prefix + base.

use std::sync::{Arc, RwLock};

use super::bound_sql::BoundSql;
use super::session_state_dialect::SessionStateDialect;
use super::skill_dialect::{SkillDialect, SkillResourcesDialect};
use super::snapshot_dialect::SnapshotDialect;
use super::store_dialect::StoreDialect;

/// Default table prefix applied to every table name.
pub const DEFAULT_TABLE_PREFIX: &str = "agentscope_";

/// Reported when both table groups are switched off, mirroring the Java builder's
/// `IllegalStateException` ("No table groups enabled: enableBaseTables(false) combined with
/// enableSkillTables(false) leaves build() nothing to create or validate.").
pub const NO_ENABLED_TABLE_GROUPS: &str =
    "No table groups enabled: enable_base_tables(false) combined with \
     enable_skill_tables(false) leaves build() nothing to create or validate. Enable at least \
     one group.";

/// Which database a dialect targets. Used for dialect auto-detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DialectKind {
    /// Generic/portable ANSI dialect (H2 and other JDBC databases).
    Ansi,
    /// MySQL / MariaDB.
    Mysql,
    /// PostgreSQL.
    Postgres,
    /// SQLite.
    Sqlite,
}

/// Host-side table configuration, mirroring the mutable fields `AbstractJdbcDialect` exposes
/// to `AbstractJdbcDialectBuilder` (`tablePrefix`, the per-table overrides and the two table
/// group switches).
///
/// Table-name resolution priority is **per-table override > prefix + base name**, matching
/// the Java `final storeTableName()` / `sessionStateTableName()` / `snapshotTableName()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableNameConfig {
    /// Unified table prefix (defaults to [`DEFAULT_TABLE_PREFIX`]).
    pub prefix: String,
    /// Full override for the KV store table name.
    pub store_table_name: Option<String>,
    /// Full override for the session-state table name.
    pub session_state_table_name: Option<String>,
    /// Full override for the snapshots table name.
    pub snapshot_table_name: Option<String>,
    /// Full override for the skills table name.
    pub skill_table_name: Option<String>,
    /// Full override for the skill-resources table name.
    pub skill_resources_table_name: Option<String>,
    /// Whether the base table group (store, sessions, snapshots) takes part in this
    /// dialect's schema work (Java `enableBaseTables`, default on).
    pub enable_base_tables: bool,
    /// Whether the skill table group (skills, skill_resources) takes part in this dialect's
    /// schema work (Java `enableSkillTables`, default **off**: opting in is explicit).
    pub enable_skill_tables: bool,
}

impl Default for TableNameConfig {
    fn default() -> Self {
        Self {
            prefix: DEFAULT_TABLE_PREFIX.to_string(),
            store_table_name: None,
            session_state_table_name: None,
            snapshot_table_name: None,
            skill_table_name: None,
            skill_resources_table_name: None,
            enable_base_tables: true,
            enable_skill_tables: false,
        }
    }
}

/// Shared, interior-mutable holder for a dialect's [`TableNameConfig`].
///
/// Java mutates the dialect's prefix/override fields during builder assembly. Rust dialects
/// are shared as `Arc<dyn AbstractJdbcDialect>`, so the configuration lives behind an
/// `RwLock` and is applied by [`JdbcDialectBuilder::build`](crate::dialect::JdbcDialectBuilder::build).
#[derive(Debug, Clone)]
pub struct TableNames {
    config: Arc<RwLock<TableNameConfig>>,
}

impl Default for TableNames {
    fn default() -> Self {
        Self {
            config: Arc::new(RwLock::new(TableNameConfig::default())),
        }
    }
}

impl TableNames {
    /// Creates a holder with the default configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a copy of the current configuration.
    pub fn snapshot(&self) -> TableNameConfig {
        self.config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Replaces the current configuration.
    pub fn apply(&self, config: TableNameConfig) {
        *self
            .config
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = config;
    }
}

/// Table-name configurability shared by all built-in dialects.
///
/// Resolution helpers on [`StoreDialect`] / [`SessionStateDialect`] / [`SnapshotDialect`]
/// read the configuration through this trait, so a dialect built by
/// [`JdbcDialectBuilder`](crate::dialect::JdbcDialectBuilder) resolves its prefix and
/// per-table overrides exactly like the Java builder.
pub trait TableNameConfigurable: Send + Sync {
    /// The dialect's table-name configuration holder.
    fn table_names(&self) -> &TableNames;

    /// Returns a copy of the dialect's current table-name configuration.
    fn table_config(&self) -> TableNameConfig {
        self.table_names().snapshot()
    }

    /// Applies a table-name configuration to this dialect instance.
    fn apply_table_config(&self, config: TableNameConfig) {
        self.table_names().apply(config);
    }
}

/// Validates a SQL identifier (table names and prefixes flow into DDL verbatim, so
/// this is the SQL-injection guard). Same pattern as the Java
/// `AbstractJdbcDialectBuilder.VALID_IDENTIFIER`.
pub fn validate_identifier(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    let mut chars = value.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(format!(
            "{field} must start with a letter or underscore: {value}"
        ));
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "{field} must contain only alphanumeric characters and underscores: {value}"
        ));
    }
    Ok(())
}

/// Aggregate dialect: implements [`StoreDialect`] + [`SessionStateDialect`] +
/// [`SnapshotDialect`] + [`SkillDialect`] + [`SkillResourcesDialect`], and provides table-name
/// resolution plus the `supports_native_locks` capability bit.
///
/// It deliberately does **not** implement `SandboxLockStrategy`: a dialect carries no
/// connection, and advisory locks are session-scoped, so the lock strategies are separate
/// types ([`crate::sandbox::TableLockStrategy`], [`crate::dialect::MysqlNativeLockStrategy`],
/// [`crate::dialect::PostgresNativeLockStrategy`]).
///
/// The trait methods themselves resolve the final, prefixed table names; vendors
/// override only the DDL/UPSERT methods where their syntax diverges.
pub trait AbstractJdbcDialect:
    StoreDialect
    + SessionStateDialect
    + SnapshotDialect
    + SkillDialect
    + SkillResourcesDialect
    + TableNameConfigurable
    + Send
    + Sync
{
    /// The dialect kind, used for auto-detection and reporting.
    fn kind(&self) -> DialectKind;

    /// The unified table prefix (default `agentscope_`).
    ///
    /// The per-table prefixes inherited from the table-domain traits read the same
    /// configuration; this accessor exists for the lock table name (which has no
    /// table-domain trait).
    fn table_prefix(&self) -> String {
        self.table_config().prefix
    }

    /// All create-schema DDL for the **enabled** table groups, in creation order
    /// (store → sessions → snapshots → skills → skill_resources).
    ///
    /// Mirrors the Java `AbstractJdbcDialect.createTableDdls()` (executed by the builder in a
    /// single pass), including its group filter: the skill group only contributes when
    /// `enableSkillTables` is on. Each table may contribute more than one statement (CREATE
    /// TABLE plus a secondary CREATE INDEX), so the returned vector is flattened. The
    /// resources table follows the skill table it references.
    fn create_table_ddls(&self) -> Vec<String> {
        let mut ddls = Vec::new();
        if self.base_tables_enabled() {
            ddls.extend(self.store_create_table_ddls());
            ddls.extend(self.session_state_create_table_ddls());
            ddls.extend(self.snapshot_create_table_ddls());
        }
        if self.skill_tables_enabled() {
            ddls.extend(self.skill_create_table_ddls());
            ddls.extend(self.skill_resources_create_table_ddls());
        }
        ddls
    }

    /// Java equivalent: `AbstractJdbcDialect.lockTableName()` = `tablePrefix + "distributed_locks"`.
    /// DDL statement for the distributed-locks table (used by the portable
    /// table-based lock). Idempotent.
    fn lock_create_table_ddl(&self) -> String {
        format!(
            "CREATE TABLE IF NOT EXISTS {} (lock_name VARCHAR(255) NOT NULL PRIMARY KEY)",
            self.resolved_lock_table_name()
        )
    }

    /// Resolved lock table name: `{prefix}distributed_locks` (mirrors Java).
    fn resolved_lock_table_name(&self) -> String {
        format!("{}distributed_locks", self.table_prefix())
    }

    /// The explicit table configuration applied by the builder (if any).
    ///
    /// Provided for introspection/tests; resolution itself happens in the table-domain
    /// `resolved_*_table_name()` helpers.
    fn configured_table_names(&self) -> TableNameConfig {
        self.table_config()
    }

    /// Whether the base table group joins this dialect's schema work (Java
    /// `isBaseTablesEnabled`).
    fn base_tables_enabled(&self) -> bool {
        self.table_config().enable_base_tables
    }

    /// Whether the skill table group joins this dialect's schema work (Java
    /// `isSkillTablesEnabled`).
    fn skill_tables_enabled(&self) -> bool {
        self.table_config().enable_skill_tables
    }

    /// Resolved table names of the enabled groups, in creation order
    /// (store → sessions → snapshots → skills → skill_resources).
    fn enabled_table_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        if self.base_tables_enabled() {
            names.push(self.resolved_store_table_name());
            names.push(self.resolved_session_state_table_name());
            names.push(self.resolved_snapshot_table_name());
        }
        if self.skill_tables_enabled() {
            names.push(self.resolved_skill_table_name());
            names.push(self.resolved_skill_resources_table_name());
        }
        names
    }

    /// Rejects the two configuration mistakes the Java builder reports at build time:
    ///
    /// * both groups disabled — `build()` would have nothing to create or validate;
    /// * one table name claimed by two domains (Java `putDdls`) — a collision silently drops
    ///   one domain's creation *and* its validation, so the table would look fine while the
    ///   component writes into another domain's table.
    fn validate_table_groups(&self) -> Result<(), String> {
        if !self.base_tables_enabled() && !self.skill_tables_enabled() {
            return Err(NO_ENABLED_TABLE_GROUPS.to_string());
        }
        let mut seen: Vec<String> = Vec::with_capacity(5);
        for name in self.enabled_table_names() {
            if seen.contains(&name) {
                return Err(format!(
                    "Duplicate JDBC table name '{name}': each table domain needs a distinct \
                     table name"
                ));
            }
            seen.push(name);
        }
        Ok(())
    }

    /// Whether this dialect supports native advisory locks (MySQL `GET_LOCK`,
    /// PostgreSQL `pg_try_advisory_lock`). The portable table lock is the default.
    fn supports_native_locks(&self) -> bool {
        false
    }

    /// A query returning the column names of `table_name`, one row per column.
    ///
    /// Feeds the assemble-time schema validation
    /// ([`crate::dialect::validate_table_schema`]). The default targets
    /// `information_schema.columns`, which ANSI/H2/PostgreSQL/MySQL all provide;
    /// vendors override it where they need a schema qualifier or a different catalog.
    fn table_columns_query(&self, table_name: &str) -> BoundSql {
        BoundSql::new(
            "SELECT column_name FROM information_schema.columns \
             WHERE LOWER(table_name) = LOWER(?)",
            vec![crate::dialect::SqlParam::Text(table_name.to_string())],
        )
    }
}
