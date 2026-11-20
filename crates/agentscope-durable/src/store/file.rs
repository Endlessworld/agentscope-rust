//! Single-host file store: one JSON document per scope plus one per work item.
//!
//! # Large values
//!
//! A scope is one document that every commit rewrites, so a value kept inline is written again on
//! every later commit of that session: a 27 MB conversation state turned each journal batch into
//! 27 MB of disk traffic. A payload above [`BLOB_THRESHOLD`] is therefore written once into
//! `blobs/<scope>/<digest>.json` and the document keeps a small reference to it, which bounds a
//! commit by the number of *facts* in the session instead of by the bytes those facts carry.
//!
//! The reference is content addressed — the file name is the SHA-256 of the serialized payload —
//! so a payload written twice (the same checkpoint state at every step of a turn, two identical
//! tool results) lands once, and a commit that changes nothing else rewrites nothing. Blob writes
//! are temp-then-rename like every other file, so two hosts storing the same digest produce one
//! complete file and never a partial one.
//!
//! Atomicity is unchanged: blobs are written first, the document second. A commit that fails after
//! its blobs landed leaves files nothing references, which cost disk until
//! [`FileStore::collect_blobs`] sweeps them and can never be read by mistake, because a reader
//! only follows a reference out of a committed document. A value a caller reads back is always the
//! logical payload: the reference never escapes `Txn::get`, so digests, event ids and execution
//! ids computed from stored values do not move.
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
//! (see [`FileStore`]). How that file is used depends on [`LockMode`]: on a local disk the lock is
//! the *kernel's* — the file is created once, never written, and taken with an OS file lock, so a
//! transaction costs two calls on an open handle and leaves the file system untouched (a process
//! that dies holding it blocks nobody, because the kernel releases it with the handle). On a
//! network share, where a client may emulate `flock`/`LockFileEx` and quietly stop excluding, the
//! conservative mode keeps the create-if-absent lock: the file's existence is the lock, which a
//! share always serialises, at the price of a create, a write and a delete per transaction.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{RecordStore, Txn, WorkItem};
use crate::{DurableError, Result};

/// One scope document as the exact JSON text of each record, not a parsed value tree.
///
/// A rewrite needs every record to keep, but none of them to be interpreted: keeping their bytes
/// costs about the document's size, while a `Value` tree of the same document costs several times
/// it and is what made a turn's peak grow with the journal behind it.
type RawDocument = BTreeMap<String, Box<RawValue>>;

// How long a contender waits for the scope lock before giving up.
//
// It is a contention bound, not a stale-lock grace: whether a lock is abandoned is decided by the
// owner's liveness, so the wait only has to cover transactions that really run. Every write
// rewrites the whole scope, which for a long session is tens of megabytes, so it has to be
// generous.
const LOCK_TIMEOUT: Duration = Duration::from_secs(60);
const LOCK_RETRY: Duration = Duration::from_millis(10);

// How often a contender retries the operating-system lock.
//
// The wait is a syscall against an already open handle, not a create, so it can afford to be far
// tighter than the file mode's: a transaction that waits a millisecond longer than it must is the
// only thing this costs, and the backoff keeps a long rewrite from being hammered with retries.
const LOCK_OS_RETRY: Duration = Duration::from_millis(1);
const LOCK_OS_RETRY_MAX: Duration = Duration::from_millis(20);

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

/// How large a stored value may be before it is kept out of line.
///
/// The document is the unit every commit rewrites, so the threshold is the point where keeping a
/// fact inline starts costing more than the extra file it would take to store it apart. One model
/// message, one tool result and certainly one conversation checkpoint are far above this — a
/// checkpoint payload is hundreds of kilobytes to megabytes — while every control-plane record
/// (a turn, a receipt, a lease, an execution) is a few hundred bytes. 16 KiB is therefore above
/// every record whose shape a scope is *about* and below every payload whose bytes dominate it,
/// so a scope stays a document of records and a directory of payloads instead of a mix.
const BLOB_THRESHOLD: usize = 16 * 1024;

/// The single key that marks a stored value as a reference to a blob.
const BLOB_MARKER: &str = "$blob";

/// The only payload representation this store writes. Named so a future one (a compressed body,
/// say) is a different `kind` rather than a guess about the bytes.
const BLOB_KIND: &str = "json";

/// How long a blob that no committed document references is left alone before a sweep deletes it.
///
/// It closes the one window the design cannot: a reader parses a document, then loads the blobs it
/// names, and a commit replacing that document in between would otherwise be able to delete a
/// payload out from under it. A reader that opened the document before the removing commit is
/// done with it far inside this grace period, and an in-flight *writer* is covered too because a
/// blob it just wrote is younger than the grace.
const BLOB_GC_GRACE: Duration = Duration::from_secs(10 * 60);

/// This process's handle on one lock file, plus the gate that serialises its own writers.
///
/// The gate is what keeps two tasks of this process from fighting through the file system: one of
/// them rewriting a multi-megabyte scope while the other created and deleted the lock file every
/// ten milliseconds. A cross-process lock alone would not stop that — it cannot tell one of our
/// threads from a stranger's process. With the gate every in-process waiter sleeps in memory and
/// the file is only ever touched by the one writer holding it.
struct ScopeGate {
    gate: Mutex<()>,
    /// The lock file of [`LockMode::Os`]; opened once and kept for the life of the store.
    /// Nothing in [`LockMode::Conservative`] needs it, and that mode must not keep the file open:
    /// its lock *is* the file's existence, which it creates and deletes per transaction.
    file: Option<File>,
}

/// The scope lock of one transaction: the in-process gate plus the cross-process lock.
struct HeldLock<'a> {
    /// The OS lock, released before the gate so the next thread of this process is admitted last.
    os: Option<&'a File>,
    /// The create-if-absent lock of [`LockMode::Conservative`].
    file: Option<ScopeLock>,
    /// Held for the whole transaction and dropped last.
    _gate: MutexGuard<'a, ()>,
}

impl HeldLock<'_> {
    /// Whether this transaction still owns the lock it took.
    fn verify(&self) -> Result<()> {
        match &self.file {
            Some(lock) => lock.verify(),
            // An OS lock cannot be taken away from its holder: the kernel releases it when the
            // handle closes, and this transaction is the only one that can close it while it runs.
            None => Ok(()),
        }
    }
}

impl Drop for HeldLock<'_> {
    fn drop(&mut self) {
        if let Some(file) = self.os.take() {
            // Best effort, like every other unlock: a failed unlock only leaves the lock to the
            // kernel when the store (and so the handle) goes away.
            let _ = file.unlock();
        }
    }
}

/// The lock path of the readiness index, or of any other store-wide lock.
const WORK_INDEX_LOCK: &str = "\u{1}work-index";

/// How big a document must be for the legacy roll-out pass to be worth its walk.
///
/// The pass serializes every value of the document once to find the ones still inline. Below this
/// size a rewrite is cheap enough that moving a few kilobytes out of line is not worth a pass;
/// above it the pass is paid for once and the document cannot come back, because every new
/// payload is stored out of line.
const ROLL_OUT_BYTES: u64 = 1 << 20;

/// How large a scope's append-only tail may grow before it is folded into the document.
///
/// Every commit since the last fold lands in the tail as one line, so a session that writes a few
/// small deltas per tick rewrites the document once every few ticks instead of once per tick. The
/// fold is the old whole-document write, so this bound sets how often it is paid.
const LOG_COMPACT_BYTES: u64 = 256 * 1024;

/// The on-disk layout version this build reads and writes.
///
/// A root names its version in `format.json`. The check is one file read at open: a root written
/// by a newer layout is refused before any transaction touches it, instead of being misread.
/// This build writes no file: version 1 is the layout every existing root already has.
const STORE_FORMAT_VERSION: u64 = 1;

/// How many blobs this process keeps parsed, and how many bytes they may hold.
const BLOB_CACHE_ENTRIES: usize = 16;
const BLOB_CACHE_BYTES: usize = 4 << 20;

/// How many scopes remember that their legacy payloads were already moved out of line.
const ROLLED_OUT_SCOPES: usize = 64;

/// How the writers of one scope exclude each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// An operating-system lock on a long-lived, empty lock file.
    ///
    /// The holder is named by the kernel instead of by the file's contents, so the file is created
    /// once per scope and then never written, renamed or deleted: a transaction costs two calls on
    /// an already open handle and leaves nothing behind on the file system. Because the kernel
    /// releases the lock when the handle closes, a process that dies holding one blocks nobody —
    /// so there is no stale body to judge and no tombstone to write.
    ///
    /// This is what a local disk wants. See [`LockMode::Conservative`] for a root on a share.
    Os,
    /// A lock file whose *existence* is the lock, named after the process holding it.
    ///
    /// A network share may emulate `flock`/`LockFileEx` in the client and quietly stop excluding,
    /// while creating a name is the one operation a share always serialises; this mode keeps that
    /// route. It costs a create, a write, a verify read and a delete per transaction, and a holder
    /// that dies leaves a body behind that the next contender has to judge by pid and age.
    Conservative,
}

/// Scope maps and the readiness index backed by plain files under one directory.
///
/// Any number of hosts (processes) may open the same root at the same time: exclusion is per
/// scope through `locks/*.lock`, and every document is replaced atomically, so readers always see
/// a complete previous or complete next document.
pub struct FileStore {
    root: PathBuf,
    /// How this store takes the cross-process lock; see [`LockMode`].
    mode: LockMode,
    /// One gate per lock path this process uses, so its own writers queue in memory.
    locks: Mutex<HashMap<String, Arc<ScopeGate>>>,
    /// Scope documents this process last answered a [`RecordStore::read_parts`] from.
    ///
    /// Only that path uses it; see [`Picked`].
    picked: Mutex<HashMap<String, Picked>>,
    /// Out-of-line payloads this process read last; see [`BlobCache`].
    blobs: Mutex<BlobCache>,
    /// Whole scope documents this process last served a read-only transaction from.
    ///
    /// [`Picked`] caches the values a `read_parts` asked for; this caches the bytes a
    /// [`RecordStore::read_transaction`] parsed, because a journal fold reads the same 1.4MB
    /// document several times per tick. An entry is valid only while the file's identity is
    /// unchanged — a write is a rename of a fresh temporary file, so it always changes — and a
    /// write transaction never reads through it.
    documents: Mutex<HashMap<String, CachedDocument>>,
    /// Parsed tails this process last read, keyed by scope; each valid while its log is unchanged.
    overlays: Mutex<HashMap<String, CachedOverlay>>,
    /// Scopes whose legacy inline payloads this process already moved out of line.
    rolled_out: Mutex<HashSet<String>>,
    /// Whole-document scans this store answered a [`RecordStore::read_parts`] from.
    ///
    /// Per store and only under `cfg(test)`. [`crate::traffic::read_stats`] is process wide by
    /// design (a whole tick is what it measures), so a unit test that asserts an exact pick count
    /// on it also counts the tests running beside it in the same binary — and one of those resets
    /// the counter. This is the same number without the neighbours.
    #[cfg(test)]
    scans: AtomicU64,
    /// Document reads a transaction performed; only under `cfg(test)`.
    ///
    /// The counterpart of [`FileStore::scans`] for [`RecordStore::read_transaction`]: a test can
    /// say "the second read-only transaction did not read the file" without watching the clock.
    #[cfg(test)]
    document_reads: AtomicU64,
    /// Records a transaction read through a [`ScopeIndex`] instead of the whole document.
    ///
    /// Only under `cfg(test)`, and the negative space of [`FileStore::document_reads`]: a test can
    /// say "the record was read and the document was not".
    #[cfg(test)]
    range_reads: AtomicU64,
}

/// The out-of-line payloads this process read last.
///
/// Content addressing makes the digest the identity of the payload, so an entry never needs
/// invalidating: a blob is immutable for the life of its digest, and a rewrite of the same payload
/// writes the same bytes. A journal fold reads the same checkpoint payload on every tick, which is
/// the repetition this avoids; the two bounds are what keep it from becoming a second copy of the
/// session.
#[derive(Default)]
struct BlobCache {
    entries: HashMap<String, Value>,
    total: usize,
}

/// A `read_parts` answer kept until the document behind it is replaced.
///
/// A control poll asks the same question several times a second against a document that is
/// rewritten whole, and scanning the bytes is three orders of magnitude more expensive than
/// comparing the file's identity. A write is a fresh temporary file renamed over the old one, so
/// `(len, modified, created)` changes on every write, and serving a stale answer would need two
/// writes with the same length and both timestamps. Nothing but the control poll reads through
/// this cache: a transaction always parses the document it commits.
struct Picked {
    stamp: (
        u64,
        Option<std::time::SystemTime>,
        Option<std::time::SystemTime>,
    ),
    /// The tail the answer was taken against, so an appended commit invalidates it.
    log_stamp: Option<crate::store::DocumentStamp>,
    parts: Vec<String>,
    values: Vec<Option<Value>>,
}

/// How many scopes keep a picked answer before the cache is cleared.
///
/// The entries are a few hundred bytes each and nothing depends on them: a miss only means the
/// next poll scans the document, which is the behaviour without the cache.
const PICKED_SCOPES: usize = 8;

/// A whole document kept for the next read-only transaction on the same scope.
struct CachedDocument {
    stamp: crate::store::DocumentStamp,
    bytes: Arc<Vec<u8>>,
}

