//! Real MongoDB driver backend for the driver-agnostic [`MongoClient`] seam.
//!
//! [`MongodbClient`] implements the whole [`MongoClient`] surface on top of the
//! official `mongodb` crate (the workspace pins `mongodb = "3"`), so every store in
//! this crate ([`crate::MongodbAgentStateStore`], [`crate::MongodbStore`],
//! [`crate::MongodbRemoteSnapshotClient`], [`crate::MongodbSandboxExecutionGuard`])
//! runs unchanged against a real server. [`crate::client::InMemoryMongoClient`]
//! remains the offline backend.
//!
//! ## Database scoping
//!
//! The store layer addresses collections through
//! [`scoped_collection`](crate::client::scoped_collection) — `"<db>::<collection>"` —
//! because the in-process backend keeps one flat map. This backend *decodes* that
//! convention back into a real `Database` + `Collection` pair, so a real server gets
//! native per-database isolation. Keys without the `::` separator (unit tests and
//! ad-hoc callers) resolve against the client's default database (the database named
//! in the connection string, else [`DEFAULT_DATABASE`]).
//!
//! ## Runtime / sync-caller bridge
//!
//! Some callers in this crate are synchronous (the harness `BaseStore` trait and
//! `SandboxGuard::is_held`) and drive the async seams with
//! `futures::executor::block_on`. The `mongodb` driver, however, spawns its topology /
//! connection-pool tasks on the Tokio runtime that was current when the client was
//! built; polling it from an unrelated executor (or from a *different* runtime) can
//! deadlock, and `block_on` inside a current-thread runtime always would.
//!
//! [`MongodbClient`] therefore routes every operation through a **process-wide** driver
//! runtime that lives on its own thread: the future is built and spawned on that
//! runtime and the caller just awaits a `oneshot`. This keeps:
//! * async callers genuinely async (no thread is blocked while awaiting),
//! * `futures::executor::block_on(async_call)` safe from any runtime flavour (the
//!   blocked thread only parks until the driver runtime completes the job), and
//! * the driver's topology-monitor / pool task shutdown free of races — the runtime
//!   outlives every client, so closing a client never cancels driver tasks mid-flight.
//!
//! ## Semantic parity with the in-process backend
//!
//! * documents round-trip through JSON/BSON conversion helpers that keep the shapes
//!   the stores assert on (numbers stay numbers, `DateTime` becomes millis since the
//!   epoch — the in-process `uploadDate` representation — and `Binary` becomes
//!   base64);
//! * `insert_one` synthesises a string `_id` when the caller omits one, so documents
//!   never come back carrying driver-generated `ObjectId`s that JSON cannot express;
//! * `gridfs_upload` is an upsert keyed by `filename` (older revisions are deleted
//!   first), matching `InMemoryMongoClient::gridfs_upload` and Java's
//!   `deleteIfExists` semantics;
//! * driver error codes are mapped onto the crate's [`MongoError`] variants
//!   (`11000` → `DuplicateKey`, `85`/`86` → `IndexOptionsConflict`, `26`/`27` →
//!   `IndexNotFound`).
//!
//! ## Known differences
//!
//! * `list_indexes` hides MongoDB's implicit `_id_` index; the in-process backend never
//!   reports it, and the index-migration helper must not consider it.
//! * `first_field_size_and_string` runs the same `$cond: [$isArray, $size, 0]` aggregation
//!   Java uses (so only a number and a hash string are transferred), which is also why it
//!   never raises `$size`'s "not an array" server error.
//! * A GridFS file document's `uploadDate` is a real BSON `DateTime` (as Java writes
//!   it), not a millis integer.
//! * `create_index` reports exactly what the server decides. MongoDB 7 raises `85` when
//!   the target index *name* already exists with a different specification (and `86`
//!   when only the key pattern differs), but it happily creates a second index when the
//!   key pattern matches while both the name *and* the options differ. The in-process
//!   backend is stricter — any same-key-pattern mismatch is `85` — so on a real server a
//!   stale, differently-named index can coexist with the desired one instead of being
//!   migrated away. Java shows the same behaviour (it simply calls `createIndex`).

use std::future::Future;
use std::sync::mpsc::{channel, Sender};
use std::time::Duration;

use async_trait::async_trait;
use mongodb::bson::{doc, Bson, DateTime, Document};
use mongodb::error::{Error as DriverError, ErrorKind, WriteFailure};
use mongodb::gridfs::GridFsBucket;
use mongodb::options::{ClientOptions, GridFsBucketOptions, IndexOptions, ReturnDocument};
use mongodb::{Client, Collection, Database};
use serde_json::{Map, Value};

use crate::client::{
    encode_binary, FindFilter, FindOneAndUpdateOptions, FindOptions, IndexInfo, MongoClient,
    MongoError, MongoResult, Update, UpdateOp, UpdateOptions,
};
use crate::constants::DEFAULT_DATABASE;
use crate::index_utils::IndexSpec;

/// MongoDB server error code for a duplicate key violation.
const ERROR_DUPLICATE_KEY: i32 = 11000;
/// MongoDB server error code for conflicting index options.
const ERROR_INDEX_OPTIONS_CONFLICT: i32 = 85;
/// MongoDB server error code for conflicting index key specs (same name, new keys).
const ERROR_INDEX_KEY_SPECS_CONFLICT: i32 = 86;
/// MongoDB server error code for a missing namespace (collection/database).
const ERROR_NAMESPACE_NOT_FOUND: i32 = 26;
/// MongoDB server error code for a missing index.
const ERROR_INDEX_NOT_FOUND: i32 = 27;

