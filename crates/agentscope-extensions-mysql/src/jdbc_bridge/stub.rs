//! Stub compiled when the `jdbc-bridge` feature is off.
//!
//! The public surface stays stable (`MysqlJdbcStore::connect` always exists) while every
//! constructor fails loudly with [`MysqlError::DriverUnavailable`] — the bridge never returns
//! a store that silently drops writes.

use agentscope_harness::agent::isolation_scope::IsolationScope;

use crate::error::{MysqlError, MysqlResult};

/// Placeholder: the real wrapper is only compiled with `--features jdbc-bridge`.
#[derive(Debug, Clone, Copy, Default)]
pub struct MysqlJdbcStore {
    _private: (),
}

impl MysqlJdbcStore {
    /// Always returns [`MysqlError::DriverUnavailable`] when the feature is off.
    pub fn connect(url: impl AsRef<str>) -> MysqlResult<Self> {
        MysqlJdbcStoreBuilder::new(url).build()
    }
}

/// Placeholder builder: chainable methods exist, [`Self::build`] always fails.
#[derive(Debug, Clone)]
pub struct MysqlJdbcStoreBuilder {
    url: String,
    initialize_schema: bool,
    native_lock_guard: bool,
    isolation_scope: IsolationScope,
}

impl MysqlJdbcStoreBuilder {
    /// Records the URL so the error message can name it.
    pub fn new(url: impl AsRef<str>) -> Self {
        Self {
            url: url.as_ref().to_string(),
            initialize_schema: true,
            native_lock_guard: true,
            isolation_scope: IsolationScope::User,
        }
    }

    /// Recorded only; the stub never builds.
    pub fn initialize_schema(mut self, initialize: bool) -> Self {
        self.initialize_schema = initialize;
        self
    }

    /// Recorded only; the stub never builds.
    pub fn native_lock_guard(mut self, native: bool) -> Self {
        self.native_lock_guard = native;
        self
    }

    /// Recorded only; the stub never builds.
    pub fn isolation_scope(mut self, scope: IsolationScope) -> Self {
        self.isolation_scope = scope;
        self
    }

    /// The configured URL.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Always returns [`MysqlError::DriverUnavailable`] when the feature is off.
    pub fn build(self) -> MysqlResult<MysqlJdbcStore> {
        Err(MysqlError::DriverUnavailable(format!(
            "the `jdbc-bridge` feature is disabled; rebuild with `--features jdbc-bridge` \
             to build the JDBC façade over {} (initialize_schema={}, native_lock_guard={}, \
             isolation_scope={:?})",
            self.url, self.initialize_schema, self.native_lock_guard, self.isolation_scope
        )))
    }
}
