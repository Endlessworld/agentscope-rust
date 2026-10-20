//! Session storage backends, mirroring `LocalSessionStorage` / `StoreSessionStorage`.
//!
//! The journal runtime never writes through model-facing filesystem tools: a session log is
//! runtime state, not workspace material the model may edit. These backends therefore live
//! under their own namespace and expose only the narrow [`AtomicSessionStorage`] capability.

use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::session::{
    AtomicSessionStorage, SessionKey, SessionLog, SessionLogStore, StorageValue,
};
use async_trait::async_trait;

use crate::filesystem::Filesystem;

/// Filesystem-backed atomic session storage.
///
/// Each value is one file; versioning rides on a sibling `.version` object so a
/// compare-and-set is a two-step check-then-write. That is sufficient for a single writer
/// process (the journal supports only one execution writer anyway) and keeps the backend
/// free of a database dependency.
pub struct WorkspaceSessionStorage {
    fs: Arc<dyn Filesystem>,
}

impl WorkspaceSessionStorage {
    pub fn new(fs: Arc<dyn Filesystem>) -> Self {
        Self { fs }
    }

    fn version_path(path: &str) -> String {
        format!("{path}.version")
    }

    async fn read_version(&self, ctx: &RuntimeContext, path: &str) -> crate::Result<u64> {
        let version_path = Self::version_path(path);
        if !self.fs.exists(ctx, &version_path).await? {
            return Ok(0);
        }
        let data = self.fs.read(ctx, &version_path, 0, usize::MAX).await?;
        Ok(data.content.trim().parse::<u64>().unwrap_or(0))
    }

    /// Lists only the immediate subdirectories of `prefix`, without trailing slashes.
    ///
    /// The filesystem reports directories with a trailing `/` and, for rooted backends, a
    /// leading `/` as well. Discovery works in the same relative namespace the journal writes
    /// in, so both are stripped before the value is used as a path again.
    pub(crate) async fn list_dirs(&self, prefix: &str) -> crate::Result<Vec<String>> {
        let ctx = RuntimeContext::new();
        let entries = match self.fs.ls(&ctx, prefix).await {
            Ok(entries) => entries,
            Err(_) => return Ok(Vec::new()),
        };
        let mut dirs: Vec<String> = entries
            .into_iter()
            .filter(|entry| entry.is_directory)
            .map(|entry| {
                entry
                    .path
                    .trim_end_matches('/')
                    .trim_start_matches('/')
                    .to_string()
            })
            .collect();
        dirs.sort();
        Ok(dirs)
    }
}

#[async_trait]
impl AtomicSessionStorage for WorkspaceSessionStorage {
    async fn read(&self, path: &str) -> crate::Result<Option<StorageValue>> {
        let ctx = RuntimeContext::new();
        if !self.fs.exists(&ctx, path).await? {
            return Ok(None);
        }
        let version = self.read_version(&ctx, path).await?;
        let data = self.fs.read(&ctx, path, 0, usize::MAX).await?;
        Ok(Some(StorageValue::new(version, data.content.into_bytes())))
    }

    async fn compare_and_set(
        &self,
        path: &str,
        expected_version: u64,
        bytes: &[u8],
    ) -> crate::Result<bool> {
        let ctx = RuntimeContext::new();
        let current = self.read_version(&ctx, path).await?;
        if current != expected_version {
            return Ok(false);
        }
        let content = String::from_utf8(bytes.to_vec())
            .map_err(|_| agentscope_core::Error::Agent("session values must be UTF-8".into()))?;
        if self.fs.exists(&ctx, path).await? {
            self.fs.delete(&ctx, path).await?;
        }
        self.fs.write(&ctx, path, &content).await?;
        let version_path = Self::version_path(path);
        if self.fs.exists(&ctx, &version_path).await? {
            self.fs.delete(&ctx, &version_path).await?;
        }
        self.fs
            .write(&ctx, &version_path, &(expected_version + 1).to_string())
            .await?;
        Ok(true)
    }

    async fn list_paths(&self, prefix: &str) -> crate::Result<Vec<String>> {
        let ctx = RuntimeContext::new();
        // A missing prefix is an empty result: the first acquisition creates it.
        let entries = match self.fs.ls(&ctx, prefix).await {
            Ok(entries) => entries,
            Err(_) => return Ok(Vec::new()),
        };
        let mut paths: Vec<String> = entries
            .into_iter()
            .map(|entry| entry.path)
            .filter(|path| !path.ends_with(".version"))
            .collect();
        paths.sort();
        Ok(paths)
    }
}

/// Opens journals under a filesystem root, one sub-tree per logical session.
#[derive(Clone)]
pub struct WorkspaceSessionLogStore {
    storage: Arc<WorkspaceSessionStorage>,
}

impl std::fmt::Debug for WorkspaceSessionLogStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WorkspaceSessionLogStore")
    }
}

impl WorkspaceSessionLogStore {
    /// Creates a store writing under the filesystem's default namespace.
    pub fn new(fs: Arc<dyn Filesystem>) -> Self {
        Self {
            storage: Arc::new(WorkspaceSessionStorage::new(fs)),
        }
    }
}

