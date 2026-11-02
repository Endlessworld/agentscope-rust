//! Driver-agnostic MongoDB document-database abstraction.
//!
//! The Java module depends directly on `com.mongodb.client.MongoClient` /
//! `MongoCollection` / `MongoDatabase`. This crate instead models the small slice of the
//! driver API the stores actually need behind a [`MongoClient`] trait, so the store logic
//! stays driver-agnostic and fully testable offline:
//!
//! * document CRUD with server-side-ish comparison/update primitives, and
//! * index lifecycle ([`create_index`](MongoClient::create_index) etc.).
//!
//! Capability boundary (verified against the Java module): it never uses change streams
//! (`watch`), bulk writes (`insertMany` / `bulkWrite` / `updateMany` / `deleteMany`) or
//! multi-document transactions, so the trait surface below is complete for the migration.
//!
//! Two implementations are provided:
//! * [`InMemoryMongoClient`] — a faithful, fully-featured in-process backend used by the
//!   contract tests (and usable as an embedded, dependency-free store).
//! * A real driver (the workspace exposes `mongodb = "3"`) plugs in by implementing this
//!   same trait, without touching any of the store logic (see the crate tracker §4.2).
//!
//! The data path deliberately mirrors MongoDB semantics we depend on:
//! * documents live in named collections inside named databases;
//! * updates are expressed with [`Update`] operators (`$set`, `$inc`, `$push`,
//!   `$setOnInsert`, `$unset`);
//! * `find_one_and_update` is atomic with respect to concurrent callers;
//! * a conflicting insert reports [`MongoError::DuplicateKey`] (code 11000).

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use crate::constants::DEFAULT_DATABASE;
use crate::index_utils::IndexSpec;

/// Default GridFS chunk size (255 KB), matching the MongoDB driver default.
pub const GRIDFS_DEFAULT_CHUNK_SIZE: usize = 255 * 1024;

/// GridFS metadata field holding the snapshot id (stored as the GridFS `filename`).
pub const GRIDFS_FIELD_FILENAME: &str = "filename";
/// GridFS chunk-document field referencing the owning file `_id`.
pub const GRIDFS_FIELD_FILES_ID: &str = "files_id";
/// GridFS chunk-document field holding the chunk sequence number.
pub const GRIDFS_FIELD_CHUNK_N: &str = "n";
/// GridFS file-document field holding the total payload length in bytes.
pub const GRIDFS_FIELD_LENGTH: &str = "length";
/// GridFS file-document field holding the upload timestamp (millis since epoch).
pub const GRIDFS_FIELD_UPLOAD_DATE: &str = "uploadDate";
/// GridFS file-document field holding the free-form metadata sub-document.
pub const GRIDFS_FIELD_METADATA: &str = "metadata";
/// GridFS metadata key mirroring Java `META_CREATED_AT`.
pub const GRIDFS_META_CREATED_AT: &str = "createdAt";
/// Legacy single-document snapshot field holding the (base64-encoded) payload.
pub const LEGACY_FIELD_DATA: &str = "data";

/// The GridFS metadata collection for `bucket` (`<bucket>.files`).
pub fn gridfs_files_collection(bucket: &str) -> String {
    format!("{bucket}.files")
}

/// The GridFS chunk collection for `bucket` (`<bucket>.chunks`).
pub fn gridfs_chunks_collection(bucket: &str) -> String {
    format!("{bucket}.chunks")
}

/// Namespaces a logical collection name by database.
///
/// A real MongoDB server scopes collections per database, whereas the in-process
/// backend keeps a single flat map keyed by collection name. Encoding the database
/// into that key preserves the per-database isolation the snapshot client relies on
/// (`MongoRemoteSnapshotClient` is constructed with a database name).
///
/// `::` is used as the separator so it cannot be confused with the `.files` /
/// `.chunks` suffix GridFS appends to the bucket name.
pub fn scoped_collection(database: &str, collection: &str) -> String {
    format!("{database}::{collection}")
}

/// Encodes binary data the way it is stored in JSON documents (base64).
///
/// The real driver stores BSON `Binary`; JSON has no binary type, so the in-process
/// backend represents both GridFS chunk payloads and legacy `data` fields as base64.
pub fn encode_binary(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Decodes base64-encoded binary data; `None` for malformed input.
pub fn decode_binary(encoded: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()
}

/// MongoDB error codes we act on.
mod codes {
    pub const DUPLICATE_KEY: i32 = 11000;
    pub const INDEX_OPTIONS_CONFLICT: i32 = 85;
    pub const INDEX_NOT_FOUND: i32 = 27;
}

/// Error type for all MongoDB backend operations.
#[derive(Debug, Clone, thiserror::Error)]
pub enum MongoError {
    #[error("duplicate key error (11000)")]
    DuplicateKey,
    #[error("index options conflict (85)")]
    IndexOptionsConflict,
    #[error("index not found (27)")]
    IndexNotFound,
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("backend error: {0}")]
    Backend(String),
}

impl MongoError {
    /// Returns the numeric MongoDB error code for this error, when applicable.
    pub fn code(&self) -> Option<i32> {
        match self {
            MongoError::DuplicateKey => Some(codes::DUPLICATE_KEY),
            MongoError::IndexOptionsConflict => Some(codes::INDEX_OPTIONS_CONFLICT),
            MongoError::IndexNotFound => Some(codes::INDEX_NOT_FOUND),
            _ => None,
        }
    }
}

/// Result alias for MongoDB backend operations.
pub type MongoResult<T> = Result<T, MongoError>;

