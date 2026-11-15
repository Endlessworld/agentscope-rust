//! Single-host file store: one JSON document per scope plus one per work item.
//!
//! # What is promised, and what is not
//!
//! Every write goes through a temporary file in the same directory followed by a rename, so a
//! *process* crash — a panic, a `kill`, a supervisor restart — leaves either the previous
//! document or the new one, never half of each, and a concurrent reader never sees a partial
//! document.
//!
//! Power loss is a different promise and this store does not make it: no `sync_all` and no
//! directory fsync are issued. A scope document is tens of megabytes and is rewritten on every
//! turn, so flushing each one to the platter would multiply the write cost of a turn — the
//! deployment this store targets (an agent host that is restarted far more often than the machine
//! is powered off) cannot pay that. A machine that loses power in the middle of a write may
//! therefore find a zero-length or half-written document; it is reported as an `INVALID` document
//! rather than silently trusted, and the previous one is gone. Restoring power-loss safety is a
//! deliberate trade against the low-consumption rule, not an oversight.
//!
//! Cross-process exclusion uses one lock file per scope; any number of hosts may share one root
//! (see [`FileStore`]).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{RecordStore, Txn, WorkItem};
use crate::{DurableError, Result};

// How long a contender waits for the scope lock before giving up.
//
// It is a contention bound, not a stale-lock grace: whether a lock is abandoned is decided by the
// owner's liveness, so the wait only has to cover transactions that really run. Every write
// rewrites the whole scope, which for a long session is tens of megabytes, so it has to be
// generous.
const LOCK_TIMEOUT: Duration = Duration::from_secs(60);
const LOCK_RETRY: Duration = Duration::from_millis(10);

// How long a lock whose owner cannot be identified by pid is left alone.
//
// Liveness is the authority (see `is_stale`); this only covers a body that names no readable
// pid at all. It is far larger than any transaction, because reclaiming a live lock is a lost
// update, not a hiccup.
const LOCK_STALE_UNKNOWN: Duration = Duration::from_secs(30 * 60);

// How long a lock body that a reclaim had to move aside is given to get back to its path.
//
// The body belongs to a live holder, so the reclaim must not drop it; but waiting out the whole
// `LOCK_TIMEOUT` would spend the *contender's* deadline on somebody else's transaction. The short
// bound covers the case it is there for — a third contender that creates the lock and drops it
// again immediately. When it expires the body stays in its aside file instead of being deleted,
// and the holder's `verify` then refuses the commit it could no longer be sure of.
const LOCK_PUT_BACK: Duration = Duration::from_secs(2);

/// The body written over a lock this process failed to delete.
///
/// `holder_alive` reads the first field as a pid, so an abandoned lock has to name a pid that is
/// provably gone: this one is above the maximum pid on both platforms (`pid_max` is below 2^22 on
/// Linux, and Windows refuses `OpenProcess` for it) while still parsing as a `u32`. A lock
/// carrying it is reclaimed by the next contender at once instead of reading as a live holder for
/// `LOCK_STALE_UNKNOWN`.
const LOCK_TOMBSTONE: &str = "2147483646 0";

// The retry budget of one file operation another process holds.
//
// On Windows a read, a `rename`, a `File::create` and a `remove_file` all answer
// `ERROR_ACCESS_DENIED` while any other process holds the target — a writer of this very store
// replacing the file, an indexer, a virus scanner, an editor that opened it without
// `FILE_SHARE_DELETE`. Sharing is transient, so every operation that can hit it retries: ten
// attempts with a doubling wait from `HELD_RETRY_BACKOFF` up to `HELD_RETRY_BACKOFF_MAX` (about
// 2.8 s in total) is long enough for a whole scope commit by another host to finish, while still
// failing a real error on the first attempt.
const HELD_RETRIES: u32 = 10;
const HELD_RETRY_BACKOFF: Duration = Duration::from_millis(25);
const HELD_RETRY_BACKOFF_MAX: Duration = Duration::from_millis(400);

/// Scope maps and the readiness index backed by plain files under one directory.
///
/// Any number of hosts (processes) may open the same root at the same time: exclusion is per
/// scope through `locks/*.lock`, and every document is replaced atomically, so readers always see
/// a complete previous or complete next document.
pub struct FileStore {
    root: PathBuf,
}

// ----------------------------------------------------------------------------
// Write-traffic counters
// ----------------------------------------------------------------------------
//
// Two relaxed atomic adds per write and nothing else: no disk, no lock, no allocation. They exist
// so a test (or a field investigation) can answer "how many transactions and how many bytes does
// one turn cost" without a profiler, which is the number every storage optimisation has to move.
// Counting is always on: a `cfg(test)` copy would be invisible to the integration tests that
// measure a whole turn, and a feature flag would make the numbers depend on the build.

