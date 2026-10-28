//! A local archive with scoped names, content verification and atomic publication
//! (`FileJevContextArchive`).
//!
//! Deviation: Java sets POSIX `rwx------`/`rw-------` permissions when the filesystem supports
//! them. The port does the same on Unix and is a no-op elsewhere (Windows ACLs are not managed by
//! this crate).

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use crate::error::JevError;

use super::archive::{
    decode_envelope, require_reference, ArchiveEnvelope, ArchiveScope, JevContextArchive,
};
use super::message::ContextMessage;

/// A local, content-addressed snapshot archive.
#[derive(Debug, Clone)]
pub struct FileJevContextArchive {
    root: PathBuf,
    max_bytes: usize,
}

impl FileJevContextArchive {
    /// Creates the archive; `max_bytes` must be positive.
    pub fn new(root: impl AsRef<Path>, max_bytes: usize) -> Result<Self, JevError> {
        if max_bytes < 1 {
            return Err(JevError::invalid("positive archive byte limit required"));
        }
        Ok(Self {
            root: normalize(root.as_ref()),
            max_bytes,
        })
    }

    /// The normalized archive root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The byte limit per snapshot.
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    async fn directory(&self, scope: &ArchiveScope) -> Result<PathBuf, JevError> {
        tokio::fs::create_dir_all(&self.root)
            .await
            .map_err(|_error| JevError::protocol("cannot create archive root"))?;
        if is_symlink(&self.root).await {
            return Err(JevError::invalid("archive root is a symlink"));
        }
        let encoded = serde_json::to_vec(scope)
            .map_err(|_error| JevError::invalid("archive scope or version mismatch"))?;
        let directory = self.root.join(sha256_hex(&encoded));
        tokio::fs::create_dir_all(&directory)
            .await
            .map_err(|_error| JevError::protocol("cannot create archive scope"))?;
        if is_symlink(&directory).await {
            return Err(JevError::invalid("archive scope is a symlink"));
        }
        restrict_permissions(&directory, 0o700).await;
        Ok(directory)
    }

    async fn verify(&self, file: &Path, reference: &str) -> Result<Vec<u8>, JevError> {
        let metadata = tokio::fs::symlink_metadata(file)
            .await
            .map_err(|_error| JevError::invalid("missing, unsafe or oversized archive"))?;
        if !metadata.file_type().is_file()
            || metadata.len() as usize > self.max_bytes
        {
            return Err(JevError::invalid("missing, unsafe or oversized archive"));
        }
        let data = tokio::fs::read(file)
            .await
            .map_err(|_error| JevError::invalid("missing, unsafe or oversized archive"))?;
        if sha256_hex(&data) != reference {
            return Err(JevError::invalid("archive checksum mismatch"));
        }
        Ok(data)
    }
}

#[async_trait]
impl JevContextArchive for FileJevContextArchive {
    async fn save(
        &self,
        scope: &ArchiveScope,
        messages: &[ContextMessage],
    ) -> Result<String, JevError> {
        let envelope = ArchiveEnvelope::new(scope.clone(), messages.to_vec());
        let data = serde_json::to_vec(&envelope)
            .map_err(|_error| JevError::invalid("archive limit"))?;
        if data.len() > self.max_bytes {
            return Err(JevError::invalid("archive limit"));
        }
        let directory = self.directory(scope).await?;
        let reference = sha256_hex(&data);
        let target = directory.join(format!("{reference}.json"));
        if tokio::fs::symlink_metadata(&target).await.is_ok() {
            self.verify(&target, &reference).await?;
            return Ok(reference);
        }
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let temporary = directory.join(format!(
            ".pending-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::fs::write(&temporary, &data)
            .await
            .map_err(|_error| JevError::protocol("cannot write archive snapshot"))?;
        restrict_permissions(&temporary, 0o600).await;
        tokio::fs::rename(&temporary, &target).await.map_err(|_error| {
            let _ = std::fs::remove_file(&temporary);
            JevError::protocol("cannot publish archive snapshot")
        })?;
        Ok(reference)
    }

    async fn restore(
        &self,
        scope: &ArchiveScope,
        reference: &str,
    ) -> Result<Vec<ContextMessage>, JevError> {
        require_reference(reference)?;
        let directory = self.directory(scope).await?;
        let file = directory.join(format!("{reference}.json"));
        let data = self.verify(&file, reference).await?;
        decode_envelope(&data, scope)
    }
}

/// Makes `path` absolute and removes `.`/`..` segments without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

async fn is_symlink(path: &Path) -> bool {
    tokio::fs::symlink_metadata(path)
        .await
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}

fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        encoded.push_str(&format!("{byte:02x}"));
    }
    encoded
}

