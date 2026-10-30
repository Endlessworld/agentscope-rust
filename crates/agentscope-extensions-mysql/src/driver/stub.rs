//! Stub compiled when the `mysql_async` feature is off.
//!
//! It keeps the public surface stable (`MysqlAsyncBackend::connect` always exists) while
//! failing loudly instead of returning a backend that cannot talk to MySQL.

use std::sync::Arc;

use super::MysqlAsyncConfig;
use crate::error::{MysqlError, MysqlResult};

/// Placeholder type: the real adapter is only compiled with `--features mysql_async`.
#[derive(Debug, Clone, Copy, Default)]
pub struct MysqlAsyncBackend {
    _private: (),
}

impl MysqlAsyncBackend {
    /// Always returns [`MysqlError::DriverUnavailable`] when the feature is off.
    pub fn connect(url: &str) -> MysqlResult<Arc<Self>> {
        Self::connect_with_config(url, MysqlAsyncConfig::default())
    }

    /// Always returns [`MysqlError::DriverUnavailable`] when the feature is off.
    pub fn connect_with_config(url: &str, _config: MysqlAsyncConfig) -> MysqlResult<Arc<Self>> {
        Err(MysqlError::DriverUnavailable(format!(
            "the `mysql_async` feature is disabled; rebuild with `--features mysql_async` \
             to connect to {url}"
        )))
    }
}