/// Builds the error reported when a GridFS file cannot be faithfully
/// reassembled (missing / out-of-order / malformed chunks).
fn corrupt_chunk(filename: &str, detail: &str) -> MongoError {
    MongoError::Backend(format!("corrupt GridFS file '{filename}': {detail}"))
}

/// Metadata describing an existing index.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexInfo {
    pub name: String,
    pub keys: Vec<(String, i32)>,
    pub unique: bool,
    pub sparse: bool,
    pub expire_after_seconds: Option<i64>,
}

/// A find filter: field path → expected value. An empty filter matches everything.
#[derive(Debug, Clone, Default)]
pub struct FindFilter {
    /// Equality constraints on top-level (or dotted) field paths.
    pub equals: Vec<(String, Value)>,
    /// Inclusive lower bounds on a field path (`$gte`).
    pub gte: Vec<(String, Value)>,
    /// Exclusive upper bounds on a field path (`$lt`).
    pub lt: Vec<(String, Value)>,
    /// Field paths that must **not** exist (`$exists: false`).
    pub not_exists: Vec<String>,
    /// Field paths that must be `<= value` (`$lte`), used for expired-lock reclaim.
    pub lte: Vec<(String, Value)>,
}

impl FindFilter {
    pub fn eq(field: impl Into<String>, value: Value) -> Self {
        Self {
            equals: vec![(field.into(), value)],
            ..Default::default()
        }
    }

    pub fn and(mut self, other: FindFilter) -> Self {
        self.equals.extend(other.equals);
        self.gte.extend(other.gte);
        self.lt.extend(other.lt);
        self.not_exists.extend(other.not_exists);
        self.lte.extend(other.lte);
        self
    }

    pub fn gte(mut self, field: impl Into<String>, value: Value) -> Self {
        self.gte.push((field.into(), value));
        self
    }

    pub fn lt(mut self, field: impl Into<String>, value: Value) -> Self {
        self.lt.push((field.into(), value));
        self
    }

    pub fn lte(mut self, field: impl Into<String>, value: Value) -> Self {
        self.lte.push((field.into(), value));
        self
    }

    pub fn not_exists(mut self, field: impl Into<String>) -> Self {
        self.not_exists.push(field.into());
        self
    }

    fn matches(&self, doc: &Map<String, Value>) -> bool {
        for (field, expected) in &self.equals {
            if resolve_field(doc, field) != Some(expected) {
                return false;
            }
        }
        for (field, bound) in &self.gte {
            match resolve_field(doc, field) {
                Some(v) if cmp_json(v, bound) != std::cmp::Ordering::Less => {}
                _ => return false,
            }
        }
        for (field, bound) in &self.lt {
            match resolve_field(doc, field) {
                Some(v) if cmp_json(v, bound) == std::cmp::Ordering::Less => {}
                _ => return false,
            }
        }
        for (field, bound) in &self.lte {
            match resolve_field(doc, field) {
                Some(v) if cmp_json(v, bound) != std::cmp::Ordering::Greater => {}
                _ => return false,
            }
        }
        for field in &self.not_exists {
            if resolve_field(doc, field).is_some() {
                return false;
            }
        }
        true
    }
}

/// A single update operator/assignment.
#[derive(Debug, Clone)]
pub(crate) enum UpdateOp {
    Set(String, Value),
    SetOnInsert(String, Value),
    Inc(String, i64),
    Push(String, Value),
    PushEach(String, Vec<Value>),
    Unset(String),
}

/// A MongoDB update document, built from a set of operators.
#[derive(Debug, Clone, Default)]
pub struct Update {
    ops: Vec<UpdateOp>,
}

impl Update {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(mut self, field: impl Into<String>, value: Value) -> Self {
        self.ops.push(UpdateOp::Set(field.into(), value));
        self
    }

    pub fn set_on_insert(mut self, field: impl Into<String>, value: Value) -> Self {
        self.ops.push(UpdateOp::SetOnInsert(field.into(), value));
        self
    }

    pub fn inc(mut self, field: impl Into<String>, amount: i64) -> Self {
        self.ops.push(UpdateOp::Inc(field.into(), amount));
        self
    }

    pub fn push(mut self, field: impl Into<String>, value: Value) -> Self {
        self.ops.push(UpdateOp::Push(field.into(), value));
        self
    }

    pub fn push_each(mut self, field: impl Into<String>, values: Vec<Value>) -> Self {
        self.ops.push(UpdateOp::PushEach(field.into(), values));
        self
    }

    pub fn unset(mut self, field: impl Into<String>) -> Self {
        self.ops.push(UpdateOp::Unset(field.into()));
        self
    }

    /// Combines two updates (e.g. set fields + setOnInsert fields).
    pub fn combine(mut self, other: Update) -> Self {
        self.ops.extend(other.ops);
        self
    }

    /// The update operators in application order.
    ///
    /// Crate-internal: the real-driver backend ([`crate::client_mongodb`]) renders these
    /// into a MongoDB update document (`$set` / `$setOnInsert` / `$inc` / `$push` /
    /// `$unset`) instead of applying them in-process, so both backends share a single
    /// source of truth for operator semantics.
    pub(crate) fn ops(&self) -> &[UpdateOp] {
        &self.ops
    }

    fn apply(&self, doc: &mut Map<String, Value>, is_insert: bool) {
        for op in &self.ops {
            match op {
                UpdateOp::Set(field, value) => set_field(doc, field, value.clone()),
                UpdateOp::SetOnInsert(field, value) if is_insert => {
                    set_field(doc, field, value.clone())
                }
                UpdateOp::SetOnInsert(_, _) => {}
                UpdateOp::Inc(field, amount) => {
                    let current = resolve_field(doc, field)
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    set_field(doc, field, Value::from(current + amount));
                }
                UpdateOp::Push(field, value) => {
                    push_field(doc, field, vec![value.clone()]);
                }
                UpdateOp::PushEach(field, values) => {
                    push_field(doc, field, values.clone());
                }
                UpdateOp::Unset(field) => unset_field(doc, field),
            }
        }
    }
}

