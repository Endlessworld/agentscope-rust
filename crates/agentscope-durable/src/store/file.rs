//! Single-host file store: one JSON document per scope plus one per work item.
//!
//! Every write goes through a temporary file in the same directory followed by a rename,
//! so a crash leaves either the previous document or the new one, never half of each.
//! Cross-process exclusion uses one lock file per scope.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{RecordStore, Txn, WorkItem};
use crate::{DurableError, Result};

// How long a contender waits for the scope lock before giving up.
//
// It has to cover a stale-lock reclaim: a lock whose holder died is only recognised as stale
// after `LOCK_STALE`, so a shorter deadline reports `scope is locked` for a lock nobody holds —
// and every write rewrites the whole scope, which for a long session is tens of megabytes.
const LOCK_TIMEOUT: Duration = Duration::from_secs(60);
const LOCK_RETRY: Duration = Duration::from_millis(10);
const LOCK_STALE: Duration = Duration::from_secs(30);
const RENAME_RETRIES: u32 = 3;

/// Scope maps and the readiness index backed by plain files under one directory.
pub struct FileStore {
    root: PathBuf,
}

impl FileStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        for dir in ["scopes", "work", "locks"] {
            fs::create_dir_all(root.join(dir))?;
        }
        Ok(Self { root })
    }

    fn scope_path(&self, scope: &str) -> PathBuf {
        self.root
            .join("scopes")
            .join(format!("{}.json", digest(scope)))
    }

    fn work_path(&self, queue: &str, work_id: &str) -> PathBuf {
        let key = format!("{queue}\u{0}{work_id}");
        self.root
            .join("work")
            .join(format!("{}.json", digest(&key)))
    }

    fn lock_path(&self, scope: &str) -> PathBuf {
        self.root
            .join("locks")
            .join(format!("{}.lock", digest(scope)))
    }

    fn read_scope(&self, scope: &str) -> Result<BTreeMap<String, Value>> {
        match fs::read(self.scope_path(scope)) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(error.into()),
        }
    }

    /// Applies a finished transaction. Only reached when the closure returned `Ok`.
    fn commit(
        &self,
        scope: &str,
        base: BTreeMap<String, Value>,
        staged: BTreeMap<String, Option<Value>>,
        scheduled: BTreeMap<String, Scheduled>,
    ) -> Result<()> {
        let mut committed = base;
        let mut touched = false;
        for (part, value) in staged {
            if scheduled.contains_key(&part) {
                continue;
            }
            match value {
                Some(value) => {
                    committed.insert(part, value);
                }
                None => {
                    committed.remove(&part);
                }
            }
            touched = true;
        }
        for entry in scheduled.values() {
            let path = self.work_path(&entry.queue, &entry.work_id);
            if entry.ready_at < 0 {
                match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                continue;
            }
            write_json(
                &path,
                &WorkRecord {
                    queue: entry.queue.clone(),
                    work_id: entry.work_id.clone(),
                    ready_at: entry.ready_at,
                    payload: entry.payload.clone(),
                },
            )?;
        }
        if touched {
            write_json(&self.scope_path(scope), &committed)?;
        }
        Ok(())
    }
}

impl RecordStore for FileStore {
    fn transaction(
        &self,
        scope: &str,
        work: &mut dyn FnMut(&mut dyn Txn) -> Result<()>,
    ) -> Result<()> {
        let _lock = acquire_lock(&self.lock_path(scope))?;
        let base = self.read_scope(scope)?;
        let mut staged = StagedTxn {
            base: &base,
            staged: BTreeMap::new(),
            scheduled: BTreeMap::new(),
            now: crate::now_millis(),
        };
        let outcome = work(&mut staged);
        let StagedTxn {
            staged, scheduled, ..
        } = staged;
        if let Err(error) = outcome {
            // Discard: nothing reached the disk.
            return Err(error);
        }
        self.commit(scope, base, staged, scheduled)
    }

