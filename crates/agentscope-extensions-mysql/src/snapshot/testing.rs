//! Test-only in-memory [`SnapshotBackend`] implementation.

use std::collections::HashMap;
use std::sync::Mutex;

use agentscope_core::error::{Error, Result};

use super::SnapshotBackend;

/// In-memory blob store standing in for the `agentscope_snapshots` BLOB table.
#[derive(Debug, Default)]
pub struct InMemorySnapshotBackend {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
}

impl SnapshotBackend for InMemorySnapshotBackend {
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
}