/// A parsed tail kept until the log behind it changes.
///
/// A journal fold and a control poll both read the same scope many times per tick, and the tail is
/// the only part that changed since the last read. Parsing it per transaction would charge the
/// conversation for a few kilobytes of deltas that mostly repeat; the log's identity changes on
/// every append, so an entry is valid exactly while nothing was written.
struct CachedOverlay {
    log_stamp: Option<crate::store::DocumentStamp>,
    overlay: Arc<Overlay>,
}

/// How many scope documents the read-only cache holds before clearing.
///
/// A bound, not a policy: a session document is about a megabyte and a process works on one
/// session at a time, so the entry that matters is one; the rest is room for a handful of
/// scopes without ever becoming a second copy of a long session.
const DOCUMENT_SCOPES: usize = 4;

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

/// Whether the file system under `root` implements `flock`.
///
/// `LockMode::Os` takes the lock from the kernel, which a FUSE mount (an Android `/sdcard`, an
/// `sshfs` mount) may refuse with `ENOSYS`; a store opened there would fail every transaction
/// with "Function not implemented". The probe costs one create, one lock attempt and one delete
/// when the store opens, and it answers for the directory the store will write in.
fn os_lock_supported(root: &Path) -> bool {
    let path = root
        .join("locks")
        .join(format!(".flock-probe-{}", std::process::id()));
    let file = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
    {
        Ok(file) => file,
        // A lock directory that cannot even be opened is not this probe's answer to give:
        // opening the root already reported it when it created the directories.
        Err(_) => return true,
    };
    let supported = match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            true
        }
        // Another host holds it right now: this file system does implement locking.
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(_)) => false,
    };
    drop(file);
    let _ = retry_while_held(|| fs::remove_file(&path));
    supported
}

