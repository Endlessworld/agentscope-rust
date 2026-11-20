//! The server half: mount any [`SessionBackend`] on an axum router.
//!
//! The service is a transport adapter, not a second backend: every route calls the same method the
//! in-process host would call, and a failure keeps the backend's [`ErrorCode`] on the wire. A
//! fenced call rebuilds its [`Lease`] from the request headers plus the path, so the server never
//! trusts a lease the client did not carry.
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use agentscope_durable::backend::TxnBackend;
//! # use agentscope_durable_http::server::DurableService;
//! let service = DurableService::new(Arc::new(TxnBackend::in_memory()));
//! let app = service.router();
//! ```

use std::sync::Arc;
use std::time::Duration;

use agentscope_durable::{
    CheckpointView, CommandReceipt, Commit, CommitReceipt, ControlCommand, DelegationTarget,
    DelegationView, DeltaFilter, DurableError, ErrorCode, EventPage, EventStreamOptions,
    ExecutionResult, Failure, Frame, HarnessSelector, HistoryPage, Lease, ModelCallPage,
    RecoveryView, ResourceRequest, ResourceValue, Restoration, RunPage, RunView, SessionBackend,
    SessionKey, SessionSpec, SessionView, Snapshot, StepPage, ThreadKey, ThreadPage, ThreadSpec,
    ThreadStateView, ThreadView, ToolExecution, TurnControl, TurnPage, TurnReceipt, TurnView,
};
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::wire::{self, CONTENT_TYPE_SSE, HEADER_LAST_EVENT_ID};

/// A backend plus the route prefix it is mounted under.
#[derive(Clone)]
pub struct DurableService {
    backend: Arc<dyn SessionBackend>,
    prefix: String,
}

impl DurableService {
    pub fn new(backend: Arc<dyn SessionBackend>) -> Self {
        Self::with_prefix(backend, wire::DEFAULT_PREFIX)
    }

    pub fn with_prefix(backend: Arc<dyn SessionBackend>, prefix: impl Into<String>) -> Self {
        Self {
            backend,
            prefix: prefix.into(),
        }
    }

    pub fn backend(&self) -> &Arc<dyn SessionBackend> {
        &self.backend
    }

    /// The router, state already attached; mount it wherever the host serves HTTP.
    pub fn router(self) -> Router {
        let prefix = self.prefix.clone();
        Router::new().nest(&prefix, routes()).with_state(self)
    }
}

/// The failure shape both halves agree on: the backend code, a message and an optional receipt.
pub struct HttpFailure {
    error: DurableError,
    receipt: Option<CommitReceipt>,
}

impl HttpFailure {
    pub fn new(error: DurableError) -> Self {
        Self {
            error,
            receipt: None,
        }
    }

    /// A committed conflict still answers the receipt that already holds the commit.
    pub fn with_receipt(error: DurableError, receipt: CommitReceipt) -> Self {
        Self {
            error,
            receipt: Some(receipt),
        }
    }
}

impl From<DurableError> for HttpFailure {
    fn from(error: DurableError) -> Self {
        Self::new(error)
    }
}

impl IntoResponse for HttpFailure {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(wire::status_for(self.error.code()))
            .unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
        let body = wire::failure_body(&self.error, self.receipt.as_ref());
        (status, Json(body)).into_response()
    }
}

type ApiResult<T> = std::result::Result<T, HttpFailure>;
type Api<T> = ApiResult<Json<T>>;

#[derive(Debug, Deserialize)]
struct PageQuery {
    after: Option<String>,
    limit: Option<usize>,
    deltas: Option<String>,
}

impl PageQuery {
    fn limit(&self) -> Result<usize, DurableError> {
        let limit = self.limit.unwrap_or(wire::MAX_PAGE_LIMIT);
        wire::page_limit(limit)?;
        Ok(limit)
    }
}