    fn ready(&self, queue: &str, now: i64, limit: usize) -> Result<Vec<WorkItem>> {
        let mut due: Vec<WorkItem> = Vec::new();
        let entries = match fs::read_dir(self.root.join("work")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let record: WorkRecord = serde_json::from_slice(&bytes)
                .map_err(|error| DurableError::invalid(format!("{}: {error}", path.display())))?;
            if record.queue != queue || record.ready_at > now {
                continue;
            }
            due.push(WorkItem {
                queue: record.queue,
                work_id: record.work_id,
                ready_at: record.ready_at,
                payload: record.payload,
            });
        }
        due.sort_by(|left, right| {
            left.ready_at
                .cmp(&right.ready_at)
                .then_with(|| left.work_id.cmp(&right.work_id))
        });
        due.dedup_by(|left, right| left.work_id == right.work_id);
        due.truncate(limit);
        Ok(due)
    }

    fn shared_across_hosts(&self) -> bool {
        true
    }
}

/// A staged readiness write. It also lands in `staged` under the reserved key so that
/// `get` and `keys` inside the transaction behave like the in-memory store.
struct Scheduled {
    queue: String,
    work_id: String,
    ready_at: i64,
    payload: Option<Value>,
}

struct StagedTxn<'a> {
    base: &'a BTreeMap<String, Value>,
    staged: BTreeMap<String, Option<Value>>,
    scheduled: BTreeMap<String, Scheduled>,
    now: i64,
}

impl StagedTxn<'_> {
    fn read(&self, part: &str) -> Option<Value> {
        match self.staged.get(part) {
            Some(value) => value.clone(),
            None => self.base.get(part).cloned(),
        }
    }
}

impl Txn for StagedTxn<'_> {
    fn now(&self) -> i64 {
        self.now
    }

    fn get(&self, part: &str) -> Result<Option<Value>> {
        Ok(self.read(part))
    }

    fn put(&mut self, part: &str, value: Option<Value>) -> Result<()> {
        self.staged.insert(part.to_string(), value);
        Ok(())
    }

    fn keys(&self, prefix: &str, after: Option<&str>, limit: usize) -> Result<Vec<String>> {
        let mut keys: Vec<String> = self
            .base
            .keys()
            .chain(self.staged.keys())
            .filter(|key| key.starts_with(prefix))
            .filter(|key| after.is_none_or(|after| key.as_str() > after))
            .filter(|key| self.read(key).is_some())
            .cloned()
            .collect();
        keys.sort_unstable();
        keys.dedup();
        keys.truncate(limit);
        Ok(keys)
    }

    fn schedule(
        &mut self,
        queue: &str,
        work_id: &str,
        ready_at: i64,
        payload: Option<Value>,
    ) -> Result<()> {
        self.staged.insert(
            schedule_key(queue, work_id),
            Some(serde_json::json!({
                "queue": queue,
                "workId": work_id,
                "readyAt": ready_at,
                "payload": payload,
            })),
        );
        self.scheduled.insert(
            schedule_key(queue, work_id),
            Scheduled {
                queue: queue.to_string(),
                work_id: work_id.to_string(),
                ready_at,
                payload,
            },
        );
        Ok(())
    }
}

fn schedule_key(queue: &str, work_id: &str) -> String {
    format!("\u{1}work:{queue}:{work_id}")
}

#[derive(Serialize, Deserialize)]
struct WorkRecord {
    queue: String,
    #[serde(rename = "workId")]
    work_id: String,
    #[serde(rename = "readyAt")]
    ready_at: i64,
    payload: Option<Value>,
}

fn digest(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    write_atomic(path, &bytes)
}