impl FileStore {
    /// Opens a root for a local disk: writers exclude each other with an OS lock.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_lock_mode(root, LockMode::Os)
    }

    /// Opens a root on a network share, where the OS lock cannot be trusted.
    pub fn open_conservative(root: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_lock_mode(root, LockMode::Conservative)
    }

    /// Opens a root with an explicit cross-process lock; see [`LockMode`].
    pub fn open_with_lock_mode(root: impl AsRef<Path>, mode: LockMode) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        for dir in ["scopes", "work", "locks", "blobs"] {
            // Creating a directory two hosts need at the same moment answers "already exists",
            // which is success; a transient hold on the parent is retried like any other.
            retry_while_held(|| fs::create_dir_all(root.join(dir)))?;
        }
        // One canonical root when the file system offers one: the lock table is keyed by lock
        // path, and two stores of this process opened on the same directory under different
        // spellings (relative, absolute, a `..` in the middle) would otherwise answer two gates
        // for one lock file. The fallback keeps a store usable where canonicalising is refused.
        let root = fs::canonicalize(&root).unwrap_or(root);
        // A file system may answer `ENOSYS` to `flock` (an Android `/sdcard` FUSE mount, some
        // network shares). An OS lock is then not takeable at all, and every transaction would
        // fail with "Function not implemented"; the create-if-absent lock needs nothing the file
        // system refuses, so the store falls back to it instead of being unusable there.
        let mode = match mode {
            LockMode::Os if !os_lock_supported(&root) => LockMode::Conservative,
            mode => mode,
        };
        let store = Self {
            root,
            mode,
            locks: Mutex::new(HashMap::new()),
            picked: Mutex::new(HashMap::new()),
            blobs: Mutex::new(BlobCache::default()),
            documents: Mutex::new(HashMap::new()),
            overlays: Mutex::new(HashMap::new()),
            rolled_out: Mutex::new(HashSet::new()),
            #[cfg(test)]
            scans: AtomicU64::new(0),
            #[cfg(test)]
            document_reads: AtomicU64::new(0),
            #[cfg(test)]
            range_reads: AtomicU64::new(0),
        };
        store.check_format()?;
        store.adopt_flat_work_records()?;
        Ok(store)
    }

    /// Refuses a root this build cannot read, before a transaction touches it.
    ///
    /// A root with no `format.json` is this build's own layout: the file is written only by a
    /// layout that has something to declare, so an existing root keeps opening.
    fn check_format(&self) -> Result<()> {
        let path = self.root.join("format.json");
        let bytes = match retry_while_held(|| fs::read(&path)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let format: StoreFormat = serde_json::from_slice(&bytes).map_err(|error| {
            DurableError::invalid(format!("store format is unreadable: {error}"))
        })?;
        if format.version != STORE_FORMAT_VERSION {
            return Err(DurableError::invalid(format!(
                "store format {} is not readable by this build (it reads {STORE_FORMAT_VERSION})",
                format.version
            )));
        }
        Ok(())
    }

    /// This process's gate and handle for one lock path.
    fn gate(&self, key: &str, path: &Path) -> Result<Arc<ScopeGate>> {
        let mut locks = self
            .locks
            .lock()
            .map_err(|_| DurableError::unavailable("lock table poisoned"))?;
        if let Some(gate) = locks.get(key) {
            return Ok(Arc::clone(gate));
        }
        let file = match self.mode {
            LockMode::Os => Some(self.open_lock_file(path)?),
            LockMode::Conservative => None,
        };
        let gate = Arc::new(ScopeGate {
            gate: Mutex::new(()),
            file,
        });
        locks.insert(key.to_string(), Arc::clone(&gate));
        Ok(gate)
    }

    /// The long-lived lock file of [`LockMode::Os`].
    ///
    /// `create(true)` and not `create_new`: the file is the *name* of a lock, not the lock, so an
    /// existing one is reused and an empty one left behind costs the next transaction nothing. It
    /// is opened for writing because Windows refuses to lock a handle opened for append only, and
    /// it is never truncated or written.
    fn open_lock_file(&self, path: &Path) -> Result<File> {
        let file = retry_while_held(|| {
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
        })?;
        Ok(file)
    }

    /// Takes the scope lock: first the in-process gate, then whichever cross-process lock the mode
    /// uses.
    fn hold<'a>(&self, gate: &'a ScopeGate, path: &Path) -> Result<HeldLock<'a>> {
        // Same-process writers queue here and never touch the file system while they wait.
        let in_process = gate
            .gate
            .lock()
            .map_err(|_| DurableError::unavailable("scope gate poisoned"))?;
        let Some(file) = gate.file.as_ref() else {
            let lock = acquire_lock(path)?;
            return Ok(HeldLock {
                os: None,
                file: Some(lock),
                _gate: in_process,
            });
        };
        let deadline = Instant::now() + LOCK_TIMEOUT;
        let mut backoff = LOCK_OS_RETRY;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err(DurableError::unavailable(
                            "scope is locked; another agent is using this workspace",
                        ));
                    }
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(LOCK_OS_RETRY_MAX);
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        Ok(HeldLock {
            os: Some(file),
            file: None,
            _gate: in_process,
        })
    }

    fn scope_path(&self, scope: &str) -> PathBuf {
        self.root
            .join("scopes")
            .join(format!("{}.json", digest(scope)))
    }

    /// The commits appended since the document was last folded, one JSON line each.
    fn log_path(&self, scope: &str) -> PathBuf {
        self.scope_path(scope).with_extension("log")
    }

    fn work_path(&self, queue: &str, work_id: &str) -> PathBuf {
        self.work_dir(queue)
            .join(format!("{}.json", digest(work_id)))
    }

    /// One directory per queue, so `ready` reads the threads of one queue.
    ///
    /// The readiness index used to be one flat directory keyed by `(queue, work id)`, which meant
    /// every `ready(queue, ..)` had to read and parse *every* work record in the root before it
    /// could throw away the ones belonging to another queue: a tick cost the whole deployment's
    /// thread count. Sharding makes that scan the size of the queue it asks about.
    fn work_dir(&self, queue: &str) -> PathBuf {
        self.root.join("work").join(digest(queue))
    }

    /// Adopts the work records a pre-shard build left in the flat directory.
    ///
    /// A record nobody rewrites is a thread nothing schedules, so adopting them is what keeps an
    /// existing root usable. It runs from [`FileStore::open`] because that is the single point
    /// every process passes through before it reads the index, it is idempotent (the record is
    /// copied, then the old name dropped), and two hosts doing it at once write the same bytes.
    /// A root that has already been adopted costs one directory listing.
    fn adopt_flat_work_records(&self) -> Result<()> {
        let flat = self.root.join("work");
        let entries = match retry_while_held(|| fs::read_dir(&flat)) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
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
            if record.ready_at >= 0 {
                let dir = self.work_dir(&record.queue);
                retry_while_held(|| fs::create_dir_all(&dir))?;
                write_json(&self.work_path(&record.queue, &record.work_id), &record)?;
            }
            match retry_while_held(|| fs::remove_file(&path)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
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

    // ------------------------------------------------------------------------
    // Out-of-line payloads
    // ------------------------------------------------------------------------

    /// One directory per scope, so a sweep only ever has to look at the blobs a single document
    /// can reference. Content addressing across scopes was the alternative and is not worth it:
    /// two sessions sharing a byte-identical payload is rare, while deciding whether a shared file
    /// is still referenced would mean walking every scope of the root — or keeping a reference
    /// count that every commit would have to update.
    fn blob_dir(&self, scope: &str) -> PathBuf {
        self.root.join("blobs").join(digest(scope))
    }

    fn blob_path(&self, scope: &str, blob: &str) -> PathBuf {
        self.blob_dir(scope).join(format!("{blob}.json"))
    }

    /// Writes `bytes` as the blob they digest to and answers the reference that replaces them.
    ///
    /// The file is checked before it is written: the name *is* the content, so an existing file of
    /// the same length is the same payload, and rewriting it would cost exactly the bytes this
    /// mechanism exists to save. Two hosts reaching this point for one digest write the same bytes
    /// through temp-then-rename, so the result is one complete file either way.
    fn store_blob(&self, scope: &str, bytes: &[u8]) -> Result<Value> {
        let blob = sha256_hex(bytes);
        let path = self.blob_path(scope, &blob);
        let present = match retry_while_held(|| fs::metadata(&path)) {
            Ok(meta) => meta.len() == bytes.len() as u64,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        if !present {
            retry_while_held(|| fs::create_dir_all(self.blob_dir(scope)))?;
            write_atomic(&path, bytes)?;
        }
        Ok(blob_reference(&blob, bytes.len()))
    }

    /// The value a reference stands for, from the cache or from its file.
    fn load_blob(&self, scope: &str, blob: &str) -> Result<Value> {
        if let Some(value) = self.cached_blob(blob) {
            return Ok(value);
        }
        let path = self.blob_path(scope, blob);
        let bytes = match retry_while_held(|| fs::read(&path)) {
            Ok(bytes) => bytes,
            // The document names it, so it has to be there: a missing file is a damaged store and
            // must stop the read instead of answering with the reference itself.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(DurableError::invalid(format!(
                    "Referenced blob {blob} is missing"
                )))
            }
            Err(error) => return Err(error.into()),
        };
        let value: Value = serde_json::from_slice(&bytes)?;
        self.keep_blob(blob, bytes.len(), &value);
        Ok(value)
    }

    /// The value of a stored part, with a reference resolved to the payload it names.
    fn resolve(&self, scope: &str, stored: Value) -> Result<Value> {
        match blob_reference_of(&stored) {
            Some(blob) => self.load_blob(scope, blob),
            None => Ok(stored),
        }
    }

    /// The value a `put` stores: out of line when it is large, unchanged otherwise.
    fn externalize(&self, scope: &str, value: Value) -> Result<Value> {
        // A reference is already the compact form; measuring it would only externalize the
        // reference itself.
        if blob_reference_of(&value).is_some() {
            return Ok(value);
        }
        // The size is counted instead of materialized: serializing into a buffer only to write
        // those same bytes into the blob file is one full payload of memory per large `put`.
        let mut counter = CountingSink {
            inner: std::io::sink(),
            written: 0,
        };
        serde_json::to_writer(&mut counter, &value)?;
        if counter.written <= BLOB_THRESHOLD {
            return Ok(value);
        }
        self.store_blob_streamed(scope, &mut |out| serde_json::to_writer(out, &value))
    }

    /// Writes the blob `write` produces while hashing it, and answers the reference.
    ///
    /// The payload never has to exist as one buffer: [`FileStore::externalize`] sends a large
    /// value here so its bytes reach the blob file straight from the serializer.
    fn store_blob_streamed(
        &self,
        scope: &str,
        write: &mut dyn FnMut(&mut dyn std::io::Write) -> serde_json::Result<()>,
    ) -> Result<Value> {
        let dir = self.blob_dir(scope);
        retry_while_held(|| fs::create_dir_all(&dir))?;
        let temp = dir.join(format!(".tmp-{}-{}", std::process::id(), next_sequence()));
        let file = fs::File::create(&temp)?;
        let mut sink = HashingSink::new(std::io::BufWriter::new(file));
        let finished = match write(&mut sink) {
            Ok(()) => sink.finish(),
            Err(error) => Err(std::io::Error::other(error)),
        };
        let (blob, bytes) = match finished {
            Ok(done) => done,
            Err(error) => {
                let _ = fs::remove_file(&temp);
                return Err(error.into());
            }
        };
        let path = self.blob_path(scope, &blob);
        let present = match retry_while_held(|| fs::metadata(&path)) {
            Ok(meta) => meta.len() == bytes as u64,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                let _ = fs::remove_file(&temp);
                return Err(error.into());
            }
        };
        if present {
            let _ = fs::remove_file(&temp);
        } else if let Err(error) = retry_while_held(|| fs::rename(&temp, &path)) {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
        Ok(blob_reference(&blob, bytes))
    }

    fn cached_blob(&self, blob: &str) -> Option<Value> {
        let cache = self.blobs.lock().ok()?;
        cache.entries.get(blob).cloned()
    }

    /// Remembers a parsed payload, dropping the previous contents when either bound is reached.
    ///
    /// Wholesale clearing is enough: a miss only means the next read parses the file again, which
    /// is the behaviour without the cache, and it keeps the bookkeeping to one addition.
    fn keep_blob(&self, blob: &str, bytes: usize, value: &Value) {
        let Ok(mut cache) = self.blobs.lock() else {
            return;
        };
        if bytes > BLOB_CACHE_BYTES {
            return;
        }
        if cache.total + bytes > BLOB_CACHE_BYTES || cache.entries.len() >= BLOB_CACHE_ENTRIES {
            cache.entries.clear();
            cache.total = 0;
        }
        if !cache.entries.contains_key(blob) {
            cache.total += bytes;
        }
        cache.entries.insert(blob.to_string(), value.clone());
    }

    /// Moves the large inline payloads of a document written before blobs existed out of line.
    ///
    /// New writes always externalize, so anything still inline is legacy, and a document only has
    /// legacy payloads until this pass has seen it once. Re-serializing every value to measure it
    /// is a walk over the whole document, which is only worth paying above [`ROLL_OUT_BYTES`] —
    /// below that a rewrite is cheap and the pass would cost more than the bytes it frees. The
    /// memo is per process and per scope: after the pass the document cannot go back to holding a
    /// large payload, so a second pass could not find one.
    fn roll_out(&self, scope: &str, committed: &mut RawDocument) -> Result<()> {
        match self.rolled_out.lock() {
            Ok(rolled) if rolled.contains(scope) => return Ok(()),
            Ok(_) => {}
            Err(_) => return Ok(()),
        }
        for value in committed.values_mut() {
            let raw = value.get();
            if blob_reference_of_raw(raw).is_some() {
                continue;
            }
            if raw.len() > BLOB_THRESHOLD {
                let reference = self.store_blob(scope, raw.as_bytes())?;
                *value = raw_of(&reference)?;
            }
        }
        if let Ok(mut rolled) = self.rolled_out.lock() {
            if rolled.len() >= ROLLED_OUT_SCOPES {
                // The set is tiny and nothing depends on it: forgetting means one more pass over
                // one more document, never a wrong answer.
                rolled.clear();
            }
            rolled.insert(scope.to_string());
        }
        Ok(())
    }

    /// Deletes the blobs no part of `committed` refers to any more.
    ///
    /// Best effort by design: an unreferenced file is only wasted disk, so a listing or a delete
    /// that fails (another host reading it, a scanner holding it) must not fail the commit that
    /// noticed it. It runs only after the document that stopped referencing them was written, and
    /// only for a commit that removed parts — that is the only way a reference can disappear. A
    /// blob younger than [`BLOB_GC_GRACE`] is left alone; see that constant for the reader it
    /// protects.
    fn collect_blobs(&self, scope: &str, committed: &RawDocument) {
        let referenced: HashSet<String> = committed
            .values()
            .filter_map(|value| blob_reference_of_raw(value.get()))
            .collect();
        let entries = match retry_while_held(|| fs::read_dir(self.blob_dir(scope))) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let Some(blob) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if referenced.contains(blob) || !older_than(&path, BLOB_GC_GRACE) {
                continue;
            }
            let _ = retry_while_held(|| fs::remove_file(&path));
        }
    }

    /// The bytes of the append-only tail, `None` when the scope never appended one.
    fn log_bytes(&self, scope: &str) -> Result<Option<Vec<u8>>> {
        match retry_while_held(|| fs::read(self.log_path(scope))) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// How many bytes the tail holds; zero when there is none.
    fn log_size(&self, scope: &str) -> u64 {
        match retry_while_held(|| fs::metadata(self.log_path(scope))) {
            Ok(meta) => meta.len(),
            Err(_) => 0,
        }
    }

    /// Appends one commit as a single line.
    ///
    /// One line is one write, so a process that dies mid-append leaves a tail no reader trusts and
    /// [`FileStore::replay_log`] discards; a complete line is exactly one commit.
    fn append_log(&self, scope: &str, delta: &LogDelta) -> Result<()> {
        let path = self.log_path(scope);
        let mut line = serde_json::to_vec(delta)?;
        line.push(b'\n');
        retry_while_held(|| {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            std::io::Write::write_all(&mut file, &line)
        })?;
        Ok(())
    }

    /// Applies every complete line of the tail to `document`.
    ///
    /// A tail that stops in the middle of a line is a commit that never finished, and is skipped; a
    /// complete line that does not parse is a damaged store and stops the read. Applying a line
    /// twice writes the same document — a put replaces and a remove deletes again — which is what
    /// makes "write the document, then clear the tail" safe in that order.
    fn replay_log(document: &mut RawDocument, log: &[u8]) -> Result<()> {
        let mut rest = log;
        while let Some(end) = rest.iter().position(|byte| *byte == b'\n') {
            let line = &rest[..end];
            rest = &rest[end + 1..];
            if line.is_empty() {
                continue;
            }
            let delta: LogDelta = serde_json::from_slice(line)
                .map_err(|error| DurableError::invalid(format!("scope log is damaged: {error}")))?;
            for (part, value) in delta.puts {
                document.insert(part, raw_of(&value)?);
            }
            for part in delta.removes {
                document.remove(&part);
            }
        }
        Ok(())
    }

    /// The scope document with its tail applied.
    fn merged_document(&self, scope: &str) -> Result<RawDocument> {
        let mut document = match retry_while_held(|| fs::read(self.scope_path(scope))) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => RawDocument::new(),
            Err(error) => return Err(error.into()),
        };
        if let Some(log) = self.log_bytes(scope)? {
            Self::replay_log(&mut document, &log)?;
        }
        Ok(document)
    }

    /// Folds the append-only tail into the document and clears it.
    ///
    /// The document is written first and the tail cleared second: a crash in between leaves a tail
    /// that is already applied, which replays to the same document. The fold is also the only place
    /// that moves legacy inline payloads out of line and collects unreferenced blobs, because it is
    /// the only place that sees the whole document.
    fn compact_scope(&self, scope: &str) -> Result<()> {
        let mut document = self.merged_document(scope)?;
        self.roll_out(scope, &mut document)?;
        let bytes = write_json(&self.scope_path(scope), &document)?;
        count_scope_write(bytes);
        self.write_scope_index(scope, &document);
        // An empty tail is a file, not a missing one: the next append reuses it.
        retry_while_held(|| fs::write(self.log_path(scope), b""))?;
        self.collect_blobs(scope, &document);
        Ok(())
    }

    /// The byte ranges of `scope`'s document, beside it; see [`ScopeIndex`].
    fn index_path(&self, scope: &str) -> PathBuf {
        self.scope_path(scope).with_extension("idx")
    }

    /// Records the ranges of the document [`FileStore::compact_scope`] just wrote.
    ///
    /// The fold is the one place that has the whole document in hand, so it is where an index
    /// costs nothing extra. Failing to write it is not a failure of the fold: the index is derived
    /// data, and a scope without one is read exactly as it was before it existed.
    fn write_scope_index(&self, scope: &str, document: &RawDocument) {
        let Some(stamp) = self.scope_stamp(scope) else {
            return;
        };
        let Some(stamp) = scope_index_stamp(&stamp) else {
            return;
        };
        let Ok(parts) = document_slots(document) else {
            return;
        };
        let _ = write_json(&self.index_path(scope), &ScopeIndex { stamp, parts });
    }

    /// The index beside `scope`'s document, when one is there and parses.
    ///
    /// Whether it describes the document on disk is decided against the handle a read opens, so
    /// this answers the file and not its freshness.
    fn load_scope_index(&self, scope: &str) -> Option<ScopeIndex> {
        let bytes = retry_while_held(|| fs::read(self.index_path(scope))).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// How many whole-document scans this store performed for [`RecordStore::read_parts`].
    ///
    /// The negative space of the [`Picked`] cache, and the only way a test can say "one scan, not
    /// two" without reading the process-wide counter its neighbours also write to.
    #[cfg(test)]
    fn scans(&self) -> u64 {
        self.scans.load(Ordering::Relaxed)
    }

    /// Counts one transaction read of a scope document; only under `cfg(test)`.
    #[cfg(test)]
    fn note_document_read(&self) {
        self.document_reads.fetch_add(1, Ordering::Relaxed);
    }

    /// How many transaction reads this store performed; only under `cfg(test)`.
    #[cfg(test)]
    fn document_reads(&self) -> u64 {
        self.document_reads.load(Ordering::Relaxed)
    }

    /// Counts one read of a document range; only under `cfg(test)`.
    #[cfg(test)]
    fn note_range_read(&self) {
        self.range_reads.fetch_add(1, Ordering::Relaxed);
    }

    /// How many document ranges this store read; only under `cfg(test)`.
    #[cfg(test)]
    fn range_reads(&self) -> u64 {
        self.range_reads.load(Ordering::Relaxed)
    }

    /// `(len, modified, created)` of one scope document; `None` when it does not exist.
    fn scope_stamp(&self, scope: &str) -> Option<crate::store::DocumentStamp> {
        let meta = retry_while_held(|| fs::metadata(self.scope_path(scope))).ok()?;
        Some((meta.len(), meta.modified().ok(), meta.created().ok()))
    }

    /// `(len, modified, created)` of one scope's tail; `None` when there is none.
    fn log_stamp(&self, scope: &str) -> Option<crate::store::DocumentStamp> {
        let meta = retry_while_held(|| fs::metadata(self.log_path(scope))).ok()?;
        Some((meta.len(), meta.modified().ok(), meta.created().ok()))
    }

    /// The parsed tail of `scope` as of `log_stamp`, when this process already parsed exactly it.
    fn cached_overlay(
        &self,
        scope: &str,
        log_stamp: &Option<crate::store::DocumentStamp>,
    ) -> Option<Arc<Overlay>> {
        let overlays = self.overlays.lock().ok()?;
        let entry = overlays.get(scope)?;
        (entry.log_stamp == *log_stamp).then(|| Arc::clone(&entry.overlay))
    }

    /// Keeps a parsed tail until the log's identity changes.
    fn keep_overlay(
        &self,
        scope: &str,
        log_stamp: Option<crate::store::DocumentStamp>,
        overlay: Arc<Overlay>,
    ) {
        let Ok(mut overlays) = self.overlays.lock() else {
            return;
        };
        if overlays.len() >= DOCUMENT_SCOPES && !overlays.contains_key(scope) {
            overlays.clear();
        }
        overlays.insert(scope.to_string(), CachedOverlay { log_stamp, overlay });
    }

    /// The document bytes of `scope`, when this process already read exactly these bytes.
    fn cached_document(&self, scope: &str) -> Option<Arc<Vec<u8>>> {
        let stamp = self.scope_stamp(scope)?;
        let documents = self.documents.lock().ok()?;
        let entry = documents.get(scope)?;
        (entry.stamp == stamp).then(|| Arc::clone(&entry.bytes))
    }

    /// Keeps `bytes` as the document `stamp` describes and hands them back to the caller.
    ///
    /// Only a read bracketed by two identical stamps gets here: the file was not replaced while
    /// it was being read, so the bytes and the stamp describe the same document.
    fn keep_document(
        &self,
        scope: &str,
        stamp: crate::store::DocumentStamp,
        bytes: Vec<u8>,
    ) -> Arc<Vec<u8>> {
        let bytes = Arc::new(bytes);
        if stamp.0 != bytes.len() as u64 {
            return bytes;
        }
        let Ok(mut documents) = self.documents.lock() else {
            return bytes;
        };
        if documents.len() >= DOCUMENT_SCOPES && !documents.contains_key(scope) {
            documents.clear();
        }
        documents.insert(
            scope.to_string(),
            CachedDocument {
                stamp,
                bytes: Arc::clone(&bytes),
            },
        );
        bytes
    }

    /// The last answer for `(scope, parts)`, if the document behind it is unchanged.
    fn pick_from_cache(
        &self,
        scope: &str,
        parts: &[&str],
        stamp: &(
            u64,
            Option<std::time::SystemTime>,
            Option<std::time::SystemTime>,
        ),
        log_stamp: &Option<crate::store::DocumentStamp>,
    ) -> Option<Vec<Option<Value>>> {
        let picked = self.picked.lock().ok()?;
        let entry = picked.get(scope)?;
        if entry.stamp != *stamp
            || entry.log_stamp != *log_stamp
            || entry.parts.len() != parts.len()
        {
            return None;
        }
        if !entry
            .parts
            .iter()
            .zip(parts)
            .all(|(cached, asked)| cached == asked)
        {
            return None;
        }
        Some(entry.values.clone())
    }

    fn keep_picked(
        &self,
        scope: &str,
        parts: &[&str],
        stamp: (
            u64,
            Option<std::time::SystemTime>,
            Option<std::time::SystemTime>,
        ),
        log_stamp: Option<crate::store::DocumentStamp>,
        values: &[Option<Value>],
    ) {
        let Ok(mut picked) = self.picked.lock() else {
            return;
        };
        if picked.len() >= PICKED_SCOPES && !picked.contains_key(scope) {
            picked.clear();
        }
        picked.insert(
            scope.to_string(),
            Picked {
                stamp,
                log_stamp,
                parts: parts.iter().map(|part| (*part).to_string()).collect(),
                values: values.to_vec(),
            },
        );
    }

    /// Reads `parts` out of one scope document without building the rest of it.
    ///
    /// A control poll needs the turn record and nothing else, and it happens several times a
    /// second while a model call streams. Parsing the whole document into a
    /// `BTreeMap<String, Value>` for that is what made a hung provider cost tens of megabytes a
    /// second; this streams the document and allocates only the values asked for, leaving every
    /// other record to be skipped as input. It also takes no scope lock: a document is replaced
    /// by an atomic rename, so a reader always sees a complete one (the same reasoning as
    /// [`FileStore::ready`]), and a poll has no reason to write a lock file.
    ///
    /// An answer is reused while the document behind it has not been replaced, because a poll
    /// repeats the same question and the answer can only change when the file does. The reuse is
    /// a stat and a comparison; the scan it skips is the whole conversation.
    fn scope_parts(&self, scope: &str, parts: &[&str]) -> Result<Vec<Option<Value>>> {
        // The tail is parsed here as well: it is what makes the document current, and it is small
        // where the document is the conversation. It is kept until the log changes, because the
        // same poll and the fold behind it repeat against an unchanged tail.
        let log_stamp = self.log_stamp(scope);
        let overlay = match self.cached_overlay(scope, &log_stamp) {
            Some(overlay) => overlay,
            None => {
                let overlay = match self.log_bytes(scope)? {
                    Some(log) if !log.is_empty() => Arc::new(Overlay::from_log(&log)?),
                    _ => Arc::new(Overlay::default()),
                };
                if self.log_stamp(scope) == log_stamp {
                    self.keep_overlay(scope, log_stamp, Arc::clone(&overlay));
                }
                overlay
            }
        };
        let path = self.scope_path(scope);
        let stamp = match retry_while_held(|| fs::metadata(&path)) {
            Ok(meta) => (meta.len(), meta.modified().ok(), meta.created().ok()),
            // A document that was never written answers `None` for everything, unless the tail
            // itself holds the part. Not cached: the first write has to be seen.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut values = Vec::with_capacity(parts.len());
                for part in parts {
                    values.push(match overlay.puts.get(*part) {
                        Some(stored) => Some(self.resolve(scope, stored.clone())?),
                        None => None,
                    });
                }
                return Ok(values);
            }
            Err(error) => return Err(error.into()),
        };
        if let Some(values) = self.pick_from_cache(scope, parts, &stamp, &log_stamp) {
            return Ok(values);
        }
        let bytes = match retry_while_held(|| fs::read(&path)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(vec![None; parts.len()])
            }
            Err(error) => return Err(error.into()),
        };
        let mut picked = pick_parts(&bytes, parts)?;
        crate::traffic::count_scope_pick();
        #[cfg(test)]
        self.scans.fetch_add(1, Ordering::Relaxed);
        let mut values: Vec<Option<Value>> = parts
            .iter()
            .map(|part| match overlay.puts.get(*part) {
                Some(value) => Some(value.clone()),
                None if overlay.removes.contains(*part) => None,
                None => picked.remove(*part),
            })
            .collect();
        for slot in values.iter_mut() {
            if let Some(stored) = slot.take() {
                // A poll only asks for control records, which are far below the blob threshold, so
                // this normally answers without touching a file at all.
                *slot = Some(self.resolve(scope, stored)?);
            }
        }
        self.keep_picked(scope, parts, stamp, log_stamp, &values);
        Ok(values)
    }

    /// Applies a finished transaction as one appended line.
    ///
    /// The document is not read: a commit carries only the parts it changed, so what a commit costs
    /// no longer follows the size of the session it touches. Once the tail reaches
    /// [`LOG_COMPACT_BYTES`] it is folded into the document — the same whole-document write this
    /// store used to do on every commit.
    fn commit_append(
        &self,
        scope: &str,
        lock: &HeldLock<'_>,
        staged: BTreeMap<String, Option<Value>>,
        scheduled: BTreeMap<String, Scheduled>,
    ) -> Result<()> {
        let mut delta = LogDelta::default();
        for (part, value) in staged {
            if scheduled.contains_key(&part) {
                continue;
            }
            match value {
                Some(value) => {
                    // A payload this large would be re-read by every fold of the document, so it
                    // is stored once and the tail keeps a reference to it.
                    delta.puts.insert(part, self.externalize(scope, value)?);
                }
                None => delta.removes.push(part),
            }
        }
        if !scheduled.is_empty() {
            self.write_scheduled(&scheduled)?;
        }
        if delta.puts.is_empty() && delta.removes.is_empty() {
            return Ok(());
        }
        // The last moment at which losing the lock can still be noticed, and the only one that
        // matters: the lock is what excludes the other writer, so a stolen lock must cost an
        // abandoned commit, not a lost update.
        lock.verify()?;
        self.append_log(scope, &delta)?;
        // The first commit of a scope also creates its document: a tail with nothing under it would
        // make every reader fold the whole scope out of the log, and a scope that never reached the
        // fold threshold would have no document at all.
        if !self.scope_path(scope).exists() || self.log_size(scope) >= LOG_COMPACT_BYTES {
            self.compact_scope(scope)?;
        }
        Ok(())
    }

    /// Writes the staged readiness records. The caller holds the scope lock and this takes the
    /// index lock after it, so there is one lock order and no cycle is possible.
    ///
    /// The readiness index is not part of any scope: two scopes — in this process or in another
    /// host — can schedule the same work id, and one commit can both write one item and drop
    /// another, so without a lock a reader (`ready`) may see half of that batch and a later writer
    /// may put the removed item back.
    fn write_scheduled(&self, scheduled: &BTreeMap<String, Scheduled>) -> Result<()> {
        let index_path = self.work_lock_path();
        let index_gate = self.gate(WORK_INDEX_LOCK, &index_path)?;
        let _index = self.hold(&index_gate, &index_path)?;
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
            retry_while_held(|| fs::create_dir_all(self.work_dir(&entry.queue)))?;
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
        Ok(())
    }
}

