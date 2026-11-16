//! Filesystem-backed `MessageBus` implementation, mirroring `WorkspaceMessageBus`.
//!
//! Works with any filesystem backend (local, remote, sandbox) and is suitable for
//! cross-process scenarios where multiple runtimes share the same workspace
//! directory. Queue delivery depends on the backend's `write` being an atomic
//! create-if-absent, which the local (exclusive create) and remote (CAS) backends are.
//!
//! Mode D (pub/sub) is degraded to polling: `subscribe` spawns a task that emits
//! an empty signal every 3 seconds; `publish` is a no-op.
//!
//! File layout under `busRoot`:
//! ```text
//! {busRoot}/
//!   queues/{key-hash}/  {entryId}.payload      — payload, written before publication
//!                       {entryId}.ready        — ready marker (legacy entries: {entryId}.json)
//!                       {entryId}.ready.claim  — exclusive drain claim
//!   logs/{key-hash}/    {entryId}.json         — Mode C entries (append-only)
//! ```
//!
//! Claims are not leased: a crash after claiming leaves the entry unavailable until
//! operator-led recovery, because automatically reclaiming it could deliver it twice.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::{Error, Result};
use async_trait::async_trait;
use serde_json::Value;

use crate::filesystem::Filesystem;

use super::message_bus::{BusEntry, MessageBus};

/// Poll interval for the degraded pub/sub mode (mirrors the Java 3-second tick).
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// `MessageBus` implementation backed by a `Filesystem`.
pub struct WorkspaceMessageBus {
    fs: Arc<dyn Filesystem>,
    bus_root: String,
    seq: AtomicU64,
    /// Context for the `MessageBus` trait entry points, which carry none.
    ///
    /// Every storage operation resolves its namespace from a context passed in explicitly (see
    /// the `*_with_context` methods); this only feeds the context-free trait shim.
    ctx: Option<RuntimeContext>,
}

impl WorkspaceMessageBus {
    /// Creates a bus rooted at `bus_root` (an absolute path within `filesystem`,
    /// e.g. `/bus`).
    pub fn new(filesystem: Arc<dyn Filesystem>, bus_root: impl Into<String>) -> Self {
        let mut root = bus_root.into();
        while root.ends_with('/') {
            root.pop();
        }
        Self {
            fs: filesystem,
            bus_root: root,
            seq: AtomicU64::new(0),
            ctx: None,
        }
    }

    /// Supplies the runtime context the context-free `MessageBus` entry points use.
    ///
    /// Without it those entry points cannot know the caller's agent/user, so every queue, log
    /// and inbox file lands in the default storage namespace. Callers that can thread a context
    /// per call should use the `*_with_context` methods instead.
    pub fn with_runtime_context(mut self, ctx: RuntimeContext) -> Self {
        self.ctx = Some(ctx);
        self
    }

