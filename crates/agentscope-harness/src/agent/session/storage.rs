//! Session storage backends, mirroring `LocalSessionStorage` / `StoreSessionStorage`.
//!
//! The journal runtime never writes through model-facing filesystem tools: a session log is
//! runtime state, not workspace material the model may edit. These backends therefore live
//! under their own namespace and expose only the narrow [`AtomicSessionStorage`] capability.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agentscope_core::agent::RuntimeContext;
use agentscope_core::session::{
    AtomicSessionStorage, SessionKey, SessionLog, SessionLogStore, StorageValue,
};
use async_trait::async_trait;

use crate::filesystem::Filesystem;

/// Filesystem-backed atomic session storage.
///
/// Each value is one file; versioning rides on a sibling `.version` object so a
/// compare-and-set is a two-step check-then-write. A compare-and-set is serialized per path
/// within this process: the journal's lease heartbeat renews `head.json` while a run's flush
/// commits the same head, and without that serialization both writers passed the version
/// check before either wrote, so the later write resurrected the older sequence number.
pub struct WorkspaceSessionStorage {
    fs: Arc<dyn Filesystem>,
    /// One asynchronous lock per value path, so the read-version → write-value → write-version
    /// sequence cannot interleave with another compare-and-set on that path.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl WorkspaceSessionStorage {
    pub fn new(fs: Arc<dyn Filesystem>) -> Self {
        Self {
            fs,
            locks: Mutex::new(HashMap::new()),
        }
    }

