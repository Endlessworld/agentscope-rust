//! A blob store on a local directory, in the layout the durable file store already uses.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::layout::{self, BLOB_DIR, BLOB_SUFFIX};
use crate::{ObjectError, ObjectStore, Result};

/// How often a file-sharing conflict is retried before the error is returned.
const HELD_RETRIES: u32 = 10;
const HELD_RETRY_BACKOFF: Duration = Duration::from_millis(25);
const HELD_RETRY_BACKOFF_MAX: Duration = Duration::from_millis(400);

/// Blobs in a directory tree, one directory per scope.
///
/// Opening a store creates only `blobs/`; the per-scope directories appear with the first object.
/// Two stores — of this process or of another one — may open the same root, and so may a
/// `agentscope_durable` file store: the layout is shared, publishing is an atomic rename, and
/// nothing here takes an exclusive lock or records an owner.
#[derive(Debug, Clone)]
pub struct LocalObjectStore {
    root: PathBuf,
}

impl LocalObjectStore {
    /// Opens the store at `root`, creating `<root>/blobs` when it is missing.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        retry_while_held(|| fs::create_dir_all(root.join(BLOB_DIR)))?;
        // Canonicalising keeps two spellings of one directory (`relative`, `absolute`, a `..` in
        // the middle) comparable when a caller hands the same root to several stores. The fallback
        // keeps the store usable where the file system refuses to canonicalise.
        let root = fs::canonicalize(&root).unwrap_or(root);
        Ok(Self { root })
    }

    /// The root this store reads and writes.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory holding `scope`'s blobs.
    pub fn blob_dir(&self, scope: &str) -> PathBuf {
        layout::scope_path(&self.root, scope)
    }

    /// The path of one blob.
    pub fn blob_path(&self, scope: &str, digest: &str) -> PathBuf {
        layout::blob_path(&self.root, scope, digest)
    }
}

impl ObjectStore for LocalObjectStore {
    fn put(&self, scope: &str, digest: &str, bytes: &[u8]) -> Result<()> {
        check(scope, digest)?;
        let path = self.blob_path(scope, digest);
        // The name is the content: a file of the expected length is the payload, so a repeated put
        // writes nothing. This is the check the file store's `store_blob` makes, for the same
        // reason — rewriting a 300 KB blob to learn it was already there is the cost the mechanism
        // exists to avoid.
        let present = match retry_while_held(|| fs::metadata(&path)) {
            Ok(meta) => meta.len() == bytes.len() as u64,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        if present {
            return Ok(());
        }
        retry_while_held(|| fs::create_dir_all(self.blob_dir(scope)))?;
        write_atomic(&path, bytes)
    }

    fn get(&self, scope: &str, digest: &str) -> Result<Vec<u8>> {
        check(scope, digest)?;
        let path = self.blob_path(scope, digest);
        match retry_while_held(|| fs::read(&path)) {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(not_found(scope, digest))
            }
            Err(error) => Err(error.into()),
        }
    }

    fn delete(&self, scope: &str, digest: &str) -> Result<()> {
        check(scope, digest)?;
        match retry_while_held(|| fs::remove_file(self.blob_path(scope, digest))) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn list(&self, scope: &str, after: Option<&str>, limit: usize) -> Result<Vec<String>> {
        if !layout::valid_scope(scope) {
            return Err(ObjectError::invalid("blob scope must not be empty"));
        }
        let dir = self.blob_dir(scope);
        let entries = match retry_while_held(|| fs::read_dir(&dir)) {
            Ok(entries) => entries,
            // A scope nothing has stored yet has no directory; that is an empty page, not an error.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut digests = Vec::new();
        for entry in entries {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(digest) = name.strip_suffix(BLOB_SUFFIX) else {
                continue;
            };
            // A temporary file of a concurrent put does not end in `.json`, and anything else here
            // is not an object this store wrote.
            if layout::valid_digest(digest) {
                digests.push(digest.to_string());
            }
        }
        digests.sort_unstable();
        let start = match after {
            Some(cursor) => digests.partition_point(|digest| digest.as_str() <= cursor),
            None => 0,
        };
        Ok(digests.into_iter().skip(start).take(limit).collect())
    }
}

/// Rejects a request that cannot name an object, before it reaches the file system.
fn check(scope: &str, digest: &str) -> Result<()> {
    if !layout::valid_scope(scope) {
        return Err(ObjectError::invalid("blob scope must not be empty"));
    }
    if !layout::valid_digest(digest) {
        return Err(ObjectError::invalid(format!(
            "blob digest must be 64 hex digits, got {digest:?}"
        )));
    }
    Ok(())
}

fn not_found(scope: &str, digest: &str) -> ObjectError {
    ObjectError::NotFound {
        scope: scope.to_string(),
        digest: digest.to_string(),
    }
}

/// Temporary file plus rename, so a reader sees the old object or the new one.
///
/// No `sync_all` and no directory fsync, for the reason the file store documents: the rename makes
/// a *process* crash safe; covering a power cut would cost a flush of every blob on every put.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
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
        // Two hosts racing on one digest: the other one published first and this rename lost. The
        // object is present either way, so only the cleanup is best effort.
        let _ = retry_while_held(|| fs::remove_file(&temporary));
        return Err(error.into());
    }
    Ok(())
}

/// Retries an operation only while the OS says another process holds the file.
///
/// The two sharing answers are `ERROR_ACCESS_DENIED` (5, also what a target held without
/// `FILE_SHARE_DELETE` produces) and `ERROR_SHARING_VIOLATION` (32); Rust maps none of them to a
/// dedicated kind, so the raw code is the only reliable test. Anything else — a read-only
/// directory, a missing file — fails on the first attempt instead of being swallowed by a loop.
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

/// A per-process counter, so two temporary names of one process never collide.
fn next_sequence() -> u64 {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    SEQUENCE.fetch_add(1, Ordering::Relaxed)
}
