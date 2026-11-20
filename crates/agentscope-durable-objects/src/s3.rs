//! [`ObjectStore`] over an injected remote bucket transport.
//!
//! No cloud SDK is compiled here: credentials, endpoints and request signing belong to the
//! [`ObjectTransport`] implementation, exactly as `HttpTransport` owns the socket in the HTTP
//! crate. What this adapter owns is the two rules the durable store depends on:
//!
//! * a put of an object named by its own digest is a no-op when the object is already there
//!   (`If-None-Match: *` in bucket terms), because the name is the content;
//! * a missing object is [`ObjectError::NotFound`], not an empty payload.

use std::sync::Arc;

use crate::layout;
use crate::transport::ObjectTransport;
use crate::{ObjectError, ObjectStore, Result};

/// An [`ObjectStore`] backed by a remote bucket.
pub struct S3ObjectStore<T: ObjectTransport> {
    transport: Arc<T>,
}

impl<T: ObjectTransport> S3ObjectStore<T> {
    pub fn new(transport: Arc<T>) -> Self {
        Self { transport }
    }

    pub fn transport(&self) -> &Arc<T> {
        &self.transport
    }
}

fn check(scope: &str, digest: &str) -> Result<()> {
    if !layout::valid_scope(scope) {
        return Err(ObjectError::invalid("An object scope cannot be empty"));
    }
    if !layout::valid_digest(digest) {
        return Err(ObjectError::invalid(format!(
            "`{digest}` is not a SHA-256 digest"
        )));
    }
    Ok(())
}

impl<T: ObjectTransport> ObjectStore for S3ObjectStore<T> {
    fn put(&self, scope: &str, digest: &str, bytes: &[u8]) -> Result<()> {
        check(scope, digest)?;
        // If-None-Match: the name is the content, so a present object already holds these bytes.
        if self.transport.get(scope, digest)?.is_some() {
            return Ok(());
        }
        self.transport.put(scope, digest, bytes)
    }

    fn get(&self, scope: &str, digest: &str) -> Result<Vec<u8>> {
        check(scope, digest)?;
        self.transport
            .get(scope, digest)?
            .ok_or_else(|| ObjectError::NotFound {
                scope: scope.to_string(),
                digest: digest.to_string(),
            })
    }

    fn delete(&self, scope: &str, digest: &str) -> Result<()> {
        check(scope, digest)?;
        self.transport.delete(scope, digest)
    }

    fn list(&self, scope: &str, after: Option<&str>, limit: usize) -> Result<Vec<String>> {
        if !layout::valid_scope(scope) {
            return Err(ObjectError::invalid("An object scope cannot be empty"));
        }
        self.transport.list(scope, after, limit)
    }
}