#[derive(Debug, Deserialize)]
struct ReadyBody {
    #[serde(default)]
    selectors: Vec<HarnessSelector>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct StreamQuery {
    after: Option<String>,
    limit: Option<usize>,
    deltas: Option<String>,
    #[serde(rename = "intervalMs")]
    interval_ms: Option<u64>,
}

fn delta_filter(value: Option<&str>) -> Result<DeltaFilter, DurableError> {
    match value {
        None => Ok(DeltaFilter::All),
        Some(value) => DeltaFilter::parse(value)
            .ok_or_else(|| DurableError::invalid(format!("Unknown deltas filter `{value}`"))),
    }
}

fn version(headers: &HeaderMap) -> Result<(), DurableError> {
    let value = headers
        .get(wire::HEADER_VERSION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if value != wire::PROTOCOL_VERSION {
        return Err(DurableError::new(
            ErrorCode::Incompatible,
            "Unknown Durable HTTP protocol",
        ));
    }
    Ok(())
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, DurableError> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| DurableError::invalid(format!("Missing {name} header")))
}

/// Rebuilds the lease the caller claims: identity from the path, fence and expiry from headers.
fn lease(headers: &HeaderMap, thread: ThreadKey) -> Result<Lease, DurableError> {
    Ok(Lease {
        thread,
        turn_id: header(headers, wire::HEADER_TURN)?.to_string(),
        owner: header(headers, wire::HEADER_OWNER)?.to_string(),
        epoch: header(headers, wire::HEADER_EPOCH)?
            .parse()
            .map_err(|_| DurableError::invalid("Invalid epoch header"))?,
        expires_at: header(headers, wire::HEADER_EXPIRES_AT)?
            .parse()
            .map_err(|_| DurableError::invalid("Invalid expiry header"))?,
    })
}

fn session_key(namespace: String, session_id: String) -> Result<SessionKey, DurableError> {
    SessionKey::new(namespace, session_id)
}

fn thread_key(session: SessionKey, thread_id: String) -> Result<ThreadKey, DurableError> {
    ThreadKey::new(session, thread_id)
}

fn turn_lease(
    headers: &HeaderMap,
    namespace: String,
    session_id: String,
    thread_id: String,
    turn_id: String,
) -> Result<Lease, DurableError> {
    let thread = thread_key(session_key(namespace, session_id)?, thread_id)?;
    let lease = lease(headers, thread)?;
    if lease.turn_id != turn_id {
        return Err(DurableError::invalid("Turn header does not match the path"));
    }
    Ok(lease)
}

// ---------------------------------------------------------------- identity

async fn create(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Json(spec): Json<SessionSpec>,
) -> Api<SessionView> {
    version(&headers)?;
    Ok(Json(service.backend.create(spec).await?))
}

async fn session(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id)): Path<(String, String)>,
) -> Api<SessionView> {
    version(&headers)?;
    let key = session_key(namespace, session_id)?;
    Ok(Json(service.backend.session(&key).await?))
}

async fn close_session(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id)): Path<(String, String)>,
) -> Api<SessionView> {
    version(&headers)?;
    let key = session_key(namespace, session_id)?;
    Ok(Json(service.backend.close_session(&key).await?))
}

async fn create_thread(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id)): Path<(String, String)>,
    Json(spec): Json<ThreadSpec>,
) -> Api<ThreadView> {
    version(&headers)?;
    let key = session_key(namespace, session_id)?;
    if spec.key.session != key {
        return Err(HttpFailure::from(DurableError::invalid(
            "Thread spec belongs to another session",
        )));
    }
    Ok(Json(service.backend.create_thread(spec).await?))
}

async fn thread(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id)): Path<(String, String, String)>,
) -> Api<ThreadView> {
    version(&headers)?;
    let key = thread_key(session_key(namespace, session_id)?, thread_id)?;
    Ok(Json(service.backend.thread(&key).await?))
}

async fn close_thread(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id)): Path<(String, String, String)>,
) -> Api<ThreadView> {
    version(&headers)?;
    let key = thread_key(session_key(namespace, session_id)?, thread_id)?;
    Ok(Json(service.backend.close_thread(&key).await?))
}

async fn thread_state(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id)): Path<(String, String, String)>,
) -> Api<ThreadStateView> {
    version(&headers)?;
    let key = thread_key(session_key(namespace, session_id)?, thread_id)?;
    Ok(Json(service.backend.thread_state(&key).await?))
}

