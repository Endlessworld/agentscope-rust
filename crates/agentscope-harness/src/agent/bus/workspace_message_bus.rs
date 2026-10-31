//! Filesystem-backed `MessageBus` implementation, mirroring `WorkspaceMessageBus`.
//!
//! Works with any filesystem backend (local, remote, sandbox) and is suitable for
//! cross-process scenarios where multiple runtimes share the same workspace
//! directory.
//!
//! Mode D (pub/sub) is degraded to polling: `subscribe` spawns a task that emits
//! an empty signal every 3 seconds; `publish` is a no-op.
//!
//! File layout under `busRoot`:
//! ```text
//! {busRoot}/
//!   queues/{key-hash}/  {entryId}.json   — Mode A entries (drain = ls + read + delete)
//!   logs/{key-hash}/    {entryId}.json   — Mode C entries (append-only)
//! ```

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
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
    ctx: RuntimeContext,
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
            ctx: RuntimeContext::default(),
        }
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

    async fn ensure_dir(&self, dir: &str) -> Result<()> {
        if !self.fs.exists(&self.ctx, dir).await? {
            self.fs
                .write(&self.ctx, &format!("{dir}/.keep"), "")
                .await?;
        }
        Ok(())
    }

    /// Lists `.json` file paths in `dir`, sorted lexicographically (equal to the
    /// entry-id order). Empty when the directory does not exist.
    async fn list_sorted(&self, dir: &str) -> Result<Vec<String>> {
        if !self.fs.exists(&self.ctx, dir).await? {
            return Ok(Vec::new());
        }
        let mut files: Vec<String> = self
            .fs
            .ls(&self.ctx, dir)
            .await?
            .into_iter()
            .filter(|f| !f.is_directory && f.path.ends_with(".json"))
            .map(|f| f.path)
            .collect();
        files.sort();
        Ok(files)
    }

    /// Reads a file's full content, or `None` on any error (transient failures).
    async fn read_file_content(&self, path: &str) -> Option<String> {
        match self
            .fs
            .read(&self.ctx, Self::fs_path(path), 0, usize::MAX)
            .await
        {
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
        name.strip_suffix(".json").unwrap_or(name).to_string()
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
        let entry_id = self.next_entry_id();
        let dir = self.queue_dir(key);
        self.ensure_dir(&dir).await?;
        let path = format!("{dir}/{entry_id}.json");
        self.fs
            .write(&self.ctx, &path, &serde_json::to_string(&payload)?)
            .await?;
        Ok(entry_id)
    }

    async fn queue_drain(&self, key: &str, max_count: usize) -> Result<Vec<BusEntry>> {
        let dir = self.queue_dir(key);
        let files = self.list_sorted(&dir).await?;
        let mut result = Vec::new();
        for path in files.iter().take(max_count) {
            let Some(content) = self.read_file_content(path).await else {
                // Transient read failure (race with concurrent drain, I/O error).
                // Do NOT delete — preserve the entry so the next drain can retry it.
                tracing::warn!("queue_drain: failed to read entry {path}, skipping without delete");
                continue;
            };
            let entry_id = Self::extract_entry_id(path);
            match serde_json::from_str::<Value>(&content) {
                Ok(payload) => result.push(BusEntry::new(entry_id, payload)),
                Err(e) => {
                    // Corrupt JSON is unrecoverable; delete so the queue isn't blocked.
                    tracing::warn!("queue_drain: corrupt JSON in entry {path} ({e}), deleting");
                }
            }
            self.fs.delete(&self.ctx, Self::fs_path(path)).await?;
        }
        Ok(result)
    }

    async fn queue_delete(&self, key: &str) -> Result<()> {
        self.fs.delete(&self.ctx, &self.queue_dir(key)).await
    }

    async fn queue_peek(&self, key: &str) -> Result<bool> {
        Ok(!self.list_sorted(&self.queue_dir(key)).await?.is_empty())
    }

    // ---- Mode C: replay log ----

    async fn log_append(&self, key: &str, payload: Value, max_len: i32) -> Result<String> {
        let entry_id = self.next_entry_id();
        let dir = self.log_dir(key);
        self.ensure_dir(&dir).await?;
        let path = format!("{dir}/{entry_id}.json");
        self.fs
            .write(&self.ctx, &path, &serde_json::to_string(&payload)?)
            .await?;

        if max_len > 0 {
            let files = self.list_sorted(&dir).await?;
            let excess = files.len().saturating_sub(max_len as usize);
            for path in files.iter().take(excess) {
                self.fs.delete(&self.ctx, Self::fs_path(path)).await?;
            }
        }
        Ok(entry_id)
    }

    async fn log_read(
        &self,
        key: &str,
        since: Option<&str>,
        max_count: usize,
    ) -> Result<Vec<BusEntry>> {
        let dir = self.log_dir(key);
        let files = self.list_sorted(&dir).await?;
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
            if let Some(content) = self.read_file_content(path).await {
                if let Ok(payload) = serde_json::from_str::<Value>(&content) {
                    result.push(BusEntry::new(Self::extract_entry_id(path), payload));
                }
            }
        }
        Ok(result)
    }

    async fn log_trim(&self, key: &str) -> Result<()> {
        self.fs.delete(&self.ctx, &self.log_dir(key)).await
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
}