/// Find options: sort, skip, limit and projection.
#[derive(Debug, Clone, Default)]
pub struct FindOptions {
    pub sort_ascending_by: Option<String>,
    pub skip: Option<usize>,
    pub limit: Option<usize>,
    /// Dotted field paths to return (an inclusive projection). `None` returns the whole
    /// document; a list restricts the transfer to exactly those fields (MONGO-05).
    pub projection: Option<Vec<String>>,
}

/// Options for `find_one_and_update`.
#[derive(Debug, Clone, Copy, Default)]
pub struct FindOneAndUpdateOptions {
    pub upsert: bool,
    pub return_after: bool,
}

/// Options for `update_one`.
#[derive(Debug, Clone, Copy, Default)]
pub struct UpdateOptions {
    pub upsert: bool,
}

/// The subset of the MongoDB driver API used by this crate.
#[async_trait]
pub trait MongoClient: Send + Sync {
    /// Inserts a document, erroring with [`MongoError::DuplicateKey`] if the `_id`
    /// (or a unique index key) already exists.
    async fn insert_one(&self, collection: &str, doc: Map<String, Value>) -> MongoResult<()>;

    /// The database this client connects to **by default** (the database named in the
    /// connection URI, else [`DEFAULT_DATABASE`]).
    ///
    /// Mirrors Java `MongoDistributedStore#fromConnectionString`, which uses
    /// `connectionString.getDatabase()` instead of a hardcoded name: a store built without an
    /// explicit database must follow the connection, otherwise migrating an existing
    /// deployment (`mongodb://host/mydb`) silently reads/writes `agentscope` and finds none of
    /// the existing data (MONGO-01).
    fn default_database(&self) -> &str {
        DEFAULT_DATABASE
    }

    /// Finds the first document matching `filter`.
    async fn find_one(
        &self,
        collection: &str,
        filter: &FindFilter,
    ) -> MongoResult<Option<Map<String, Value>>>;

    /// Finds the first document matching `filter`, returning **only** the dotted field
    /// paths in `projection`.
    ///
    /// Java projects single-key reads the same way (`Projections.include(stateField)` in
    /// `MongoAgentStateStore.get` / `getVersioned`), so reading one state key never
    /// transfers the whole session document (MONGO-05). The returned document keeps the
    /// requested paths nested (`["states.foo"]` → `{"states": {"foo": …}}`); a field
    /// that is absent from the stored document is absent from the result.
    async fn find_one_projected(
        &self,
        collection: &str,
        filter: &FindFilter,
        projection: &[&str],
    ) -> MongoResult<Option<Map<String, Value>>>;

    /// Finds documents matching `filter`, sorted/paginated per `options`.
    async fn find_many(
        &self,
        collection: &str,
        filter: &FindFilter,
        options: &FindOptions,
    ) -> MongoResult<Vec<Map<String, Value>>>;

    /// Atomically updates the first matching document, returning it per `options`.
    async fn find_one_and_update(
        &self,
        collection: &str,
        filter: &FindFilter,
        update: &Update,
        options: &FindOneAndUpdateOptions,
    ) -> MongoResult<Option<Map<String, Value>>>;

    /// Updates (or upserts) the first matching document.
    async fn update_one(
        &self,
        collection: &str,
        filter: &FindFilter,
        update: &Update,
        options: &UpdateOptions,
    ) -> MongoResult<()>;

    /// Deletes the first matching document; returns whether one was removed.
    async fn delete_one(&self, collection: &str, filter: &FindFilter) -> MongoResult<bool>;

    /// Counts documents matching `filter`.
    async fn count(&self, collection: &str, filter: &FindFilter) -> MongoResult<u64>;

    /// Returns the distinct values of `field` for documents matching `filter`.
    async fn distinct(
        &self,
        collection: &str,
        field: &str,
        filter: &FindFilter,
    ) -> MongoResult<Vec<Value>>;

    /// Returns the array length at `size_field` **and** the string at `string_field`
    /// (both possibly-dotted paths) of the first document matching `filter`, in a single
    /// round trip.
    ///
    /// Mirrors the aggregation Java `MongoAgentStateStore.save(List)` runs before every
    /// list write: `$project: {count: {$cond: [{$isArray: "$<size_field>"},
    /// {$size: "$<size_field>"}, 0]}, storedHash: "$<string_field>"}` — only a number and
    /// a short string cross the wire, never the array itself (MONGO-05).
    ///
    /// `(None, None)` when no document matches; otherwise the count is `Some(n)` (`0`
    /// when the field is missing or not an array, exactly like the `$cond`) and the
    /// string is `None` when absent or not a string.
    async fn first_field_size_and_string(
        &self,
        collection: &str,
        filter: &FindFilter,
        size_field: &str,
        string_field: &str,
    ) -> MongoResult<(Option<usize>, Option<String>)>;

    // ── index lifecycle ──

    /// Creates an index; errors with [`MongoError::IndexOptionsConflict`] on error 85.
    async fn create_index(&self, collection: &str, spec: &IndexSpec) -> MongoResult<()>;

    /// Drops an index by name.
    async fn drop_index(&self, collection: &str, name: &str) -> MongoResult<()>;

    /// Lists existing indexes on a collection.
    async fn list_indexes(&self, collection: &str) -> MongoResult<Vec<IndexInfo>>;

    // ── gridfs-style blob storage (snapshots) ──