/// Number of worker threads in the dedicated driver runtime.
const DRIVER_WORKER_THREADS: usize = 4;
/// Name of the driver thread and of its Tokio runtime workers (diagnostics only).
const DRIVER_THREAD_NAME: &str = "agentscope-mongodb-driver";
const DRIVER_WORKER_THREAD_NAME: &str = "agentscope-mongodb-rt";

// ─────────────────────────────── bridge ───────────────────────────────

/// A unit of work submitted to the dedicated driver runtime.
///
/// The body is expected to hand the actual operation to the runtime (via
/// `Runtime::spawn`) and return immediately, so the dispatching thread is never the
/// bottleneck.
type Job = Box<dyn FnOnce(&tokio::runtime::Runtime) + Send + 'static>;

/// The process-wide driver runtime handle.
///
/// The runtime is owned by a dedicated thread and lives for the process lifetime. That
/// is deliberate:
/// * the driver must not be polled on the caller's runtime (see the module docs), and
/// * tearing a runtime down while the driver still has topology-monitor / connection-pool
///   tasks in flight makes those tasks observe closed channels and panic inside the
///   driver (`cmap::connection_requester`). Keeping one runtime alive removes that race
///   entirely — dropping a client just closes its pool while the runtime keeps running.
fn driver_sender() -> MongoResult<&'static Sender<Job>> {
    static DRIVER: std::sync::OnceLock<Option<Sender<Job>>> = std::sync::OnceLock::new();
    let slot = DRIVER.get_or_init(|| {
        let (jobs, rx) = channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name(DRIVER_THREAD_NAME.to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(DRIVER_WORKER_THREADS)
                    .thread_name(DRIVER_WORKER_THREAD_NAME)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            "[mongo-driver] failed to start the driver runtime"
                        );
                        return;
                    }
                };
                // The sender lives in a `OnceLock`, so this loop runs for the process
                // lifetime (the runtime is intentionally not torn down).
                while let Ok(job) = rx.recv() {
                    job(&runtime);
                }
            });
        match spawned {
            Ok(_) => Some(jobs),
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "[mongo-driver] failed to spawn the MongoDB driver thread"
                );
                None
            }
        }
    });
    slot.as_ref().ok_or_else(|| {
        MongoError::Backend("the MongoDB driver runtime could not be started".into())
    })
}

/// Runs `make()` on the driver runtime and awaits its result.
///
/// `make` is invoked *on the driver thread* and its future is spawned onto the driver
/// runtime, so both construction and polling — and every background task the driver
/// spawns — belong to that runtime. Callers simply await a `oneshot`, which makes the
/// seam safe to drive from plain async code, from another runtime, and from
/// `futures::executor::block_on`.
async fn exec_on_driver<T, F, M>(make: M) -> MongoResult<T>
where
    T: Send + 'static,
    F: Future<Output = MongoResult<T>> + Send + 'static,
    M: FnOnce() -> F + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    let job: Job = Box::new(move |runtime: &tokio::runtime::Runtime| {
        runtime.spawn(async move {
            let outcome = make().await;
            // The receiver is gone only if the caller was cancelled; ignore.
            let _ = tx.send(outcome);
        });
    });
    driver_sender()?
        .send(job)
        .map_err(|_| MongoError::Backend("the MongoDB driver thread is no longer running".into()))?;
    rx.await
        .map_err(|_| MongoError::Backend("the MongoDB driver dropped the operation".into()))?
}

// ─────────────────────────────── client ───────────────────────────────

/// [`MongoClient`] backed by the official MongoDB driver.
///
/// Cheap to clone: clones share the same connection pool and driver runtime.
#[derive(Clone, Debug)]
pub struct MongodbClient {
    client: Client,
    default_database: String,
    /// The driver options this client was built from, retained so callers can inspect the
    /// connection settings they injected (MONGO-09).
    options: ClientOptions,
}

impl MongodbClient {
    /// Connects to `uri` and verifies the deployment is reachable with a `ping`.
    ///
    /// The ping makes a misconfigured/unreachable server fail immediately instead of
    /// surfacing later as a confusing per-operation server-selection timeout.
    pub async fn connect(uri: impl AsRef<str>) -> MongoResult<Self> {
        let uri = uri.as_ref().to_string();
        let options = exec_on_driver(move || async move {
            ClientOptions::parse(uri)
                .await
                .map_err(map_driver_error)
        })
        .await?;
        Self::connect_with_options(options).await
    }

    /// Connects with explicit driver options and verifies the deployment with a `ping`.
    ///
    /// This is the programmatic equivalent of the Java `create(MongoClient)` overload:
    /// `app_name` / `max_pool_size` / `server_selection_timeout` / `connect_timeout` /
    /// `retry_writes` / `write_concern` etc. can be set on the [`ClientOptions`] instead
    /// of (or on top of) connection-string keywords (MONGO-09). Parse a URI with
    /// [`ClientOptions::parse`] and mutate the result to keep the URI's settings.
    pub async fn connect_with_options(options: ClientOptions) -> MongoResult<Self> {
        let client = Self::from_options(options).await?;
        client.ping().await?;
        Ok(client)
    }

