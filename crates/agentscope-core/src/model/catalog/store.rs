//! Disk cache for the normalized catalog and raw source payloads.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::types::{CatalogSnapshot, SourceInfo, CATALOG_SCHEMA_VERSION};
use crate::Result;

/// Filesystem layout under `~/.agentscope/model-catalog`.
#[derive(Debug, Clone)]
pub struct CatalogStorage {
    root: PathBuf,
}

impl CatalogStorage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn snapshot_path(&self) -> PathBuf {
        self.root.join("catalog.v1.json")
    }

    pub fn raw_path(&self, name: &str) -> PathBuf {
        self.root.join("sources").join(format!("{name}.raw.json"))
    }

    pub fn meta_path(&self, name: &str) -> PathBuf {
        self.root.join("sources").join(format!("{name}.meta.json"))
    }

    /// Reads the normalized snapshot. A missing, corrupt, or version-mismatched file is
    /// treated as a cache miss.
    pub fn read_snapshot(&self) -> Option<CatalogSnapshot> {
        let bytes = fs::read(self.snapshot_path()).ok()?;
        let mut snapshot: CatalogSnapshot = serde_json::from_slice(&bytes).ok()?;
        if snapshot.schema_version != CATALOG_SCHEMA_VERSION {
            return None;
        }
        snapshot.rebuild_indices();
        Some(snapshot)
    }

    pub fn write_snapshot(&self, snapshot: &CatalogSnapshot) -> Result<()> {
        let bytes = serde_json::to_vec(snapshot)?;
        write_atomic(&self.snapshot_path(), &bytes)
    }

    pub fn read_raw(&self, name: &str) -> Option<Vec<u8>> {
        fs::read(self.raw_path(name)).ok()
    }

    pub fn write_raw(&self, name: &str, bytes: &[u8]) -> Result<()> {
        write_atomic(&self.raw_path(name), bytes)
    }

    pub fn read_meta(&self, name: &str) -> Option<SourceInfo> {
        let bytes = fs::read(self.meta_path(name)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn write_meta(&self, name: &str, meta: &SourceInfo) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(meta)?;
        write_atomic(&self.meta_path(name), &bytes)
    }

    /// Removes files for one source. Used only by tests and recovery paths.
    pub fn remove_source(&self, name: &str) {
        let _ = fs::remove_file(self.raw_path(name));
        let _ = fs::remove_file(self.meta_path(name));
    }

    /// Removes the normalized snapshot.
    pub fn remove_snapshot(&self) {
        let _ = fs::remove_file(self.snapshot_path());
    }
}

/// SHA-256 of a payload, used for cache diagnostics.
pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_file_name(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("catalog"),
        uuid::Uuid::new_v4()
    ));
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }

    // On Windows, rename over an existing file can fail while an indexer or antivirus
    // holds it. Retry briefly, removing the destination only as a last resort.
    let mut last_error = None;
    for attempt in 0..3 {
        match fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                if path.exists() {
                    let _ = fs::remove_file(path);
                }
                sleep(Duration::from_millis(25 * (attempt + 1)));
            }
        }
    }
    let _ = fs::remove_file(&tmp);
    Err(last_error
        .map(crate::Error::Io)
        .unwrap_or_else(|| crate::Error::Io(std::io::Error::other("catalog: atomic write failed"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_storage() -> (CatalogStorage, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "agentscope-catalog-test-{}",
            uuid::Uuid::new_v4()
        ));
        (CatalogStorage::new(&root), root)
    }

    #[test]
    fn snapshot_roundtrips_and_rebuilds_indices() {
        let (storage, root) = temp_storage();
        let parsed = super::super::source::parse_models_json(
            br#"{"p/m": {"name": "M", "limit": {"context": 1000}}}"#,
        )
        .unwrap();
        let mut snapshot = CatalogSnapshot::empty();
        for entry in parsed.entries {
            snapshot.entries.insert(entry.key.clone(), std::sync::Arc::new(entry));
        }
        snapshot.rebuild_indices();
        storage.write_snapshot(&snapshot).unwrap();
        let loaded = storage.read_snapshot().unwrap();
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.by_bare["m"].len(), 1);
        assert_eq!(loaded.by_provider["p"].len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn version_mismatch_is_a_cache_miss() {
        let (storage, root) = temp_storage();
        let mut snapshot = CatalogSnapshot::empty();
        snapshot.schema_version = CATALOG_SCHEMA_VERSION + 1;
        storage.write_snapshot(&snapshot).unwrap();
        assert!(storage.read_snapshot().is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn raw_and_meta_roundtrip() {
        let (storage, root) = temp_storage();
        storage.write_raw("models", b"{}").unwrap();
        assert_eq!(storage.read_raw("models").unwrap(), b"{}");
        let meta = SourceInfo {
            name: "models".into(),
            url: "https://example.com/models.json".into(),
            etag: Some("W/\"1\"".into()),
            last_modified: None,
            fetched_at: chrono::Utc::now(),
            checked_at: chrono::Utc::now(),
            bytes: 2,
            sha256: Some(sha256_hex(b"{}")),
        };
        storage.write_meta("models", &meta).unwrap();
        assert_eq!(storage.read_meta("models").unwrap().etag.as_deref(), Some("W/\"1\""));
        let _ = fs::remove_dir_all(root);
    }
}
