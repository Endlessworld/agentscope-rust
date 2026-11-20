//! The port a remote object store implements.
//!
//! [`ObjectStore`](crate::ObjectStore) answers with bytes and reports a missing object as an
//! error. A remote bucket answers the same question with "absent" as a value, so the transport
//! port says `None` and the adapter ([`S3ObjectStore`](crate::s3::S3ObjectStore)) maps it.
//!
//! The transport owns the path mapping. Every implementation must place an object at
//! `blobs/<sha256(scope)>/<digest>.json`, which [`crate::layout::object_key`] builds, so a remote
//! bucket and a local file store can serve the same references.

use crate::Result;

/// The four answers a remote bucket has to give for content-addressed blobs.
pub trait ObjectTransport: Send + Sync {
    /// The bytes stored under `(scope, digest)`, or `None` when the object is absent.
    fn get(&self, scope: &str, digest: &str) -> Result<Option<Vec<u8>>>;

    /// Stores `bytes`, or does nothing when `(scope, digest)` already exists.
    fn put(&self, scope: &str, digest: &str, bytes: &[u8]) -> Result<()>;

    /// Removes the object; removing an absent object is not an error.
    fn delete(&self, scope: &str, digest: &str) -> Result<()>;

    /// Up to `limit` digests of `scope` in ascending order, strictly after `after` when given.
    fn list(&self, scope: &str, after: Option<&str>, limit: usize) -> Result<Vec<String>>;
}
