//! Content-addressed storage for the durable runtime's out-of-line payloads.
//!
//! [`agentscope_durable::store::file`] keeps a `Value` above 16 KiB out of its scope document and
//! stores it as `blobs/<sha256(scope)>/<sha256(content)>.json`, with the document holding
//! `{"$blob":{"digest","bytes","kind"}}`. This crate is that mechanism with the local directory
//! replaced by a trait, so the same references can be served by a local directory or, later, by an
//! object-storage bucket.
//!
//! The layout is deliberately the file store's own ([`layout`]):
//!
//! * the directory is `blobs/<sha256(scope)>` — one directory per scope, so a sweep of a scope
//!   never has to walk another scope, and nothing ever has to keep a cross-scope reference count;
//! * the file is `<digest>.json` — the payload is the JSON encoding of the value;
//! * the name *is* the content, so a write is idempotent by construction and two hosts writing one
//!   digest write the same bytes.
//!
//! Because path and encoding match, a [`LocalObjectStore`] and an `agentscope_durable` file store
//! can open the same root at the same time: neither owns it, both see the same objects, and the
//! atomic temp-then-rename publish means a reader sees the old object or the new one, never a torn
//! file. Nothing in this crate takes an exclusive lock, marks an owner or refuses a second host.
//!
//! Costs stay where the caller can reason about them: a `put` of an object that is already there
//! (same length, same name) writes nothing, `get` reads exactly one file, and `list` sorts only the
//! page it answers. Only the two file-sharing answers of the OS (raw errors 5 and 32) are retried;
//! every other error is returned on the first attempt.

pub mod layout;
mod local;
mod memory;
#[cfg(feature = "s3")]
pub mod s3;
pub mod transport;

pub use local::LocalObjectStore;
pub use memory::MemoryObjectStore;
pub use transport::ObjectTransport;

/// The result of an object-store operation.
pub type Result<T> = std::result::Result<T, ObjectError>;

/// Why an object-store operation failed.
#[derive(Debug)]
pub enum ObjectError {
    /// The object is not stored (and, for a delete, nothing needed to change).
    NotFound { scope: String, digest: String },
    /// The request cannot name an object: an empty scope or a digest that is not 64 hex digits.
    Invalid(String),
    /// The medium refused the operation for a reason that is not a sharing conflict.
    Io(std::io::Error),
}

impl ObjectError {
    /// A request that cannot name an object.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    /// The object this failure is about, when it is about one.
    pub fn digest(&self) -> Option<&str> {
        match self {
            Self::NotFound { digest, .. } => Some(digest),
            Self::Invalid(_) | Self::Io(_) => None,
        }
    }
}

impl std::fmt::Display for ObjectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { scope, digest } => {
                write!(formatter, "blob {digest} is not stored in scope {scope}")
            }
            Self::Invalid(message) => formatter.write_str(message),
            Self::Io(error) => write!(formatter, "object store: {error}"),
        }
    }
}

impl std::error::Error for ObjectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotFound { .. } | Self::Invalid(_) => None,
            Self::Io(error) => Some(error),
        }
    }
}

impl From<std::io::Error> for ObjectError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Content-addressed blobs, one namespace per scope.
///
/// The digest is the SHA-256 of `bytes` as 64 lowercase hex digits ([`layout::digest_of`]); it is
/// the caller's job to name the content it passes, exactly as it is for the file store, which
/// computes the digest right before calling [`ObjectStore::put`]. Verifying it here would hash the
/// payload a second time on every write, which is the one cost this mechanism exists to avoid.
///
/// Every method is `&self`: a store is shared, not owned, and implementations are expected to be
/// safe for concurrent use from several threads and from several processes.
pub trait ObjectStore {
    /// Stores `bytes` under `(scope, digest)`, or does nothing when it is already stored.
    ///
    /// Repeating a put is the normal case — two hosts externalizing one payload, or a retried
    /// commit — and costs no write when the object named by the digest is already present.
    fn put(&self, scope: &str, digest: &str, bytes: &[u8]) -> Result<()>;

    /// The bytes stored under `(scope, digest)`.
    fn get(&self, scope: &str, digest: &str) -> Result<Vec<u8>>;

    /// Removes the object; removing one that is absent is not an error (idempotent delete).
    fn delete(&self, scope: &str, digest: &str) -> Result<()>;

    /// Up to `limit` digests of `scope` in ascending order, strictly after `after` when given.
    ///
    /// The order is the sort order of the digest strings, so a caller can page with the last
    /// digest it received as the cursor without the store keeping any index.
    fn list(&self, scope: &str, after: Option<&str>, limit: usize) -> Result<Vec<String>>;
}
