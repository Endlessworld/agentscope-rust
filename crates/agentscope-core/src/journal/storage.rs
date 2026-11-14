//! Narrow atomic storage capability, mirroring `AtomicSessionStorage`.
//!
//! Compare-and-set must be atomic across all writers sharing this namespace. A missing value
//! is `Ok(None)`; read/write failures are errors. Implementations must durably store bytes
//! before acknowledging.

use async_trait::async_trait;

use super::session_error;

/// A stored value paired with its storage version.
///
/// `version` must advance on every write; `expected_version = 0` means "create if absent".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageValue {
    pub version: u64,
    pub bytes: Vec<u8>,
}

impl StorageValue {
    pub fn new(version: u64, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            version,
            bytes: bytes.into(),
        }
    }
}

/// Atomic session storage. Runtime journal storage must not be writable through model
/// filesystem tools.
#[async_trait]
pub trait AtomicSessionStorage: Send + Sync {
    /// Reads a value; `Ok(None)` when absent.
    async fn read(&self, path: &str) -> crate::error::Result<Option<StorageValue>>;

    /// Atomic compare-and-set. `expected_version = 0` creates only when absent.
    async fn compare_and_set(
        &self,
        path: &str,
        expected_version: u64,
        bytes: &[u8],
    ) -> crate::error::Result<bool>;

    /// Optional read-only discovery under a prefix. Recovery never uses listing.
    async fn list_paths(&self, _prefix: &str) -> crate::error::Result<Vec<String>> {
        Err(session_error(
            "This session storage does not support discovery",
        ))
    }

    /// Deletes a value; `Ok(false)` when it was already gone.
    ///
    /// Only reclaimed history uses this. A backend without deletion reports an error, and the
    /// journal then keeps the commits a snapshot already covers.
    async fn remove(&self, _path: &str) -> crate::error::Result<bool> {
        Err(session_error(
            "This session storage does not support deletion",
        ))
    }
}