impl RecordStore for FileStore {
    fn transaction(
        &self,
        scope: &str,
        work: &mut dyn FnMut(&mut dyn Txn) -> Result<()>,
    ) -> Result<()> {
        let path = self.lock_path(scope);
        let gate = self.gate(scope, &path)?;
        let lock = self.hold(&gate, &path)?;
        let mut staged = StagedTxn {
            store: self,
            scope,
            read_only: false,
            bytes: RefCell::new(None),
            overlay: RefCell::new(None),
            parts: RefCell::new(BTreeMap::new()),
            index: RefCell::new(None),
            persisted: RefCell::new(None),
            staged: BTreeMap::new(),
            scheduled: BTreeMap::new(),
            now: crate::now_millis(),
        };
        let outcome = work(&mut staged);
        if let Err(error) = outcome {
            // Discard: nothing reached the disk.
            return Err(error);
        }
        TRANSACTIONS.fetch_add(1, Ordering::Relaxed);
        // The commit carries only the parts it changed, so it does not read the document: what a
        // commit costs is its own delta, not the conversation behind it.
        let StagedTxn {
            staged: staged_map,
            scheduled,
            ..
        } = staged;
        self.commit_append(scope, &lock, staged_map, scheduled)
    }

    fn read_transaction(
        &self,
        scope: &str,
        work: &mut dyn FnMut(&mut dyn Txn) -> Result<()>,
    ) -> Result<()> {
        // The lock is still taken: a reader must not see a half-replaced document, and a writer
        // must not slip a commit between two reads of one transaction. What the cache buys is the
        // file read, not the exclusion.
        let path = self.lock_path(scope);
        let gate = self.gate(scope, &path)?;
        let _lock = self.hold(&gate, &path)?;
        let mut staged = StagedTxn {
            store: self,
            scope,
            read_only: true,
            bytes: RefCell::new(None),
            overlay: RefCell::new(None),
            parts: RefCell::new(BTreeMap::new()),
            index: RefCell::new(None),
            persisted: RefCell::new(None),
            staged: BTreeMap::new(),
            scheduled: BTreeMap::new(),
            now: crate::now_millis(),
        };
        work(&mut staged)
    }

    fn scope_bytes(&self, scope: &str) -> Result<Option<u64>> {
        // What a reader of this scope has to hold: the document plus the tail not folded into it
        // yet. A backend that decides what to drop from the scope size must see the whole scope,
        // not just the last fold.
        let document = match fs::metadata(self.scope_path(scope)) {
            Ok(meta) => meta.len(),
            // A scope that was never folded has no document yet; its tail is the whole scope.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error.into()),
        };
        Ok(Some(document + self.log_size(scope)))
    }