async fn threads(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id)): Path<(String, String)>,
    Query(page): Query<PageQuery>,
) -> Api<ThreadPage> {
    version(&headers)?;
    let key = session_key(namespace, session_id)?;
    let limit = page.limit()?;
    Ok(Json(
        service
            .backend
            .threads(&key, page.after.as_deref(), limit)
            .await?,
    ))
}

// ---------------------------------------------------------------- queue

async fn submit(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id)): Path<(String, String, String)>,
    Json(body): Json<serde_json::Value>,
) -> Api<TurnReceipt> {
    version(&headers)?;
    let key = thread_key(session_key(namespace, session_id)?, thread_id)?;
    let request_id = body
        .get("requestId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| DurableError::invalid("Missing requestId"))?;
    let input = body
        .get("input")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    Ok(Json(service.backend.submit(&key, request_id, input).await?))
}

async fn admission(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, request_id)): Path<(String, String, String)>,
) -> Api<Option<TurnReceipt>> {
    version(&headers)?;
    let key = session_key(namespace, session_id)?;
    Ok(Json(service.backend.admission(&key, &request_id).await?))
}

async fn turn(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id, turn_id)): Path<(String, String, String, String)>,
) -> Api<TurnView> {
    version(&headers)?;
    let key = thread_key(session_key(namespace, session_id)?, thread_id)?;
    Ok(Json(service.backend.turn(&key, &turn_id).await?))
}

async fn turns(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id)): Path<(String, String, String)>,
    Query(page): Query<PageQuery>,
) -> Api<TurnPage> {
    version(&headers)?;
    let key = thread_key(session_key(namespace, session_id)?, thread_id)?;
    let limit = page.limit()?;
    Ok(Json(
        service
            .backend
            .turns(&key, page.after.as_deref(), limit)
            .await?,
    ))
}

async fn history(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id)): Path<(String, String, String)>,
    Query(page): Query<PageQuery>,
) -> Api<HistoryPage> {
    version(&headers)?;
    let key = thread_key(session_key(namespace, session_id)?, thread_id)?;
    let limit = page.limit()?;
    Ok(Json(
        service
            .backend
            .history(&key, page.after.as_deref(), limit)
            .await?,
    ))
}

async fn control(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id)): Path<(String, String, String)>,
    Json(command): Json<ControlCommand>,
) -> Api<CommandReceipt> {
    version(&headers)?;
    let key = thread_key(session_key(namespace, session_id)?, thread_id)?;
    Ok(Json(service.backend.control(&key, command).await?))
}

async fn signal(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id, turn_id)): Path<(String, String, String, String)>,
    Json(signal): Json<agentscope_durable::Signal>,
) -> Api<()> {
    version(&headers)?;
    let key = thread_key(session_key(namespace, session_id)?, thread_id)?;
    service.backend.signal(&key, &turn_id, signal).await?;
    Ok(Json(()))
}

// ---------------------------------------------------------------- ownership

async fn claim(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id)): Path<(String, String, String)>,
    Json(body): Json<serde_json::Value>,
) -> Api<Option<Lease>> {
    version(&headers)?;
    let key = thread_key(session_key(namespace, session_id)?, thread_id)?;
    let owner = body
        .get("owner")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| DurableError::invalid("Missing owner"))?;
    let ttl = body
        .get("ttlMillis")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| DurableError::invalid("Missing ttlMillis"))?;
    Ok(Json(
        service
            .backend
            .claim(&key, owner, Duration::from_millis(ttl))
            .await?,
    ))
}

async fn renew(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id, turn_id)): Path<(String, String, String, String)>,
    Json(body): Json<serde_json::Value>,
) -> Api<()> {
    version(&headers)?;
    let lease = turn_lease(&headers, namespace, session_id, thread_id, turn_id)?;
    let ttl = body
        .get("ttlMillis")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| DurableError::invalid("Missing ttlMillis"))?;
    service
        .backend
        .renew(&lease, Duration::from_millis(ttl))
        .await?;
    Ok(Json(()))
}

