//! Test-only in-memory [`SnapshotBackend`] implementation.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use agentscope_core::error::{Error, Result};

use super::SnapshotBackend;

/// In-memory blob store standing in for the `agentscope_snapshots` BLOB table.
///
/// `execute_ddl` records the statement (so tests can assert the exact schema bootstrap the
/// crate emits) instead of running it against a server.
#[derive(Debug, Default)]
pub struct InMemorySnapshotBackend {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
    ddl_log: Mutex<Vec<String>>,
    fail_ddl: AtomicBool,
}

impl InMemorySnapshotBackend {
    /// Makes every subsequent `execute_ddl` fail (simulates a read-only / locked table).
    pub fn fail_ddl(&self, fail: bool) {
        self.fail_ddl.store(fail, Ordering::SeqCst);
    }

    /// DDL statements issued so far, in order.
    pub fn ddl_log(&self) -> Vec<String> {
        self.ddl_log.lock().unwrap().clone()
    }
}

impl SnapshotBackend for InMemorySnapshotBackend {
    fn execute_ddl(&self, sql: &str) -> Result<()> {
        if self.fail_ddl.load(Ordering::SeqCst) {
            return Err(Error::Tool("access denied for DDL".to_string()));
        }
        self.ddl_log.lock().unwrap().push(sql.to_string());
        Ok(())
    }

    fn upload(&self, snapshot_id: &str, data: &[u8]) -> Result<()> {
        self.blobs
            .lock()
            .unwrap()
            .insert(snapshot_id.to_string(), data.to_vec());
        Ok(())
    }

    fn download(&self, snapshot_id: &str) -> Result<Vec<u8>> {
        self.blobs
            .lock()
            .unwrap()
            .get(snapshot_id)
            .cloned()
            .ok_or_else(|| Error::Tool(format!("snapshot not found: {snapshot_id}")))
    }

    fn exists(&self, snapshot_id: &str) -> Result<bool> {
        Ok(self.blobs.lock().unwrap().contains_key(snapshot_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_download_exists() {
        let b = InMemorySnapshotBackend::default();
        assert!(!b.exists("s").unwrap());
        b.upload("s", b"data").unwrap();
        assert!(b.exists("s").unwrap());
        assert_eq!(b.download("s").unwrap(), b"data");
        assert!(b.download("missing").is_err());
    }

    #[test]
    fn ddl_is_recorded_and_can_be_made_to_fail() {
        let b = InMemorySnapshotBackend::default();
        b.execute_ddl("CREATE TABLE t (id INT)").unwrap();
        assert_eq!(b.ddl_log(), vec!["CREATE TABLE t (id INT)"]);
        b.fail_ddl(true);
        assert!(b.execute_ddl("CREATE TABLE u (id INT)").is_err());
        assert_eq!(b.ddl_log().len(), 1, "a failed DDL must not be logged");
    }
}