    /// Uploads/replaces a blob keyed by `filename`.
    async fn gridfs_upload(&self, bucket: &str, filename: &str, data: Vec<u8>) -> MongoResult<()>;

    /// Downloads a blob by `filename`.
    async fn gridfs_download(&self, bucket: &str, filename: &str) -> MongoResult<Option<Vec<u8>>>;

    /// Whether a blob exists.
    async fn gridfs_exists(&self, bucket: &str, filename: &str) -> MongoResult<bool>;

    /// Deletes a blob; returns whether anything was removed.
    async fn gridfs_delete(&self, bucket: &str, filename: &str) -> MongoResult<bool>;

    // ── test/seeding hook ──

    /// Inserts an index descriptor directly (bypassing conflict checks). Used by the
    /// index-migration tests to simulate DBA-renamed indexes.
    async fn insert_raw_index(&self, collection: &str, info: IndexInfo) -> MongoResult<()>;
}

// ─────────────────────────── field-path helpers ───────────────────────────

/// Resolves a possibly-dotted field path against a JSON document.
fn resolve_field<'a>(doc: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    let mut parts = path.split('.');
    let first = parts.next()?;
    let mut current = doc.get(first)?;
    for part in parts {
        current = current.as_object()?.get(part)?;
    }
    Some(current)
}

/// Sets a possibly-dotted field path, creating intermediate objects as needed.
fn set_field(doc: &mut Map<String, Value>, path: &str, value: Value) {
    let parts: Vec<&str> = path.split('.').collect();
    if parts.len() == 1 {
        doc.insert(parts[0].to_string(), value);
        return;
    }
    let mut current = doc;
    for part in &parts[..parts.len() - 1] {
        let entry = current
            .entry((*part).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        current = entry.as_object_mut().expect("object just ensured");
    }
    current.insert(parts[parts.len() - 1].to_string(), value);
}

/// Keeps only the requested (possibly dotted) field paths of `doc`.
///
/// Mirrors an inclusive MongoDB projection: requested paths keep their nesting
/// (`["states.foo"]` → `{"states": {"foo": …}}`), fields absent from `doc` are absent
/// from the result. The in-process backend applies it locally; the real driver pushes the
/// same field set down to the server.
fn project_doc(doc: &Map<String, Value>, projection: &[String]) -> Map<String, Value> {
    let mut out = Map::new();
    for field in projection {
        if let Some(value) = resolve_field(doc, field) {
            set_field(&mut out, field, value.clone());
        }
    }
    out
}

/// Unsets a possibly-dotted field path.
fn unset_field(doc: &mut Map<String, Value>, path: &str) {
    let parts: Vec<&str> = path.split('.').collect();
    if parts.len() == 1 {
        doc.remove(parts[0]);
        return;
    }
    let mut current = doc;
    for part in &parts[..parts.len() - 1] {
        match current.get_mut(*part).and_then(|v| v.as_object_mut()) {
            Some(next) => current = next,
            None => return,
        }
    }
    current.remove(parts[parts.len() - 1]);
}

/// Appends values to an array at a possibly-dotted field path.
fn push_field(doc: &mut Map<String, Value>, path: &str, values: Vec<Value>) {
    let existing = resolve_field(doc, path).cloned();
    let mut arr = match existing {
        Some(Value::Array(items)) => items,
        _ => Vec::new(),
    };
    arr.extend(values);
    set_field(doc, path, Value::Array(arr));
}

/// Orders two JSON scalars for range comparisons. Numbers compare numerically,
/// strings lexicographically; mismatched/other types are treated as equal.
fn cmp_json(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let x = x.as_f64().unwrap_or(0.0);
            let y = y.as_f64().unwrap_or(0.0);
            x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal)
        }
        (Value::String(x), Value::String(y)) => x.cmp(y),
        _ => std::cmp::Ordering::Equal,
    }
}

// ─────────────────────────── in-memory backend ───────────────────────────

#[derive(Debug, Default)]
struct CollectionState {
    docs: Vec<Map<String, Value>>,
    indexes: Vec<IndexInfo>,
    /// Monotonic counter used to auto-name indexes.
    next_index_id: u64,
    /// Monotonic counter used to allocate GridFS file ids within a bucket.
    next_file_id: u64,
}

/// A fully-featured in-process [`MongoClient`].
///
/// Atomicity of `find_one_and_update` and duplicate-key detection on `insert_one`
/// matches the server contracts the stores rely on, so the contract tests exercise
/// real concurrency semantics rather than a fake.
#[derive(Debug, Default)]
pub struct InMemoryMongoClient {
    collections: Mutex<BTreeMap<String, CollectionState>>,
    /// Test hook: number of upcoming `create_index` calls to force-fail with error
    /// 85, simulating a concurrent node that keeps re-creating a conflicting index.
    forced_index_conflicts: std::sync::atomic::AtomicUsize,
    /// Default database reported through [`MongoClient::default_database`]; `None` until a
    /// test simulates a URI that names a database.
    default_database: Option<String>,
    /// Test hook: projection of the most recent projected read.
    last_projection: Mutex<Option<Vec<String>>>,
}

impl InMemoryMongoClient {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test hook: reports `database` as the default database, as if the connection URI were
    /// `mongodb://host/<database>` (MONGO-01).
    pub fn with_default_database(mut self, database: impl Into<String>) -> Self {
        self.default_database = Some(database.into());
        self
    }