    /// Builds the client from explicit driver options **without** health-checking the
    /// deployment (the caller decides when to validate, e.g. right after construction).
    ///
    /// Construction always happens on the dedicated driver runtime, so the driver's
    /// topology/pool tasks are not tied to the caller's runtime.
    pub async fn from_options(options: ClientOptions) -> MongoResult<Self> {
        let default_database = options
            .default_database
            .clone()
            .unwrap_or_else(|| DEFAULT_DATABASE.to_string());
        let driver_options = options.clone();
        let client = exec_on_driver(move || async move {
            Client::with_options(driver_options).map_err(map_driver_error)
        })
        .await?;
        Ok(Self {
            client,
            default_database,
            options,
        })
    }

    /// The driver options this client was created with.
    pub fn options(&self) -> &ClientOptions {
        &self.options
    }

    async fn ping(&self) -> MongoResult<()> {
        let db = self.client.database("admin");
        exec_on_driver(move || async move {
            db.run_command(doc_ping()).await.map_err(map_driver_error)?;
            Ok(())
        })
        .await
    }

    /// The database used for collection keys that are not `"<db>::<collection>"`
    /// scoped.
    pub fn default_database(&self) -> &str {
        &self.default_database
    }

    /// Overrides the default database (used for unscoped collection keys).
    pub fn with_default_database(mut self, database: impl Into<String>) -> Self {
        self.default_database = database.into();
        self
    }

    /// The underlying driver client (for administrative work such as dropping the
    /// throw-away databases created by integration tests).
    pub fn driver_client(&self) -> &Client {
        &self.client
    }

    /// Drops a whole database. Administrative helper; not part of the [`MongoClient`]
    /// seam because the stores never need it.
    pub async fn drop_database(&self, database: &str) -> MongoResult<()> {
        let db = self.client.database(database);
        exec_on_driver(move || async move { db.drop().await.map_err(map_driver_error) })
            .await
    }

    // ── helpers ──

    fn split_key<'a>(&'a self, key: &'a str) -> (&'a str, &'a str) {
        match key.split_once("::") {
            Some((database, collection)) => (database, collection),
            None => (self.default_database.as_str(), key),
        }
    }

    fn database(&self, name: &str) -> Database {
        self.client.database(name)
    }

    fn collection(&self, key: &str) -> Collection<Document> {
        let (database, collection) = self.split_key(key);
        self.database(database).collection::<Document>(collection)
    }

    fn bucket(&self, key: &str) -> GridFsBucket {
        let (database, bucket) = self.split_key(key);
        self.database(database).gridfs_bucket(
            GridFsBucketOptions::builder()
                .bucket_name(bucket.to_string())
                .build(),
        )
    }
}

// ───────────────────────── JSON ⇄ BSON conversion ─────────────────────────

fn doc_ping() -> Document {
    let mut doc = Document::new();
    doc.insert("ping".to_string(), Bson::Int32(1));
    doc
}

/// Converts a JSON object into a BSON document.
pub(crate) fn json_map_to_document(map: &Map<String, Value>) -> Document {
    let mut doc = Document::new();
    for (key, value) in map {
        doc.insert(key.clone(), json_to_bson(value));
    }
    doc
}

/// Converts a JSON value into BSON.
pub(crate) fn json_to_bson(value: &Value) -> Bson {
    match value {
        Value::Null => Bson::Null,
        Value::Bool(flag) => Bson::Boolean(*flag),
        Value::Number(number) => {
            if let Some(i) = number.as_i64() {
                Bson::Int64(i)
            } else if let Some(u) = number.as_u64() {
                match i64::try_from(u) {
                    Ok(i) => Bson::Int64(i),
                    Err(_) => Bson::Double(u as f64),
                }
            } else {
                Bson::Double(number.as_f64().unwrap_or(f64::NAN))
            }
        }
        Value::String(text) => Bson::String(text.clone()),
        Value::Array(items) => Bson::Array(items.iter().map(json_to_bson).collect()),
        Value::Object(map) => Bson::Document(json_map_to_document(map)),
    }
}

/// Converts a BSON document into a JSON object.
pub(crate) fn document_to_json_map(doc: &Document) -> Map<String, Value> {
    let mut map = Map::new();
    for (key, value) in doc {
        map.insert(key.clone(), bson_to_json(value));
    }
    map
}

/// Converts a BSON value into JSON.
///
/// Types with no JSON counterpart are projected onto the representation the
/// in-process backend uses: `DateTime` → millis since the epoch (its `uploadDate`
/// shape), `Binary` → base64, `ObjectId` → hex. Unrepresentable scalars degrade to
/// their textual form rather than failing the whole read.
pub(crate) fn bson_to_json(value: &Bson) -> Value {
    match value {
        Bson::Double(f) => f64_to_json(*f),
        Bson::String(text) => Value::String(text.clone()),
        Bson::Boolean(flag) => Value::Bool(*flag),
        Bson::Null | Bson::Undefined => Value::Null,
        Bson::Int32(i) => Value::from(*i),
        Bson::Int64(i) => Value::from(*i),
        Bson::ObjectId(oid) => Value::String(oid.to_hex()),
        Bson::DateTime(datetime) => Value::from(datetime.timestamp_millis()),
        Bson::Binary(binary) => Value::String(encode_binary(&binary.bytes)),
        Bson::Array(items) => Value::Array(items.iter().map(bson_to_json).collect()),
        Bson::Document(doc) => Value::Object(document_to_json_map(doc)),
        Bson::Symbol(text) => Value::String(text.clone()),
        Bson::Decimal128(decimal) => decimal
            .to_string()
            .parse::<f64>()
            .map(f64_to_json)
            .unwrap_or(Value::String(decimal.to_string())),
        #[allow(unreachable_patterns)]
        other => Value::String(format!("{other:?}")),
    }
}