async fn release(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id, turn_id)): Path<(String, String, String, String)>,
    Json(body): Json<serde_json::Value>,
) -> Api<()> {
    version(&headers)?;
    let lease = turn_lease(&headers, namespace, session_id, thread_id, turn_id)?;
    let delay = body
        .get("delayMillis")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    service
        .backend
        .release(&lease, Duration::from_millis(delay))
        .await?;
    Ok(Json(()))
}

async fn frame(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id, turn_id)): Path<(String, String, String, String)>,
) -> Api<Frame> {
    version(&headers)?;
    let lease = turn_lease(&headers, namespace, session_id, thread_id, turn_id)?;
    Ok(Json(service.backend.frame(&lease).await?))
}

async fn control_state(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id, turn_id)): Path<(String, String, String, String)>,
) -> Api<TurnControl> {
    version(&headers)?;
    let lease = turn_lease(&headers, namespace, session_id, thread_id, turn_id)?;
    Ok(Json(service.backend.control_state(&lease).await?))
}

async fn record_failure(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id, turn_id)): Path<(String, String, String, String)>,
    Json(failure): Json<Failure>,
) -> Api<RecoveryView> {
    version(&headers)?;
    let lease = turn_lease(&headers, namespace, session_id, thread_id, turn_id)?;
    Ok(Json(service.backend.record_failure(&lease, failure).await?))
}

/// A commit keeps the applied receipt on a conflict, so a `409` still answers with it.
async fn commit(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id, turn_id)): Path<(String, String, String, String)>,
    Json(commit): Json<Commit>,
) -> ApiResult<Json<CommitReceipt>> {
    version(&headers)?;
    let lease = turn_lease(&headers, namespace, session_id, thread_id, turn_id)?;
    match service.backend.commit(&lease, commit).await {
        Ok(receipt) => Ok(Json(receipt)),
        Err(error) => Err(HttpFailure::from(error)),
    }
}

async fn receipt(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id, turn_id, commit_id)): Path<(
        String,
        String,
        String,
        String,
        String,
    )>,
) -> Api<Option<CommitReceipt>> {
    version(&headers)?;
    let key = thread_key(session_key(namespace, session_id)?, thread_id)?;
    Ok(Json(
        service.backend.receipt(&key, &turn_id, &commit_id).await?,
    ))
}

// ---------------------------------------------------------------- invocation facts

async fn prepare_run(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id, turn_id)): Path<(String, String, String, String)>,
    Json(body): Json<serde_json::Value>,
) -> Api<RunView> {
    version(&headers)?;
    let lease = turn_lease(&headers, namespace, session_id, thread_id, turn_id)?;
    let request_id = body
        .get("requestId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| DurableError::invalid("Missing requestId"))?;
    Ok(Json(service.backend.prepare_run(&lease, request_id).await?))
}

async fn run(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id, turn_id, run_id)): Path<(
        String,
        String,
        String,
        String,
        String,
    )>,
) -> Api<RunView> {
    version(&headers)?;
    let key = thread_key(session_key(namespace, session_id)?, thread_id)?;
    Ok(Json(service.backend.run(&key, &turn_id, &run_id).await?))
}

async fn runs(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id, turn_id)): Path<(String, String, String, String)>,
    Query(page): Query<PageQuery>,
) -> Api<RunPage> {
    version(&headers)?;
    let key = thread_key(session_key(namespace, session_id)?, thread_id)?;
    let limit = page.limit()?;
    Ok(Json(
        service
            .backend
            .runs(&key, &turn_id, page.after.as_deref(), limit)
            .await?,
    ))
}

async fn steps(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id, turn_id, run_id)): Path<(
        String,
        String,
        String,
        String,
        String,
    )>,
    Query(page): Query<PageQuery>,
) -> Api<StepPage> {
    version(&headers)?;
    let key = thread_key(session_key(namespace, session_id)?, thread_id)?;
    let limit = page.limit()?;
    Ok(Json(
        service
            .backend
            .steps(&key, &turn_id, &run_id, page.after.as_deref(), limit)
            .await?,
    ))
}