#[cfg(unix)]
async fn restrict_permissions(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await;
}

#[cfg(not(unix))]
async fn restrict_permissions(_path: &Path, _mode: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> ArchiveScope {
        ArchiveScope::new("user", "agent", "session").unwrap()
    }

    fn messages() -> Vec<ContextMessage> {
        vec![
            ContextMessage::user("m1", "hello"),
            ContextMessage::assistant("m2", "world"),
        ]
    }

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(name: &str) -> Self {
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "judge-archive-{name}-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn snapshots_round_trip_under_a_scoped_directory() {
        let root = TempRoot::new("roundtrip");
        let archive = FileJevContextArchive::new(&root.0, 10_000).unwrap();
        let reference = archive.save(&scope(), &messages()).await.unwrap();
        assert_eq!(reference.len(), 64);
        assert!(reference.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(archive.root().is_absolute());
        assert_eq!(archive.max_bytes(), 10_000);
        assert_eq!(archive.restore(&scope(), &reference).await.unwrap(), messages());

        // Saving the same content is idempotent and reuses the content address.
        assert_eq!(
            archive.save(&scope(), &messages()).await.unwrap(),
            reference
        );
    }

    #[tokio::test]
    async fn scopes_are_isolated() {
        let root = TempRoot::new("scopes");
        let archive = FileJevContextArchive::new(&root.0, 10_000).unwrap();
        let reference = archive.save(&scope(), &messages()).await.unwrap();
        let other = ArchiveScope::new("user", "agent", "other").unwrap();
        assert_eq!(
            archive.restore(&other, &reference).await.unwrap_err().message(),
            "missing, unsafe or oversized archive"
        );
    }

    #[tokio::test]
    async fn references_sizes_and_checksums_are_enforced() {
        let root = TempRoot::new("limits");
        let archive = FileJevContextArchive::new(&root.0, 10_000).unwrap();
        assert_eq!(
            archive
                .restore(&scope(), "nope")
                .await
                .unwrap_err()
                .message(),
            "invalid archive reference"
        );

        let small = FileJevContextArchive::new(&root.0, 8).unwrap();
        assert_eq!(
            small.save(&scope(), &messages()).await.unwrap_err().message(),
            "archive limit"
        );

        let reference = archive.save(&scope(), &messages()).await.unwrap();
        let file = archive
            .root()
            .join(sha256_hex(&serde_json::to_vec(&scope()).unwrap()))
            .join(format!("{reference}.json"));
        std::fs::write(&file, b"tampered").unwrap();
        assert_eq!(
            archive.restore(&scope(), &reference).await.unwrap_err().message(),
            "archive checksum mismatch"
        );

        assert_eq!(
            FileJevContextArchive::new(&root.0, 0).unwrap_err().message(),
            "positive archive byte limit required"
        );
    }

    #[test]
    fn normalization_removes_relative_segments() {
        let normalized = normalize(Path::new("a/./b/../c"));
        assert!(normalized.is_absolute());
        assert!(normalized.ends_with(Path::new("a").join("c")));
        assert!(!normalized.to_string_lossy().contains(".."));
    }

    #[test]
    fn digests_are_lowercase_sha256_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256_hex(b"").len(), 64);
    }
}