fn f64_to_json(value: f64) -> Value {
    match serde_json::Number::from_f64(value) {
        Some(number) => Value::Number(number),
        // NaN / Infinity have no JSON representation; `null` is the closest analogue
        // and matches what BSON consumers see for a non-finite double.
        None => Value::Null,
    }
}

// ───────────────────────── filter / update rendering ─────────────────────────

/// Renders a [`FindFilter`] as a MongoDB query document.
///
/// Dotted paths are passed through verbatim: MongoDB interprets them as document
/// paths, which is exactly what the stores rely on (`"states.<key>"`,
/// `"versions.<key>"`).
pub(crate) fn find_filter_to_document(filter: &FindFilter) -> Document {
    let mut doc = Document::new();
    for (field, value) in &filter.equals {
        doc.insert(field.clone(), json_to_bson(value));
    }
    for (field, value) in &filter.gte {
        insert_operator(&mut doc, field, "$gte", json_to_bson(value));
    }
    for (field, value) in &filter.lt {
        insert_operator(&mut doc, field, "$lt", json_to_bson(value));
    }
    for (field, value) in &filter.lte {
        insert_operator(&mut doc, field, "$lte", json_to_bson(value));
    }
    for field in &filter.not_exists {
        insert_operator(&mut doc, field, "$exists", Bson::Boolean(false));
    }
    doc
}

/// Adds an operator constraint on `field`, merging with an operator document already
/// present (and folding a plain equality value into an explicit `$eq`).
fn insert_operator(doc: &mut Document, field: &str, operator: &str, value: Bson) {
    match doc.get_mut(field) {
        None => {
            doc.insert(field.to_string(), operator_document(operator, value));
        }
        Some(Bson::Document(existing)) if existing.keys().any(|k| k.starts_with('$')) => {
            existing.insert(operator.to_string(), value);
        }
        Some(_) => {
            let previous = doc
                .get(field)
                .cloned()
                .unwrap_or(Bson::Null);
            let mut merged = Document::new();
            merged.insert("$eq".to_string(), previous);
            merged.insert(operator.to_string(), value);
            doc.insert(field.to_string(), Bson::Document(merged));
        }
    }
}

fn operator_document(operator: &str, value: Bson) -> Bson {
    let mut doc = Document::new();
    doc.insert(operator.to_string(), value);
    Bson::Document(doc)
}

/// Renders an [`Update`] as a MongoDB update document.
pub(crate) fn update_to_document(update: &Update) -> Document {
    let mut doc = Document::new();
    for op in update.ops() {
        match op {
            UpdateOp::Set(field, value) => {
                push_operator(&mut doc, "$set", field, json_to_bson(value));
            }
            UpdateOp::SetOnInsert(field, value) => {
                push_operator(&mut doc, "$setOnInsert", field, json_to_bson(value));
            }
            UpdateOp::Inc(field, amount) => {
                push_operator(&mut doc, "$inc", field, Bson::Int64(*amount));
            }
            UpdateOp::Push(field, value) => {
                push_operator(&mut doc, "$push", field, json_to_bson(value));
            }
            UpdateOp::PushEach(field, values) => {
                let mut each = Document::new();
                each.insert(
                    "$each".to_string(),
                    Bson::Array(values.iter().map(json_to_bson).collect()),
                );
                push_operator(&mut doc, "$push", field, Bson::Document(each));
            }
            UpdateOp::Unset(field) => {
                // MongoDB ignores the `$unset` value; `""` is the conventional one.
                push_operator(&mut doc, "$unset", field, Bson::String(String::new()));
            }
        }
    }
    doc
}

fn push_operator(doc: &mut Document, operator: &str, field: &str, value: Bson) {
    match doc.get_mut(operator) {
        Some(Bson::Document(inner)) => {
            inner.insert(field.to_string(), value);
        }
        _ => {
            let mut inner = Document::new();
            inner.insert(field.to_string(), value);
            doc.insert(operator.to_string(), Bson::Document(inner));
        }
    }
}

// ───────────────────────────── error mapping ─────────────────────────────

/// Maps a MongoDB error code onto the crate's error vocabulary.
fn map_error_code(code: Option<i32>) -> Option<MongoError> {
    match code {
        Some(ERROR_DUPLICATE_KEY) => Some(MongoError::DuplicateKey),
        Some(ERROR_INDEX_OPTIONS_CONFLICT) | Some(ERROR_INDEX_KEY_SPECS_CONFLICT) => {
            Some(MongoError::IndexOptionsConflict)
        }
        Some(ERROR_INDEX_NOT_FOUND) | Some(ERROR_NAMESPACE_NOT_FOUND) => {
            Some(MongoError::IndexNotFound)
        }
        _ => None,
    }
}

/// Extracts the server error code from a driver error, including the write-error path
/// used by inserts/updates.
fn driver_error_code(err: &DriverError) -> Option<i32> {
    match err.kind.as_ref() {
        ErrorKind::Command(command) => Some(command.code),
        ErrorKind::Write(failure) => match failure {
            WriteFailure::WriteError(write) => Some(write.code),
            WriteFailure::WriteConcernError(write) => Some(write.code),
            _ => None,
        },
        _ => None,
    }
}

/// Maps a driver error onto [`MongoError`].
fn map_driver_error(err: DriverError) -> MongoError {
    match map_error_code(driver_error_code(&err)) {
        Some(mapped) => mapped,
        None => MongoError::Backend(err.to_string()),
    }
}