async fn model_calls(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id, turn_id)): Path<(String, String, String, String)>,
    Query(page): Query<PageQuery>,
) -> Api<ModelCallPage> {
    version(&headers)?;
    let key = thread_key(session_key(namespace, session_id)?, thread_id)?;
    let limit = page.limit()?;
    Ok(Json(
        service
            .backend
            .model_calls(&key, &turn_id, page.after.as_deref(), limit)
            .await?,
    ))
}

async fn journal(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id, turn_id)): Path<(String, String, String, String)>,
    Query(page): Query<PageQuery>,
) -> Api<Vec<serde_json::Value>> {
    version(&headers)?;
    let lease = turn_lease(&headers, namespace, session_id, thread_id, turn_id)?;
    let limit = page.limit()?;
    let after = page.after.as_deref().unwrap_or("0");
    let after = after
        .parse()
        .map_err(|_| DurableError::invalid("Invalid journal offset"))?;
    Ok(Json(service.backend.journal(&lease, after, limit).await?))
}

async fn read_journal(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id)): Path<(String, String, String)>,
    Query(page): Query<PageQuery>,
) -> Api<Vec<serde_json::Value>> {
    version(&headers)?;
    let key = thread_key(session_key(namespace, session_id)?, thread_id)?;
    let limit = page.limit()?;
    let after = page.after.as_deref().unwrap_or("0");
    let after = after
        .parse()
        .map_err(|_| DurableError::invalid("Invalid journal offset"))?;
    Ok(Json(
        service.backend.read_journal(&key, after, limit).await?,
    ))
}

// ---------------------------------------------------------------- tools

async fn execution(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id, execution_id)): Path<(String, String, String, String)>,
) -> Api<ToolExecution> {
    version(&headers)?;
    let key = thread_key(session_key(namespace, session_id)?, thread_id)?;
    Ok(Json(service.backend.execution(&key, &execution_id).await?))
}

async fn begin_dispatch(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id, turn_id)): Path<(String, String, String, String)>,
    Json(body): Json<serde_json::Value>,
) -> Api<ToolExecution> {
    version(&headers)?;
    let lease = turn_lease(&headers, namespace, session_id, thread_id, turn_id)?;
    let execution_id = body
        .get("executionId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| DurableError::invalid("Missing executionId"))?;
    Ok(Json(
        service.backend.begin_dispatch(&lease, execution_id).await?,
    ))
}

async fn accept_result(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id, execution_id)): Path<(String, String, String, String)>,
    Json(body): Json<serde_json::Value>,
) -> Api<ToolExecution> {
    version(&headers)?;
    let key = thread_key(session_key(namespace, session_id)?, thread_id)?;
    let result: ExecutionResult = serde_json::from_value(
        body.get("result")
            .cloned()
            .ok_or_else(|| DurableError::invalid("Missing result"))?,
    )
    .map_err(|error| DurableError::invalid(format!("Invalid result: {error}")))?;
    Ok(Json(
        service
            .backend
            .accept_result(&key, &execution_id, result)
            .await?,
    ))
}

// ---------------------------------------------------------------- discovery

async fn ready(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Json(body): Json<ReadyBody>,
) -> Api<Vec<ThreadSpec>> {
    version(&headers)?;
    let limit = body.limit.unwrap_or(wire::MAX_PAGE_LIMIT);
    wire::page_limit(limit)?;
    Ok(Json(service.backend.ready(&body.selectors, limit).await?))
}

async fn checkpoints(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id)): Path<(String, String, String)>,
    Query(page): Query<PageQuery>,
) -> Api<Vec<u64>> {
    version(&headers)?;
    let key = thread_key(session_key(namespace, session_id)?, thread_id)?;
    let limit = page.limit()?;
    let after = page.after.as_deref().unwrap_or("0");
    let after = after
        .parse()
        .map_err(|_| DurableError::invalid("Invalid checkpoint offset"))?;
    Ok(Json(service.backend.checkpoints(&key, after, limit).await?))
}

async fn checkpoint(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id, position)): Path<(String, String, String, u64)>,
) -> Api<CheckpointView> {
    version(&headers)?;
    let key = thread_key(session_key(namespace, session_id)?, thread_id)?;
    Ok(Json(service.backend.checkpoint(&key, position).await?))
}