    fn ready(&self, queue: &str, now: i64, limit: usize) -> Result<Vec<WorkItem>> {
        let mut due: Vec<WorkItem> = Vec::new();
        // Reading needs no index lock: every document is replaced by an atomic rename, so a
        // reader sees either the whole previous record or the whole next one. It does need the
        // retry, because the file being replaced is briefly held by its writer. The directory is
        // the queue's own, so this opens that queue's records and nothing else.
        let entries = match retry_while_held(|| fs::read_dir(self.work_dir(queue))) {
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

    fn read_parts(&self, scope: &str, parts: &[&str]) -> Result<Vec<Option<Value>>> {
        self.scope_parts(scope, parts)
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

/// The commits appended since the document was folded, in the order they were written.
///
/// A reader applies this to whatever the document holds instead of folding it into the document:
/// the tail is a few kilobytes where the document is the conversation, and serializing the merged
/// document back into bytes for every reader would give back exactly what the tail saves.
#[derive(Default)]
struct Overlay {
    puts: BTreeMap<String, Value>,
    removes: HashSet<String>,
}

impl Overlay {
    /// The final state the tail describes, one complete line at a time.
    fn from_log(log: &[u8]) -> Result<Self> {
        let mut overlay = Self::default();
        let mut rest = log;
        while let Some(end) = rest.iter().position(|byte| *byte == b'\n') {
            let line = &rest[..end];
            rest = &rest[end + 1..];
            if line.is_empty() {
                continue;
            }
            let delta: LogDelta = serde_json::from_slice(line)
                .map_err(|error| DurableError::invalid(format!("scope log is damaged: {error}")))?;
            overlay.apply(delta);
        }
        Ok(overlay)
    }

    fn apply(&mut self, delta: LogDelta) {
        for (part, value) in delta.puts {
            self.removes.remove(&part);
            self.puts.insert(part, value);
        }
        for part in delta.removes {
            self.puts.remove(&part);
            self.removes.insert(part);
        }
    }
}

struct StagedTxn<'a> {
    store: &'a FileStore,
    scope: &'a str,
    /// Whether the transaction may only read.
    ///
    /// A read-only transaction may answer from [`FileStore::cached_document`]; a writing one reads
    /// the file itself — the whole document, or one record's bytes through [`ScopeIndex`] — because
    /// cached bytes are not a safe base to rewrite (another host may have replaced the document
    /// since; two writes with the same length and timestamps would make that invisible).
    read_only: bool,
    /// The document bytes, read once per transaction and shared by every pick, key walk and the
    /// rewrite. An `Arc` so a read-only transaction keeps the same buffer as the cache instead of
    /// copying a document-sized allocation per tick.
    ///
    /// `None` — not read; `Some(None)` — the scope has no document yet.
    bytes: RefCell<Option<Option<Arc<Vec<u8>>>>>,
    /// The committed tail, applied on top of the document when asked for a part.
    ///
    /// `None` — not read; `Some` — the tail since the last fold (empty when nothing was appended).
    overlay: RefCell<Option<Arc<Overlay>>>,
    /// Values asked for by [`Txn::get`], picked out of the document one record at a time.
    parts: RefCell<BTreeMap<String, Option<Value>>>,
    /// The top-level byte ranges of the document, built on first read and reused for the rest of
    /// the transaction.
    index: RefCell<Option<Arc<DocumentIndex>>>,
    /// The index written beside the document, loaded on first use and only for a transaction that
    /// reads records out of the document.
    ///
    /// `None` — not tried; `Some(None)` — the scope has no index this transaction can use.
    persisted: RefCell<Option<Option<ScopeIndex>>>,
    staged: BTreeMap<String, Option<Value>>,
    scheduled: BTreeMap<String, Scheduled>,
    now: i64,
}

impl StagedTxn<'_> {
    /// The document bytes, read once and reused for this transaction's picks, keys and rewrite.
    ///
    /// Bounded by the document's own size, not by the number of records in it: a journal page and
    /// the picks it reads share one copy, where re-reading per record would multiply the file read
    /// by the page. Holding it is cheap next to the value tree this store deliberately no longer
    /// builds; it is taken out before the commit serializes, so no two document-sized buffers
    /// overlap at the peak.
    fn ensure_bytes(&self) -> Result<()> {
        self.ensure_overlay()?;
        if self.bytes.borrow().is_some() {
            return Ok(());
        }
        if self.read_only {
            if let Some(bytes) = self.store.cached_document(self.scope) {
                *self.bytes.borrow_mut() = Some(Some(bytes));
                return Ok(());
            }
        }
        // A read-only transaction may keep what it read, but only when the file was not replaced
        // while it was being read: two identities that agree bracket the read.
        let before = if self.read_only {
            self.store.scope_stamp(self.scope)
        } else {
            None
        };
        #[cfg(test)]
        self.store.note_document_read();
        let loaded = match retry_while_held(|| fs::read(self.store.scope_path(self.scope))) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let loaded = match loaded {
            None => Some(None),
            Some(bytes) if !self.read_only => Some(Some(Arc::new(bytes))),
            Some(bytes) => {
                let after = self.store.scope_stamp(self.scope);
                let stable = matches!(
                    (&before, &after),
                    (Some(before), Some(after)) if before == after
                );
                match (stable, after) {
                    (true, Some(stamp)) => {
                        Some(Some(self.store.keep_document(self.scope, stamp, bytes)))
                    }
                    // The file moved under the read: a complete document either way, but not one
                    // a later stamp comparison would describe.
                    _ => Some(Some(Arc::new(bytes))),
                }
            }
        };
        *self.bytes.borrow_mut() = loaded;
        Ok(())
    }

    /// The tail since the last fold, parsed once per transaction.
    ///
    /// Separate from [`StagedTxn::ensure_bytes`] because a read of one record needs the tail — it
    /// is what makes the document current — but not the document.
    fn ensure_overlay(&self) -> Result<()> {
        if self.overlay.borrow().is_some() {
            return Ok(());
        }
        // The tail is parsed first: it is what makes the document's answer current, and it is
        // small — a few lines per commit — where folding it into the document would serialize
        // the conversation again for every reader. It is kept until the log changes, because the
        // same fold reads several times per tick against an unchanged tail.
        let before = self.store.log_stamp(self.scope);
        let overlay = match self.store.cached_overlay(self.scope, &before) {
            Some(overlay) => overlay,
            None => {
                let overlay = match self.store.log_bytes(self.scope)? {
                    Some(log) if !log.is_empty() => Arc::new(Overlay::from_log(&log)?),
                    _ => Arc::new(Overlay::default()),
                };
                // A tail that grew while it was being read is left uncached: the next reader
                // parses it again, which is what keeps a cached answer from hiding a commit.
                if self.store.log_stamp(self.scope) == before {
                    self.store
                        .keep_overlay(self.scope, before, Arc::clone(&overlay));
                }
                overlay
            }
        };
        *self.overlay.borrow_mut() = Some(overlay);
        Ok(())
    }

    /// The top-level index of the document bytes, built on first use.
    ///
    /// `None` when the scope has no document yet; the index is what turns the per-record walks a
    /// transaction performs into one walk for all of them.
    fn document_index(&self) -> Result<Option<Arc<DocumentIndex>>> {
        if let Some(index) = self.index.borrow().as_ref() {
            return Ok(Some(Arc::clone(index)));
        }
        self.ensure_bytes()?;
        let index = {
            let bytes = self.bytes.borrow();
            match &*bytes {
                Some(Some(document)) => Arc::new(index_document(document.as_slice())?),
                _ => return Ok(None),
            }
        };
        *self.index.borrow_mut() = Some(Arc::clone(&index));
        Ok(Some(index))
    }

    /// One value from the document bytes: through the index when it is usable, by a scan when it
    /// is not (a document whose ranges could not be placed).
    ///
    /// A read-only transaction scans: building the index costs one walk plus a `String` per
    /// record, and a read-only transaction usually asks for one or two records, so the index only
    /// pays in a transaction that reads several — which is what a write transaction does (state,
    /// thread, turn, then a page of journal entries).
    fn document_value(&self, part: &str) -> Result<Option<Value>> {
        if !self.read_only {
            if let Some(index) = self.document_index()? {
                if index.complete {
                    let bytes = self.bytes.borrow();
                    return match &*bytes {
                        Some(Some(document)) => index.value(document.as_slice(), part),
                        _ => Ok(None),
                    };
                }
            }
        }
        let bytes = self.bytes.borrow();
        Ok(match &*bytes {
            Some(Some(document)) => pick_parts(document.as_slice(), &[part])?.remove(part),
            _ => None,
        })
    }

    /// The stored value of `part`, without building the rest of the document.
    ///
    /// The tail is consulted first: a part a commit appended is answered without touching the
    /// document at all. Then the index beside the document, when a write transaction has one: a
    /// write reads the state, the thread and the turn and rewrites nothing but its own delta, so
    /// reading the whole document for them is what a commit used to cost. The document itself is
    /// read only when neither can answer.
    fn read(&self, part: &str) -> Result<Option<Value>> {
        if let Some(value) = self.staged.get(part) {
            return Ok(value.clone());
        }
        if let Some(cached) = self.parts.borrow().get(part) {
            return Ok(cached.clone());
        }
        self.ensure_overlay()?;
        let value = {
            let overlay = self.overlay.borrow();
            let overlay = overlay.as_ref().expect("ensure_overlay parses the tail");
            if let Some(value) = overlay.puts.get(part) {
                Some(value.clone())
            } else if overlay.removes.contains(part) {
                None
            } else {
                match self.indexed_value(part)? {
                    Some(value) => value,
                    None => {
                        self.ensure_bytes()?;
                        self.document_value(part)?
                    }
                }
            }
        };
        self.parts
            .borrow_mut()
            .insert(part.to_string(), value.clone());
        Ok(value)
    }

    /// The index beside the document, when this transaction can read records out of it.
    ///
    /// A read-only transaction does not: it serves the same document again from
    /// [`FileStore::cached_document`] on the next tick, which a per-record range read would neither
    /// replace nor fill, and the fold that follows it reads every record anyway.
    fn persisted_index(&self) -> Option<ScopeIndex> {
        if self.read_only || self.bytes.borrow().is_some() {
            return None;
        }
        if let Some(entry) = self.persisted.borrow().as_ref() {
            return entry.clone();
        }
        let loaded = self.store.load_scope_index(self.scope);
        *self.persisted.borrow_mut() = Some(loaded.clone());
        loaded
    }

    /// The value of `part` out of the document's own bytes, when the index describes the document
    /// on disk.
    ///
    /// `None` means the index cannot answer — none, stale, a range that does not read — and the
    /// caller reads the document instead; that is the behaviour of a scope the fold never touched.
    /// `Some(None)` is the value of a part the document does not hold.
    fn indexed_value(&self, part: &str) -> Result<Option<Option<Value>>> {
        let Some(index) = self.persisted_index() else {
            return Ok(None);
        };
        let Some((start, end)) = index.parts.get(part).copied() else {
            return Ok(Some(None));
        };
        #[cfg(test)]
        self.store.note_range_read();
        let path = self.store.scope_path(self.scope);
        let text = match read_document_range(&path, &index.stamp, start, end) {
            Some(text) => text,
            None => return Ok(None),
        };
        match serde_json::from_slice(&text) {
            Ok(value) => Ok(Some(Some(value))),
            // A range the index placed wrongly is a miss, not an error.
            Err(_) => Ok(None),
        }
    }

    /// The keys of the document, as `Null` values, for `merge_keys` to walk.
    ///
    /// Only the keys are materialized; every value is skipped as input. Reading a long journal
    /// walks its keys page after page, and building the whole value tree to learn the keys of one
    /// page is what made a page cost the conversation.
    fn document_keys(&self) -> Result<BTreeMap<String, Value>> {
        self.ensure_bytes()?;
        let index = if self.read_only {
            // A read-only transaction scans: see [`StagedTxn::document_value`].
            None
        } else {
            self.document_index()?
        };
        let mut keys = match index {
            Some(index) if index.complete => index.keys(),
            _ => {
                let bytes = self.bytes.borrow();
                match &*bytes {
                    Some(Some(document)) => key_map(document.as_slice())?,
                    _ => BTreeMap::new(),
                }
            }
        };
        let overlay = self.overlay.borrow();
        let overlay = overlay.as_ref().expect("ensure_bytes parses the tail");
        for part in overlay.puts.keys() {
            keys.insert(part.clone(), Value::Null);
        }
        for part in &overlay.removes {
            keys.remove(part);
        }
        Ok(keys)
    }
}

impl Txn for StagedTxn<'_> {
    fn now(&self) -> i64 {
        self.now
    }

    fn get(&self, part: &str) -> Result<Option<Value>> {
        // A payload stored out of line is read here and nowhere else, so a caller never sees a
        // reference and every digest it computes stays the digest of the payload.
        match self.read(part)? {
            Some(stored) => Ok(Some(self.store.resolve(self.scope, stored)?)),
            None => Ok(None),
        }
    }

    fn put(&mut self, part: &str, value: Option<Value>) -> Result<()> {
        if self.read_only {
            return Err(crate::DurableError::invalid(
                "a read-only transaction cannot write",
            ));
        }
        self.staged.insert(part.to_string(), value);
        Ok(())
    }

    fn keys(&self, prefix: &str, after: Option<&str>, limit: usize) -> Result<Vec<String>> {
        let base = self.document_keys()?;
        Ok(crate::store::merge_keys(
            &base,
            &self.staged,
            prefix,
            after,
            limit,
        ))
    }

    fn get_all(&self, parts: &[String]) -> Result<Vec<Option<Value>>> {
        // One scan of the document returns the whole page, where a `get` per record would scan
        // it once per record and make a page cost the history it sits in.
        self.ensure_bytes()?;
        let overlay = self.overlay.borrow();
        let overlay = overlay.as_ref().expect("ensure_bytes parses the tail");
        let uncovered: Vec<&str> = parts
            .iter()
            .map(String::as_str)
            .filter(|part| {
                !self.staged.contains_key(*part)
                    && !overlay.puts.contains_key(*part)
                    && !overlay.removes.contains(*part)
            })
            .collect();
        let mut picked = {
            let bytes = self.bytes.borrow();
            match &*bytes {
                Some(Some(document)) if !uncovered.is_empty() => {
                    pick_parts(document.as_slice(), &uncovered)?
                }
                _ => BTreeMap::new(),
            }
        };
        let mut values = Vec::with_capacity(parts.len());
        for part in parts {
            let stored = match self.staged.get(part) {
                Some(staged) => staged.clone(),
                None => match overlay.puts.get(part) {
                    Some(value) => Some(value.clone()),
                    None if overlay.removes.contains(part) => None,
                    None => picked.remove(part.as_str()),
                },
            };
            values.push(match stored {
                Some(stored) => Some(self.store.resolve(self.scope, stored)?),
                None => None,
            });
        }
        Ok(values)
    }

    fn schedule(
        &mut self,
        queue: &str,
        work_id: &str,
        ready_at: i64,
        payload: Option<Value>,
    ) -> Result<()> {
        if self.read_only {
            return Err(crate::DurableError::invalid(
                "a read-only transaction cannot schedule work",
            ));
        }
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

/// One commit's changes, as appended to `scopes/<digest>.log`.
///
/// A line per commit, not a document: a transaction writes this and lets the fold merge it into
/// the document later, so a commit no longer reads or parses the document it changes. Values are
/// already externalized, so a line carries references where the document would carry bytes.
#[derive(Default, Serialize, Deserialize)]
struct LogDelta {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    puts: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    removes: Vec<String>,
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
    sha256_hex(input.as_bytes())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// The blob a stored value stands for, when the value is a reference.
///
/// The shape is checked strictly — `$blob` is the only key, `kind` is the one representation this
/// store writes, `bytes` is a number and `digest` is 64 hex characters — for two reasons. A part
/// whose own JSON happens to carry a `$blob` field has to keep being read as itself, and a
/// reference this store cannot act on has to fail as data rather than be silently resolved to
/// whatever a loose match found.
fn blob_reference_of(value: &Value) -> Option<&str> {
    let map = value.as_object()?;
    if map.len() != 1 {
        return None;
    }
    let reference = map.get(BLOB_MARKER)?.as_object()?;
    if reference.len() != 3
        || reference.get("kind")?.as_str()? != BLOB_KIND
        || reference.get("bytes")?.as_u64().is_none()
    {
        return None;
    }
    let blob = reference.get("digest")?.as_str()?;
    (blob.len() == 64 && blob.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(blob)
}

/// The document form of a reference: the digest, the payload length and how it is encoded.
fn blob_reference(blob: &str, bytes: usize) -> Value {
    let mut reference = serde_json::Map::new();
    reference.insert("digest".to_string(), Value::String(blob.to_string()));
    reference.insert("bytes".to_string(), Value::from(bytes as u64));
    reference.insert("kind".to_string(), Value::String(BLOB_KIND.to_string()));
    let mut marker = serde_json::Map::new();
    marker.insert(BLOB_MARKER.to_string(), Value::Object(reference));
    Value::Object(marker)
}

/// Whether `path` was last written longer than `age` ago.
fn older_than(path: &Path, age: Duration) -> bool {
    match retry_while_held(|| fs::metadata(path)) {
        Ok(meta) => meta
            .modified()
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|elapsed| elapsed >= age),
        Err(_) => false,
    }
}

/// The top-level records of one serialized scope document, by byte range.
///
/// A transaction reads the same document several times — the state, the thread, the turn, then a
/// page of journal entries — and every `pick_parts` call walks the whole document even though it
/// materializes one record. This is built once per document bytes and answers each later read by
/// slicing the record's own bytes, so the document is walked once per transaction instead of once
/// per record.
struct DocumentIndex {
    /// Whether the ranges describe the document. A borrow that did not land inside the input
    /// leaves them unusable, and the caller falls back to a scan.
    complete: bool,
    /// `part` → `(start, end)` of its value in the document bytes.
    slots: BTreeMap<String, (usize, usize)>,
}

impl DocumentIndex {
    /// The value of `part`, parsed from its own bytes; `None` when the document has no such part.
    fn value(&self, bytes: &[u8], part: &str) -> Result<Option<Value>> {
        match self.slots.get(part) {
            Some((start, end)) => Ok(Some(serde_json::from_slice(&bytes[*start..*end])?)),
            None => Ok(None),
        }
    }

    /// The keys the document holds, as `Null` values for `merge_keys` to walk.
    fn keys(&self) -> BTreeMap<String, Value> {
        self.slots
            .keys()
            .map(|key| (key.clone(), Value::Null))
            .collect()
    }
}

/// Indexes the top level of one serialized scope document.
///
/// Each value is borrowed as a `RawValue`, so the ranges point into the input itself and the walk
/// builds one `String` per record and nothing else — no `Value`, no copy of the record.
fn index_document(bytes: &[u8]) -> Result<DocumentIndex> {
    use serde::de::{MapAccess, Visitor};
    use serde::Deserializer;
    use serde_json::value::RawValue;

    struct Index {
        base: usize,
        slots: BTreeMap<String, (usize, usize)>,
    }

    impl<'de> Visitor<'de> for Index {
        type Value = BTreeMap<String, (usize, usize)>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a stored scope document")
        }

        fn visit_map<A: MapAccess<'de>>(
            mut self,
            mut map: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            while let Some(key) = map.next_key::<String>()? {
                let raw: &'de RawValue = map.next_value()?;
                let text = raw.get();
                let start = text.as_ptr() as usize - self.base;
                self.slots.insert(key, (start, start + text.len()));
            }
            Ok(self.slots)
        }
    }

    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let slots = deserializer.deserialize_map(Index {
        base: bytes.as_ptr() as usize,
        slots: BTreeMap::new(),
    })?;
    deserializer.end()?;
    // A zero-copy borrow always lands inside the input; the check keeps a future change to
    // `RawValue` from turning a wrong offset into a wrong value.
    let complete = slots
        .values()
        .all(|(start, end)| start <= end && *end <= bytes.len());
    Ok(DocumentIndex { complete, slots })
}

/// The byte ranges of one scope document, kept beside it as `scopes/<digest>.idx`.
///
/// A transaction reads a handful of records — the state, the thread, the turn — and used to read
/// the whole document to get them. The fold is the one place that already has the whole document
/// in hand, so it writes down where each record lands and a later transaction reads only those
/// bytes. It is derived data: `stamp` names the document it describes, and a missing, damaged or
/// stale index only means the read falls back to the document.
/// The on-disk layout version of a store root, as `format.json` names it.
#[derive(Serialize, Deserialize)]
struct StoreFormat {
    version: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct ScopeIndex {
    stamp: ScopeStamp,
    parts: BTreeMap<String, (u64, u64)>,
}

/// [`crate::store::DocumentStamp`] in the form an index file carries.
///
/// A file time is written as nanoseconds since the epoch, which serde cannot derive for a
/// `SystemTime`; a time the epoch cannot express leaves the index unwritten rather than compared
/// against a rounded value.
type ScopeStamp = (u64, Option<u128>, Option<u128>);

/// A document stamp in the form [`ScopeIndex`] stores, or `None` when it cannot be expressed.
fn scope_index_stamp(stamp: &crate::store::DocumentStamp) -> Option<ScopeStamp> {
    fn nanos(time: Option<std::time::SystemTime>) -> Option<Option<u128>> {
        match time {
            Some(time) => Some(Some(
                time.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos(),
            )),
            None => Some(None),
        }
    }
    Some((stamp.0, nanos(stamp.1)?, nanos(stamp.2)?))
}

/// `part` → `(start, end)` of its value in the bytes [`write_json`] writes for `document`.
///
/// The same walk `serde_json` makes over a map — the entries in key order, `"key":value` joined
/// by commas — counted instead of materialized, so indexing a fold costs one `String` per record
/// and not a second copy of the document it just streamed to disk.
fn document_slots(document: &RawDocument) -> Result<BTreeMap<String, (u64, u64)>> {
    let mut slots = BTreeMap::new();
    let mut position = 1u64; // the opening `{`
    for (part, raw) in document {
        if !slots.is_empty() {
            position += 1; // the `,` between two entries
        }
        let key = serde_json::to_string(part)?;
        position += key.len() as u64 + 1; // `"key":`
        let start = position;
        position += raw.get().len() as u64;
        slots.insert(part.clone(), (start, position));
    }
    Ok(slots)
}

/// `[start, end)` of the document at `path`, provided the file is still the one `stamp` describes.
///
/// The stamp is taken from the open handle and not from the path, so the bytes come from the very
/// file that was compared: a document replaced between the check and the read would otherwise
/// answer with another document's bytes.
fn read_document_range(path: &Path, stamp: &ScopeStamp, start: u64, end: u64) -> Option<Vec<u8>> {
    let mut file = retry_while_held(|| File::open(path)).ok()?;
    let meta = file.metadata().ok()?;
    let current = scope_index_stamp(&(meta.len(), meta.modified().ok(), meta.created().ok()))?;
    if current != *stamp || end < start || end > meta.len() {
        return None;
    }
    let mut bytes = vec![0u8; (end - start) as usize];
    file.seek(SeekFrom::Start(start)).ok()?;
    file.read_exact(&mut bytes).ok()?;
    Some(bytes)
}

/// The values of `wanted` in one serialized scope document, skipping everything else.
///
/// Skips are `IgnoredAny`: the bytes are still scanned (a document cannot be jumped over) but no
/// `Value` is built for them, which is the difference between a control poll allocating the whole
/// conversation and allocating one record.
fn pick_parts(bytes: &[u8], wanted: &[&str]) -> Result<BTreeMap<String, Value>> {
    use serde::de::{IgnoredAny, MapAccess, Visitor};
    use serde::Deserializer;

    struct Picker<'a> {
        wanted: &'a [&'a str],
        picked: BTreeMap<String, Value>,
    }

    impl<'de> Visitor<'de> for Picker<'_> {
        type Value = BTreeMap<String, Value>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a stored scope document")
        }