/// Maps a driver error raised by a GridFS stream (`futures_io::Error` carries the
/// driver error as its source).
fn map_gridfs_io_error(err: futures::io::Error) -> MongoError {
    MongoError::Backend(format!("GridFS stream error: {err}"))
}

// ───────────────────────────── trait impl ─────────────────────────────

/// Lists the `_id`s of every GridFS file named `filename` in `bucket`.
async fn gridfs_file_ids(bucket: &GridFsBucket, filename: &str) -> MongoResult<Vec<Bson>> {
    use futures::TryStreamExt as _;

    let mut filter = Document::new();
    filter.insert("filename".to_string(), Bson::String(filename.to_string()));
    let mut cursor = bucket.find(filter).await.map_err(map_driver_error)?;
    let mut ids = Vec::new();
    while let Some(file) = cursor.try_next().await.map_err(map_driver_error)? {
        ids.push(file.id);
    }
    Ok(ids)
}

#[async_trait]
impl MongoClient for MongodbClient {
    fn default_database(&self) -> &str {
        &self.default_database
    }

    async fn insert_one(&self, collection: &str, doc: Map<String, Value>) -> MongoResult<()> {
        let coll = self.collection(collection);
        let mut document = json_map_to_document(&doc);
        if !document.contains_key("_id") {
            // Keep every stored document JSON-representable: an omitted `_id` would
            // otherwise become a driver-generated `ObjectId`, which cannot round-trip
            // back into `serde_json::Value`.
            document.insert(
                "_id".to_string(),
                Bson::String(uuid::Uuid::new_v4().to_string()),
            );
        }
        exec_on_driver(move || async move {
                coll.insert_one(document).await.map_err(map_driver_error)?;
                Ok(())
            })
            .await
    }

    async fn find_one(
        &self,
        collection: &str,
        filter: &FindFilter,
    ) -> MongoResult<Option<Map<String, Value>>> {
        let coll = self.collection(collection);
        let filter = find_filter_to_document(filter);
        exec_on_driver(move || async move {
                coll.find_one(filter)
                    .await
                    .map_err(map_driver_error)
                    .map(|found| found.map(|doc| document_to_json_map(&doc)))
            })
            .await
    }

    async fn find_one_projected(
        &self,
        collection: &str,
        filter: &FindFilter,
        projection: &[&str],
    ) -> MongoResult<Option<Map<String, Value>>> {
        let coll = self.collection(collection);
        let filter = find_filter_to_document(filter);
        let projection = include_projection(projection);
        exec_on_driver(move || async move {
                coll.find_one(filter)
                    .projection(projection)
                    .await
                    .map_err(map_driver_error)
                    .map(|found| found.map(|doc| document_to_json_map(&doc)))
            })
            .await
    }

    async fn find_many(
        &self,
        collection: &str,
        filter: &FindFilter,
        options: &FindOptions,
    ) -> MongoResult<Vec<Map<String, Value>>> {
        use futures::TryStreamExt as _;

        let coll = self.collection(collection);
        let filter = find_filter_to_document(filter);
        let sort_field = options.sort_ascending_by.clone();
        let skip = options.skip;
        let limit = options.limit;
        let projection = options.projection.as_ref().map(|fields| {
            include_projection(&fields.iter().map(String::as_str).collect::<Vec<_>>())
        });
        exec_on_driver(move || async move {
                let mut find = coll.find(filter);
                if let Some(projection) = projection {
                    find = find.projection(projection);
                }
                if let Some(field) = sort_field {
                    let mut sort = Document::new();
                    sort.insert(field, Bson::Int32(1));
                    find = find.sort(sort);
                }
                if let Some(skip) = skip {
                    find = find.skip(skip as u64);
                }
                if let Some(limit) = limit {
                    find = find.limit(limit as i64);
                }
                let cursor = find.await.map_err(map_driver_error)?;
                let docs: Vec<Document> = cursor.try_collect().await.map_err(map_driver_error)?;
                Ok(docs.iter().map(document_to_json_map).collect())
            })
            .await
    }

    async fn find_one_and_update(
        &self,
        collection: &str,
        filter: &FindFilter,
        update: &Update,
        options: &FindOneAndUpdateOptions,
    ) -> MongoResult<Option<Map<String, Value>>> {
        let coll = self.collection(collection);
        let filter = find_filter_to_document(filter);
        let update = update_to_document(update);
        let upsert = options.upsert;
        let return_after = options.return_after;
        exec_on_driver(move || async move {
                let mut action = coll.find_one_and_update(filter, update);
                if upsert {
                    action = action.upsert(true);
                }
                action = action.return_document(if return_after {
                    ReturnDocument::After
                } else {
                    ReturnDocument::Before
                });
                action
                    .await
                    .map_err(map_driver_error)
                    .map(|found| found.map(|doc| document_to_json_map(&doc)))
            })
            .await
    }

    async fn update_one(
        &self,
        collection: &str,
        filter: &FindFilter,
        update: &Update,
        options: &UpdateOptions,
    ) -> MongoResult<()> {
        let coll = self.collection(collection);
        let filter = find_filter_to_document(filter);
        let update = update_to_document(update);
        let upsert = options.upsert;
        exec_on_driver(move || async move {
                coll.update_one(filter, update)
                    .upsert(upsert)
                    .await
                    .map_err(map_driver_error)?;
                Ok(())
            })
            .await
    }