static TRANSACTIONS: AtomicU64 = AtomicU64::new(0);
static SCOPE_WRITES: AtomicU64 = AtomicU64::new(0);
static SCOPE_BYTES: AtomicU64 = AtomicU64::new(0);
static BYTES_WRITTEN: AtomicU64 = AtomicU64::new(0);

/// What this process wrote through [`FileStore`] since the last reset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriteStats {
    /// Committed scope transactions.
    pub transactions: u64,
    /// Committed transactions that rewrote the scope document.
    pub scope_writes: u64,
    /// Bytes of scope documents written by those rewrites.
    pub scope_bytes: u64,
    /// Bytes written in total: scope documents, work records, temporary files.
    pub bytes_written: u64,
}

/// The counters above, as of now.
pub fn write_stats() -> WriteStats {
    WriteStats {
        transactions: TRANSACTIONS.load(Ordering::Relaxed),
        scope_writes: SCOPE_WRITES.load(Ordering::Relaxed),
        scope_bytes: SCOPE_BYTES.load(Ordering::Relaxed),
        bytes_written: BYTES_WRITTEN.load(Ordering::Relaxed),
    }
}

/// Zeroes the counters; a measurement takes one before and one after the turn it costs.
pub fn reset_write_stats() {
    TRANSACTIONS.store(0, Ordering::Relaxed);
    SCOPE_WRITES.store(0, Ordering::Relaxed);
    SCOPE_BYTES.store(0, Ordering::Relaxed);
    BYTES_WRITTEN.store(0, Ordering::Relaxed);
}

fn count_scope_write(bytes: usize) {
    SCOPE_WRITES.fetch_add(1, Ordering::Relaxed);
    SCOPE_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
}