        fn visit_map<A: MapAccess<'de>>(
            mut self,
            mut map: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            while let Some(key) = map.next_key::<String>()? {
                if self.wanted.contains(&key.as_str()) {
                    self.picked.insert(key, map.next_value::<Value>()?);
                } else {
                    map.next_value::<IgnoredAny>()?;
                }
            }
            Ok(self.picked)
        }
    }

    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let picked = deserializer.deserialize_map(Picker {
        wanted,
        picked: BTreeMap::new(),
    })?;
    deserializer.end()?;
    Ok(picked)
}

/// The raw JSON text of one value, for the raw document map.
fn raw_of(value: &Value) -> Result<Box<RawValue>> {
    let text = serde_json::to_string(value)?;
    Ok(serde_json::from_str::<Box<RawValue>>(&text)?)
}

/// The digest a raw document record points at, if it is a blob reference.
///
/// Cheapest first: only a record whose bytes even mention the marker is parsed at all, so the scan
/// a rewrite runs over every record stays a byte search rather than a parse.
fn blob_reference_of_raw(raw: &str) -> Option<String> {
    if !raw.contains(BLOB_MARKER) {
        return None;
    }
    let value: Value = serde_json::from_str(raw).ok()?;
    blob_reference_of(&value).map(str::to_string)
}

/// The keys of one serialized scope document, every value skipped as input.
fn key_map(bytes: &[u8]) -> Result<BTreeMap<String, Value>> {
    use serde::de::{IgnoredAny, MapAccess, Visitor};
    use serde::Deserializer;

    struct Keys;

    impl<'de> Visitor<'de> for Keys {
        type Value = BTreeMap<String, Value>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a stored scope document")
        }

        fn visit_map<A: MapAccess<'de>>(
            self,
            mut map: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut keys = BTreeMap::new();
            while let Some(key) = map.next_key::<String>()? {
                map.next_value::<IgnoredAny>()?;
                keys.insert(key, Value::Null);
            }
            Ok(keys)
        }
    }

    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let keys = deserializer.deserialize_map(Keys)?;
    deserializer.end()?;
    Ok(keys)
}

/// Serializes `value` straight into the temporary file, then renames it into place.
///
/// A scope document is the largest buffer a commit handles: materializing it as `Vec<u8>` first
/// would hold the whole document (megabytes once a checkpoint is inlined) next to the values it
/// was built from. Returns the number of bytes written, for the traffic counters.
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<usize> {
    let sequence = next_sequence();
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = path.with_file_name(format!(".{name}.{}-{sequence}.tmp", std::process::id()));
    let outcome = retry_while_held(|| {
        let sink = CountingSink {
            inner: std::io::BufWriter::new(fs::File::create(&temporary)?),
            written: 0,
        };
        let mut sink = sink;
        serde_json::to_writer(&mut sink, value)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error))?;
        std::io::Write::flush(&mut sink)?;
        Ok(sink.written)
    });
    let bytes = match outcome {
        Ok(bytes) => bytes,
        Err(error) => {
            let _ = retry_while_held(|| fs::remove_file(&temporary));
            return Err(error.into());
        }
    };
    BYTES_WRITTEN.fetch_add(bytes as u64, Ordering::Relaxed);
    if let Err(error) = retry_while_held(|| fs::rename(&temporary, path)) {
        let _ = retry_while_held(|| fs::remove_file(&temporary));
        return Err(error.into());
    }
    Ok(bytes)
}

/// Hashes and counts the bytes handed to it while writing them on.
struct HashingSink<W> {
    inner: W,
    hasher: Sha256,
    written: usize,
}

impl<W> HashingSink<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            written: 0,
        }
    }

    fn finish(self) -> std::io::Result<(String, usize)>
    where
        W: std::io::Write,
    {
        let Self {
            mut inner,
            hasher,
            written,
        } = self;
        inner.flush()?;
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(64);
        for byte in digest {
            hex.push_str(&format!("{byte:02x}"));
        }
        Ok((hex, written))
    }
}

impl<W: std::io::Write> std::io::Write for HashingSink<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        self.written += written;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Counts the bytes handed to the writer so a streamed document can report its size.
struct CountingSink<W> {
    inner: W,
    written: usize,
}

impl<W: std::io::Write> std::io::Write for CountingSink<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.written += written;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
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
                    let holder = lock_holder(path)
                        .map(|holder| format!(" (held by {holder})"))
                        .unwrap_or_default();
                    return Err(DurableError::unavailable(format!(
                        "scope is locked{holder}; another agent is using this workspace"
                    )));
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

