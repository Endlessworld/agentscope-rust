//! What a blob store has to promise: a round trip, an idempotent put, a concurrent put that does
//! not fail, a stable page order, an idempotent delete, and the layout the durable file store
//! already writes.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentscope_durable_objects::layout::{digest_of, sha256_hex};
use agentscope_durable_objects::{LocalObjectStore, MemoryObjectStore, ObjectError, ObjectStore};
use sha2::{Digest, Sha256};

const SCOPE: &str = "thread-for-tests";

/// A directory under the system temporary directory, removed when the test ends.
struct TempRoot(PathBuf);

impl TempRoot {
    fn new(name: &str) -> Self {
        let unique = format!(
            "agentscope-durable-objects-{}-{}-{name}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        );
        let root = std::env::temp_dir().join(unique);
        fs::create_dir_all(&root).expect("temp root");
        Self(root)
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

/// A JSON payload of a size worth externalizing, distinct per `tag`.
fn payload(tag: &str) -> Vec<u8> {
    format!("{{\"tag\":\"{tag}\",\"text\":\"{}\"}}", "x".repeat(4096)).into_bytes()
}

/// The digest and on-disk path of `bytes` as `agentscope_durable::store::file` computes them,
/// written out here independently so the interop test fails if the layout drifts.
fn file_store_location(root: &Path, scope: &str, bytes: &[u8]) -> (String, PathBuf) {
    let digest = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let scope_dir = Sha256::digest(scope.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let path = root
        .join("blobs")
        .join(scope_dir)
        .join(format!("{digest}.json"));
    (digest, path)
}

#[test]
fn a_stored_blob_comes_back_byte_for_byte() {
    let bytes = payload("round-trip");
    let digest = digest_of(&bytes);

    let root = TempRoot::new("round-trip");
    let local = LocalObjectStore::open(root.path()).expect("open local store");
    local.put(SCOPE, &digest, &bytes).expect("put local");
    assert_eq!(local.get(SCOPE, &digest).expect("get local"), bytes);

    let memory = MemoryObjectStore::new();
    memory.put(SCOPE, &digest, &bytes).expect("put memory");
    assert_eq!(memory.get(SCOPE, &digest).expect("get memory"), bytes);
}

#[test]
fn putting_one_digest_again_writes_nothing() {
    let bytes = payload("idempotent");
    let digest = digest_of(&bytes);
    let root = TempRoot::new("idempotent");
    let store = LocalObjectStore::open(root.path()).expect("open local store");

    store.put(SCOPE, &digest, &bytes).expect("first put");
    let stored = store.blob_path(SCOPE, &digest);
    let first = fs::metadata(&stored).expect("first metadata");

    // The name is the content: the second put must not rewrite the file, so its timestamp may not
    // move even though some time has passed.
    std::thread::sleep(Duration::from_millis(60));
    store.put(SCOPE, &digest, &bytes).expect("second put");
    let second = fs::metadata(&stored).expect("second metadata");

    assert_eq!(first.len(), second.len());
    assert_eq!(first.modified().ok(), second.modified().ok());
    assert_eq!(fs::read(&stored).expect("read blob"), bytes);
    assert_eq!(
        entries(store.blob_dir(SCOPE)).len(),
        1,
        "one file per digest"
    );

    let memory = MemoryObjectStore::new();
    memory.put(SCOPE, &digest, &bytes).expect("first put");
    memory.put(SCOPE, &digest, &bytes).expect("second put");
    assert_eq!(memory.blob_count(), 1);
    assert_eq!(memory.get(SCOPE, &digest).expect("get"), bytes);
}

#[test]
fn many_threads_writing_one_digest_all_succeed() {
    let bytes = payload("concurrent");
    let digest = digest_of(&bytes);
    let root = TempRoot::new("concurrent");
    let store = Arc::new(LocalObjectStore::open(root.path()).expect("open local store"));

    let mut threads = Vec::new();
    for thread in 0..8 {
        let store = Arc::clone(&store);
        let bytes = bytes.clone();
        let digest = digest.clone();
        threads.push(std::thread::spawn(move || {
            // One payload everybody writes, plus one only this thread writes.
            let own = payload(&format!("own-{thread}"));
            for round in 0..20 {
                store
                    .put(SCOPE, &digest, &bytes)
                    .unwrap_or_else(|error| panic!("thread {thread} round {round}: {error}"));
                store
                    .put(SCOPE, &digest_of(&own), &own)
                    .unwrap_or_else(|error| panic!("thread {thread} round {round}: {error}"));
            }
        }));
    }
    for thread in threads {
        thread.join().expect("no thread panics");
    }

    assert_eq!(store.get(SCOPE, &digest).expect("get shared"), bytes);
    let listed = store.list(SCOPE, None, 1024).expect("list");
    assert_eq!(listed.len(), 9, "one shared payload and one per thread");
    assert!(
        entries(store.blob_dir(SCOPE))
            .iter()
            .all(|name| name.ends_with(".json")),
        "no temporary file is left behind"
    );
}

#[test]
fn a_listing_pages_through_the_digests_in_order() {
    let root = TempRoot::new("paging");
    let local = LocalObjectStore::open(root.path()).expect("open local store");
    let memory = MemoryObjectStore::new();
    let mut expected = Vec::new();
    for index in 0..5 {
        let bytes = format!("blob-{index}").into_bytes();
        let digest = digest_of(&bytes);
        local.put(SCOPE, &digest, &bytes).expect("put local");
        memory.put(SCOPE, &digest, &bytes).expect("put memory");
        expected.push(digest);
    }
    expected.sort_unstable();

    for store in [&local as &dyn ObjectStore, &memory as &dyn ObjectStore] {
        let first = store.list(SCOPE, None, 2).expect("first page");
        assert_eq!(first, expected[..2]);
        let second = store
            .list(SCOPE, Some(first.last().expect("cursor")), 2)
            .expect("second page");
        assert_eq!(second, expected[2..4]);
        let third = store
            .list(SCOPE, Some(second.last().expect("cursor")), 2)
            .expect("third page");
        assert_eq!(third, expected[4..]);
        let done = store
            .list(SCOPE, Some(third.last().expect("cursor")), 2)
            .expect("past the end");
        assert!(done.is_empty());
        assert!(store.list(SCOPE, None, 0).expect("empty limit").is_empty());
        // A scope nothing was stored in, and a scope the other one written to, are both empty.
        assert!(store
            .list("another-scope", None, 8)
            .expect("other scope")
            .is_empty());
    }
}

#[test]
fn a_deleted_blob_is_gone_and_deleting_it_again_is_fine() {
    let bytes = payload("delete");
    let digest = digest_of(&bytes);
    let root = TempRoot::new("delete");
    let local = LocalObjectStore::open(root.path()).expect("open local store");
    let memory = MemoryObjectStore::new();
    local.put(SCOPE, &digest, &bytes).expect("put local");
    memory.put(SCOPE, &digest, &bytes).expect("put memory");

    for store in [&local as &dyn ObjectStore, &memory as &dyn ObjectStore] {
        store.delete(SCOPE, &digest).expect("delete");
        let error = store.get(SCOPE, &digest).expect_err("gone");
        assert!(matches!(error, ObjectError::NotFound { .. }), "{error}");
        store.delete(SCOPE, &digest).expect("repeated delete");
        assert!(store.list(SCOPE, None, 8).expect("list").is_empty());
    }
    assert!(!root
        .path()
        .join("blobs")
        .join(sha256_hex(SCOPE.as_bytes()))
        .join(format!("{digest}.json"))
        .exists());
}

#[test]
fn the_file_store_layout_is_read_and_written() {
    let root = TempRoot::new("interop");
    let bytes = payload("written-by-the-file-store");
    let (digest, path) = file_store_location(root.path(), SCOPE, &bytes);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdirs");
    fs::write(&path, &bytes).expect("write like the file store");

    let store = LocalObjectStore::open(root.path()).expect("open local store");
    // The store canonicalises its root, so compare resolved paths rather than spellings.
    assert_eq!(
        fs::canonicalize(store.blob_path(SCOPE, &digest)).expect("canonical blob path"),
        fs::canonicalize(&path).expect("canonical expected path"),
        "the object sits exactly where the file store expects it"
    );
    assert_eq!(
        store.get(SCOPE, &digest).expect("read file-store blob"),
        bytes
    );
    assert_eq!(
        store.list(SCOPE, None, 8).expect("list"),
        vec![digest.clone()]
    );

    // And the other direction: what this store puts is at the path the file store reads.
    let own = payload("written-by-the-object-store");
    let own_digest = digest_of(&own);
    store.put(SCOPE, &own_digest, &own).expect("put");
    let (expected_digest, expected_path) = file_store_location(root.path(), SCOPE, &own);
    assert_eq!(own_digest, expected_digest);
    assert_eq!(
        fs::read(&expected_path).expect("read from the file store's view"),
        own
    );
}

#[test]
fn requests_that_cannot_name_an_object_are_rejected() {
    let root = TempRoot::new("invalid");
    let local = LocalObjectStore::open(root.path()).expect("open local store");
    let memory = MemoryObjectStore::new();
    let bytes = payload("invalid");

    let short_digest = "a".repeat(63);
    for store in [&local as &dyn ObjectStore, &memory as &dyn ObjectStore] {
        for digest in ["", "not-a-digest", "../escape", short_digest.as_str()] {
            let error = store.put(SCOPE, digest, &bytes).expect_err("bad digest");
            assert!(matches!(error, ObjectError::Invalid(_)), "{error}");
            assert!(matches!(
                store.get(SCOPE, digest),
                Err(ObjectError::Invalid(_))
            ));
            assert!(matches!(
                store.delete(SCOPE, digest),
                Err(ObjectError::Invalid(_))
            ));
        }
        let error = store
            .put("", &digest_of(&bytes), &bytes)
            .expect_err("no scope");
        assert!(matches!(error, ObjectError::Invalid(_)), "{error}");
    }
    assert_eq!(memory.blob_count(), 0);
}

/// The names of the entries of `dir`, unsorted; a directory that does not exist has none.
fn entries(dir: PathBuf) -> Vec<String> {
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}
