//! An in-memory blob store, for tests and for a process that wants the object API without a disk.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use crate::layout;
use crate::{ObjectError, ObjectStore, Result};

/// Blobs held in a `BTreeMap`, keyed by scope and digest.
///
/// The map is ordered by that key, so [`ObjectStore::list`] is the same ascending walk the local
/// store answers with — a test can use either implementation without changing its expectations.
#[derive(Debug, Default)]
pub struct MemoryObjectStore {
    blobs: Mutex<BTreeMap<(String, String), Vec<u8>>>,
}

impl MemoryObjectStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many objects are held, across every scope.
    pub fn blob_count(&self) -> usize {
        self.blobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

impl ObjectStore for MemoryObjectStore {
    fn put(&self, scope: &str, digest: &str, bytes: &[u8]) -> Result<()> {
        check(scope, digest)?;
        let mut blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
        // Idempotent like the local store: the key is the content, so an existing entry is the
        // payload and a repeated put does not replace it.
        blobs
            .entry((scope.to_string(), digest.to_string()))
            .or_insert_with(|| bytes.to_vec());
        Ok(())
    }

    fn get(&self, scope: &str, digest: &str) -> Result<Vec<u8>> {
        check(scope, digest)?;
        let blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
        blobs
            .get(&(scope.to_string(), digest.to_string()))
            .cloned()
            .ok_or_else(|| not_found(scope, digest))
    }

    fn delete(&self, scope: &str, digest: &str) -> Result<()> {
        check(scope, digest)?;
        let mut blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
        // Removing an absent object is what a replayed delete does; it is not an error.
        blobs.remove(&(scope.to_string(), digest.to_string()));
        Ok(())
    }

    fn list(&self, scope: &str, after: Option<&str>, limit: usize) -> Result<Vec<String>> {
        if !layout::valid_scope(scope) {
            return Err(ObjectError::invalid("blob scope must not be empty"));
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
        let mut digests = Vec::new();
        for (stored_scope, digest) in blobs.keys() {
            if stored_scope != scope {
                continue;
            }
            if after.is_some_and(|cursor| digest.as_str() <= cursor) {
                continue;
            }
            digests.push(digest.clone());
            if digests.len() >= limit {
                break;
            }
        }
        Ok(digests)
    }
}

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