async fn events(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id)): Path<(String, String)>,
    Query(page): Query<PageQuery>,
) -> Api<EventPage> {
    version(&headers)?;
    let key = session_key(namespace, session_id)?;
    let limit = page.limit()?;
    let options = EventStreamOptions::defaults().with_deltas(delta_filter(page.deltas.as_deref())?);
    Ok(Json(
        service
            .backend
            .events_with(&key, page.after.as_deref(), limit, options)
            .await?,
    ))
}

async fn snapshot(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id)): Path<(String, String)>,
) -> Api<Snapshot> {
    version(&headers)?;
    let key = session_key(namespace, session_id)?;
    Ok(Json(service.backend.snapshot(&key).await?))
}

async fn prune_events(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id)): Path<(String, String)>,
    Json(body): Json<serde_json::Value>,
) -> Api<u64> {
    version(&headers)?;
    let key = session_key(namespace, session_id)?;
    let keep_newest = body
        .get("keepNewest")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    Ok(Json(service.backend.prune_events(&key, keep_newest).await?))
}

async fn resources(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id)): Path<(String, String, String)>,
    Json(request): Json<ResourceRequest>,
) -> Api<Vec<ResourceValue>> {
    version(&headers)?;
    let key = thread_key(session_key(namespace, session_id)?, thread_id)?;
    let lease = lease(&headers, key)?;
    Ok(Json(service.backend.resources(&lease, request).await?))
}

async fn restore(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id)): Path<(String, String)>,
    Json(restoration): Json<Restoration>,
) -> Api<ThreadView> {
    version(&headers)?;
    let key = session_key(namespace, session_id)?;
    if restoration.target.key.session != key {
        return Err(HttpFailure::from(DurableError::invalid(
            "Restoration targets another session",
        )));
    }
    Ok(Json(service.backend.restore(restoration).await?))
}