    async fn delete_one(&self, collection: &str, filter: &FindFilter) -> MongoResult<bool> {
        let coll = self.collection(collection);
        let filter = find_filter_to_document(filter);
        exec_on_driver(move || async move {
                let result = coll.delete_one(filter).await.map_err(map_driver_error)?;
                Ok(result.deleted_count > 0)
            })
            .await
    }

    async fn count(&self, collection: &str, filter: &FindFilter) -> MongoResult<u64> {
        let coll = self.collection(collection);
        let filter = find_filter_to_document(filter);
        exec_on_driver(move || async move { coll.count_documents(filter).await.map_err(map_driver_error) })
            .await
    }

    async fn distinct(
        &self,
        collection: &str,
        field: &str,
        filter: &FindFilter,
    ) -> MongoResult<Vec<Value>> {
        let coll = self.collection(collection);
        let filter = find_filter_to_document(filter);
        let field = field.to_string();
        exec_on_driver(move || async move {
                let values = coll.distinct(field, filter).await.map_err(map_driver_error)?;
                Ok(values.iter().map(bson_to_json).collect())
            })
            .await
    }

    async fn first_field_size_and_string(
        &self,
        collection: &str,
        filter: &FindFilter,
        size_field: &str,
        string_field: &str,
    ) -> MongoResult<(Option<usize>, Option<String>)> {
        use futures::TryStreamExt as _;

        let coll = self.collection(collection);
        let filter = find_filter_to_document(filter);
        let size_ref = format!("${size_field}");
        let hash_ref = format!("${string_field}");
        // Same `$cond: [$isArray, $size, 0]` as Java `MongoAgentStateStore.save(List)`.
        let count_expr = doc! {
            "$cond": [
                doc! { "$isArray": size_ref.clone() },
                doc! { "$size": size_ref },
                0,
            ]
        };
        exec_on_driver(move || async move {
                // Only the count and the hash string cross the wire, never the array
                // itself (MONGO-05).
                let pipeline = vec![
                    doc! { "$match": filter },
                    doc! { "$limit": 1 },
                    doc! {
                        "$project": {
                            "_id": 0,
                            "count": count_expr,
                            "storedHash": hash_ref,
                        }
                    },
                ];
                let mut cursor = coll.aggregate(pipeline).await.map_err(map_driver_error)?;
                let Some(doc) = cursor.try_next().await.map_err(map_driver_error)? else {
                    return Ok((None, None));
                };
                let count = match doc.get("count") {
                    Some(Bson::Int32(n)) => Some(*n as usize),
                    Some(Bson::Int64(n)) => Some(*n as usize),
                    Some(Bson::Double(n)) => Some(*n as usize),
                    _ => None,
                };
                let stored_hash = doc.get_str("storedHash").ok().map(|s| s.to_string());
                Ok((count, stored_hash))
            })
            .await
    }

    async fn create_index(&self, collection: &str, spec: &IndexSpec) -> MongoResult<()> {
        let coll = self.collection(collection);
        let model = index_model(spec);
        exec_on_driver(move || async move {
                coll.create_index(model).await.map_err(map_driver_error)?;
                Ok(())
            })
            .await
    }

    async fn drop_index(&self, collection: &str, name: &str) -> MongoResult<()> {
        let coll = self.collection(collection);
        let name = name.to_string();
        exec_on_driver(move || async move {
                coll.drop_index(name).await.map_err(map_driver_error)?;
                Ok(())
            })
            .await
    }

    async fn list_indexes(&self, collection: &str) -> MongoResult<Vec<IndexInfo>> {
        use futures::TryStreamExt as _;

        let coll = self.collection(collection);
        exec_on_driver(move || async move {
                let cursor = match coll.list_indexes().await {
                    Ok(cursor) => cursor,
                    // No namespace yet == no indexes (the in-process backend returns an
                    // empty list rather than an error for an unknown collection).
                    Err(e) if driver_error_code(&e) == Some(ERROR_NAMESPACE_NOT_FOUND) => {
                        return Ok(Vec::new())
                    }
                    Err(e) => return Err(map_driver_error(e)),
                };
                let models: Vec<mongodb::IndexModel> =
                    cursor.try_collect().await.map_err(map_driver_error)?;
                Ok(models
                    .iter()
                    .filter_map(index_model_to_info)
                    .collect::<Vec<_>>())
            })
            .await
    }

    async fn gridfs_upload(&self, bucket: &str, filename: &str, data: Vec<u8>) -> MongoResult<()> {
        use futures::io::AsyncWriteExt as _;

        let bucket = self.bucket(bucket);
        let filename = filename.to_string();
        exec_on_driver(move || async move {
                // Upload is an upsert keyed by `filename` (Java `deleteIfExists`): drop
                // older revisions so `download`/`exists` stay unambiguous.
                for id in gridfs_file_ids(&bucket, &filename).await? {
                    bucket.delete(id).await.map_err(map_driver_error)?;
                }
                let mut metadata = Document::new();
                metadata.insert(
                    crate::client::GRIDFS_META_CREATED_AT.to_string(),
                    Bson::DateTime(DateTime::now()),
                );
                let mut stream = bucket
                    .open_upload_stream(&filename)
                    .metadata(metadata)
                    .await
                    .map_err(map_driver_error)?;
                stream
                    .write_all(&data)
                    .await
                    .map_err(map_gridfs_io_error)?;
                stream.close().await.map_err(map_gridfs_io_error)?;
                Ok(())
            })
            .await
    }