impl FileStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        for dir in ["scopes", "work", "locks"] {
            // Creating a directory two hosts need at the same moment answers "already exists",
            // which is success; a transient hold on the parent is retried like any other.
            retry_while_held(|| fs::create_dir_all(root.join(dir)))?;
        }
        // One canonical root when the file system offers one: the in-process holder table is
        // keyed by lock path, and two stores of this process opened on the same directory under
        // different spellings (relative, absolute, a `..` in the middle) would otherwise disagree
        // about what a lock path *is* — one store would take the other's live lock for an orphan.
        // The fallback keeps a store usable where canonicalising is refused.
        let root = fs::canonicalize(&root).unwrap_or(root);
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

    /// One lock for the whole readiness index; see [`FileStore::commit`].
    fn work_lock_path(&self) -> PathBuf {
        self.root.join("locks").join("work-index.lock")
    }

    fn read_scope(&self, scope: &str) -> Result<BTreeMap<String, Value>> {
        // A share violation from the host that is replacing the document right now is transient;
        // a missing document is not, and is answered at once.
        match retry_while_held(|| fs::read(self.scope_path(scope))) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(error.into()),
        }
    }

    /// Applies a finished transaction. Only reached when the closure returned `Ok`.
    fn commit(
        &self,
        scope: &str,
        lock: &ScopeLock,
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
        // The readiness index is not part of any scope: two scopes — in this process or in
        // another host — can schedule the same work id, and one commit can both write one item
        // and drop another, so without a lock a reader (`ready`) may see half of that batch and a
        // later writer may put the removed item back. The lock is always taken *after* the scope
        // lock (this function runs inside `transaction`), so there is one lock order and no cycle
        // is possible.
        if !scheduled.is_empty() {
            let _index = acquire_lock(&self.work_lock_path())?;
            for entry in scheduled.values() {
                let path = self.work_path(&entry.queue, &entry.work_id);
                if entry.ready_at < 0 {
                    match retry_while_held(|| fs::remove_file(&path)) {
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
        }
        if touched {
            // The last moment at which losing the lock can still be noticed, and the only one that
            // matters: the lock is what excludes the other writer, so a stolen lock must cost an
            // abandoned commit, not a lost update. One small read against a whole-document
            // rewrite, so the check is cheap enough to make unconditionally.
            lock.verify()?;
            let bytes = serde_json::to_vec(&committed)?;
            count_scope_write(bytes.len());
            write_atomic(&self.scope_path(scope), &bytes)?;
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
        let lock = acquire_lock(&self.lock_path(scope))?;
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
        TRANSACTIONS.fetch_add(1, Ordering::Relaxed);
        self.commit(scope, &lock, base, staged, scheduled)
    }

    fn scope_bytes(&self, scope: &str) -> Result<Option<u64>> {
        match fs::metadata(self.scope_path(scope)) {
            Ok(meta) => Ok(Some(meta.len())),
            // A scope that was never written has no document yet.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Some(0)),
            Err(error) => Err(error.into()),
        }
    }

    fn ready(&self, queue: &str, now: i64, limit: usize) -> Result<Vec<WorkItem>> {
        let mut due: Vec<WorkItem> = Vec::new();
        // Reading needs no index lock: every document is replaced by an atomic rename, so a
        // reader sees either the whole previous record or the whole next one. It does need the
        // retry, because the file being replaced is briefly held by its writer.
        let entries = match retry_while_held(|| fs::read_dir(self.root.join("work"))) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let bytes = match retry_while_held(|| fs::read(&path)) {
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

/// Temporary file plus rename, retried because another host may briefly hold the target.
///
/// Deliberately no `sync_all` and no directory fsync: see the module note. The rename is what
/// makes a *process* crash safe; a power cut is not covered, and buying that coverage would cost a
/// platter flush of every scope document on every turn.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    BYTES_WRITTEN.fetch_add(bytes.len() as u64, Ordering::Relaxed);
    let sequence = next_sequence();
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = path.with_file_name(format!(".{name}.{}-{sequence}.tmp", std::process::id()));
    if let Err(error) = retry_while_held(|| fs::write(&temporary, bytes)) {
        let _ = retry_while_held(|| fs::remove_file(&temporary));
        return Err(error.into());
    }
    if let Err(error) = retry_while_held(|| fs::rename(&temporary, path)) {
        let _ = retry_while_held(|| fs::remove_file(&temporary));
        return Err(error.into());
    }
    Ok(())
}

/// Retries an operation while the file system says another process holds the file.
///
/// Only the two sharing answers are retried: `ERROR_ACCESS_DENIED` (5, also what a target held
/// without `FILE_SHARE_DELETE` produces) and `ERROR_SHARING_VIOLATION` (32). Rust maps 5 to
/// `PermissionDenied` and 32 to no dedicated kind, so the raw codes are the only reliable test —
/// and anything else, a read-only directory or a missing file for instance, must fail on the
/// first attempt instead of being swallowed by a retry loop.
///
/// Every operation on a shared document goes through it: writing the temporary file, the rename
/// that publishes it, removing a work record, reading a scope or a work record, removing a lock.
fn retry_while_held<T>(mut operation: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut backoff = HELD_RETRY_BACKOFF;
    let mut attempt = 0;
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if attempt < HELD_RETRIES && is_transient_hold(&error) => {
                attempt += 1;
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(HELD_RETRY_BACKOFF_MAX);
            }
            Err(error) => return Err(error),
        }
    }
}

fn is_transient_hold(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(5) | Some(32))
}

/// A per-process counter, so two temporary names of the same process never collide.
fn next_sequence() -> u64 {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

/// Locks this process currently holds, `path -> body`.
///
/// `process_alive` answers for a pid, not for a lock: after a delete that failed, a lock still
/// names a live process — us — and would block its scope until the process exits, `LOCK_TIMEOUT`
/// at a time, with no way to recover. This table is the missing half of the answer: a lock whose
/// body names *this* process but is not registered here is a leftover of this process, and taking
/// it over is always right because nothing in this process is behind it. Cross-process exclusion
/// is untouched: another process's lock names a pid that is not ours and stays subject to the
/// liveness rule.
fn held_locks() -> &'static Mutex<BTreeMap<PathBuf, String>> {
    static HELD: OnceLock<Mutex<BTreeMap<PathBuf, String>>> = OnceLock::new();
    HELD.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Registers a lock this process now holds. Failing to register means the liveness answer can no
/// longer be trusted for it, so the acquisition is refused instead.
fn hold(path: &Path, stamp: &str) -> Result<()> {
    let mut held = held_locks()
        .lock()
        .map_err(|_| DurableError::unavailable("lock table poisoned"))?;
    held.insert(path.to_path_buf(), stamp.to_string());
    Ok(())
}

/// Forgets a lock, but only while the table still names the body this holder wrote: a successor of
/// the same process may already hold the path and must keep its entry.
fn release_held(path: &Path, stamp: &str) {
    if let Ok(mut held) = held_locks().lock() {
        if held.get(path).is_some_and(|current| current == stamp) {
            held.remove(path);
        }
    }
}

/// Whether `body` is the lock this process is holding at `path` right now.
fn registered(path: &Path, body: &str) -> bool {
    match held_locks().lock() {
        Ok(held) => held.get(path).is_some_and(|current| current == body),
        // A poisoned table cannot be read, and not knowing must not turn a live lock into an
        // orphan: answering "registered" only leaves such a lock for the older rules to decide.
        Err(_) => true,
    }
}

/// Held for the lifetime of one transaction; removal happens on every exit path.
struct ScopeLock {
    path: PathBuf,
    /// The exact body this holder wrote, so the exit path cannot delete a successor's lock.
    stamp: String,
}

impl ScopeLock {
    /// Whether this transaction still owns the scope lock it took.
    ///
    /// Two things can take a lock away: a contender that judged it stale (only possible when its
    /// pid is unreadable or reused), and `reclaim_stale` that had to move it aside and could not
    /// put it back. Either way the only answer that is safe is to abandon the commit — writing the
    /// scope would be exactly the lost update the lock exists to prevent. The check costs one
    /// small read next to a whole-document rewrite, so it is done immediately before the write.
    fn verify(&self) -> Result<()> {
        let body = retry_while_held(|| fs::read_to_string(&self.path)).map_err(|error| {
            DurableError::unavailable(format!("scope lock is unreadable: {error}"))
        })?;
        if body != self.stamp {
            return Err(DurableError::unavailable("scope lock was lost"));
        }
        Ok(())
    }
}

impl Drop for ScopeLock {
    fn drop(&mut self) {
        // Only the lock this transaction created: a lock written by a later holder belongs to it,
        // and removing it would let a third writer in while that one is still writing the scope.
        // The read is retried like the removal: a transient hold here must not leave a lock
        // behind that blocks the scope until its timeout. Bodies are unique per acquisition, so a
        // match here is this transaction's own file, never a successor's.
        let ours = retry_while_held(|| fs::read_to_string(&self.path))
            .is_ok_and(|body| body == self.stamp);
        if ours {
            match retry_while_held(|| fs::remove_file(&self.path)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    // The delete failed and the file still names this live pid. Left alone it
                    // stops the scope for as long as this process runs, so it is overwritten with
                    // a body whose owner is provably gone and the next contender reclaims it at
                    // once. The re-read is inside the retried closure so a lock that changed
                    // hands in the meantime is never touched.
                    let _ = retry_while_held(|| {
                        let body = fs::read_to_string(&self.path)?;
                        if body != self.stamp {
                            return Ok(());
                        }
                        fs::write(&self.path, LOCK_TOMBSTONE.as_bytes())
                    });
                }
            }
        }
        // Dropped last: even when both the delete and the tombstone failed, removing the entry
        // turns the leftover into "a lock naming this process that nobody here holds", which is
        // what `stale_body` reclaims at once.
        release_held(&self.path, &self.stamp);
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
                // Two fields would not identify a *lock*: two threads of this process can take the
                // lock in the same millisecond, and identical bodies would then let one of them
                // delete the other's file in `Drop` while it still holds it — two writers on one
                // scope. The per-process sequence makes every acquisition's body unique.
                let stamp = format!(
                    "{} {} {}",
                    std::process::id(),
                    crate::now_millis(),
                    next_sequence()
                );
                // Registered before the body is written, so that no contender can ever read a
                // body naming this process that the holder table does not yet know: that would be
                // the "orphan" rule stealing a lock that was live a microsecond later. An empty
                // body is unattributable and therefore not taken either way.
                if let Err(error) = hold(path, &stamp) {
                    let _ = retry_while_held(|| fs::remove_file(path));
                    return Err(error);
                }
                // The body names the holder, so writing it belongs to taking the lock: an empty
                // body cannot be attributed to anybody, and a lock nobody can attribute keeps the
                // scope busy for `LOCK_STALE_UNKNOWN` if it survives. A failure therefore hands
                // the lock back instead of leaving an unattributable file behind.
                if let Err(error) = retry_while_held(|| file.write_all(stamp.as_bytes()))
                    .and_then(|()| retry_while_held(|| file.flush()))
                {
                    drop(file);
                    let _ = retry_while_held(|| fs::remove_file(path));
                    release_held(path, &stamp);
                    return Err(error.into());
                }
                drop(file);
                return Ok(ScopeLock {
                    path: path.to_path_buf(),
                    stamp,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // Only a lock whose owner is provably gone may be taken over; a live owner —
                // including another thread of this process — is always waited for.
                if let Some(observed) = stale_body(path) {
                    reclaim_stale(path, &observed);
                }
                if Instant::now() >= deadline {
                    return Err(DurableError::unavailable("scope is locked"));
                }
                std::thread::sleep(LOCK_RETRY);
            }
            // A path another host is creating or removing right now answers `ACCESS_DENIED`:
            // Windows keeps a just-deleted file "pending delete" for a moment, and `create_new` on
            // such a path is refused with 5 instead of `AlreadyExists`. It clears on its own, so
            // it is retried with the same deadline as a held lock.
            Err(error) if is_transient_hold(&error) => {
                if Instant::now() >= deadline {
                    return Err(error.into());
                }
                std::thread::sleep(LOCK_RETRY);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Takes a stale lock away from nobody.
///
/// Two contenders can both find the same lock stale, so reclaiming with `remove_file` is unsafe:
/// the loser deletes the *fresh* lock the winner has just created, and both then write the same
/// scope — the lost update this lock exists to prevent. Here the file is moved aside first
/// (`rename` moves a given path at most once, so it picks one winner) and the moved body is
/// compared with the one the caller saw as stale: if they differ, the winner had already
/// recovered and that lock is put back exactly as it was, never replacing a lock that appeared in
/// the meantime.
///
/// The body of a live lock is never deleted, whatever happens next: its holder would go on writing
/// the scope while a third contender held the lock. A body that cannot be put back stays in its
/// aside file, and `ScopeLock::verify` makes that holder abandon the commit instead.
fn reclaim_stale(path: &Path, observed: &str) {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let aside = path.with_file_name(format!(
        ".{name}.{}-{}.stale",
        std::process::id(),
        next_sequence()
    ));
    // The lock that read as stale is not necessarily the lock still on disk, and the second look
    // is authoritative in both directions: a body that changed means another contender already
    // dealt with it, and a body that is *gone* — the usual case, because the winner of a reclaim
    // leaves the path free for a moment before it retries — means there is nothing to reclaim at
    // all. Renaming a lock that appeared in that moment would take it away from a live holder
    // while a third contender creates its own: two writers on one scope.
    if !retry_while_held(|| fs::read_to_string(path)).is_ok_and(|body| body == observed) {
        return;
    }
    // Somebody else won the race in the last instant, or the file is gone: the loop decides with
    // `create_new`.
    if retry_while_held(|| fs::rename(path, &aside)).is_err() {
        return;
    }
    match retry_while_held(|| fs::read_to_string(&aside)) {
        Ok(body) if body == observed => {
            // Really the stale lock: nobody is behind it, so dropping it *is* the reclaim.
            let _ = retry_while_held(|| fs::remove_file(&aside));
        }
        Ok(body) => {
            // A lock somebody else had just created. `create_new` puts it back without ever
            // overwriting a lock a third contender may have created in the meantime.
            if put_back(path, &body).is_ok() {
                let _ = retry_while_held(|| fs::remove_file(&aside));
            }
            // Otherwise the body stays in `aside`: deleting it would be the lost update this
            // function exists to prevent, and its holder notices through `verify`.
        }
        Err(_) => {
            // The body cannot be read, so nothing is known about its owner. The path is free,
            // which is what a reclaim wants; inventing a body to put back would be worse than
            // leaving the aside file behind.
        }
    }
}

/// Puts a lock body that had to be moved aside back at its path.
///
/// `create_new` is what makes this safe: the path may have been taken by a third contender while
/// the file was aside, and overwriting that lock would be a lost update for *its* holder. A busy
/// path is therefore waited for, but only for [`LOCK_PUT_BACK`]: waiting out `LOCK_TIMEOUT` would
/// spend the recovering contender's own deadline on another host's transaction.
fn put_back(path: &Path, body: &str) -> std::io::Result<()> {
    let deadline = Instant::now() + LOCK_PUT_BACK;
    loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                // The body is a few bytes; if writing it fails the path names a holder that wrote
                // nothing, which `LOCK_STALE_UNKNOWN` still reclaims.
                let _ = file.write_all(body.as_bytes());
                let _ = file.flush();
                return Ok(());
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists
                    || is_transient_hold(&error) =>
            {
                if Instant::now() >= deadline {
                    return Err(error);
                }
                std::thread::sleep(LOCK_RETRY);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Whether a lock may be taken away from its owner; see [`stale_body`] for the rule.
#[cfg(test)]
fn is_stale(path: &Path) -> bool {
    stale_body(path).is_some()
}

/// The body of a lock that may be taken over; `None` while its owner is alive.
///
/// The owner's liveness is the authority, not the clock. A transaction rewrites its whole scope,
/// so a live commit easily outlives any grace period a clock could give it, and reclaiming a live
/// lock means two writers on one scope — a lost update, not a hiccup. The mtime rule survives
/// only for a lock whose owner cannot be identified at all (an unreadable body), with a threshold
/// far beyond any transaction.
///
/// No heartbeat is maintained while the lock is held: it would cost a write per transaction (or a
/// background thread per store) and would still mis-rank a holder that is alive but slow, while
/// the pid answer is exact on both supported platforms. A live-but-stuck holder cannot block a
/// scope silently either — `acquire_lock` gives up after `LOCK_TIMEOUT` and reports
/// `scope is locked`, so the stall is reported instead of hidden.
///
/// The body is returned so a reclaim can prove it moved *the* lock it judged stale.
fn stale_body(path: &Path) -> Option<String> {
    // Retried like every other read: a transient hold must not be mistaken for "the lock is gone"
    // by one contender after another.
    let body = retry_while_held(|| fs::read_to_string(path)).ok()?;
    match holder_alive(&body) {
        // This process's pid is live, but that says nothing about *this lock*: a delete that
        // failed leaves a body naming us with nobody behind it. Only a lock registered in the
        // holder table is a live owner here.
        Some(true) if is_orphan_of_this_process(path, &body) => Some(body),
        // Another thread of this very process is a live owner too: waiting is correct.
        Some(true) => None,
        // A lock whose writer already exited can be taken over at once: waiting out a grace
        // period would report `scope is locked` to every contender for its whole length.
        Some(false) => Some(body),
        // No readable pid: only age can decide, and it has to be beyond any transaction.
        None => is_old(path).then_some(body),
    }
}

/// Whether this lock names this very process but is not held by it.
///
/// `process_alive` answers for a pid, not for a lock. After a delete that failed, the body still
/// names a live process — us — and would block the scope until this process exits, with no
/// recovery path at all; the holder table is what tells that leftover from a lock another thread
/// of this process is really holding.
fn is_orphan_of_this_process(path: &Path, body: &str) -> bool {
    match body
        .split_whitespace()
        .next()
        .and_then(|pid| pid.parse::<u32>().ok())
    {
        Some(pid) if pid == std::process::id() => !registered(path, body),
        _ => false,
    }
}

/// Whether the process that wrote this lock body still runs; `None` when the pid is unreadable.
fn holder_alive(body: &str) -> Option<bool> {
    let pid: u32 = body.split_whitespace().next()?.parse().ok()?;
    Some(process_alive(pid))
}

/// Whether a lock nobody can attribute is old enough to be considered abandoned.
fn is_old(path: &Path) -> bool {
    match fs::metadata(path).and_then(|meta| meta.modified()) {
        Ok(modified) => modified
            .elapsed()
            .map(|elapsed| elapsed > LOCK_STALE_UNKNOWN)
            .unwrap_or(false),
        // The file is gone already: the next `create_new` decides who gets the scope.
        Err(_) => true,
    }
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
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

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
    fn a_lock_this_process_holds_is_not_stale() {
        let path = lock_file("live");
        // Taking the lock registers it: a registered lock of this process is a live owner, not an
        // orphan, however much the file looks like one.
        let lock = acquire_lock(&path).unwrap();
        assert!(!is_stale(&path));
        drop(lock);
        assert!(!path.exists(), "a normal release removes the lock");
        cleanup(&path);
    }

    #[test]
    fn a_lock_naming_this_process_that_nobody_holds_is_an_orphan() {
        // The field symptom: a `remove_file` that failed leaves the lock naming a live pid — this
        // one — with nothing behind it. `process_alive` alone would keep the scope unusable for
        // the whole life of the process, `LOCK_TIMEOUT` per transaction.
        let path = lock_file("orphan");
        fs::write(&path, format!("{} 1", std::process::id())).unwrap();
        assert!(
            is_stale(&path),
            "a lock of our pid that we do not hold is a leftover"
        );

        let started = Instant::now();
        let lock = acquire_lock(&path).expect("an orphan of this process is reclaimed at once");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "an orphan must not be waited for: {:?}",
            started.elapsed()
        );
        drop(lock);
        cleanup(&path);
    }

    #[test]
    fn a_lock_left_behind_as_a_tombstone_is_reclaimed() {
        // What `Drop` writes when the delete fails: a body whose owner is provably gone, so the
        // next contender reclaims it instead of waiting out a grace period.
        let path = lock_file("tombstone");
        fs::write(&path, LOCK_TOMBSTONE).unwrap();
        assert_eq!(LOCK_TOMBSTONE, format!("{DEAD_PID} 0"));
        assert!(is_stale(&path));

        let started = Instant::now();
        let lock = acquire_lock(&path).unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(lock);
        cleanup(&path);
    }

    #[test]
    fn a_holder_that_lost_its_lock_cannot_commit() {
        // A reclaim that could not put a live body back leaves its holder owning a lock file that
        // is no longer there. The holder has to notice before it writes the scope, not after.
        let path = lock_file("verify");
        let lock = acquire_lock(&path).unwrap();
        fs::write(&path, format!("{DEAD_PID} 0")).unwrap();
        let refused = lock.verify().is_err();
        drop(lock);
        assert!(
            refused,
            "a holder whose lock was taken away must be refused"
        );
        cleanup(&path);
    }

    /// Ages a file, so the mtime rule can be exercised without waiting for it.
    fn make_old(path: &Path, age: Duration) {
        let modified = std::time::SystemTime::now() - age;
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    }

    #[test]
    fn a_lock_of_a_live_owner_is_never_reclaimed_by_age() {
        // A transaction that rewrites a tens-of-megabytes scope outlives any grace period a clock
        // could give it. Taking the lock away from a live owner is a lost update, so age must not
        // decide while the owner can be identified.
        let path = lock_file("live-old");
        let lock = acquire_lock(&path).unwrap();
        make_old(&path, Duration::from_secs(3600));
        assert!(
            !is_stale(&path),
            "a live owner keeps its lock however old the file is"
        );
        drop(lock);
        cleanup(&path);
    }

    #[test]
    fn a_lock_without_a_readable_pid_is_reclaimed_only_after_the_large_grace() {
        let path = lock_file("garbage-old");
        fs::write(&path, "not-a-pid").unwrap();
        make_old(&path, Duration::from_secs(3600));
        assert!(
            is_stale(&path),
            "an unidentifiable lock is reclaimed once it is far older than any transaction"
        );
        cleanup(&path);
    }

    #[test]
    fn a_lock_without_a_readable_pid_and_a_fresh_mtime_is_not_stale() {
        // No pid to ask, and the file was just written: nothing says the owner is gone, so the
        // lock stands until it is far older than any transaction.
        let path = lock_file("garbage-fresh");
        fs::write(&path, "not-a-pid").unwrap();
        assert!(
            !is_stale(&path),
            "an unidentifiable lock is not stale while it is young"
        );
        cleanup(&path);
    }

    #[test]
    fn a_lock_held_by_a_thread_of_this_process_is_waited_for() {
        // A lock another thread of this process holds must be waited for — reclaiming it would put
        // two writers on one scope. What makes it live is the holder table (the pid alone cannot
        // tell it from an orphan), and the wait uses the real retry interval.
        let path = lock_file("same-process");
        let lock = acquire_lock(&path).unwrap();
        make_old(&path, Duration::from_secs(3600));
        let holder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(lock);
        });
        let started = Instant::now();
        let second = acquire_lock(&path).expect("the release must be waited for");
        holder.join().unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "a lock of a live holder must not be reclaimed early: {:?}",
            started.elapsed()
        );
        drop(second);
        cleanup(&path);
    }

    #[test]
    fn a_late_reclaim_never_deletes_the_lock_a_winner_created() {
        let path = lock_file("late-reclaim");
        fs::write(&path, format!("{DEAD_PID} 0")).unwrap();
        let observed = stale_body(&path).expect("a lock of a dead owner is reclaimable");
        // The winner already reclaimed it and created its own lock while the loser still held the
        // stale body — the interleaving two contenders produce. Only the verification inside
        // `reclaim_stale` keeps the winner's lock alive.
        let winner = format!("{} 42", std::process::id());
        fs::write(&path, &winner).unwrap();
        reclaim_stale(&path, &observed);
        assert_eq!(
            fs::read_to_string(&path).ok(),
            Some(winner),
            "a late reclaim must not delete the lock the winner created"
        );
        cleanup(&path);
    }

    #[test]
    fn only_one_contender_reclaims_a_stale_lock() {
        // Two contenders that both find the lock stale must not both "reclaim" it: the second one
        // would delete the fresh lock the first created and both would write the scope.
        let path = lock_file("contended");
        fs::write(&path, format!("{DEAD_PID} 0")).unwrap();
        let holding = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let errors = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..6 {
            let path = path.clone();
            let holding = Arc::clone(&holding);
            let peak = Arc::clone(&peak);
            let errors = Arc::clone(&errors);
            handles.push(std::thread::spawn(move || {
                for _ in 0..5 {
                    match acquire_lock(&path) {
                        Ok(lock) => {
                            let now = holding.fetch_add(1, Ordering::SeqCst) + 1;
                            peak.fetch_max(now, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(2));
                            holding.fetch_sub(1, Ordering::SeqCst);
                            drop(lock);
                        }
                        Err(_) => {
                            errors.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(errors.load(Ordering::SeqCst), 0, "no contender gives up");
        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "the scope lock is exclusive even while stale locks are reclaimed"
        );
        cleanup(&path);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use serde_json::json;

    #[test]
    fn only_the_windows_sharing_answers_are_retried() {
        // 5 = ERROR_ACCESS_DENIED (also a target held without FILE_SHARE_DELETE),
        // 32 = ERROR_SHARING_VIOLATION.
        assert!(is_transient_hold(&std::io::Error::from_raw_os_error(5)));
        assert!(is_transient_hold(&std::io::Error::from_raw_os_error(32)));
        // A full disk (112), a missing path (2) and a read-only directory (19) do not clear on
        // their own: retrying them would only delay the error.
        for code in [2, 13, 19, 112] {
            assert!(!is_transient_hold(&std::io::Error::from_raw_os_error(code)));
        }
    }

    #[test]
    fn an_error_that_is_not_a_sharing_answer_is_raised_at_once() {
        let mut attempts = 0;
        let error = retry_while_held(|| -> std::io::Result<()> {
            attempts += 1;
            Err(std::io::Error::from_raw_os_error(13))
        })
        .unwrap_err();
        assert_eq!(attempts, 1, "only 5 and 32 are retried");
        assert_eq!(error.raw_os_error(), Some(13));
    }

    #[test]
    fn a_held_target_is_retried_but_a_hard_error_is_not() {
        let mut attempts = 0;
        let recovered = retry_while_held(|| {
            attempts += 1;
            if attempts < 3 {
                Err(std::io::Error::from_raw_os_error(5))
            } else {
                Ok(attempts)
            }
        });
        assert_eq!(
            recovered.unwrap(),
            3,
            "the write must survive a held handle"
        );

        let mut attempts = 0;
        let error = retry_while_held(|| -> std::io::Result<()> {
            attempts += 1;
            Err(std::io::Error::from_raw_os_error(19))
        })
        .unwrap_err();
        assert_eq!(attempts, 1, "a read-only directory must fail at once");
        assert_eq!(error.raw_os_error(), Some(19));
    }

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

    // -----------------------------------------------------------------------
    // Several hosts on one root
    // -----------------------------------------------------------------------

    /// How long a concurrent test may run before it is reported as a deadlock.
    const CONCURRENCY_TIMEOUT: Duration = Duration::from_secs(120);

    /// Runs `worker` on `threads` threads and fails the test instead of hanging forever.
    ///
    /// A store that never returns the scope lock is a deadlock, not a slow test, so the deadline
    /// is explicit: a hung run reports it instead of blocking the suite.
    fn run_concurrently(
        threads: usize,
        worker: impl Fn(usize) -> std::result::Result<(), String> + Send + Sync + 'static,
    ) {
        let worker = Arc::new(worker);
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut handles = Vec::new();
        for thread in 0..threads {
            let worker = Arc::clone(&worker);
            let sender = sender.clone();
            handles.push(std::thread::spawn(move || {
                let _ = sender.send(worker(thread));
            }));
        }
        drop(sender);
        let deadline = Instant::now() + CONCURRENCY_TIMEOUT;
        let mut done = 0;
        while done < threads {
            let left = deadline.saturating_duration_since(Instant::now());
            match receiver.recv_timeout(left) {
                Ok(Ok(())) => done += 1,
                Ok(Err(message)) => panic!("{message}"),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    panic!("deadlock: no progress in {CONCURRENCY_TIMEOUT:?}")
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("a worker panicked before reporting")
                }
            }
        }
        for handle in handles {
            handle.join().expect("worker thread");
        }
    }

    #[test]
    fn two_hosts_on_one_root_never_lose_an_update() -> Result<()> {
        // Two stores, one root: exactly what two agent processes on one workspace look like, and
        // the case the per-scope lock exists for. Every host increments the same counter through
        // a read-modify-write inside one transaction, so a lock that admits both writers at once
        // loses increments.
        let root = TempRoot::new("two-hosts");
        let hosts: Vec<Arc<FileStore>> = (0..2)
            .map(|_| FileStore::open(root.path()).map(Arc::new))
            .collect::<Result<Vec<_>>>()?;
        const THREADS: usize = 4;
        const ROUNDS: usize = 50;
        run_concurrently(THREADS, move |thread| {
            let store = Arc::clone(&hosts[thread % hosts.len()]);
            for round in 0..ROUNDS {
                store
                    .transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
                        let current = txn
                            .get("counter")?
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0);
                        txn.put("counter", Some(json!(current + 1)))?;
                        txn.put(
                            &format!("host{}-round{round}", thread % 2),
                            Some(json!(thread)),
                        )
                    })
                    .map_err(|error| format!("host {thread} round {round}: {error}"))?;
            }
            Ok(())
        });

        let reader = FileStore::open(root.path())?;
        let counter = read_value(&reader, "session", "counter")?;
        assert_eq!(
            counter,
            Some(json!((THREADS * ROUNDS) as u64)),
            "every increment survived: no write was lost"
        );
        // Both hosts of every round are present: no host's document replaced the other's.
        for host in 0..2 {
            (0..ROUNDS).for_each(|round| {
                assert!(
                    read_value(&reader, "session", &format!("host{host}-round{round}"))
                        .expect("read")
                        .is_some(),
                    "host {host} round {round} is missing"
                );
            });
        }
        Ok(())
    }

    #[test]
    fn two_hosts_scheduling_the_same_work_item_keep_a_valid_record() -> Result<()> {
        // The readiness index lives outside every scope, so two hosts on two scopes can schedule
        // the same work id at the same time. The record is replaced atomically, so whichever
        // scheduling wins the last write, the index stays readable and consistent.
        let root = TempRoot::new("work-index");
        let hosts: Vec<Arc<FileStore>> = (0..2)
            .map(|_| FileStore::open(root.path()).map(Arc::new))
            .collect::<Result<Vec<_>>>()?;
        run_concurrently(4, move |thread| {
            let store = Arc::clone(&hosts[thread % hosts.len()]);
            for round in 0..25u64 {
                let scope = if thread % 2 == 0 {
                    "scope-a"
                } else {
                    "scope-b"
                };
                store
                    .transaction(scope, &mut |txn: &mut dyn Txn| -> Result<()> {
                        txn.schedule(
                            "queue",
                            "shared",
                            round as i64 + 1,
                            Some(json!({ "by": thread })),
                        )?;
                        txn.schedule("queue", &format!("own-{thread}-{round}"), 1, None)
                    })
                    .map_err(|error| format!("thread {thread} round {round}: {error}"))?;
            }
            Ok(())
        });

        let reader = FileStore::open(root.path())?;
        let due = reader.ready("queue", i64::MAX, 1000)?;
        assert_eq!(
            due.iter().filter(|item| item.work_id == "shared").count(),
            1,
            "the shared item is one record, not a duplicate"
        );
        // Every host's own item landed: the index lock does not drop a neighbour's write.
        for thread in 0..4 {
            for round in 0..25 {
                assert!(
                    due.iter()
                        .any(|item| item.work_id == format!("own-{thread}-{round}")),
                    "own-{thread}-{round} is missing"
                );
            }
        }
        Ok(())
    }
}
