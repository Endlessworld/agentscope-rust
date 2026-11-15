//! The on-disk layout of a blob, shared by every implementation that has a file system.
//!
//! These are the rules `agentscope_durable::store::file` already uses
//! (`FileStore::blob_dir` / `FileStore::blob_path`, `store/file.rs`), reproduced here so a
//! [`LocalObjectStore`](crate::LocalObjectStore) and a file store can read each other's objects:
//!
//! ```text
//! <root>/blobs/<sha256(scope)>/<digest>.json
//! ```
//!
//! The `.json` suffix is part of the file store's layout, not a detail of this crate: a blob is the
//! JSON encoding of a value, and reading a root written by the other side has to find exactly that
//! name.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The directory under a root that holds every scope's blobs.
pub const BLOB_DIR: &str = "blobs";

/// The suffix of a blob file; see the module note.
pub const BLOB_SUFFIX: &str = ".json";

/// The length of a SHA-256 digest in hex digits.
pub const DIGEST_LEN: usize = 64;

/// The hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(DIGEST_LEN);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// The digest a caller must use for `bytes`.
pub fn digest_of(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

/// The directory name of `scope`: scopes are hashed, so a scope string never has to be a legal path.
pub fn scope_dir(scope: &str) -> String {
    sha256_hex(scope.as_bytes())
}

/// The file name of `digest`.
pub fn blob_file(digest: &str) -> String {
    let mut name = String::with_capacity(digest.len() + BLOB_SUFFIX.len());
    name.push_str(digest);
    name.push_str(BLOB_SUFFIX);
    name
}

/// Whether `digest` can name an object: 64 hex digits, nothing else.
///
/// This is what keeps a digest from becoming a path (`..`, a separator, a drive letter) and what
/// makes a malformed reference fail as data instead of as a missing file somewhere else.
pub fn valid_digest(digest: &str) -> bool {
    digest.len() == DIGEST_LEN && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Whether `scope` can name a namespace; the empty scope cannot.
pub fn valid_scope(scope: &str) -> bool {
    !scope.is_empty()
}

/// `<root>/blobs/<sha256(scope)>`.
pub fn scope_path(root: &Path, scope: &str) -> PathBuf {
    root.join(BLOB_DIR).join(scope_dir(scope))
}

/// `<root>/blobs/<sha256(scope)>/<digest>.json`.
pub fn blob_path(root: &Path, scope: &str, digest: &str) -> PathBuf {
    scope_path(root, scope).join(blob_file(digest))
}