#[async_trait]
impl SessionLogStore for WorkspaceSessionLogStore {
    async fn open(
        &self,
        key: &SessionKey,
        _context: &RuntimeContext,
    ) -> crate::Result<Arc<dyn SessionLog>> {
        Ok(Arc::new(agentscope_core::session::JournalSessionLog::new(
            Arc::clone(&self.storage) as Arc<dyn AtomicSessionStorage>,
            key.storage_path(),
        )))
    }

    async fn list(&self, context: &RuntimeContext) -> crate::Result<Vec<SessionKey>> {
        let user_id = context.user_id();
        let agents = self.storage.list_dirs("agents").await?;
        let mut keys = Vec::new();
        for agent_path in agents {
            let Some(agent_id) = decode_component(agent_path.rsplit('/').next().unwrap_or("")) else {
                continue;
            };
            let users = self
                .storage
                .list_dirs(&format!("{agent_path}/sessions"))
                .await?;
            for user_path in users {
                let Some(decoded_user) = decode_component(user_path.rsplit('/').next().unwrap_or(""))
                else {
                    continue;
                };
                // Discovery never crosses user namespaces.
                let decoded_user = if decoded_user.is_empty() {
                    None
                } else {
                    Some(decoded_user)
                };
                if decoded_user.as_deref() != user_id {
                    continue;
                }
                for session_path in self.storage.list_dirs(&user_path).await? {
                    let Some(session_id) =
                        decode_component(session_path.rsplit('/').next().unwrap_or(""))
                    else {
                        continue;
                    };
                    if let Ok(key) = SessionKey::new(
                        decoded_user.clone(),
                        agent_id.clone(),
                        session_id,
                    ) {
                        keys.push(key);
                    }
                }
            }
        }
        keys.sort();
        Ok(keys)
    }
}

/// Decodes the `s_<base64url>` component produced by [`SessionKey::storage_path`].
fn decode_component(component: &str) -> Option<String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let encoded = component.strip_prefix("s_")?;
    let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::session::JournalSessionLog;
    use crate::filesystem::LocalFilesystem;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let base =
            std::env::temp_dir().join(format!("agentscope_sess_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[tokio::test]
    async fn storage_round_trips_and_enforces_versions() {
        let dir = temp_dir("cas");
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::new(dir.clone()));
        let storage = WorkspaceSessionStorage::new(fs);

        assert!(storage.read("s1/head.json").await.unwrap().is_none());
        // Create-if-absent succeeds, then reports version 1.
        assert!(storage.compare_and_set("s1/head.json", 0, b"first").await.unwrap());
        let value = storage.read("s1/head.json").await.unwrap().unwrap();
        assert_eq!(value.bytes, b"first".to_vec());
        assert_eq!(value.version, 1);

        // A stale expectation fails without writing.
        assert!(!storage.compare_and_set("s1/head.json", 0, b"stale").await.unwrap());
        assert_eq!(storage.read("s1/head.json").await.unwrap().unwrap().bytes, b"first".to_vec());

        // The current version succeeds.
        assert!(storage.compare_and_set("s1/head.json", 1, b"second").await.unwrap());
        assert_eq!(storage.read("s1/head.json").await.unwrap().unwrap().bytes, b"second".to_vec());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_journal_persists_across_store_instances() {
        let dir = temp_dir("reopen");
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::new(dir.clone()));
        let key = SessionKey::new(Some("alice"), "agent", "s1").unwrap();
        let ctx = RuntimeContext::new().with_user_id("alice");

        {
            let store = WorkspaceSessionLogStore::new(Arc::clone(&fs));
            let log = store.open(&key, &ctx).await.unwrap();
            let writer = log
                .acquire("run-1", std::time::Duration::from_secs(60))
                .await
                .unwrap();
            let event = agentscope_core::session::SessionEvent::new(
                "e1",
                1,
                0,
                "run/start",
                Some("run-1".into()),
                Some("t1".into()),
                true,
                "{}",
            )
            .unwrap();
            log.commit(&writer, "b1", 0, &[event]).await.unwrap();
            log.release(&writer).await.unwrap();
        }

        // A fresh process (new store) recovers the committed prefix.
        let store = WorkspaceSessionLogStore::new(Arc::clone(&fs));
        let log = store.open(&key, &ctx).await.unwrap();
        assert_eq!(log.head().await.unwrap().seq, 1);
        assert_eq!(log.scan(0, 1).await.unwrap().len(), 1);

        // Discovery finds the session for its owner only.
        assert_eq!(store.list(&ctx).await.unwrap(), vec![key.clone()]);
        let other = RuntimeContext::new().with_user_id("bob");
        assert!(store.list(&other).await.unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn storage_components_round_trip() {
        let key = SessionKey::new(Some("alice"), "agent/x", "s 1").unwrap();
        let path = key.storage_path();
        let agent_component = path.split('/').nth(1).unwrap();
        assert_eq!(decode_component(agent_component).as_deref(), Some("agent/x"));
        // A non-encoded component is rejected rather than guessed.
        assert!(decode_component("raw").is_none());
    }

    #[test]
    fn journal_type_is_constructible_from_the_storage() {
        // Compile-time guard: the store must expose the core journal implementation.
        let dir = temp_dir("type");
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::new(dir.clone()));
        let storage: Arc<dyn AtomicSessionStorage> =
            Arc::new(WorkspaceSessionStorage::new(fs));
        let _journal = JournalSessionLog::new(storage, "agents/x");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