    /// Test hook: the projection of the most recent projected read (see
    /// [`MongoClient::find_one_projected`] / [`FindOptions::projection`]); `None` for an
    /// unprojected read. Lets a test assert a caller really restricts the transfer
    /// (MONGO-05).
    pub fn last_projection(&self) -> Option<Vec<String>> {
        self.last_projection
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn record_projection(&self, projection: Option<&Vec<String>>) {
        let mut slot = self
            .last_projection
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *slot = projection.cloned();
    }

    /// Test hook: forces the next `n` [`MongoClient::create_index`] calls to fail with
    /// [`MongoError::IndexOptionsConflict`] (error 85), even when the spec would
    /// otherwise be accepted. Used to exercise the index-migration retry/exhaustion
    /// paths deterministically.
    pub fn force_index_conflicts(&self, n: usize) {
        self.forced_index_conflicts
            .store(n, std::sync::atomic::Ordering::SeqCst);
    }

    fn with_collection<R>(&self, name: &str, f: impl FnOnce(&mut CollectionState) -> R) -> R {
        let mut guard = self.collections.lock().unwrap_or_else(|p| p.into_inner());
        let state = guard.entry(name.to_string()).or_default();
        f(state)
    }

    /// Operates on several collections under a single lock; required by GridFS, which
    /// spans `<bucket>.files` and `<bucket>.chunks` and must keep them consistent.
    fn with_collections<R>(
        &self,
        f: impl FnOnce(&mut BTreeMap<String, CollectionState>) -> R,
    ) -> R {
        let mut guard = self.collections.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut guard)
    }

    /// Number of documents in a collection (test helper).
    pub fn document_count(&self, collection: &str) -> usize {
        self.with_collection(collection, |s| s.docs.len())
    }
}

#[async_trait]
impl MongoClient for InMemoryMongoClient {
    fn default_database(&self) -> &str {
        self.default_database.as_deref().unwrap_or(DEFAULT_DATABASE)
    }

    async fn insert_one(&self, collection: &str, doc: Map<String, Value>) -> MongoResult<()> {
        self.with_collection(collection, |state| {
            let id = doc.get("_id").cloned();
            if id.is_some() && state.docs.iter().any(|d| d.get("_id") == id.as_ref()) {
                return Err(MongoError::DuplicateKey);
            }
            state.docs.push(doc);
            Ok(())
        })
    }

    async fn find_one(
        &self,
        collection: &str,
        filter: &FindFilter,
    ) -> MongoResult<Option<Map<String, Value>>> {
        self.with_collection(collection, |state| {
            Ok(state.docs.iter().find(|d| filter.matches(d)).cloned())
        })
    }

    async fn find_one_projected(
        &self,
        collection: &str,
        filter: &FindFilter,
        projection: &[&str],
    ) -> MongoResult<Option<Map<String, Value>>> {
        let projection: Vec<String> = projection.iter().map(|f| (*f).to_string()).collect();
        self.record_projection(Some(&projection));
        self.with_collection(collection, |state| {
            Ok(state
                .docs
                .iter()
                .find(|d| filter.matches(d))
                .map(|d| project_doc(d, &projection)))
        })
    }

    async fn find_many(
        &self,
        collection: &str,
        filter: &FindFilter,
        options: &FindOptions,
    ) -> MongoResult<Vec<Map<String, Value>>> {
        self.record_projection(options.projection.as_ref());
        self.with_collection(collection, |state| {
            let mut matches: Vec<Map<String, Value>> = state
                .docs
                .iter()
                .filter(|d| filter.matches(d))
                .cloned()
                .collect();
            if let Some(field) = &options.sort_ascending_by {
                matches.sort_by(|a, b| {
                    let av = resolve_field(a, field).cloned().unwrap_or(Value::Null);
                    let bv = resolve_field(b, field).cloned().unwrap_or(Value::Null);
                    cmp_json(&av, &bv)
                });
            }
            let start = options.skip.unwrap_or(0).min(matches.len());
            let mut page: Vec<Map<String, Value>> = matches.split_off(start);
            if let Some(limit) = options.limit {
                page.truncate(limit);
            }
            if let Some(projection) = &options.projection {
                page = page.iter().map(|d| project_doc(d, projection)).collect();
            }
            Ok(page)
        })
    }

    async fn find_one_and_update(
        &self,
        collection: &str,
        filter: &FindFilter,
        update: &Update,
        options: &FindOneAndUpdateOptions,
    ) -> MongoResult<Option<Map<String, Value>>> {
        self.with_collection(collection, |state| {
            let position = state.docs.iter().position(|d| filter.matches(d));
            match position {
                Some(idx) => {
                    if options.return_after {
                        let doc = &mut state.docs[idx];
                        update.apply(doc, false);
                        Ok(Some(doc.clone()))
                    } else {
                        let before = state.docs[idx].clone();
                        update.apply(&mut state.docs[idx], false);
                        Ok(Some(before))
                    }
                }
                None if options.upsert => {
                    let mut doc = Map::new();
                    // Seed equality constraints from the filter.
                    for (field, value) in &filter.equals {
                        set_field(&mut doc, field, value.clone());
                    }
                    update.apply(&mut doc, true);
                    // An upsert insert that collides with an existing `_id` is rejected
                    // by MongoDB with a duplicate-key error, even when the matched
                    // document failed the filter (e.g. an `$exists: false` guard).
                    let id = doc.get("_id");
                    if id.is_some() && state.docs.iter().any(|d| d.get("_id") == id) {
                        return Err(MongoError::DuplicateKey);
                    }
                    state.docs.push(doc.clone());
                    if options.return_after {
                        Ok(Some(doc))
                    } else {
                        Ok(None)
                    }
                }
                None => Ok(None),
            }
        })
    }