    async fn gridfs_download(
        &self,
        bucket: &str,
        filename: &str,
    ) -> MongoResult<Option<Vec<u8>>> {
        use futures::io::AsyncReadExt as _;

        let bucket = self.bucket(bucket);
        let filename = filename.to_string();
        exec_on_driver(move || async move {
                let Some(id) = gridfs_file_ids(&bucket, &filename).await?.into_iter().next() else {
                    return Ok(None);
                };
                let mut stream = bucket
                    .open_download_stream(id)
                    .await
                    .map_err(map_driver_error)?;
                let mut data = Vec::new();
                stream
                    .read_to_end(&mut data)
                    .await
                    .map_err(map_gridfs_io_error)?;
                Ok(Some(data))
            })
            .await
    }

    async fn gridfs_exists(&self, bucket: &str, filename: &str) -> MongoResult<bool> {
        let bucket = self.bucket(bucket);
        let filename = filename.to_string();
        exec_on_driver(move || async move {
                Ok(!gridfs_file_ids(&bucket, &filename).await?.is_empty())
            })
            .await
    }

    async fn gridfs_delete(&self, bucket: &str, filename: &str) -> MongoResult<bool> {
        let bucket = self.bucket(bucket);
        let filename = filename.to_string();
        exec_on_driver(move || async move {
                let ids = gridfs_file_ids(&bucket, &filename).await?;
                let removed = !ids.is_empty();
                for id in ids {
                    bucket.delete(id).await.map_err(map_driver_error)?;
                }
                Ok(removed)
            })
            .await
    }

    async fn insert_raw_index(&self, collection: &str, info: IndexInfo) -> MongoResult<()> {
        let coll = self.collection(collection);
        let mut keys = Document::new();
        for (field, direction) in &info.keys {
            keys.insert(field.clone(), Bson::Int32(*direction));
        }
        let model = mongodb::IndexModel::builder()
            .keys(keys)
            .options(index_options(
                Some(info.name.clone()),
                info.unique,
                info.sparse,
                info.expire_after_seconds,
            ))
            .build();
        exec_on_driver(move || async move {
                coll.create_index(model).await.map_err(map_driver_error)?;
                Ok(())
            })
            .await
    }
}

/// Builds an inclusive projection document (`{field: 1, …}`) from dotted paths.
fn include_projection(fields: &[&str]) -> Document {
    let mut projection = Document::new();
    for field in fields {
        projection.insert((*field).to_string(), Bson::Int32(1));
    }
    projection
}

fn index_options(
    name: Option<String>,
    unique: bool,
    sparse: bool,
    expire_after_seconds: Option<i64>,
) -> IndexOptions {
    let mut options = IndexOptions::builder()
        .unique(unique)
        .sparse(sparse)
        .build();
    options.name = name;
    options.expire_after = expire_after_seconds
        .map(|seconds| Duration::from_secs(seconds.max(0) as u64));
    options
}

fn index_model(spec: &IndexSpec) -> mongodb::IndexModel {
    let mut keys = Document::new();
    for (field, direction) in &spec.keys {
        keys.insert(field.clone(), Bson::Int32(*direction));
    }
    mongodb::IndexModel::builder()
        .keys(keys)
        .options(index_options(
            None,
            spec.unique,
            spec.sparse,
            spec.expire_after_seconds,
        ))
        .build()
}