async fn delegate(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id, thread_id, turn_id)): Path<(String, String, String, String)>,
    Json(body): Json<serde_json::Value>,
) -> Api<DelegationView> {
    version(&headers)?;
    let lease = turn_lease(&headers, namespace, session_id, thread_id, turn_id)?;
    let execution_id = body
        .get("executionId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| DurableError::invalid("Missing executionId"))?;
    let request_id = body
        .get("requestId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| DurableError::invalid("Missing requestId"))?;
    let target: DelegationTarget = serde_json::from_value(
        body.get("target")
            .cloned()
            .ok_or_else(|| DurableError::invalid("Missing target"))?,
    )
    .map_err(|error| DurableError::invalid(format!("Invalid target: {error}")))?;
    Ok(Json(
        service
            .backend
            .delegate(&lease, execution_id, request_id, target)
            .await?,
    ))
}

// ---------------------------------------------------------------- events stream

async fn stream_events(
    State(service): State<DurableService>,
    headers: HeaderMap,
    Path((namespace, session_id)): Path<(String, String)>,
    Query(query): Query<StreamQuery>,
) -> Response {
    match stream_response(service, headers, namespace, session_id, query) {
        Ok(response) => response,
        Err(failure) => failure.into_response(),
    }
}

fn stream_response(
    service: DurableService,
    headers: HeaderMap,
    namespace: String,
    session_id: String,
    query: StreamQuery,
) -> Result<Response, HttpFailure> {
    version(&headers)?;
    let session = session_key(namespace, session_id)?;
    let limit = query.limit.unwrap_or(100);
    wire::page_limit(limit)?;
    let interval = Duration::from_millis(query.interval_ms.unwrap_or(250).max(1));
    let after = headers
        .get(HEADER_LAST_EVENT_ID)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or(query.after);
    let options =
        EventStreamOptions::defaults().with_deltas(delta_filter(query.deltas.as_deref())?);
    let stream = crate::sse::stream(
        service.backend.clone(),
        session,
        after,
        limit,
        options,
        interval,
    );
    Response::builder()
        .header(CONTENT_TYPE, CONTENT_TYPE_SSE)
        .header(CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .map_err(|error| HttpFailure::from(DurableError::unavailable(error.to_string())))
}

// ---------------------------------------------------------------- routes

fn routes() -> Router<DurableService> {
    Router::new()
        .route("/sessions", post(create))
        .route("/sessions/:ns/:sid", get(session))
        .route("/sessions/:ns/:sid/close", post(close_session))
        .route(
            "/sessions/:ns/:sid/threads",
            post(create_thread).get(threads),
        )
        .route("/sessions/:ns/:sid/threads/:tid", get(thread))
        .route("/sessions/:ns/:sid/threads/:tid/close", post(close_thread))
        .route("/sessions/:ns/:sid/threads/:tid/state", get(thread_state))
        .route(
            "/sessions/:ns/:sid/threads/:tid/turns",
            post(submit).get(turns),
        )
        .route("/sessions/:ns/:sid/threads/:tid/history", get(history))
        .route("/sessions/:ns/:sid/threads/:tid/control", post(control))
        .route("/sessions/:ns/:sid/threads/:tid/journal", get(read_journal))
        .route("/sessions/:ns/:sid/threads/:tid/claim", post(claim))
        .route(
            "/sessions/:ns/:sid/threads/:tid/checkpoints",
            get(checkpoints),
        )
        .route(
            "/sessions/:ns/:sid/threads/:tid/checkpoints/:position",
            get(checkpoint),
        )
        .route(
            "/sessions/:ns/:sid/threads/:tid/executions/:execution",
            get(execution),
        )
        .route(
            "/sessions/:ns/:sid/threads/:tid/executions/:execution/result",
            post(accept_result),
        )
        .route("/sessions/:ns/:sid/admission/:request", get(admission))
        .route("/sessions/:ns/:sid/threads/:tid/turns/:turn", get(turn))
        .route(
            "/sessions/:ns/:sid/threads/:tid/turns/:turn/signals",
            post(signal),
        )
        .route(
            "/sessions/:ns/:sid/threads/:tid/turns/:turn/lease/renew",
            post(renew),
        )
        .route(
            "/sessions/:ns/:sid/threads/:tid/turns/:turn/lease/release",
            post(release),
        )
        .route(
            "/sessions/:ns/:sid/threads/:tid/turns/:turn/frame",
            get(frame),
        )
        .route(
            "/sessions/:ns/:sid/threads/:tid/turns/:turn/control-state",
            get(control_state),
        )
        .route(
            "/sessions/:ns/:sid/threads/:tid/turns/:turn/failure",
            post(record_failure),
        )
        .route(
            "/sessions/:ns/:sid/threads/:tid/turns/:turn/commits",
            post(commit),
        )
        .route(
            "/sessions/:ns/:sid/threads/:tid/turns/:turn/commits/:commit/receipt",
            get(receipt),
        )
        .route(
            "/sessions/:ns/:sid/threads/:tid/turns/:turn/runs",
            post(prepare_run).get(runs),
        )
        .route(
            "/sessions/:ns/:sid/threads/:tid/turns/:turn/runs/:run",
            get(run),
        )
        .route(
            "/sessions/:ns/:sid/threads/:tid/turns/:turn/runs/:run/steps",
            get(steps),
        )
        .route(
            "/sessions/:ns/:sid/threads/:tid/turns/:turn/model-calls",
            get(model_calls),
        )
        .route(
            "/sessions/:ns/:sid/threads/:tid/turns/:turn/journal",
            get(journal),
        )
        .route(
            "/sessions/:ns/:sid/threads/:tid/turns/:turn/dispatch",
            post(begin_dispatch),
        )
        .route(
            "/sessions/:ns/:sid/threads/:tid/turns/:turn/delegate",
            post(delegate),
        )
        .route("/sessions/:ns/:sid/events", get(events))
        .route("/sessions/:ns/:sid/events/stream", get(stream_events))
        .route("/sessions/:ns/:sid/events/prune", post(prune_events))
        .route("/sessions/:ns/:sid/snapshot", get(snapshot))
        .route("/sessions/:ns/:sid/threads/:tid/resources", post(resources))
        .route("/sessions/:ns/:sid/restore", post(restore))
        .route("/ready", post(ready))
}
