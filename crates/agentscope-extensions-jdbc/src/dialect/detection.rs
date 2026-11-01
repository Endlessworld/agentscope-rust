//! Dialect auto-detection and chainable builder, mirroring
//! `AbstractJdbcDialectBuilder`.
//!
//! Java uses JDK SPI to discover dialects. Rust has no equivalent runtime ServiceLoader
//! for a fixed set of built-in dialects, so detection here matches the JDBC connection's
//! reported product name (mirroring each vendor's `supports(DatabaseMetaData)`) against
//! the built-in dialects, defaulting to [`AnsiDialect`].

use std::sync::Arc;

use super::abstract_dialect::{
    validate_identifier, AbstractJdbcDialect, TableNameConfig, DEFAULT_TABLE_PREFIX,
};
use super::vendor::{AnsiDialect, MysqlDialect, PostgresDialect, SqliteDialect};

/// Detects a dialect from a database product name string, mirroring the SPI candidates'
/// `supports(DatabaseMetaData)` checks.
///
/// * contains `mysql` or `mariadb` → [`MysqlDialect`]
/// * contains `postgres` → [`PostgresDialect`]
/// * contains `sqlite` → [`SqliteDialect`]
/// * otherwise (`h2`, unknown) → [`AnsiDialect`]
///
/// Returns an [`Arc`] so the chosen dialect can be shared by every component of a
/// [`crate::JdbcDistributedStore`].
pub fn detect_dialect(product_name: &str) -> Arc<dyn AbstractJdbcDialect> {
    let lower = product_name.to_ascii_lowercase();
    let dialect: Arc<dyn AbstractJdbcDialect> = if lower.contains("mysql") || lower.contains("mariadb") {
        Arc::new(MysqlDialect::new())
    } else if lower.contains("postgres") {
        Arc::new(PostgresDialect::new())
    } else if lower.contains("sqlite") {
        Arc::new(SqliteDialect::new())
    } else {
        Arc::new(AnsiDialect::new())
    };
    // Java logs the SPI candidate it picked; here the same decision is worth a `debug`
    // because an unexpected fallback to `AnsiDialect` is otherwise invisible.
    tracing::debug!(
        product_name,
        dialect = ?dialect.kind(),
        "[jdbc-dialect] detected"
    );
    dialect
}

/// Chainable builder for selecting/configuring a dialect, mirroring
/// `AbstractJdbcDialectBuilder`.
///
/// Unlike Java, the result does not hold a live `DataSource` connection; callers pass the
/// already-detected product name (typically `SELECT sqlite_version()` / JDBC
/// `DatabaseMetaData.getDatabaseProductName()`). `auto_create_table` is recorded here and
/// honored by the component builders.
#[derive(Clone)]
pub struct JdbcDialectBuilder {
    dialect: Arc<dyn AbstractJdbcDialect>,
    table_prefix: String,
    store_table_name: Option<String>,
    session_state_table_name: Option<String>,
    snapshot_table_name: Option<String>,
    skill_table_name: Option<String>,
    skill_resources_table_name: Option<String>,
    enable_base_tables: bool,
    enable_skill_tables: bool,
    auto_create_table: bool,
}

impl std::fmt::Debug for JdbcDialectBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JdbcDialectBuilder")
            .field("kind", &self.dialect.kind())
            .field("table_prefix", &self.table_prefix)
            .field("store_table_name", &self.store_table_name)
            .field("session_state_table_name", &self.session_state_table_name)
            .field("snapshot_table_name", &self.snapshot_table_name)
            .field("skill_table_name", &self.skill_table_name)
            .field(
                "skill_resources_table_name",
                &self.skill_resources_table_name,
            )
            .field("enable_base_tables", &self.enable_base_tables)
            .field("enable_skill_tables", &self.enable_skill_tables)
            .field("auto_create_table", &self.auto_create_table)
            .finish()
    }
}

impl JdbcDialectBuilder {
    /// Creates a builder around an explicit dialect.
    pub fn new(dialect: Arc<dyn AbstractJdbcDialect>) -> Self {
        Self {
            dialect,
            table_prefix: DEFAULT_TABLE_PREFIX.to_string(),
            store_table_name: None,
            session_state_table_name: None,
            snapshot_table_name: None,
            skill_table_name: None,
            skill_resources_table_name: None,
            enable_base_tables: true,
            enable_skill_tables: false,
            auto_create_table: true,
        }
    }

    /// Creates a builder that auto-detects from a database product name.
    pub fn from_product_name(product_name: &str) -> Self {
        Self::new(detect_dialect(product_name))
    }

    /// Sets the unified table prefix (default `agentscope_`).
    pub fn table_prefix(mut self, prefix: &str) -> Result<Self, String> {
        validate_identifier(prefix, "tablePrefix")?;
        self.table_prefix = prefix.to_string();
        Ok(self)
    }

    /// Overrides the full KV store table name.
    pub fn store_table_name(mut self, name: &str) -> Result<Self, String> {
        validate_identifier(name, "storeTableName")?;
        self.store_table_name = Some(name.to_string());
        Ok(self)
    }