    async fn update_one(
        &self,
        collection: &str,
        filter: &FindFilter,
        update: &Update,
        options: &UpdateOptions,
    ) -> MongoResult<()> {
        self.with_collection(collection, |state| {
            match state.docs.iter().position(|d| filter.matches(d)) {
                Some(idx) => {
                    update.apply(&mut state.docs[idx], false);
                    Ok(())
                }
                None if options.upsert => {
                    let mut doc = Map::new();
                    for (field, value) in &filter.equals {
                        set_field(&mut doc, field, value.clone());
                    }
                    update.apply(&mut doc, true);
                    // Reject an upsert insert that collides with an existing `_id`,
                    // mirroring MongoDB's duplicate-key (11000) behaviour.
                    let id = doc.get("_id").cloned();
                    if id.is_some() && state.docs.iter().any(|d| d.get("_id") == id.as_ref()) {
                        return Err(MongoError::DuplicateKey);
                    }
                    state.docs.push(doc);
                    Ok(())
                }
                None => Ok(()),
            }
        })
    }

    async fn delete_one(&self, collection: &str, filter: &FindFilter) -> MongoResult<bool> {
        self.with_collection(collection, |state| {
            match state.docs.iter().position(|d| filter.matches(d)) {
                Some(idx) => {
                    state.docs.remove(idx);
                    Ok(true)
                }
                None => Ok(false),
            }
        })
    }

    async fn count(&self, collection: &str, filter: &FindFilter) -> MongoResult<u64> {
        self.with_collection(collection, |state| {
            Ok(state.docs.iter().filter(|d| filter.matches(d)).count() as u64)
        })
    }

    async fn distinct(
        &self,
        collection: &str,
        field: &str,
        filter: &FindFilter,
    ) -> MongoResult<Vec<Value>> {
        self.with_collection(collection, |state| {
            let mut out: Vec<Value> = Vec::new();
            for doc in state.docs.iter().filter(|d| filter.matches(d)) {
                if let Some(v) = resolve_field(doc, field) {
                    if !out.contains(v) {
                        out.push(v.clone());
                    }
                }
            }
            Ok(out)
        })
    }

    async fn first_field_size_and_string(
        &self,
        collection: &str,
        filter: &FindFilter,
        size_field: &str,
        string_field: &str,
    ) -> MongoResult<(Option<usize>, Option<String>)> {
        self.with_collection(collection, |state| {
            Ok(match state.docs.iter().find(|d| filter.matches(d)) {
                None => (None, None),
                Some(doc) => (
                    // `$cond: [$isArray, $size, 0]` — a missing or non-array field counts
                    // as 0, like the aggregation.
                    Some(
                        resolve_field(doc, size_field)
                            .and_then(|v| v.as_array().map(|a| a.len()))
                            .unwrap_or(0),
                    ),
                    resolve_field(doc, string_field)
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                ),
            })
        })
    }

