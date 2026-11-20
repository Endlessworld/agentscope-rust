#![cfg(feature = "s3")]
//! The remote adapter keeps the two rules the durable store relies on.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentscope_durable_objects::s3::S3ObjectStore;
use agentscope_durable_objects::transport::ObjectTransport;
use agentscope_durable_objects::{layout, ObjectError, ObjectStore, Result};

#[derive(Default)]
struct FakeTransport {
    objects: Mutex<HashMap<(String, String), Vec<u8>>>,
    writes: AtomicUsize,
}

impl ObjectTransport for FakeTransport {
    fn get(&self, scope: &str, digest: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .objects
            .lock()
            .expect("objects")
            .get(&(scope.to_string(), digest.to_string()))
            .cloned())
    }

    fn put(&self, scope: &str, digest: &str, bytes: &[u8]) -> Result<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.objects
            .lock()
            .expect("objects")
            .insert((scope.to_string(), digest.to_string()), bytes.to_vec());
        Ok(())
    }

    fn delete(&self, scope: &str, digest: &str) -> Result<()> {
        self.objects
            .lock()
            .expect("objects")
            .remove(&(scope.to_string(), digest.to_string()));
        Ok(())
    }

    fn list(&self, scope: &str, after: Option<&str>, limit: usize) -> Result<Vec<String>> {
        let objects = self.objects.lock().expect("objects");
        let mut digests: Vec<String> = objects
            .keys()
            .filter(|(stored, _)| stored == scope)
            .map(|(_, digest)| digest.clone())
            .filter(|digest| after.is_none_or(|after| digest.as_str() > after))
            .collect();
        digests.sort();
        digests.truncate(limit);
        Ok(digests)
    }
}

fn store() -> (Arc<FakeTransport>, S3ObjectStore<FakeTransport>) {
    let transport = Arc::new(FakeTransport::default());
    let store = S3ObjectStore::new(Arc::clone(&transport));
    (transport, store)
}

#[test]
fn a_put_of_the_same_content_writes_once() {
    let (transport, store) = store();
    let bytes = b"{\"reply\":\"ok\"}";
    let digest = layout::digest_of(bytes);
    store.put("scope-a", &digest, bytes).expect("first put");
    store.put("scope-a", &digest, bytes).expect("second put");
    assert_eq!(transport.writes.load(Ordering::SeqCst), 1);
    assert_eq!(store.get("scope-a", &digest).expect("get"), bytes);
}

#[test]
fn a_missing_object_is_not_found() {
    let (_, store) = store();
    let digest = layout::digest_of(b"never stored");
    let error = store.get("scope-a", &digest).expect_err("missing object");
    assert!(matches!(error, ObjectError::NotFound { .. }));
}

#[test]
fn list_pages_by_digest() {
    let (_, store) = store();
    let mut digests = Vec::new();
    for index in 0..3u8 {
        let bytes = [index];
        let digest = layout::digest_of(&bytes);
        store.put("scope-a", &digest, &bytes).expect("put");
        digests.push(digest);
    }
    digests.sort();

    let first = store.list("scope-a", None, 2).expect("first page");
    assert_eq!(first, digests[..2].to_vec());
    let second = store
        .list("scope-a", Some(&first[1]), 2)
        .expect("second page");
    assert_eq!(second, digests[2..].to_vec());
}

#[test]
fn an_invalid_digest_is_rejected_before_the_transport() {
    let (transport, store) = store();
    let error = store
        .put("scope-a", "../escape", b"x")
        .expect_err("invalid digest");
    assert!(matches!(error, ObjectError::Invalid(_)));
    assert_eq!(transport.writes.load(Ordering::SeqCst), 0);
}