/// The holder named in a lock body (`<pid> <millis> <seq>`), for a diagnosis message.
fn lock_holder(path: &Path) -> Option<String> {
    let body = std::fs::read_to_string(path).ok()?;
    let mut fields = body.split_whitespace();
    let pid = fields.next()?;
    Some(match fields.next() {
        Some(since) => format!("pid {pid} since {since}"),
        None => format!("pid {pid}"),
    })
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
    fn a_read_only_transaction_reads_the_document_once() -> Result<()> {
        let root = TempRoot::new("read-only-cache");
        let store = FileStore::open(root.path())?;
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("a", Some(json!({"n": 1})))
        })?;
        // A commit only appends its delta; the fold is what gives the scope its document.
        store.compact_scope("session")?;
        let reads = store.document_reads();
        for _ in 0..3 {
            let value = crate::store::read(&store, "session", |txn: &mut dyn Txn| txn.get("a"))?;
            assert_eq!(value, Some(json!({"n": 1})));
        }
        assert_eq!(
            store.document_reads(),
            reads + 1,
            "three read-only transactions against an unchanged document read the file once"
        );

        // A commit appends a tail, so the next read has to apply it to the document.
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("a", Some(json!({"n": 2})))
        })?;
        assert_eq!(
            read_value(&store, "session", "a")?,
            Some(json!({"n": 2})),
            "a write invalidates the cached document"
        );
        Ok(())
    }

    #[test]
    fn a_committed_delta_reads_back_before_it_is_folded() -> Result<()> {
        let root = TempRoot::new("log-before-fold");
        let store = FileStore::open(root.path())?;
        // The first commit of a scope creates its document, so the tail under test is the one the
        // commit after it appends.
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("a", Some(json!(1)))
        })?;
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("b", Some(json!(2)))
        })?;
        // Nothing folded yet: the second part lives in the tail only.
        assert!(store.log_size("session") > 0);
        let document = fs::read_to_string(store.scope_path("session"))?;
        assert!(
            !document.contains("\"b\":"),
            "the tail is not in the document yet"
        );
        assert_eq!(read_value(&store, "session", "a")?, Some(json!(1)));
        assert_eq!(read_value(&store, "session", "b")?, Some(json!(2)));
        assert_eq!(
            store.read_parts("session", &["a", "b"])?,
            vec![Some(json!(1)), Some(json!(2))]
        );
        Ok(())
    }

    #[test]
    fn a_tail_that_stops_before_its_newline_is_ignored() -> Result<()> {
        let root = TempRoot::new("log-partial");
        let store = FileStore::open(root.path())?;
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("kept", Some(json!(1)))
        })?;
        // A process that died mid-append leaves a line without its newline: it must not be read
        // as a commit, because the newline is what completes one.
        let mut tail = fs::read(store.log_path("session"))?;
        tail.extend_from_slice(b"{\"puts\":{\"half\":9}}");
        fs::write(store.log_path("session"), tail)?;
        assert_eq!(read_value(&store, "session", "kept")?, Some(json!(1)));
        assert_eq!(read_value(&store, "session", "half")?, None);
        Ok(())
    }

    #[test]
    fn folding_the_tail_changes_nothing_a_reader_sees() -> Result<()> {
        let root = TempRoot::new("log-fold");
        let store = FileStore::open(root.path())?;
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("a", Some(json!(1)))
        })?;
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("b", Some(json!(2)))?;
            txn.put("a", None)
        })?;
        assert_eq!(read_value(&store, "session", "a")?, None);
        assert_eq!(read_value(&store, "session", "b")?, Some(json!(2)));

        store.compact_scope("session")?;
        assert_eq!(store.log_size("session"), 0, "the fold clears the tail");
        assert_eq!(read_value(&store, "session", "a")?, None);
        assert_eq!(read_value(&store, "session", "b")?, Some(json!(2)));
        Ok(())
    }

    #[test]
    fn a_reopened_store_reads_the_appended_tail() -> Result<()> {
        let root = TempRoot::new("log-reopen");
        {
            let store = FileStore::open(root.path())?;
            store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
                txn.put("a", Some(json!("first")))
            })?;
        }
        let store = FileStore::open(root.path())?;
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("b", Some(json!("second")))
        })?;
        assert_eq!(read_value(&store, "session", "a")?, Some(json!("first")));
        assert_eq!(read_value(&store, "session", "b")?, Some(json!("second")));
        Ok(())
    }

    #[test]
    fn another_hosts_write_invalidates_the_cached_document() -> Result<()> {
        let root = TempRoot::new("read-only-cache-host");
        let reader = FileStore::open(root.path())?;
        let writer = FileStore::open(root.path())?;
        writer.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("a", Some(json!(1)))
        })?;
        assert_eq!(read_value(&reader, "session", "a")?, Some(json!(1)));
        writer.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("a", Some(json!(2)))
        })?;
        assert_eq!(
            read_value(&reader, "session", "a")?,
            Some(json!(2)),
            "the reader must not serve the bytes it read before the other host wrote"
        );
        Ok(())
    }

    #[test]
    fn a_read_only_transaction_rejects_writes() -> Result<()> {
        let root = TempRoot::new("read-only-write");
        let store = FileStore::open(root.path())?;
        let error = store
            .read_transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
                txn.put("a", Some(json!(1)))
            })
            .unwrap_err();
        assert!(error.to_string().contains("read-only"), "{error}");
        let error = crate::store::read(&store, "session", |txn: &mut dyn Txn| {
            txn.schedule("queue", "work", 0, None)
        })
        .unwrap_err();
        assert!(error.to_string().contains("read-only"), "{error}");
        Ok(())
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
        assert_eq!(
            file_count(&root.path().join("scopes")),
            3,
            "one document, its index and one tail, no temporary file"
        );
        assert_eq!(file_count(&root.path().join("work")), 0);
        // The OS mode leaves its lock file behind by design: it is the *name* of the lock, and
        // keeping it is what makes the next transaction cost nothing. What must not stay behind is
        // a temporary file or a body somebody has to judge — neither exists in this mode.
        let locks = root.path().join("locks");
        assert_eq!(file_count(&locks), 1, "one long-lived lock file");
        let lock = fs::read_dir(&locks)?
            .next()
            .ok_or_else(|| DurableError::invalid("no lock file"))??;
        assert_eq!(
            fs::metadata(lock.path())?.len(),
            0,
            "the lock file is a name, never a body"
        );
        Ok(())
    }

    #[test]
    fn an_os_lock_file_is_never_rewritten() -> Result<()> {
        let root = TempRoot::new("os-lock");
        let store = FileStore::open(root.path())?;
        let locks = root.path().join("locks");
        for round in 0..5 {
            store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
                txn.put("n", Some(json!(round)))
            })?;
        }
        // One file for the scope; the readiness index was never scheduled, so no second one.
        assert_eq!(file_count(&locks), 1);
        let lock = fs::read_dir(&locks)?
            .next()
            .ok_or_else(|| DurableError::invalid("no lock file"))??
            .path();
        let before = fs::metadata(&lock)?;
        assert_eq!(before.len(), 0, "the lock file is never written to");
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("n", Some(json!(99)))
        })?;
        let after = fs::metadata(&lock)?;
        assert_eq!(
            before.modified().ok(),
            after.modified().ok(),
            "a transaction must not touch the lock file at all"
        );
        Ok(())
    }

    /// Two stores of this process on one root are two *hosts* as far as the lock is concerned:
    /// their gates are separate and only the OS lock keeps them apart.
    #[test]
    fn two_stores_on_one_root_do_not_lose_each_others_updates() -> Result<()> {
        let root = TempRoot::new("os-exclusive");
        let first = Arc::new(FileStore::open(root.path())?);
        let second = Arc::new(FileStore::open(root.path())?);
        let bump = |store: &FileStore, hold: Option<Duration>| -> Result<()> {
            store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
                let value = txn.get("v")?.and_then(|value| value.as_i64()).unwrap_or(0);
                if let Some(hold) = hold {
                    // Still holding the lock: a writer that does not exclude would read the same
                    // zero and overwrite this increment.
                    std::thread::sleep(hold);
                }
                txn.put("v", Some(json!(value + 1)))
            })
        };
        bump(&first, None)?;
        let (taken, taken_rx) = std::sync::mpsc::channel();
        let holder = {
            let first = Arc::clone(&first);
            std::thread::spawn(move || {
                let outcome =
                    first.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
                        let value = txn.get("v")?.and_then(|value| value.as_i64()).unwrap_or(0);
                        taken.send(()).ok();
                        std::thread::sleep(Duration::from_millis(150));
                        txn.put("v", Some(json!(value + 1)))
                    });
                outcome.map_err(|error| error.to_string())
            })
        };
        taken_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the first writer takes the lock");
        let started = Instant::now();
        bump(&second, None)?;
        let waited = started.elapsed();
        holder.join().expect("writer thread").expect("first write");
        assert!(
            waited >= Duration::from_millis(100),
            "the second store must wait for the first: {waited:?}"
        );
        assert_eq!(
            read_value(&first, "session", "v")?,
            Some(json!(3)),
            "both increments must survive"
        );
        Ok(())
    }

    /// The conservative mode keeps the create-if-absent lock, and deletes it again.
    #[test]
    fn the_conservative_mode_still_takes_and_drops_its_lock_file() -> Result<()> {
        let root = TempRoot::new("conservative");
        let store = FileStore::open_conservative(root.path())?;
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("a", Some(json!(1)))
        })?;
        assert_eq!(
            file_count(&root.path().join("locks")),
            0,
            "the file mode removes its lock again"
        );
        assert_eq!(read_value(&store, "session", "a")?, Some(json!(1)));
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
    fn keys_merge_staged_writes_over_the_document() -> Result<()> {
        let root = TempRoot::new("keys-staged");
        let store = FileStore::open(root.path())?;
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            for key in ["p:1", "p:2", "p:3"] {
                txn.put(key, Some(json!(key)))?;
            }
            Ok(())
        })?;
        // One transaction that deletes the first, replaces the second and appends a fourth: the
        // page must be the document with those three edits, in order.
        let merged = crate::store::txn(&store, "session", |txn: &mut dyn Txn| {
            txn.put("p:1", None)?;
            txn.put("p:2", Some(json!("replaced")))?;
            txn.put("p:4", Some(json!("p:4")))?;
            txn.keys("p:", None, 10)
        })?;
        assert_eq!(merged, vec!["p:2", "p:3", "p:4"]);
        Ok(())
    }

    #[test]
    fn read_parts_answers_from_the_document_and_sees_a_rewrite() -> Result<()> {
        let root = TempRoot::new("read-parts");
        let store = FileStore::open(root.path())?;
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("a", Some(json!(1)))?;
            txn.put("b", Some(json!(2)))
        })?;
        // The scan reads the document, so the fold has to happen first.
        store.compact_scope("session")?;
        let first = store.read_parts("session", &["a", "b", "missing"])?;
        assert_eq!(first, vec![Some(json!(1)), Some(json!(2)), None]);
        assert_eq!(store.scans(), 1, "the first question scans the document");

        // The same question against an unchanged document is answered from the last scan: one
        // scan for the two calls, which is what makes a control poll cheap.
        assert_eq!(store.read_parts("session", &["a", "b", "missing"])?, first);
        assert_eq!(
            store.scans(),
            1,
            "the second question is served from the cache"
        );

        // A rewrite is a new document, so the cached answer is not used — and it is asked with the
        // same parts list, so the parts cannot be what tells the two documents apart: only the
        // stamp can. The rewrite keeps the document the same length, so the length alone would
        // not catch it either.
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("a", Some(json!(3)))
        })?;
        assert_eq!(
            store.read_parts("session", &["a", "b", "missing"])?,
            vec![Some(json!(3)), Some(json!(2)), None]
        );
        assert_eq!(store.scans(), 2, "the rewrite is scanned, not served stale");

        // A scope that was never written answers `None`.
        assert_eq!(store.read_parts("other", &["a"])?, vec![None]);
        Ok(())
    }

    #[test]
    fn the_document_index_places_exactly_what_a_scan_reads() -> Result<()> {
        // Every shape the index has to place: a quoted quote, an escaped newline, a multi-byte
        // character, an array holding an object, a string that contains the closing braces of its
        // container, an empty object and a null.
        let bytes = r#"{"a":{"t":"q\"n\nu✓"},"b":[1,{"c":"]}"}],"d":{},"e":null}"#.as_bytes();
        let index = index_document(bytes)?;
        assert!(index.complete, "every range lands inside the document");
        assert_eq!(
            index.keys().len(),
            4,
            "a, b, d and e are the top-level parts"
        );
        assert_eq!(index.value(bytes, "a")?, Some(json!({"t": "q\"n\nu✓"})));
        assert_eq!(index.value(bytes, "b")?, Some(json!([1, {"c": "]}"}])));
        assert_eq!(index.value(bytes, "d")?, Some(json!({})));
        assert_eq!(index.value(bytes, "e")?, Some(Value::Null));
        assert_eq!(index.value(bytes, "missing")?, None);
        Ok(())
    }

    #[test]
    fn a_root_of_another_format_is_refused() -> Result<()> {
        let root = TempRoot::new("format");
        FileStore::open(root.path())?;
        assert!(
            !root.path().join("format.json").exists(),
            "version 1 declares nothing"
        );
        fs::write(root.path().join("format.json"), br#"{"version":99}"#)?;
        let error = match FileStore::open(root.path()) {
            Ok(_) => panic!("a root of another format must not open"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("store format 99"),
            "the refusal names the version: {error}"
        );
        Ok(())
    }

    #[test]
    fn a_transaction_reads_several_records_through_one_index() -> Result<()> {
        let root = TempRoot::new("index-transaction");
        let store = FileStore::open(root.path())?;
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("a", Some(json!({"text": "one\"two"})))?;
            txn.put("b", Some(json!([1, 2, 3])))?;
            txn.put("c", Some(json!({"nested": {"deep": [{"x": "}"}]}})))
        })?;
        // The fold puts every record in the document; the reads below all come out of one index.
        store.compact_scope("session")?;
        let mut values = Vec::new();
        store.read_transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            for part in ["a", "b", "c", "missing"] {
                values.push(txn.get(part)?);
            }
            Ok(())
        })?;
        assert_eq!(values[0], Some(json!({"text": "one\"two"})));
        assert_eq!(values[1], Some(json!([1, 2, 3])));
        assert_eq!(values[2], Some(json!({"nested": {"deep": [{"x": "}"}]}})));
        assert_eq!(values[3], None);
        Ok(())
    }

    #[test]
    fn an_indexed_get_reads_only_the_part() -> Result<()> {
        let root = TempRoot::new("indexed-get");
        let store = FileStore::open(root.path())?;
        // Below the blob threshold, so the document really carries them.
        let big = "x".repeat(8 * 1024);
        // One part whose name holds a control character, which the document escapes: the index
        // counts the key's own bytes and must count the escaped ones.
        let odd = "k\u{1}odd";
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("a", Some(json!({"text": big.clone()})))?;
            txn.put("b", Some(json!({"text": big.clone()})))?;
            txn.put(odd, Some(json!(3)))
        })?;
        store.compact_scope("session")?;

        // The index has to place the records exactly where the document bytes do: a range off by
        // one byte parses as something else, or not at all.
        let document = fs::read(store.scope_path("session"))?;
        let placed = index_document(&document)?;
        assert!(placed.complete, "every range lands inside the document");
        let expected: BTreeMap<String, (u64, u64)> = placed
            .slots
            .iter()
            .map(|(part, (start, end))| (part.clone(), (*start as u64, *end as u64)))
            .collect();
        let written = fs::read(store.index_path("session"))?;
        let index: ScopeIndex = serde_json::from_slice(&written)?;
        assert_eq!(index.parts, expected, "the index places what a scan reads");
        let stamp = store
            .scope_stamp("session")
            .expect("the fold writes a document");
        assert_eq!(
            scope_index_stamp(&stamp),
            Some(index.stamp),
            "the index describes the document that landed"
        );

        // A write transaction is what reads records without rewriting the document.
        let documents = store.document_reads();
        let ranges = store.range_reads();
        let value = crate::store::txn(&store, "session", |txn: &mut dyn Txn| txn.get("a"))?;
        assert_eq!(value, Some(json!({"text": big})));
        assert_eq!(
            store.document_reads(),
            documents,
            "the document is not read"
        );
        assert_eq!(store.range_reads(), ranges + 1, "its record is");

        // A part the document does not hold is a miss the index answers too.
        let asked = crate::store::txn(&store, "session", |txn: &mut dyn Txn| {
            Ok((txn.get(odd)?, txn.get("c")?))
        })?;
        assert_eq!(asked, (Some(json!(3)), None));
        assert_eq!(store.document_reads(), documents, "still no document read");
        Ok(())
    }

    #[test]
    fn a_stale_index_falls_back_to_the_document() -> Result<()> {
        let root = TempRoot::new("stale-index");
        let store = FileStore::open(root.path())?;
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("a", Some(json!(1)))
        })?;
        store.compact_scope("session")?;

        // A document replaced without its index — by another host, or by a build that wrote none —
        // leaves ranges describing bytes that are gone. The new value has the length of the old
        // one, so only the file's identity can tell the two documents apart.
        let mut document: RawDocument =
            serde_json::from_slice(&fs::read(store.scope_path("session"))?)?;
        document.insert("a".to_string(), raw_of(&json!(7))?);
        write_json(&store.scope_path("session"), &document)?;
        let written = fs::read(store.index_path("session"))?;
        let index: ScopeIndex = serde_json::from_slice(&written)?;
        let stamp = store.scope_stamp("session").expect("the document exists");
        assert_ne!(
            scope_index_stamp(&stamp),
            Some(index.stamp),
            "the rewrite leaves the index describing bytes that are gone"
        );

        let documents = store.document_reads();
        let value = crate::store::txn(&store, "session", |txn: &mut dyn Txn| txn.get("a"))?;
        assert_eq!(
            value,
            Some(json!(7)),
            "a stale index must not answer with another document's bytes"
        );
        assert_eq!(
            store.document_reads(),
            documents + 1,
            "the read falls back to the document"
        );
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

    /// A work record written by the pre-shard layout is adopted, not lost.
    ///
    /// The readiness index is one directory per queue now, so a record left in the flat directory
    /// by an older build would be invisible — and a thread nothing schedules never runs again.
    #[test]
    fn a_record_from_the_flat_index_is_adopted_at_open() -> Result<()> {
        let root = TempRoot::new("work-adopt");
        let legacy = root.path().join("work");
        fs::create_dir_all(&legacy)?;
        let record = WorkRecord {
            queue: "thread:h".to_string(),
            work_id: "main".to_string(),
            ready_at: 1,
            payload: Some(json!({ "legacy": true })),
        };
        let name = digest(&format!("thread:h\u{0}main"));
        fs::write(
            legacy.join(format!("{name}.json")),
            serde_json::to_vec(&record)?,
        )?;

        let store = FileStore::open(root.path())?;
        let due = store.ready("thread:h", 10, 10)?;
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].payload, Some(json!({ "legacy": true })));
        assert_eq!(
            file_count(&legacy),
            1,
            "only the queue directory is left behind"
        );
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
    // Out-of-line payloads
    // -----------------------------------------------------------------------

    /// A payload well above [`BLOB_THRESHOLD`], distinct per `marker`.
    fn big_payload(marker: &str) -> Value {
        json!({
            "marker": marker,
            "messages": (0..40)
                .map(|index| format!("{index}:{}", "x".repeat(8 * 1024)))
                .collect::<Vec<_>>(),
        })
    }

    fn blob_files(store: &FileStore, scope: &str) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = fs::read_dir(store.blob_dir(scope))
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                    .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        files
    }

    #[test]
    fn a_large_value_is_stored_out_of_line_and_the_scope_shrinks() -> Result<()> {
        let root = TempRoot::new("blob-large");
        let store = FileStore::open(root.path())?;
        let payload = big_payload("large");
        let inline = serde_json::to_vec(&payload)?.len() as u64;

        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("big", Some(payload.clone()))
        })?;
        let stored = store
            .scope_bytes("session")?
            .expect("a file store measures");
        eprintln!("BLOB scope_bytes inline={inline} stored={stored}");
        assert!(
            inline > BLOB_THRESHOLD as u64 * 10,
            "the payload is worth moving: {inline}"
        );
        assert!(
            stored < 4 * 1024,
            "the document holds a reference: {stored}"
        );
        assert!(
            stored * 50 < inline,
            "{inline} inline bytes became {stored} stored bytes"
        );
        assert_eq!(blob_files(&store, "session").len(), 1);

        // Committing the same payload again writes the document and nothing else: the blob is
        // already there under the digest that names it. The blob's own timestamp is the witness —
        // the process-wide write counters are shared with every test running beside this one.
        let blob = blob_files(&store, "session").pop().expect("the blob");
        let untouched = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&blob)?
            .set_modified(untouched)?;
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("big", Some(payload.clone()))
        })?;
        assert_eq!(
            fs::metadata(&blob)?.modified()?,
            untouched,
            "the stored payload was not written again"
        );
        assert!(store.scope_bytes("session")?.expect("measures") < 4 * 1024);
        assert_eq!(read_value(&store, "session", "big")?, Some(payload));
        Ok(())
    }

    #[test]
    fn a_stored_payload_reads_back_byte_for_byte() -> Result<()> {
        let root = TempRoot::new("blob-round-trip");
        let store = FileStore::open(root.path())?;
        // Non-ASCII, escapes, nested containers and numbers: everything a JSON round trip can
        // change by accident if it goes through a canonical form instead of the stored bytes.
        let payload = json!({
            "text": format!(
                "{}{}",
                "会话状态\u{1F600}\"\\\n\t".repeat(4000),
                "x".repeat(64 * 1024)
            ),
            "nested": { "list": [1, 2, 3.5, true, null], "empty": {} },
        });
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("big", Some(payload.clone()))
        })?;
        let read = read_value(&store, "session", "big")?.expect("stored");
        assert_eq!(serde_json::to_vec(&read)?, serde_json::to_vec(&payload)?);

        // The control-poll path resolves a reference the same way.
        let picked = store.read_parts("session", &["big"])?;
        let picked = picked[0].clone().expect("picked");
        assert_eq!(serde_json::to_vec(&picked)?, serde_json::to_vec(&payload)?);
        Ok(())
    }

    #[test]
    fn a_document_written_before_blobs_existed_still_reads() -> Result<()> {
        let root = TempRoot::new("blob-legacy");
        let store = FileStore::open(root.path())?;
        // Over the roll-out threshold: this is the 27 MB scope the pass exists for, in miniature.
        let payload = json!({ "text": "y".repeat(1500 * 1024) });
        // An older build wrote the payload inline, straight into the scope document.
        let document = json!({ "legacy": payload, "small": 1 });
        fs::write(store.scope_path("session"), serde_json::to_vec(&document)?)?;
        let before = store
            .scope_bytes("session")?
            .expect("a file store measures");
        assert!(
            before > ROLL_OUT_BYTES,
            "the legacy document is big: {before}"
        );
        assert_eq!(
            read_value(&store, "session", "legacy")?,
            Some(payload.clone())
        );

        // The next commit of any part moves the legacy payload out of line, once. A commit only
        // appends its delta; the fold is the pass that sees, and moves, the whole document.
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("small", Some(json!(2)))
        })?;
        store.compact_scope("session")?;
        let after = store
            .scope_bytes("session")?
            .expect("a file store measures");
        eprintln!("BLOB legacy scope_bytes {before} -> {after}");
        assert!(after < 4 * 1024, "the legacy payload moved: {after}");
        assert_eq!(read_value(&store, "session", "legacy")?, Some(payload));
        Ok(())
    }

    #[test]
    fn the_digest_of_a_payload_does_not_change_when_it_is_stored_out_of_line() -> Result<()> {
        let root = TempRoot::new("blob-digest");
        let store = FileStore::open(root.path())?;
        let payload = big_payload("digest");
        // What an event id or an execution id is derived from.
        let digest = crate::json::hash(&payload)?;
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("big", Some(payload.clone()))
        })?;
        let read = read_value(&store, "session", "big")?.expect("stored");
        assert_eq!(crate::json::hash(&read)?, digest);

        // The document itself holds the reference, so a digest of the *document* differs — which
        // is exactly why callers must only ever see the payload, never the reference.
        store.compact_scope("session")?;
        let document: Value = serde_json::from_slice(&fs::read(store.scope_path("session"))?)?;
        assert_ne!(crate::json::hash(&document)?, digest);
        Ok(())
    }

    #[test]
    fn two_hosts_storing_the_same_payload_leave_one_copy() {
        let root = TempRoot::new("blob-concurrent");
        let hosts: Vec<Arc<FileStore>> = (0..2)
            .map(|_| FileStore::open(root.path()).map(Arc::new))
            .collect::<Result<Vec<_>>>()
            .expect("hosts");
        let payload = Arc::new(big_payload("concurrent"));
        let expected = Arc::clone(&payload);
        run_concurrently(4, move |thread| {
            let store = Arc::clone(&hosts[thread % hosts.len()]);
            let scope = format!("session-{}", thread % 2);
            for _ in 0..5 {
                store
                    .transaction(&scope, &mut |txn: &mut dyn Txn| -> Result<()> {
                        txn.put("big", Some((*payload).clone()))
                    })
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
        });

        // Blobs are per scope (that is what makes a sweep safe), so two scopes keep two files and
        // neither host's concurrent write left a second copy or a broken one.
        let store = FileStore::open(root.path()).expect("reopened store");
        for scope in ["session-0", "session-1"] {
            assert_eq!(blob_files(&store, scope).len(), 1, "one copy in {scope}");
            assert_eq!(
                read_value(&store, scope, "big").expect("read"),
                Some((*expected).clone())
            );
        }
    }

    #[test]
    fn a_blob_nothing_references_is_reclaimed_and_the_rest_still_reads() -> Result<()> {
        let root = TempRoot::new("blob-gc");
        let store = FileStore::open(root.path())?;
        let kept = big_payload("kept");
        let dropped = big_payload("dropped");
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("kept", Some(kept.clone()))?;
            txn.put("dropped", Some(dropped.clone()))
        })?;
        assert_eq!(blob_files(&store, "session").len(), 2);

        // The sweep only deletes a blob older than its grace, so age the one that is about to lose
        // its last reference. It is the reader that parsed the document before the removing
        // commit that the grace protects.
        let dropped_blob = store.blob_path("session", &sha256_hex(&serde_json::to_vec(&dropped)?));
        std::fs::File::options()
            .write(true)
            .open(&dropped_blob)?
            .set_modified(std::time::SystemTime::now() - 2 * BLOB_GC_GRACE)?;

        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("dropped", None)
        })?;
        // The sweep runs in the fold: it is the only pass that sees the whole document.
        store.compact_scope("session")?;
        assert!(!dropped_blob.exists(), "the unreferenced blob is gone");
        assert_eq!(blob_files(&store, "session").len(), 1);
        assert_eq!(read_value(&store, "session", "kept")?, Some(kept));
        assert_eq!(read_value(&store, "session", "dropped")?, None);
        Ok(())
    }

    #[test]
    fn a_payload_under_the_threshold_stays_in_the_document() -> Result<()> {
        let root = TempRoot::new("blob-small");
        let store = FileStore::open(root.path())?;
        let payload = json!({ "text": "x".repeat(BLOB_THRESHOLD - 32) });
        assert!(serde_json::to_vec(&payload)?.len() <= BLOB_THRESHOLD);
        store.transaction("session", &mut |txn: &mut dyn Txn| -> Result<()> {
            txn.put("small", Some(payload.clone()))
        })?;
        assert!(blob_files(&store, "session").is_empty());
        assert_eq!(read_value(&store, "session", "small")?, Some(payload));
        Ok(())
    }

    #[test]
    fn only_the_exact_reference_shape_is_a_reference() {
        // A part whose own JSON happens to carry a `$blob` field must keep being read as data.
        assert!(blob_reference_of(&json!({
            "$blob": { "digest": "not a digest", "bytes": 1, "kind": "json" }
        }))
        .is_none());
        assert!(blob_reference_of(&json!({
            "$blob": { "digest": "a".repeat(64), "bytes": 1, "kind": "json" },
            "extra": true
        }))
        .is_none());
        assert!(blob_reference_of(&json!({
            "$blob": { "digest": "a".repeat(64), "bytes": 1, "kind": "gzip" }
        }))
        .is_none());
        assert!(blob_reference_of(&blob_reference(&"a".repeat(64), 7)).is_some());
        assert_eq!(BLOB_THRESHOLD, 16 * 1024);
        assert_eq!(BLOB_MARKER, "$blob");
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