    /// Overrides the full session-state table name.
    pub fn session_state_table_name(mut self, name: &str) -> Result<Self, String> {
        validate_identifier(name, "sessionStateTableName")?;
        self.session_state_table_name = Some(name.to_string());
        Ok(self)
    }

    /// Overrides the full snapshots table name.
    pub fn snapshot_table_name(mut self, name: &str) -> Result<Self, String> {
        validate_identifier(name, "snapshotTableName")?;
        self.snapshot_table_name = Some(name.to_string());
        Ok(self)
    }

    /// Overrides the full skills table name.
    pub fn skill_table_name(mut self, name: &str) -> Result<Self, String> {
        validate_identifier(name, "skillTableName")?;
        self.skill_table_name = Some(name.to_string());
        Ok(self)
    }

    /// Overrides the full skill-resources table name.
    pub fn skill_resources_table_name(mut self, name: &str) -> Result<Self, String> {
        validate_identifier(name, "skillResourcesTableName")?;
        self.skill_resources_table_name = Some(name.to_string());
        Ok(self)
    }

    /// Whether the base table group (store, sessions, snapshots) joins this build's schema
    /// work (default true). Orthogonal to create-vs-validate; the last call wins.
    ///
    /// The group flags scope schema work only. The lock table
    /// (`{prefix}distributed_locks`) belongs to no group: the table lock creates it lazily on
    /// first use, so a skill-only deployment (`enable_base_tables(false)` +
    /// `enable_skill_tables(true)`) that takes sandbox locks still creates and uses it.
    pub fn enable_base_tables(mut self, enabled: bool) -> Self {
        self.enable_base_tables = enabled;
        self
    }

    /// Whether the skill table group (skills, skill_resources) joins this build's schema
    /// work (default **false**, so opting in is explicit). Semantics as
    /// [`Self::enable_base_tables`].
    pub fn enable_skill_tables(mut self, enabled: bool) -> Self {
        self.enable_skill_tables = enabled;
        self
    }

    /// Whether to auto-create tables during component construction (default true).
    pub fn auto_create_table(mut self, value: bool) -> Self {
        self.auto_create_table = value;
        self
    }

    /// Whether the base table group is enabled on this builder.
    pub fn base_tables_enabled(&self) -> bool {
        self.enable_base_tables
    }

    /// Whether the skill table group is enabled on this builder.
    pub fn skill_tables_enabled(&self) -> bool {
        self.enable_skill_tables
    }

    /// The table prefix configured on this builder.
    pub fn prefix(&self) -> &str {
        &self.table_prefix
    }

    /// Whether auto table creation is enabled.
    pub fn should_auto_create_table(&self) -> bool {
        self.auto_create_table
    }

    /// Store table name override, if any.
    pub fn store_override(&self) -> Option<&str> {
        self.store_table_name.as_deref()
    }

    /// Session-state table name override, if any.
    pub fn session_state_override(&self) -> Option<&str> {
        self.session_state_table_name.as_deref()
    }

    /// Snapshot table name override, if any.
    pub fn snapshot_override(&self) -> Option<&str> {
        self.snapshot_table_name.as_deref()
    }

    /// Skills table name override, if any.
    pub fn skill_override(&self) -> Option<&str> {
        self.skill_table_name.as_deref()
    }

    /// Skill-resources table name override, if any.
    pub fn skill_resources_override(&self) -> Option<&str> {
        self.skill_resources_table_name.as_deref()
    }

    /// Returns the selected dialect.
    ///
    /// Chainable configuration (prefix, per-table overrides, group switches) is applied to the
    /// dialect instance before it is returned, mirroring the Java
    /// `AbstractJdbcDialectBuilder.build()` which sets the dialect's fields. `auto_create_table`
    /// is record-only here: schema creation is owned by the component constructors (which
    /// receive this builder's value through [`Self::should_auto_create_table`]).
    ///
    /// Fails on the two misconfigurations Java's `build()` rejects: both table groups disabled,
    /// and a table name claimed by two domains (see
    /// [`AbstractJdbcDialect::validate_table_groups`]).
    pub fn build(self) -> Result<Arc<dyn AbstractJdbcDialect>, String> {
        self.apply_table_name_config();
        self.dialect.validate_table_groups()?;
        Ok(self.dialect)
    }

    /// Returns the configured [`TableNameConfig`] without mutating the dialect.
    pub fn table_name_config(&self) -> TableNameConfig {
        TableNameConfig {
            prefix: self.table_prefix.clone(),
            store_table_name: self.store_table_name.clone(),
            session_state_table_name: self.session_state_table_name.clone(),
            snapshot_table_name: self.snapshot_table_name.clone(),
            skill_table_name: self.skill_table_name.clone(),
            skill_resources_table_name: self.skill_resources_table_name.clone(),
            enable_base_tables: self.enable_base_tables,
            enable_skill_tables: self.enable_skill_tables,
        }
    }

    /// Applies the chainable table-name configuration to the underlying dialect instance.
    ///
    /// [`AbstractJdbcDialect`] requires [`TableNameConfigurable`], so this works for every
    /// dialect (built-in or third-party) sharing the hook.
    fn apply_table_name_config(&self) {
        self.dialect.apply_table_config(self.table_name_config());
    }
}