    async fn create_index(&self, collection: &str, spec: &IndexSpec) -> MongoResult<()> {
        // Test hook: simulate a concurrent conflicting create before touching state.
        if self
            .forced_index_conflicts
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |n| if n > 0 { Some(n - 1) } else { None },
            )
            .is_ok()
        {
            return Err(MongoError::IndexOptionsConflict);
        }
        self.with_collection(collection, |state| {
            if let Some(existing) = state.indexes.iter().find(|i| i.keys == spec.keys) {
                let same_options = existing.unique == spec.unique
                    && existing.sparse == spec.sparse
                    && existing.expire_after_seconds == spec.expire_after_seconds;
                if same_options {
                    return Ok(());
                }
                return Err(MongoError::IndexOptionsConflict);
            }
            let name = default_index_name(spec, state);
            state.indexes.push(IndexInfo {
                name,
                keys: spec.keys.clone(),
                unique: spec.unique,
                sparse: spec.sparse,
                expire_after_seconds: spec.expire_after_seconds,
            });
            Ok(())
        })
    }

    async fn drop_index(&self, collection: &str, name: &str) -> MongoResult<()> {
        self.with_collection(collection, |state| {
            match state.indexes.iter().position(|i| i.name == name) {
                Some(idx) => {
                    state.indexes.remove(idx);
                    Ok(())
                }
                None => Err(MongoError::IndexNotFound),
            }
        })
    }

    async fn list_indexes(&self, collection: &str) -> MongoResult<Vec<IndexInfo>> {
        self.with_collection(collection, |state| Ok(state.indexes.clone()))
    }

    async fn gridfs_upload(&self, bucket: &str, filename: &str, data: Vec<u8>) -> MongoResult<()> {
        self.with_collections(|map| {
            let files_name = gridfs_files_collection(bucket);
            let chunks_name = gridfs_chunks_collection(bucket);
            // Upload is an upsert keyed by `filename`: drop any existing file and its
            // chunks first (GridFS `deleteIfExists` semantics).
            let old_id = {
                let files = map.entry(files_name.clone()).or_default();
                files
                    .docs
                    .iter()
                    .position(|d| d.get(GRIDFS_FIELD_FILENAME) == Some(&json!(filename)))
                    .and_then(|pos| files.docs.remove(pos).get("_id").cloned())
            };
            if let Some(old_id) = old_id {
                map.entry(chunks_name.clone())
                    .or_default()
                    .docs
                    .retain(|c| c.get(GRIDFS_FIELD_FILES_ID) != Some(&old_id));
            }

            let chunk_size = GRIDFS_DEFAULT_CHUNK_SIZE;
            let file_id = {
                let files = map.entry(files_name).or_default();
                files.next_file_id += 1;
                let file_id = format!("{}", files.next_file_id);
                let mut file = Map::new();
                file.insert("_id".to_string(), json!(file_id));
                file.insert(GRIDFS_FIELD_FILENAME.to_string(), json!(filename));
                file.insert(GRIDFS_FIELD_LENGTH.to_string(), json!(data.len()));
                file.insert("chunkSize".to_string(), json!(chunk_size));
                // Mirror the Java `GridFSUploadOptions.metadata({createdAt: new Date()})`:
                // the driver stamps `uploadDate` on the file document and stores the
                // caller-supplied metadata verbatim.
                let now = chrono::Utc::now().timestamp_millis();
                file.insert(GRIDFS_FIELD_UPLOAD_DATE.to_string(), json!(now));
                let mut metadata = Map::new();
                metadata.insert(GRIDFS_META_CREATED_AT.to_string(), json!(now));
                file.insert(GRIDFS_FIELD_METADATA.to_string(), Value::Object(metadata));
                files.docs.push(file);
                file_id
            };

            let chunks = map.entry(chunks_name).or_default();
            // A file shorter than one chunk still produces a single (partial) chunk;
            // an empty payload produces no chunks, matching the driver (length 0).
            for (n, chunk) in data.chunks(chunk_size.max(1)).enumerate() {
                let mut doc = Map::new();
                doc.insert("_id".to_string(), json!(format!("{file_id}:{n}")));
                doc.insert(GRIDFS_FIELD_FILES_ID.to_string(), json!(file_id));
                doc.insert(GRIDFS_FIELD_CHUNK_N.to_string(), json!(n));
                doc.insert(LEGACY_FIELD_DATA.to_string(), json!(encode_binary(chunk)));
                chunks.docs.push(doc);
            }
            Ok(())
        })
    }

    async fn gridfs_download(&self, bucket: &str, filename: &str) -> MongoResult<Option<Vec<u8>>> {
        self.with_collections(|map| {
            let files_name = gridfs_files_collection(bucket);
            let chunks_name = gridfs_chunks_collection(bucket);
            let Some(files) = map.get(files_name.as_str()) else {
                return Ok(None);
            };
            let Some(file) = files
                .docs
                .iter()
                .find(|d| d.get(GRIDFS_FIELD_FILENAME) == Some(&json!(filename)))
            else {
                return Ok(None);
            };
            let file_id = file.get("_id").cloned();
            let expected_len = file
                .get(GRIDFS_FIELD_LENGTH)
                .and_then(|v| v.as_u64())
                .map(|n| n as usize);

            // Collect `(n, data)` for the owning file. A malformed chunk (missing `n`
            // or non-base64 `data`) is treated as a corrupt store rather than being
            // silently skipped — matching the driver, which throws
            // `MongoGridFSException` on a truncated/corrupt file.
            let mut chunks: Vec<(i64, Vec<u8>)> = Vec::new();
            if let Some(store) = map.get(chunks_name.as_str()) {
                for d in store
                    .docs
                    .iter()
                    .filter(|d| d.get(GRIDFS_FIELD_FILES_ID) == file_id.as_ref())
                {
                    let n = d
                        .get(GRIDFS_FIELD_CHUNK_N)
                        .and_then(|v| v.as_i64())
                        .ok_or_else(|| {
                            corrupt_chunk(filename, "chunk document has no integer 'n'")
                        })?;
                    let raw = d
                        .get(LEGACY_FIELD_DATA)
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            corrupt_chunk(filename, "chunk document has no 'data' field")
                        })?;
                    let bytes = decode_binary(raw).ok_or_else(|| {
                        corrupt_chunk(filename, "chunk 'data' is not valid base64")
                    })?;
                    chunks.push((n, bytes));
                }
            }

            // GridFS chunks must form the contiguous sequence `0..chunk_count`; a gap
            // means a chunk was lost and the file cannot be faithfully reassembled.
            chunks.sort_by_key(|(n, _)| *n);
            let mut data = Vec::with_capacity(expected_len.unwrap_or(0));
            for (expected, (n, chunk)) in chunks.iter().enumerate() {
                if *n != expected as i64 {
                    return Err(corrupt_chunk(
                        filename,
                        &format!("non-contiguous chunks: expected n={expected}, found n={n} ",),
                    ));
                }
                data.extend_from_slice(chunk);
            }

            // `length` is authoritative; a mismatch indicates truncation or stray
            // chunks left behind by a botched re-upload.
            if let Some(expected) = expected_len {
                if data.len() != expected {
                    return Err(corrupt_chunk(
                        filename,
                        &format!(
                            "reassembled length {} does not match stored length {expected}",
                            data.len()
                        ),
                    ));
                }
            }
            Ok(Some(data))
        })
    }

    async fn gridfs_exists(&self, bucket: &str, filename: &str) -> MongoResult<bool> {
        self.with_collections(|map| {
            let files_name = gridfs_files_collection(bucket);
            Ok(map
                .get(files_name.as_str())
                .map(|files| {
                    files
                        .docs
                        .iter()
                        .any(|d| d.get(GRIDFS_FIELD_FILENAME) == Some(&json!(filename)))
                })
                .unwrap_or(false))
        })
    }

    async fn gridfs_delete(&self, bucket: &str, filename: &str) -> MongoResult<bool> {
        self.with_collections(|map| {
            let files_name = gridfs_files_collection(bucket);
            let chunks_name = gridfs_chunks_collection(bucket);
            let Some(files) = map.get_mut(files_name.as_str()) else {
                return Ok(false);
            };
            let Some(pos) = files
                .docs
                .iter()
                .position(|d| d.get(GRIDFS_FIELD_FILENAME) == Some(&json!(filename)))
            else {
                return Ok(false);
            };
            let file_id = files.docs.remove(pos).get("_id").cloned();
            if let Some(file_id) = file_id {
                if let Some(chunks) = map.get_mut(chunks_name.as_str()) {
                    chunks
                        .docs
                        .retain(|c| c.get(GRIDFS_FIELD_FILES_ID) != Some(&file_id));
                }
            }
            Ok(true)
        })
    }

    async fn insert_raw_index(&self, collection: &str, info: IndexInfo) -> MongoResult<()> {
        self.with_collection(collection, |state| {
            state.indexes.push(info);
            Ok(())
        })
    }
}