    /// Context for the context-free trait entry points.
    ///
    /// Returns an owned value so the fallback needs no process-wide static: the trait shims hold
    /// it for the duration of one call.
    fn ctx_for_trait(&self) -> RuntimeContext {
        if let Some(ctx) = &self.ctx {
            return ctx.clone();
        }
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            tracing::warn!(
                "WorkspaceMessageBus was built without a runtime context; bus entries are \
                 written to the default storage namespace. Configure \
                 `with_runtime_context(..)` to scope them to the caller's agent and user."
            );
        });
        RuntimeContext::default()
    }

    fn queue_dir(&self, key: &str) -> String {
        format!("{}/queues/{}", self.bus_root, hash_key(key))
    }

    fn log_dir(&self, key: &str) -> String {
        format!("{}/logs/{}", self.bus_root, hash_key(key))
    }

    fn next_entry_id(&self) -> String {
        let n = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        format!("{n:020}")
    }

    async fn ensure_dir(&self, ctx: &RuntimeContext, dir: &str) -> Result<()> {
        if !self.fs.exists(ctx, dir).await? {
            // Concurrent producers race on the marker; an existing marker is the goal, so a
            // create-if-absent conflict is not an error.
            if let Err(e) = self.fs.write(ctx, &format!("{dir}/.keep"), "").await {
                if !crate::filesystem::is_already_exists_error(&e) {
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Lists `.json` file paths in `dir`, sorted lexicographically (equal to the
    /// entry-id order). Empty when the directory does not exist.
    async fn list_sorted(&self, ctx: &RuntimeContext, dir: &str) -> Result<Vec<String>> {
        self.list_matching(ctx, dir, |p| p.ends_with(".json")).await
    }

    /// Lists queue entry markers (`.json` legacy entries and `.ready` markers).
    async fn list_queue_entries(&self, ctx: &RuntimeContext, dir: &str) -> Result<Vec<String>> {
        self.list_matching(ctx, dir, |p| p.ends_with(".json") || p.ends_with(".ready"))
            .await
    }

    async fn list_matching(
        &self,
        ctx: &RuntimeContext,
        dir: &str,
        include: impl Fn(&str) -> bool,
    ) -> Result<Vec<String>> {
        if !self.fs.exists(ctx, dir).await? {
            return Ok(Vec::new());
        }
        let mut files: Vec<String> = self
            .fs
            .ls(ctx, dir)
            .await?
            .into_iter()
            .filter(|f| !f.is_directory && include(&f.path))
            .map(|f| f.path)
            .collect();
        files.sort();
        Ok(files)
    }

    /// Reads a file's full content, or `None` on any error (transient failures).
    async fn read_file_content(&self, ctx: &RuntimeContext, path: &str) -> Option<String> {
        match self.fs.read(ctx, Self::fs_path(path), 0, usize::MAX).await {
            Ok(data) => Some(data.content),
            Err(_) => None,
        }
    }

    /// Turns a path reported by [`list_sorted`](Self::list_sorted) back into one the
    /// filesystem accepts.
    ///
    /// `Filesystem::ls` reports paths relative to the filesystem root with a leading `/`
    /// (see `LocalFilesystem::display_path`). An *unrestricted* filesystem — the default
    /// `HarnessAgent` one — resolves such a path as absolute, so reading or deleting it
    /// silently misses. Everything the bus lists lives below its own root, so the separator
    /// can be dropped.
    fn fs_path(path: &str) -> &str {
        path.trim_start_matches('/')
    }

    fn extract_entry_id(path: &str) -> String {
        let name = path.rsplit('/').next().unwrap_or(path);
        name.strip_suffix(".json")
            .or_else(|| name.strip_suffix(".ready"))
            .unwrap_or(name)
            .to_string()
    }

    /// Reads the payload behind a queue entry: a `.ready` marker resolves to its
    /// sibling `.payload`, a legacy entry *is* its own payload.
    async fn read_queue_entry_content(&self, ctx: &RuntimeContext, path: &str) -> Option<String> {
        match Self::fs_path(path).strip_suffix(".ready") {
            Some(prefix) => {
                let payload = format!("{prefix}.payload");
                self.read_file_content(ctx, &payload).await
            }
            None => self.read_file_content(ctx, path).await,
        }
    }

    /// Deletes a claimed entry; a `.ready` marker is removed together with its payload.
    async fn delete_queue_entry(&self, ctx: &RuntimeContext, path: &str) -> Result<()> {
        let path = Self::fs_path(path);
        let Some(prefix) = path.strip_suffix(".ready") else {
            return self.fs.delete(ctx, path).await;
        };
        self.fs.delete(ctx, path).await?;
        self.fs.delete(ctx, &format!("{prefix}.payload")).await
    }

    /// Releases a claim after a read failure so a later drain can retry the entry.
    async fn release_claim(&self, ctx: &RuntimeContext, claim_path: &str) {
        if let Err(e) = self.fs.delete(ctx, claim_path).await {
            tracing::warn!(
                "queue_drain: failed to release claim {claim_path} ({e}); entry will remain unavailable"
            );
        }
    }
}

/// Sanitizes `key` (non `[a-zA-Z0-9._-]` chars become `_`) and appends a
/// lowercase hex `DefaultHasher` digest, e.g. `agentscope:wakeups` →
/// `agentscope_wakeups_<hex>`.
pub(crate) fn hash_key(key: &str) -> String {
    let sanitized: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    format!("{sanitized}_{:x}", hasher.finish())
}

#[async_trait]
impl MessageBus for WorkspaceMessageBus {
    // ---- Mode A: drain queue ----

    async fn queue_push(&self, key: &str, payload: Value) -> Result<String> {
        let ctx = self.ctx_for_trait();
        self.queue_push_with_context(&ctx, key, payload).await
    }

    async fn queue_drain(&self, key: &str, max_count: usize) -> Result<Vec<BusEntry>> {
        let ctx = self.ctx_for_trait();
        self.queue_drain_with_context(&ctx, key, max_count).await
    }

    async fn queue_delete(&self, key: &str) -> Result<()> {
        let ctx = self.ctx_for_trait();
        self.queue_delete_with_context(&ctx, key).await
    }

    async fn queue_peek(&self, key: &str) -> Result<bool> {
        let ctx = self.ctx_for_trait();
        self.queue_peek_with_context(&ctx, key).await
    }

    // ---- Mode C: replay log ----

    async fn log_append(&self, key: &str, payload: Value, max_len: i32) -> Result<String> {
        let ctx = self.ctx_for_trait();
        self.log_append_with_context(&ctx, key, payload, max_len)
            .await
    }

    async fn log_read(
        &self,
        key: &str,
        since: Option<&str>,
        max_count: usize,
    ) -> Result<Vec<BusEntry>> {
        let ctx = self.ctx_for_trait();
        self.log_read_with_context(&ctx, key, since, max_count)
            .await
    }

    async fn log_trim(&self, key: &str) -> Result<()> {
        let ctx = self.ctx_for_trait();
        self.log_trim_with_context(&ctx, key).await
    }

    // ---- Mode D: degraded pub/sub (deliberate simplification) ----

    async fn publish(&self, _key: &str, _payload: Value) -> Result<()> {
        Ok(())
    }

    fn subscribe(&self, _key: &str) -> tokio::sync::mpsc::UnboundedReceiver<Value> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            // First tick after the initial 3-second delay, matching `Flux.interval`.
            let start = tokio::time::Instant::now() + POLL_INTERVAL;
            let mut interval = tokio::time::interval_at(start, POLL_INTERVAL);
            loop {
                interval.tick().await;
                if tx.send(serde_json::json!({})).is_err() {
                    break; // receiver dropped
                }
            }
        });
        rx
    }
}

/// Context-carrying entry points.
///
/// The `MessageBus` trait carries no `RuntimeContext`, so its shims above have to fall back to a
/// single configured (or default) context. These variants let a caller that holds the run's
/// context scope every queue, log and inbox file to the right agent/user namespace.
impl WorkspaceMessageBus {
    /// Appends `payload` to the drain queue at `key`, under `ctx`.
    pub async fn queue_push_with_context(
        &self,
        ctx: &RuntimeContext,
        key: &str,
        payload: Value,
    ) -> Result<String> {
        let entry_id = self.next_entry_id();
        let dir = self.queue_dir(key);
        self.ensure_dir(ctx, &dir).await?;
        let payload_path = format!("{dir}/{entry_id}.payload");
        let ready_path = format!("{dir}/{entry_id}.ready");
        let json = serde_json::to_string(&payload)?;
        if let Err(e) = self.fs.write(ctx, &payload_path, &json).await {
            return Err(Error::Other(format!(
                "queue_push: failed to write payload {payload_path} ({e})"
            )));
        }
        if let Err(e) = self.fs.write(ctx, &ready_path, "").await {
            // A failed write does not establish ownership of the marker: removing either
            // file here could discard an entry another writer published.
            tracing::warn!(
                "queue_push: publication of {ready_path} failed ({e}); retaining payload \
                 {payload_path} for possible drain or operator recovery"
            );
            return Err(Error::Other(format!(
                "queue_push: failed to publish entry {ready_path} ({e})"
            )));
        }
        Ok(entry_id)
    }

    /// Drains up to `max_count` entries from the queue at `key`, under `ctx`.
    pub async fn queue_drain_with_context(
        &self,
        ctx: &RuntimeContext,
        key: &str,
        max_count: usize,
    ) -> Result<Vec<BusEntry>> {
        let dir = self.queue_dir(key);
        let files = self.list_queue_entries(ctx, &dir).await?;
        let mut result = Vec::new();
        for path in files {
            if result.len() >= max_count {
                break;
            }
            // Atomic create-if-absent: exactly one drainer wins an entry.
            let claim_path = format!("{}.claim", Self::fs_path(&path));
            if let Err(e) = self.fs.write(ctx, &claim_path, "").await {
                if crate::filesystem::is_already_exists_error(&e) {
                    tracing::debug!("queue_drain: entry {path} is already claimed, skipping");
                    continue;
                }
                if !result.is_empty() {
                    tracing::warn!(
                        "queue_drain: failed to claim entry {path} ({e}); returning {} \
                         already-drained entries",
                        result.len()
                    );
                    break;
                }
                return Err(Error::Other(format!(
                    "queue_drain: failed to claim entry {path} ({e})"
                )));
            }
            let Some(content) = self.read_queue_entry_content(ctx, &path).await else {
                // A transient read failure is retryable because no entry was returned.
                tracing::warn!("queue_drain: failed to read entry {path}, skipping without delete");
                self.release_claim(ctx, &claim_path).await;
                continue;
            };
            let entry_id = Self::extract_entry_id(&path);
            match serde_json::from_str::<Value>(&content) {
                Ok(payload) => result.push(BusEntry::new(entry_id, payload)),
                Err(e) => {
                    // Corrupt JSON is unrecoverable; delete so the queue isn't blocked.
                    tracing::warn!("queue_drain: corrupt JSON in entry {path} ({e}), deleting");
                }
            }
            match self.delete_queue_entry(ctx, &path).await {
                Ok(()) => {
                    if let Err(e) = self.fs.delete(ctx, &claim_path).await {
                        tracing::warn!(
                            "queue_drain: failed to delete claim {claim_path} ({e}) after \
                             deleting its entry"
                        );
                    }
                }
                Err(e) => tracing::warn!(
                    "queue_drain: failed to delete claimed entry {path} ({e}); retaining claim \
                     to prevent duplicate delivery"
                ),
            }
        }
        Ok(result)
    }

    /// Deletes the drain queue at `key`, under `ctx`.
    pub async fn queue_delete_with_context(&self, ctx: &RuntimeContext, key: &str) -> Result<()> {
        self.fs.delete(ctx, &self.queue_dir(key)).await
    }

    /// Reports whether the drain queue at `key` has entries, without consuming them.
    pub async fn queue_peek_with_context(&self, ctx: &RuntimeContext, key: &str) -> Result<bool> {
        Ok(!self
            .list_queue_entries(ctx, &self.queue_dir(key))
            .await?
            .is_empty())
    }

    /// Appends `payload` to the replay log at `key`, under `ctx`.
    pub async fn log_append_with_context(
        &self,
        ctx: &RuntimeContext,
        key: &str,
        payload: Value,
        max_len: i32,
    ) -> Result<String> {
        let entry_id = self.next_entry_id();
        let dir = self.log_dir(key);
        self.ensure_dir(ctx, &dir).await?;
        let path = format!("{dir}/{entry_id}.json");
        self.fs
            .write(ctx, &path, &serde_json::to_string(&payload)?)
            .await?;

        if max_len > 0 {
            let files = self.list_sorted(ctx, &dir).await?;
            let excess = files.len().saturating_sub(max_len as usize);
            for path in files.iter().take(excess) {
                self.fs.delete(ctx, Self::fs_path(path)).await?;
            }
        }
        Ok(entry_id)
    }

    /// Reads up to `max_count` entries from the replay log at `key`, under `ctx`.
    pub async fn log_read_with_context(
        &self,
        ctx: &RuntimeContext,
        key: &str,
        since: Option<&str>,
        max_count: usize,
    ) -> Result<Vec<BusEntry>> {
        let dir = self.log_dir(key);
        let files = self.list_sorted(ctx, &dir).await?;
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let mut start = 0;
        if let Some(since) = since {
            for (i, path) in files.iter().enumerate() {
                if Self::extract_entry_id(path) == since {
                    start = i + 1;
                    break;
                }
            }
        }
        let mut result = Vec::new();
        let end = start.saturating_add(max_count).min(files.len());
        for path in files.iter().take(end).skip(start) {
            if let Some(content) = self.read_file_content(ctx, path).await {
                if let Ok(payload) = serde_json::from_str::<Value>(&content) {
                    result.push(BusEntry::new(Self::extract_entry_id(path), payload));
                }
            }
        }
        Ok(result)
    }

    /// Deletes the replay log at `key`, under `ctx`.
    pub async fn log_trim_with_context(&self, ctx: &RuntimeContext, key: &str) -> Result<()> {
        self.fs.delete(ctx, &self.log_dir(key)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_key_sanitizes_and_appends_lowercase_hex() {
        let h = hash_key("agentscope:wakeups");
        assert!(h.starts_with("agentscope_wakeups_"));
        assert!(h.ends_with(&h.split('_').last().unwrap().to_lowercase()));
        // Deterministic for the same key.
        assert_eq!(h, hash_key("agentscope:wakeups"));
        // Dot and dash survive sanitization.
        let h2 = hash_key("a.b-c");
        assert!(h2.starts_with("a.b-c_"));
    }

    #[test]
    fn next_entry_id_is_20_digit_zero_padded_and_monotonic() {
        let fs: Arc<dyn Filesystem> = Arc::new(crate::filesystem::LocalFilesystem::new("."));
        let bus = WorkspaceMessageBus::new(fs, "/bus");
        let a = bus.next_entry_id();
        let b = bus.next_entry_id();
        assert_eq!(a, "00000000000000000001");
        assert_eq!(b, "00000000000000000002");
        assert_eq!(a.len(), 20);
        assert!(a < b);
    }

    #[tokio::test]
    async fn a_supplied_runtime_context_scopes_queue_storage() {
        use crate::filesystem::remote::store::{InMemoryStore, NamespaceFactory};
        use crate::filesystem::remote::RemoteFilesystem;

        struct UserNamespace;
        impl NamespaceFactory for UserNamespace {
            fn get_namespace(&self, ctx: &RuntimeContext) -> Vec<String> {
                vec![
                    "users".to_string(),
                    ctx.user_id().unwrap_or("anon").to_string(),
                    "bus".to_string(),
                ]
            }
        }

        let fs: Arc<dyn Filesystem> = Arc::new(RemoteFilesystem::new(
            Arc::new(InMemoryStore::new()),
            Arc::new(UserNamespace),
        ));
        let bus = WorkspaceMessageBus::new(fs, "/bus");
        let alice = RuntimeContext::default().with_user_id("alice");
        let bob = RuntimeContext::default().with_user_id("bob");

        bus.queue_push_with_context(&alice, "q", serde_json::json!({"n": 1}))
            .await
            .expect("push");
        assert!(
            bus.queue_drain_with_context(&bob, "q", 10)
                .await
                .expect("bob drain")
                .is_empty(),
            "another user's namespace must stay empty"
        );
        let drained = bus
            .queue_drain_with_context(&alice, "q", 10)
            .await
            .expect("alice drain");
        assert_eq!(drained.len(), 1);
    }
}