/// Converts a driver index model into [`IndexInfo`], hiding the implicit `_id_` index.
fn index_model_to_info(model: &mongodb::IndexModel) -> Option<IndexInfo> {
    let name = model.options.as_ref().and_then(|o| o.name.clone())?;
    if name == "_id_" {
        return None;
    }
    let keys: Vec<(String, i32)> = model
        .keys
        .iter()
        .map(|(field, value)| {
            let direction = match value {
                Bson::Int32(d) => *d,
                Bson::Int64(d) => *d as i32,
                Bson::Double(d) => *d as i32,
                _ => 1,
            };
            (field.clone(), direction)
        })
        .collect();
    let options = model.options.as_ref();
    Some(IndexInfo {
        name,
        keys,
        unique: options.and_then(|o| o.unique).unwrap_or(false),
        sparse: options.and_then(|o| o.sparse).unwrap_or(false),
        expire_after_seconds: options
            .and_then(|o| o.expire_after)
            .map(|d| d.as_secs() as i64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn find_filter_renders_equality_and_range_operators() {
        let filter = FindFilter::default()
            .gte("namespace", json!("ns\u{1f}"))
            .lt("namespace", json!("ns\u{1f}\u{ffff}"));
        let doc = find_filter_to_document(&filter);
        let rendered = bson_to_json(&Bson::Document(doc));
        assert_eq!(
            rendered,
            json!({"namespace": {"$gte": "ns\u{1f}", "$lt": "ns\u{1f}\u{ffff}"}})
        );
    }

    #[test]
    fn find_filter_renders_not_exists_and_equality_on_same_field() {
        let filter = FindFilter::eq("versions.k", json!(3)).not_exists("versions.k");
        let doc = find_filter_to_document(&filter);
        let rendered = bson_to_json(&Bson::Document(doc));
        assert_eq!(rendered, json!({"versions.k": {"$eq": 3, "$exists": false}}));
    }

    /// MONGO-09: connection settings can be injected programmatically (the Java
    /// `create(MongoClient)` equivalent) instead of only through URI keywords, and they
    /// are carried into the driver client. `from_options` skips the ping, so this runs
    /// offline.
    #[tokio::test]
    async fn connection_options_are_carried_into_the_client() {
        let mut options = ClientOptions::parse("mongodb://127.0.0.1:27017/mydb")
            .await
            .expect("a plain URI parses without a server");
        options.app_name = Some("agentscope-test".to_string());
        options.max_pool_size = Some(7);
        options.server_selection_timeout = Some(Duration::from_secs(5));

        let client = MongodbClient::from_options(options)
            .await
            .expect("the client builds from explicit options");
        // The URI's database becomes the default for unscoped collection keys.
        assert_eq!(client.default_database(), "mydb");
        assert_eq!(
            client.options().app_name.as_deref(),
            Some("agentscope-test")
        );
        assert_eq!(client.options().max_pool_size, Some(7));
        assert_eq!(
            client.options().server_selection_timeout,
            Some(Duration::from_secs(5))
        );
    }

    #[test]
    fn update_renders_all_operators() {
        let update = Update::new()
            .set("a", json!(1))
            .set_on_insert("u", json!("user"))
            .inc("versions.k", 1)
            .push("list", json!(2))
            .push_each("list2", vec![json!(3), json!(4)])
            .unset("gone");
        let rendered = bson_to_json(&Bson::Document(update_to_document(&update)));
        assert_eq!(
            rendered,
            json!({
                "$set": {"a": 1},
                "$setOnInsert": {"u": "user"},
                "$inc": {"versions.k": 1},
                "$push": {"list": 2, "list2": {"$each": [3, 4]}},
                "$unset": {"gone": ""}
            })
        );
    }

    #[test]
    fn json_bson_round_trip_preserves_nested_values() {
        let value = json!({
            "_id": {"user": "u", "session": "s"},
            "box": {"$dollar": "x", "dot.path": 2},
            "list": [1, 2.5, null, true, "s"],
            "nested": {"a": [{"b": 1}]}
        });
        let doc = json_map_to_document(value.as_object().unwrap());
        assert_eq!(Value::Object(document_to_json_map(&doc)), value);
    }

    #[test]
    fn bson_to_json_projects_driver_only_types() {
        let millis = 1_700_000_000_000_i64;
        let doc = mongodb::bson::doc! {
            "when": DateTime::from_millis(millis),
            "blob": Bson::Binary(mongodb::bson::Binary {
                subtype: mongodb::bson::spec::BinarySubtype::Generic,
                bytes: b"hi".to_vec(),
            }),
        };
        let rendered = bson_to_json(&Bson::Document(doc));
        assert_eq!(rendered["when"], json!(millis));
        assert_eq!(rendered["blob"], json!(crate::client::encode_binary(b"hi")));
    }

    #[test]
    fn error_codes_map_onto_crate_variants() {
        assert!(matches!(
            map_error_code(Some(11000)),
            Some(MongoError::DuplicateKey)
        ));
        assert!(matches!(
            map_error_code(Some(85)),
            Some(MongoError::IndexOptionsConflict)
        ));
        assert!(matches!(
            map_error_code(Some(27)),
            Some(MongoError::IndexNotFound)
        ));
        assert!(map_error_code(Some(13)).is_none());
        assert!(map_error_code(None).is_none());
    }

    #[test]
    fn scoped_keys_split_into_database_and_collection() {
        let split = |key: &str, default_db: &str| {
            let default_database = default_db.to_string();
            let (database, collection) = match key.split_once("::") {
                Some((database, collection)) => (database, collection),
                None => (default_database.as_str(), key),
            };
            (database.to_string(), collection.to_string())
        };
        assert_eq!(
            split("db_a::agentscope_sessions", "agentscope"),
            ("db_a".to_string(), "agentscope_sessions".to_string())
        );
        assert_eq!(
            split("sessions", "agentscope"),
            ("agentscope".to_string(), "sessions".to_string())
        );
    }

    #[test]
    fn index_model_renders_ttl_and_uniqueness() {
        let spec = IndexSpec::ascending(&["updated_at"])
            .sparse()
            .expire_after_seconds(60);
        let model = index_model(&spec);
        let options = model.options.expect("options");
        assert_eq!(options.sparse, Some(true));
        assert_eq!(options.unique, Some(false));
        assert_eq!(options.expire_after, Some(Duration::from_secs(60)));
        assert_eq!(
            bson_to_json(&Bson::Document(model.keys)),
            json!({"updated_at": 1})
        );

        let unique = index_model(&IndexSpec::compound_ascending(&["a", "b"]).unique());
        assert_eq!(unique.options.expect("options").unique, Some(true));
    }

    #[test]
    fn index_model_to_info_hides_the_implicit_id_index() {
        let id_doc = mongodb::bson::doc! {"_id": 1};
        let id_model = mongodb::IndexModel::builder()
            .keys(id_doc)
            .options(index_options(Some("_id_".to_string()), true, false, None))
            .build();
        assert!(index_model_to_info(&id_model).is_none());

        let ttl = mongodb::IndexModel::builder()
            .keys(mongodb::bson::doc! {"updated_at": 1})
            .options(index_options(
                Some("updated_at_1".to_string()),
                false,
                true,
                Some(3600),
            ))
            .build();
        let info = index_model_to_info(&ttl).expect("info");
        assert_eq!(info.name, "updated_at_1");
        assert_eq!(info.keys, vec![("updated_at".to_string(), 1)]);
        assert!(info.sparse);
        assert!(!info.unique);
        assert_eq!(info.expire_after_seconds, Some(3600));
    }
}