fn default_index_name(spec: &IndexSpec, state: &mut CollectionState) -> String {
    let mut name = String::new();
    for (field, dir) in &spec.keys {
        if !name.is_empty() {
            name.push('_');
        }
        name.push_str(field);
        name.push('_');
        name.push_str(&dir.to_string());
    }
    if name.is_empty() {
        name = format!("idx_{}", state.next_index_id);
    }
    state.next_index_id += 1;
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(id: &str, extra: Value) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("_id".to_string(), json!(id));
        if let Value::Object(extra) = extra {
            for (k, v) in extra {
                m.insert(k, v);
            }
        }
        m
    }

    #[tokio::test]
    async fn insert_and_find_roundtrip() {
        let c = InMemoryMongoClient::new();
        c.insert_one("s", doc("a", json!({"x": 1}))).await.unwrap();
        let found = c
            .find_one("s", &FindFilter::eq("_id", json!("a")))
            .await
            .unwrap();
        assert_eq!(found.unwrap().get("x"), Some(&json!(1)));
    }

    /// MONGO-05: a projected read transfers only the requested fields, and the seam keeps
    /// dotted paths nested exactly like an inclusive MongoDB projection.
    #[tokio::test]
    async fn projected_reads_return_only_the_requested_fields() {
        let c = InMemoryMongoClient::new();
        c.insert_one(
            "sessions",
            doc(
                "a",
                json!({"states": {"foo": 1, "bar": 2}, "hashes": {"foo": "h"}, "user_id": "u"}),
            ),
        )
        .await
        .unwrap();

        let filter = FindFilter::eq("_id", json!("a"));
        let found = c
            .find_one_projected("sessions", &filter, &["states.foo", "hashes.foo"])
            .await
            .unwrap()
            .expect("document exists");
        assert_eq!(found.get("states"), Some(&json!({"foo": 1})));
        assert_eq!(found.get("hashes"), Some(&json!({"foo": "h"})));
        assert!(
            found.get("user_id").is_none(),
            "unprojected fields must not be transferred: {found:?}"
        );
        assert_eq!(
            c.last_projection(),
            Some(vec!["states.foo".to_string(), "hashes.foo".to_string()])
        );

        // An absent path stays absent instead of materialising nulls.
        let empty = c
            .find_one_projected("sessions", &filter, &["states.missing"])
            .await
            .unwrap()
            .expect("document exists");
        assert!(empty.is_empty(), "nothing to project: {empty:?}");

        // `find_many` honours the same projection.
        let page = c
            .find_many(
                "sessions",
                &filter,
                &FindOptions {
                    projection: Some(vec!["states.bar".to_string()]),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].get("states"), Some(&json!({"bar": 2})));
    }

    #[tokio::test]
    async fn duplicate_id_errors() {
        let c = InMemoryMongoClient::new();
        c.insert_one("s", doc("a", json!({}))).await.unwrap();
        assert!(matches!(
            c.insert_one("s", doc("a", json!({}))).await,
            Err(MongoError::DuplicateKey)
        ));
    }

    #[tokio::test]
    async fn atomic_find_one_and_update_matches_only_expired() {
        let c = InMemoryMongoClient::new();
        c.insert_one("locks", doc("k", json!({"expiresAt": 100})))
            .await
            .unwrap();
        let filter = FindFilter::eq("_id", json!("k")).lte("expiresAt", json!(50));
        assert!(c
            .find_one_and_update(
                "locks",
                &filter,
                &Update::new().set("owner", json!("me")),
                &FindOneAndUpdateOptions {
                    upsert: false,
                    return_after: true
                }
            )
            .await
            .unwrap()
            .is_none());
        let filter = FindFilter::eq("_id", json!("k")).lte("expiresAt", json!(200));
        let reclaimed = c
            .find_one_and_update(
                "locks",
                &filter,
                &Update::new().set("owner", json!("me")),
                &FindOneAndUpdateOptions {
                    upsert: false,
                    return_after: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(reclaimed.unwrap().get("owner"), Some(&json!("me")));
    }

    #[tokio::test]
    async fn dotted_path_set_and_resolve() {
        let c = InMemoryMongoClient::new();
        c.insert_one("s", doc("a", json!({}))).await.unwrap();
        c.update_one(
            "s",
            &FindFilter::eq("_id", json!("a")),
            &Update::new().set("states.foo", json!({"v": 1})),
            &UpdateOptions::default(),
        )
        .await
        .unwrap();
        let found = c
            .find_one("s", &FindFilter::eq("_id", json!("a")))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.get("states"), Some(&json!({"foo": {"v": 1}})));
    }

    #[tokio::test]
    async fn range_query_on_namespace() {
        let c = InMemoryMongoClient::new();
        c.insert_one("b", doc("1", json!({"namespace": "ns\u{1f}"})))
            .await
            .unwrap();
        c.insert_one("b", doc("2", json!({"namespace": "ns\u{1f}child\u{1f}"})))
            .await
            .unwrap();
        c.insert_one("b", doc("3", json!({"namespace": "other\u{1f}"})))
            .await
            .unwrap();
        let filter = FindFilter::default()
            .gte("namespace", json!("ns\u{1f}"))
            .lt("namespace", json!("ns\u{1f}\u{ffff}"));
        let found = c
            .find_many("b", &filter, &FindOptions::default())
            .await
            .unwrap();
        assert_eq!(found.len(), 2);
    }
}