/// Temporary file plus rename, retried because Windows may briefly hold the target.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = path.with_file_name(format!(".{name}.{}-{sequence}.tmp", std::process::id()));
    if let Err(error) = fs::write(&temporary, bytes) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    let mut last: Option<std::io::Error> = None;
    for attempt in 0..=RENAME_RETRIES {
        match fs::rename(&temporary, path) {
            Ok(()) => return Ok(()),
            Err(error) => {
                last = Some(error);
                if attempt < RENAME_RETRIES {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
    let _ = fs::remove_file(&temporary);
    match last {
        Some(error) => Err(error.into()),
        None => Err(DurableError::unavailable("commit renamed nothing")),
    }
}

/// Held for the lifetime of one transaction; removal happens on every exit path.
struct ScopeLock {
    path: PathBuf,
}

impl Drop for ScopeLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_lock(path: &Path) -> Result<ScopeLock> {
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                let stamp = format!("{} {}", std::process::id(), crate::now_millis());
                let _ = file.write_all(stamp.as_bytes());
                let _ = file.flush();
                return Ok(ScopeLock {
                    path: path.to_path_buf(),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if is_stale(path) {
                    let _ = fs::remove_file(path);
                }
                if Instant::now() >= deadline {
                    return Err(DurableError::unavailable("scope is locked"));
                }
                std::thread::sleep(LOCK_RETRY);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn is_stale(path: &Path) -> bool {
    // A lock whose writer already exited can be taken over at once: waiting out `LOCK_STALE`
    // would report `scope is locked` to every contender for the whole grace period.
    if holder_alive(path) == Some(false) {
        return true;
    }
    // Same-process holders and locks without a readable pid still fall back to the mtime rule.
    match fs::metadata(path).and_then(|meta| meta.modified()) {
        Ok(modified) => modified
            .elapsed()
            .map(|elapsed| elapsed > LOCK_STALE)
            .unwrap_or(false),
        Err(_) => true,
    }
}

/// Whether the process that wrote the lock file still runs; `None` when the pid is unreadable.
fn holder_alive(path: &Path) -> Option<bool> {
    let body = fs::read_to_string(path).ok()?;
    let pid: u32 = body.split_whitespace().next()?.parse().ok()?;
    Some(process_alive(pid))
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // `kill(pid, 0)` performs the existence/permission check without delivering a signal;
    // `EPERM` means the process exists but belongs to somebody else.
    match unsafe { libc::kill(pid as libc::pid_t, 0) } {
        0 => true,
        _ => std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM),
    }
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_INVALID_PARAMETER};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // A pid that no longer exists cannot be opened at all; a process owned by somebody else
    // answers `ERROR_ACCESS_DENIED`, which still means alive.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return GetLastError() != ERROR_INVALID_PARAMETER;
        }
        let mut code = 0u32;
        let queried = GetExitCodeProcess(handle, &mut code) != 0;
        CloseHandle(handle);
        // `STILL_ACTIVE` is 259: the process has not exited yet.
        !queried || code == 259
    }
}

#[cfg(not(any(unix, windows)))]
fn process_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod lock_liveness_tests {
    use super::*;

    /// `u32::MAX - 1` is above the maximum pid on every platform, so no process owns it.
    const DEAD_PID: u32 = 2_147_483_646;

    fn lock_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agentscope-durable-lock-{}-{}-{name}",
            std::process::id(),
            crate::now_millis()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir.join("scope.lock")
    }

    fn cleanup(path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn a_lock_from_a_dead_holder_is_reclaimed_without_waiting() {
        let path = lock_file("dead");
        fs::write(&path, format!("{DEAD_PID} 0")).unwrap();
        assert!(is_stale(&path), "a lock whose holder exited must be stale");

        let started = Instant::now();
        let lock = acquire_lock(&path).unwrap();
        drop(lock);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a dead holder must be reclaimed at once, not after LOCK_STALE"
        );
        cleanup(&path);
    }

    #[test]
    fn a_lock_held_by_a_live_process_is_not_stale() {
        let path = lock_file("live");
        fs::write(&path, format!("{} 0", std::process::id())).unwrap();
        assert!(!is_stale(&path));
        cleanup(&path);
    }

    #[test]
    fn a_lock_without_a_readable_pid_falls_back_to_the_mtime_rule() {
        let path = lock_file("garbage");
        fs::write(&path, "not-a-pid").unwrap();
        assert!(
            !is_stale(&path),
            "an unreadable body must not be treated as dead"
        );
        cleanup(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Removes the tree even when a test fails or returns early.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(name: &str) -> Self {
            let unique = format!(
                "agentscope-durable-file-{}-{}-{name}",
                std::process::id(),
                crate::now_millis()
            );
            Self(std::env::temp_dir().join(unique))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn read_value(store: &FileStore, scope: &str, part: &str) -> Result<Option<Value>> {
        crate::store::read(store, scope, |txn: &mut dyn Txn| txn.get(part))
    }

    fn file_count(dir: &Path) -> usize {
        match fs::read_dir(dir) {
            Ok(entries) => entries.filter_map(|entry| entry.ok()).count(),
            Err(_) => 0,
        }
    }

    #[test]
    fn put_get_and_delete_round_trip() -> Result<()> {
        let root = TempRoot::new("round-trip");
        let store = FileStore::open(root.path())?;
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("a", Some(json!({"n": 1})))?;
            txn.put("b", Some(json!("two")))
        })?;
        assert_eq!(read_value(&store, "session", "a")?, Some(json!({"n": 1})));
        assert_eq!(read_value(&store, "session", "b")?, Some(json!("two")));
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("a", None)
        })?;
        assert_eq!(read_value(&store, "session", "a")?, None);
        assert_eq!(read_value(&store, "session", "b")?, Some(json!("two")));
        Ok(())
    }

    #[test]
    fn failed_transaction_leaves_no_trace() -> Result<()> {
        let root = TempRoot::new("rollback");
        let store = FileStore::open(root.path())?;
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("keep", Some(json!(1)))
        })?;
        let outcome = store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("keep", Some(json!(2)))?;
            txn.put("fresh", Some(json!("x")))?;
            txn.schedule("queue", "work", 10, None)?;
            Err(DurableError::invalid("abort"))
        });
        assert!(outcome.is_err());
        assert_eq!(read_value(&store, "session", "keep")?, Some(json!(1)));
        assert_eq!(read_value(&store, "session", "fresh")?, None);
        assert!(store.ready("queue", i64::MAX, 10)?.is_empty());
        assert_eq!(file_count(&root.path().join("scopes")), 1);
        assert_eq!(file_count(&root.path().join("work")), 0);
        assert_eq!(file_count(&root.path().join("locks")), 0);
        Ok(())
    }

    #[test]
    fn keys_honour_prefix_after_and_limit() -> Result<()> {
        let root = TempRoot::new("keys");
        let store = FileStore::open(root.path())?;
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            for key in ["p:1", "p:2", "p:3", "q:1"] {
                txn.put(key, Some(json!(key)))?;
            }
            Ok(())
        })?;
        let all = crate::store::read(&store, "session", |txn: &mut dyn Txn| {
            txn.keys("p:", None, 10)
        })?;
        assert_eq!(all, vec!["p:1", "p:2", "p:3"]);
        let after = crate::store::read(&store, "session", |txn: &mut dyn Txn| {
            txn.keys("p:", Some("p:1"), 10)
        })?;
        assert_eq!(after, vec!["p:2", "p:3"]);
        let limited = crate::store::read(&store, "session", |txn: &mut dyn Txn| {
            txn.keys("p:", None, 2)
        })?;
        assert_eq!(limited, vec!["p:1", "p:2"]);
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("p:2", None)
        })?;
        let after_delete = crate::store::read(&store, "session", |txn: &mut dyn Txn| {
            txn.keys("p:", None, 10)
        })?;
        assert_eq!(after_delete, vec!["p:1", "p:3"]);
        Ok(())
    }

    #[test]
    fn schedule_orders_ready_entries_and_negative_removes_them() -> Result<()> {
        let root = TempRoot::new("schedule");
        let store = FileStore::open(root.path())?;
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.schedule("queue", "b", 200, Some(json!({"id": "b"})))?;
            txn.schedule("queue", "a", 100, None)?;
            txn.schedule("other", "z", 50, None)
        })?;
        let due = store.ready("queue", 150, 10)?;
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].work_id, "a");
        assert_eq!(due[0].payload, None);
        let due = store.ready("queue", 1000, 10)?;
        let ids: Vec<&str> = due.iter().map(|item| item.work_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
        assert_eq!(due[0].queue, "queue");
        assert_eq!(due[1].payload, Some(json!({"id": "b"})));
        assert_eq!(store.ready("queue", 1000, 1)?.len(), 1);
        let other = store.ready("other", 1000, 10)?;
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].work_id, "z");
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.schedule("queue", "a", -1, None)
        })?;
        let due = store.ready("queue", 1000, 10)?;
        let ids: Vec<&str> = due.iter().map(|item| item.work_id.as_str()).collect();
        assert_eq!(ids, vec!["b"]);
        Ok(())
    }

    #[test]
    fn reopening_the_same_root_keeps_data() -> Result<()> {
        let root = TempRoot::new("reopen");
        {
            let store = FileStore::open(root.path())?;
            store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
                txn.put("k", Some(json!(7)))?;
                txn.schedule("queue", "work", 5, Some(json!("payload")))
            })?;
        }
        let reopened = FileStore::open(root.path())?;
        assert!(reopened.shared_across_hosts());
        assert_eq!(read_value(&reopened, "session", "k")?, Some(json!(7)));
        let due = reopened.ready("queue", 10, 10)?;
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].payload, Some(json!("payload")));
        Ok(())
    }
}