    fn lock_for(&self, path: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().unwrap();
        Arc::clone(
            locks
                .entry(path.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    /// Whether a compare-and-set needs the per-path lock.
    ///
    /// Only mutable values (heads, session metadata, export cursors) race a second writer.
    /// Commits, payload blobs and writer seals are content-addressed and written once; two
    /// writers either write identical bytes or the immutable-conflict check rejects them.
    /// Locking them would also retain one lock entry per event forever, so a long session grew
    /// with its journal even after it stopped executing.
    fn needs_cas_lock(path: &str) -> bool {
        !path.contains("/commits/") && !path.contains("/blobs/") && !path.contains("/writer-seals/")
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

    /// Replaces a value without ever exposing a missing or half-written file.
    ///
    /// The filesystem `write` is create-if-absent (that guarantee keeps model-facing tools from
    /// clobbering files), so an overwrite used to delete the old value first. A reader landing
    /// in that window saw "no value" for a path that does exist — `head.json` in particular —
    /// and the journal then reported seq 0 to a live run. That surfaced as a false
    /// `interrupted` receipt while the run was still executing, and made the host try to
    /// continue a turn that never stopped. Writing a sibling and moving it over the target
    /// keeps the previous bytes visible until the new value is complete.
    async fn replace(&self, ctx: &RuntimeContext, path: &str, content: &str) -> crate::Result<()> {
        let temp = format!("{path}.tmp-{}", uuid::Uuid::new_v4());
        self.fs.write(ctx, &temp, content).await?;
        if let Err(error) = self.fs.move_path(ctx, &temp, path).await {
            let _ = self.fs.delete(ctx, &temp).await;
            return Err(error);
        }
        Ok(())
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
            // A value is replaced without ever disappearing (`replace`), so a missing value
            // normally means "not written yet". A version marker without its value can only
            // be a write that was interrupted or a writer from an older build (which deleted
            // before writing). Wait for it briefly instead of reporting an empty journal: the
            // journal maps "no head" to seq 0, which reads as "no history" even though its
            // commits exist.
            let version_path = Self::version_path(path);
            if !self.fs.exists(&ctx, &version_path).await? {
                return Ok(None);
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
            while std::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                if self.fs.exists(&ctx, path).await? {
                    break;
                }
            }
            if !self.fs.exists(&ctx, path).await? {
                return Ok(None);
            }
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
        // Serialize the whole check-then-write. The lease heartbeat and the run's flush
        // both compare-and-set `head.json`; let the version check and both writes run under
        // one lock so a stale renew cannot overwrite a newer sequence number. Immutable,
        // content-addressed paths do not need the lock (and must not accumulate one entry
        // per committed event).
        let lock = Self::needs_cas_lock(path).then(|| self.lock_for(path));
        let _guard = match &lock {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };
        let current = self.read_version(&ctx, path).await?;
        if current != expected_version {
            return Ok(false);
        }
        let content = String::from_utf8(bytes.to_vec())
            .map_err(|_| agentscope_core::Error::Agent("session values must be UTF-8".into()))?;
        if path.contains("/commits/") || path.contains("/blobs/") {
            // Immutable, content-addressed objects are written once. Their bytes are the
            // checksum, so a lost acknowledgement can be resolved by re-reading; writing a
            // sibling `.version` file for each commit doubles the journal's file count and
            // exposes a second crash window that has no purpose. Create-if-absent is atomic
            // for the immutable file itself.
            if self.fs.exists(&ctx, path).await? {
                let existing = self.fs.read(&ctx, path, 0, usize::MAX).await?;
                if existing.content.as_bytes() != bytes {
                    return Err(agentscope_core::Error::Agent(
                        "immutable session object conflict".into(),
                    ));
                }
                return Ok(false);
            }
            self.fs.write(&ctx, path, &content).await?;
            return Ok(true);
        }
        self.replace(&ctx, path, &content).await?;
        self.replace(
            &ctx,
            &Self::version_path(path),
            &(expected_version + 1).to_string(),
        )
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
            .filter(|path| !path.ends_with(".version") && !path.contains(".tmp-"))
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
            let Some(agent_id) = decode_component(agent_path.rsplit('/').next().unwrap_or(""))
            else {
                continue;
            };
            let users = self
                .storage
                .list_dirs(&format!("{agent_path}/sessions"))
                .await?;
            for user_path in users {
                let Some(decoded_user) =
                    decode_component(user_path.rsplit('/').next().unwrap_or(""))
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
                    if let Ok(key) =
                        SessionKey::new(decoded_user.clone(), agent_id.clone(), session_id)
                    {
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
    use crate::filesystem::LocalFilesystem;
    use agentscope_core::session::JournalSessionLog;

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
        assert!(storage
            .compare_and_set("s1/head.json", 0, b"first")
            .await
            .unwrap());
        let value = storage.read("s1/head.json").await.unwrap().unwrap();
        assert_eq!(value.bytes, b"first".to_vec());
        assert_eq!(value.version, 1);

        // A stale expectation fails without writing.
        assert!(!storage
            .compare_and_set("s1/head.json", 0, b"stale")
            .await
            .unwrap());
        assert_eq!(
            storage.read("s1/head.json").await.unwrap().unwrap().bytes,
            b"first".to_vec()
        );

        // The current version succeeds.
        assert!(storage
            .compare_and_set("s1/head.json", 1, b"second")
            .await
            .unwrap());
        assert_eq!(
            storage.read("s1/head.json").await.unwrap().unwrap().bytes,
            b"second".to_vec()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn values_are_replaced_without_a_visible_gap() {
        // The journal reads `head.json` on every receipt poll. Replacing a value used to
        // delete it before writing the new bytes, so a reader in that window saw "no value"
        // and the journal reported seq 0 — a live run looked interrupted and a restored
        // session looked empty. Readers must always see either the old or the new value.
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = temp_dir("atomic");
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::new(dir.clone()));
        let storage = Arc::new(WorkspaceSessionStorage::new(fs));
        storage
            .compare_and_set("s1/head.json", 0, b"0")
            .await
            .unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let storage = Arc::clone(&storage);
            let stop = Arc::clone(&stop);
            tokio::spawn(async move {
                let mut missing = 0usize;
                let mut reads = 0usize;
                while !stop.load(Ordering::SeqCst) {
                    if storage.read("s1/head.json").await.unwrap().is_none() {
                        missing += 1;
                    }
                    reads += 1;
                    tokio::task::yield_now().await;
                }
                (missing, reads)
            })
        };
        for version in 1..200u64 {
            assert!(storage
                .compare_and_set("s1/head.json", version, version.to_string().as_bytes())
                .await
                .unwrap());
        }
        stop.store(true, Ordering::SeqCst);
        let (missing, reads) = reader.await.unwrap();
        assert!(reads > 0, "the reader never ran");
        assert_eq!(
            missing, 0,
            "a reader observed an existing value as missing while it was replaced"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn concurrent_compare_and_set_keeps_one_winner() {
        // The lease heartbeat renews `head.json` every 30s while the run's flush may commit
        // the same head. Both used to read the same version before either wrote it, so both
        // reported success and the later writer overwrote the head with its older sequence
        // number. The next commit then failed with "stale committed prefix", the run's
        // `run/end` was lost, and the host auto-resumed a turn that had already finished.
        let dir = temp_dir("cas_race");
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::new(dir.clone()));
        let storage = Arc::new(WorkspaceSessionStorage::new(fs));

        for round in 0..32u32 {
            let path = format!("s{round}/head.json");
            let first = {
                let storage = Arc::clone(&storage);
                let path = path.clone();
                tokio::spawn(async move { storage.compare_and_set(&path, 0, b"first").await })
            };
            let second = {
                let storage = Arc::clone(&storage);
                let path = path.clone();
                tokio::spawn(async move { storage.compare_and_set(&path, 0, b"second").await })
            };
            let (first, second) = tokio::join!(first, second);
            let first = first.unwrap().unwrap();
            let second = second.unwrap().unwrap();
            assert!(
                first ^ second,
                "both writers won the same compare-and-set: first={first} second={second}"
            );

            let value = storage.read(&path).await.unwrap().unwrap();
            assert_eq!(value.version, 1);
            assert_eq!(
                value.bytes,
                if first {
                    b"first".to_vec()
                } else {
                    b"second".to_vec()
                },
                "the loser's value replaced the winner's"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn immutable_commits_do_not_get_a_version_file() {
        // A commit's bytes are its checksum, so a sibling `.version` doubles the file count
        // (one per event) and adds a second crash window with no benefit. A replayed write
        // must still be idempotent when the bytes match.
        let dir = temp_dir("immutable");
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::new(dir.clone()));
        let storage = WorkspaceSessionStorage::new(Arc::clone(&fs));
        let ctx = RuntimeContext::new();

        assert!(storage
            .compare_and_set("s1/commits/abc.json", 0, b"payload")
            .await
            .unwrap());
        assert!(!storage
            .compare_and_set("s1/commits/abc.json", 0, b"payload")
            .await
            .unwrap());
        assert!(!fs
            .exists(&ctx, "s1/commits/abc.json.version")
            .await
            .unwrap());
        let err = storage
            .compare_and_set("s1/commits/abc.json", 0, b"different")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("conflict"), "{err}");

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
        assert_eq!(
            decode_component(agent_component).as_deref(),
            Some("agent/x")
        );
        // A non-encoded component is rejected rather than guessed.
        assert!(decode_component("raw").is_none());
    }

    #[test]
    fn journal_type_is_constructible_from_the_storage() {
        // Compile-time guard: the store must expose the core journal implementation.
        let dir = temp_dir("type");
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::new(dir.clone()));
        let storage: Arc<dyn AtomicSessionStorage> = Arc::new(WorkspaceSessionStorage::new(fs));
        let _journal = JournalSessionLog::new(storage, "agents/x");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
