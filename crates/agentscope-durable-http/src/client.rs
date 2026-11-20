//! The client half: request shapes plus the [`SessionBackend`] implementation.
//!
//! Every call builds one [`HttpRequest`] from [`crate::wire`], hands it to the injected
//! [`Transport`] and decodes the answer. Nothing here retries: an unconfirmed outcome must reach
//! the caller so it can adopt the receipt instead of blindly resending a commit.

use std::sync::Arc;
use std::time::Duration;

use agentscope_durable::{
    BackendCapabilities, CheckpointView, CommandReceipt, Commit, CommitReceipt, ControlCommand,
    DelegationTarget, DelegationView, DeltaFilter, DurableError, EventPage, EventStreamOptions,
    ExecutionResult, Failure, Frame, HarnessSelector, HistoryPage, Lease, ModelCallPage,
    RecoveryView, ResourceRequest, ResourceValue, Restoration, RunPage, RunView, SessionBackend,
    SessionKey, SessionSpec, SessionView, Snapshot, StepPage, ThreadKey, ThreadPage, ThreadSpec,
    ThreadStateView, ThreadView, ToolExecution, TurnControl, TurnPage, TurnReceipt, TurnView,
};
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::wire::{self, DEFAULT_PREFIX, HEADER_CONTENT_TYPE};
use crate::{
    decode, decode_optional, decode_unit, HttpError, HttpRequest, HttpResponse, HttpResult, Method,
    Transport,
};

fn json_headers() -> Vec<(String, String)> {
    let mut headers = wire::version_headers();
    headers.push((
        HEADER_CONTENT_TYPE.to_string(),
        wire::CONTENT_TYPE_JSON.to_string(),
    ));
    headers
}

/// The version header is added by [`wire::lease_headers`], so a fenced call must not repeat it.
fn content_type_header() -> Vec<(String, String)> {
    vec![(
        HEADER_CONTENT_TYPE.to_string(),
        wire::CONTENT_TYPE_JSON.to_string(),
    )]
}

fn get(path: String, query: Vec<(String, String)>, headers: Vec<(String, String)>) -> HttpRequest {
    HttpRequest {
        method: Method::Get,
        path,
        query,
        headers,
        body: Vec::new(),
    }
}

fn post(
    path: String,
    value: &impl Serialize,
    headers: Vec<(String, String)>,
) -> Result<HttpRequest, DurableError> {
    Ok(HttpRequest {
        method: Method::Post,
        path,
        query: Vec::new(),
        headers,
        body: serde_json::to_vec(value)?,
    })
}

fn empty_post(path: String, headers: Vec<(String, String)>) -> HttpRequest {
    HttpRequest {
        method: Method::Post,
        path,
        query: Vec::new(),
        headers,
        body: Vec::new(),
    }
}

fn timed(body: Value) -> Value {
    body
}
fn query(after: Option<&str>, limit: usize) -> Vec<(String, String)> {
    let mut query = Vec::with_capacity(2);
    if let Some(after) = after {
        query.push(("after".to_string(), wire::encode_segment(after)));
    }
    query.push(("limit".to_string(), limit.to_string()));
    query
}

// ---------------------------------------------------------------- request shapes

/// Claims the writer lease of a thread. `200` answers the lease, or `null` when no turn is ready.
pub fn claim_request(
    prefix: &str,
    thread: &ThreadKey,
    owner: &str,
    ttl: Duration,
) -> Result<HttpRequest, DurableError> {
    if owner.trim().is_empty() {
        return Err(DurableError::invalid("Lease owner must not be blank"));
    }
    post(
        wire::thread_action(prefix, thread, "claim"),
        &serde_json::json!({ "owner": owner, "ttlMillis": ttl.as_millis() as u64 }),
        json_headers(),
    )
}

/// Commits one fenced transition. `200` answers the [`CommitReceipt`]; `409` answers the receipt
/// that already holds the commit.
pub fn commit_request(
    prefix: &str,
    lease: &Lease,
    commit: &Commit,
) -> Result<HttpRequest, DurableError> {
    let mut headers = content_type_header();
    headers.extend(wire::lease_headers(lease));
    post(
        wire::turn_action(prefix, &lease.thread, &lease.turn_id, "commits"),
        commit,
        headers,
    )
}

/// Reads everything a running invocation needs about its turn, under the same fence.
pub fn frame_request(prefix: &str, lease: &Lease) -> Result<HttpRequest, DurableError> {
    Ok(get(
        wire::turn_action(prefix, &lease.thread, &lease.turn_id, "frame"),
        Vec::new(),
        wire::lease_headers(lease),
    ))
}

/// Reads one page of a session's public events after a cursor.
pub fn events_request(
    prefix: &str,
    session: &SessionKey,
    after: Option<&str>,
    limit: usize,
) -> Result<HttpRequest, DurableError> {
    events_request_with(prefix, session, after, limit, DeltaFilter::All)
}

/// Reads one page of a session's public events after a cursor, narrowed by `filter`.
pub fn events_request_with(
    prefix: &str,
    session: &SessionKey,
    after: Option<&str>,
    limit: usize,
    filter: DeltaFilter,
) -> Result<HttpRequest, DurableError> {
    wire::page_limit(limit)?;
    let mut parameters = query(after, limit);
    if filter != DeltaFilter::All {
        parameters.push(("deltas".to_string(), filter.as_str().to_string()));
    }
    Ok(get(
        wire::events_path(prefix, session),
        parameters,
        wire::version_headers(),
    ))
}

/// Reads the receipt of one commit; `200` answers it or `null` when the commit never landed.
pub fn receipt_request(
    prefix: &str,
    thread: &ThreadKey,
    turn_id: &str,
    commit_id: &str,
) -> Result<HttpRequest, DurableError> {
    Ok(get(
        wire::commit_receipt_path(prefix, thread, turn_id, commit_id),
        Vec::new(),
        wire::version_headers(),
    ))
}

// ---------------------------------------------------------------- five-call client

/// The five calls a durable host needs, with no state of its own.
pub struct HttpBackend {
    transport: Arc<dyn Transport>,
    prefix: String,
}

impl HttpBackend {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self::with_prefix(transport, DEFAULT_PREFIX)
    }

    pub fn with_prefix(transport: Arc<dyn Transport>, prefix: impl Into<String>) -> Self {
        Self {
            transport,
            prefix: prefix.into(),
        }
    }

    async fn send(&self, request: HttpRequest) -> HttpResult<HttpResponse> {
        self.transport.send(request).await.map_err(|error| {
            HttpError::from(DurableError::unavailable(format!(
                "durable HTTP outcome is unconfirmed: {error}"
            )))
        })
    }

    pub async fn claim(
        &self,
        thread: &ThreadKey,
        owner: &str,
        ttl: Duration,
    ) -> HttpResult<Option<Lease>> {
        let request = claim_request(&self.prefix, thread, owner, ttl)?;
        decode_optional(self.send(request).await?)
    }

    pub async fn commit(&self, lease: &Lease, commit: &Commit) -> HttpResult<CommitReceipt> {
        let request = commit_request(&self.prefix, lease, commit)?;
        decode(self.send(request).await?)
    }

    pub async fn frame(&self, lease: &Lease) -> HttpResult<Frame> {
        let request = frame_request(&self.prefix, lease)?;
        decode(self.send(request).await?)
    }

    pub async fn events(
        &self,
        session: &SessionKey,
        after: Option<&str>,
        limit: usize,
    ) -> HttpResult<EventPage> {
        let request = events_request(&self.prefix, session, after, limit)?;
        decode(self.send(request).await?)
    }

    pub async fn receipt(
        &self,
        thread: &ThreadKey,
        turn_id: &str,
        commit_id: &str,
    ) -> HttpResult<Option<CommitReceipt>> {
        let request = receipt_request(&self.prefix, thread, turn_id, commit_id)?;
        decode_optional(self.send(request).await?)
    }
}

// ---------------------------------------------------------------- full backend

/// A [`SessionBackend`] that runs every call over the injected transport.
///
/// The remote server owns the transaction; this type only proves the caller asked correctly and
/// never pretends a failure was a success. `capabilities()` is answered from what the server
/// declared at construction time, because the trait asks it synchronously while a running turn
/// polls it.
pub struct HttpSessionBackend {
    transport: Arc<dyn Transport>,
    prefix: String,
    capabilities: BackendCapabilities,
}

impl HttpSessionBackend {
    /// Assumes the guarantees a versioned remote backend offers: atomic commit, fences, waits and
    /// replay across hosts. A server that narrows them passes its own answer to
    /// [`with_capabilities`](Self::with_capabilities).
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self::with_prefix(transport, DEFAULT_PREFIX)
    }

    pub fn with_prefix(transport: Arc<dyn Transport>, prefix: impl Into<String>) -> Self {
        Self {
            transport,
            prefix: prefix.into(),
            capabilities: BackendCapabilities {
                shared_across_hosts: true,
                ..BackendCapabilities::default()
            },
        }
    }

    pub fn with_capabilities(mut self, capabilities: BackendCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    fn prefix(&self) -> &str {
        &self.prefix
    }

    async fn send(&self, request: HttpRequest) -> HttpResult<HttpResponse> {
        self.transport.send(request).await.map_err(|error| {
            HttpError::from(DurableError::unavailable(format!(
                "durable HTTP outcome is unconfirmed: {error}"
            )))
        })
    }

    async fn read<T: DeserializeOwned>(&self, request: HttpRequest) -> HttpResult<T> {
        decode(self.send(request).await?)
    }

    async fn read_optional<T: DeserializeOwned>(
        &self,
        request: HttpRequest,
    ) -> HttpResult<Option<T>> {
        decode_optional(self.send(request).await?)
    }

    async fn write<T: DeserializeOwned>(&self, request: HttpRequest) -> HttpResult<T> {
        decode(self.send(request).await?)
    }

    async fn write_unit(&self, request: HttpRequest) -> HttpResult<()> {
        decode_unit(self.send(request).await?)
    }

    fn leased(&self, lease: &Lease) -> (String, Vec<(String, String)>) {
        (self.prefix().to_string(), wire::lease_headers(lease))
    }
}

#[async_trait]
impl SessionBackend for HttpSessionBackend {
    fn capabilities(&self) -> BackendCapabilities {
        self.capabilities.clone()
    }

    async fn create(&self, spec: SessionSpec) -> agentscope_durable::Result<SessionView> {
        let request = post(wire::sessions_path(self.prefix()), &spec, json_headers())?;
        self.read(request).await.map_err(Into::into)
    }

    async fn session(&self, key: &SessionKey) -> agentscope_durable::Result<SessionView> {
        let request = get(
            wire::session_path(self.prefix(), key),
            Vec::new(),
            wire::version_headers(),
        );
        self.read(request).await.map_err(Into::into)
    }

    async fn close_session(&self, key: &SessionKey) -> agentscope_durable::Result<SessionView> {
        let request = empty_post(
            wire::session_action(self.prefix(), key, "close"),
            json_headers(),
        );
        self.write(request).await.map_err(Into::into)
    }

    async fn create_thread(&self, spec: ThreadSpec) -> agentscope_durable::Result<ThreadView> {
        let request = post(
            wire::threads_path(self.prefix(), &spec.key.session),
            &spec,
            json_headers(),
        )?;
        self.write(request).await.map_err(Into::into)
    }

    async fn thread(&self, key: &ThreadKey) -> agentscope_durable::Result<ThreadView> {
        let request = get(
            wire::thread_path(self.prefix(), key),
            Vec::new(),
            wire::version_headers(),
        );
        self.read(request).await.map_err(Into::into)
    }

    async fn close_thread(&self, key: &ThreadKey) -> agentscope_durable::Result<ThreadView> {
        let request = empty_post(
            wire::thread_action(self.prefix(), key, "close"),
            json_headers(),
        );
        self.write(request).await.map_err(Into::into)
    }

    async fn thread_state(&self, key: &ThreadKey) -> agentscope_durable::Result<ThreadStateView> {
        let request = get(
            wire::thread_action(self.prefix(), key, "state"),
            Vec::new(),
            wire::version_headers(),
        );
        self.read(request).await.map_err(Into::into)
    }

    async fn threads(
        &self,
        session: &SessionKey,
        cursor: Option<&str>,
        limit: usize,
    ) -> agentscope_durable::Result<ThreadPage> {
        wire::page_limit(limit)?;
        let request = get(
            wire::threads_path(self.prefix(), session),
            query(cursor, limit),
            wire::version_headers(),
        );
        self.read(request).await.map_err(Into::into)
    }

    async fn submit(
        &self,
        thread: &ThreadKey,
        request_id: &str,
        input: Value,
    ) -> agentscope_durable::Result<TurnReceipt> {
        let request = post(
            wire::thread_action(self.prefix(), thread, "turns"),
            &serde_json::json!({ "requestId": request_id, "input": input }),
            json_headers(),
        )?;
        self.write(request).await.map_err(Into::into)
    }

    async fn admission(
        &self,
        session: &SessionKey,
        request_id: &str,
    ) -> agentscope_durable::Result<Option<TurnReceipt>> {
        let request = get(
            wire::admission_path(self.prefix(), session, request_id),
            Vec::new(),
            wire::version_headers(),
        );
        self.read_optional(request).await.map_err(Into::into)
    }

    async fn turn(
        &self,
        thread: &ThreadKey,
        turn_id: &str,
    ) -> agentscope_durable::Result<TurnView> {
        let request = get(
            wire::turn_path(self.prefix(), thread, turn_id),
            Vec::new(),
            wire::version_headers(),
        );
        self.read(request).await.map_err(Into::into)
    }

    async fn turns(
        &self,
        thread: &ThreadKey,
        cursor: Option<&str>,
        limit: usize,
    ) -> agentscope_durable::Result<TurnPage> {
        wire::page_limit(limit)?;
        let request = get(
            wire::thread_action(self.prefix(), thread, "turns"),
            query(cursor, limit),
            wire::version_headers(),
        );
        self.read(request).await.map_err(Into::into)
    }

    async fn history(
        &self,
        thread: &ThreadKey,
        cursor: Option<&str>,
        limit: usize,
    ) -> agentscope_durable::Result<HistoryPage> {
        wire::page_limit(limit)?;
        let request = get(
            wire::thread_action(self.prefix(), thread, "history"),
            query(cursor, limit),
            wire::version_headers(),
        );
        self.read(request).await.map_err(Into::into)
    }

    async fn control(
        &self,
        thread: &ThreadKey,
        command: ControlCommand,
    ) -> agentscope_durable::Result<CommandReceipt> {
        let request = post(
            wire::thread_action(self.prefix(), thread, "control"),
            &command,
            json_headers(),
        )?;
        self.write(request).await.map_err(Into::into)
    }

    async fn signal(
        &self,
        thread: &ThreadKey,
        turn_id: &str,
        signal: agentscope_durable::Signal,
    ) -> agentscope_durable::Result<()> {
        let request = post(
            wire::turn_action(self.prefix(), thread, turn_id, "signals"),
            &signal,
            json_headers(),
        )?;
        self.write_unit(request).await.map_err(Into::into)
    }

    async fn claim(
        &self,
        thread: &ThreadKey,
        owner: &str,
        ttl: Duration,
    ) -> agentscope_durable::Result<Option<Lease>> {
        let request = claim_request(self.prefix(), thread, owner, ttl)?;
        self.read_optional(request).await.map_err(Into::into)
    }

    async fn renew(&self, lease: &Lease, ttl: Duration) -> agentscope_durable::Result<()> {
        let (_, mut headers) = self.leased(lease);
        headers.push((
            HEADER_CONTENT_TYPE.to_string(),
            wire::CONTENT_TYPE_JSON.to_string(),
        ));
        let request = post(
            wire::turn_action(self.prefix(), &lease.thread, &lease.turn_id, "lease/renew"),
            &timed(serde_json::json!({ "ttlMillis": ttl.as_millis() as u64 })),
            headers,
        )?;
        self.write_unit(request).await.map_err(Into::into)
    }

    async fn release(&self, lease: &Lease, delay: Duration) -> agentscope_durable::Result<()> {
        let (_, mut headers) = self.leased(lease);
        headers.push((
            HEADER_CONTENT_TYPE.to_string(),
            wire::CONTENT_TYPE_JSON.to_string(),
        ));
        let request = post(
            wire::turn_action(
                self.prefix(),
                &lease.thread,
                &lease.turn_id,
                "lease/release",
            ),
            &timed(serde_json::json!({ "delayMillis": delay.as_millis() as u64 })),
            headers,
        )?;
        self.write_unit(request).await.map_err(Into::into)
    }

    async fn frame(&self, lease: &Lease) -> agentscope_durable::Result<Frame> {
        let request = frame_request(self.prefix(), lease)?;
        self.read(request).await.map_err(Into::into)
    }

    async fn control_state(&self, lease: &Lease) -> agentscope_durable::Result<TurnControl> {
        let request = get(
            wire::turn_action(
                self.prefix(),
                &lease.thread,
                &lease.turn_id,
                "control-state",
            ),
            Vec::new(),
            wire::lease_headers(lease),
        );
        self.read(request).await.map_err(Into::into)
    }

    async fn record_failure(
        &self,
        lease: &Lease,
        failure: Failure,
    ) -> agentscope_durable::Result<RecoveryView> {
        let (_, mut headers) = self.leased(lease);
        headers.push((
            HEADER_CONTENT_TYPE.to_string(),
            wire::CONTENT_TYPE_JSON.to_string(),
        ));
        let request = post(
            wire::turn_action(self.prefix(), &lease.thread, &lease.turn_id, "failure"),
            &failure,
            headers,
        )?;
        self.write(request).await.map_err(Into::into)
    }

    async fn commit(
        &self,
        lease: &Lease,
        commit: Commit,
    ) -> agentscope_durable::Result<CommitReceipt> {
        let request = commit_request(self.prefix(), lease, &commit)?;
        self.write(request).await.map_err(Into::into)
    }

    async fn receipt(
        &self,
        thread: &ThreadKey,
        turn_id: &str,
        commit_id: &str,
    ) -> agentscope_durable::Result<Option<CommitReceipt>> {
        let request = receipt_request(self.prefix(), thread, turn_id, commit_id)?;
        self.read_optional(request).await.map_err(Into::into)
    }

    async fn prepare_run(
        &self,
        lease: &Lease,
        request_id: &str,
    ) -> agentscope_durable::Result<RunView> {
        let (_, mut headers) = self.leased(lease);
        headers.push((
            HEADER_CONTENT_TYPE.to_string(),
            wire::CONTENT_TYPE_JSON.to_string(),
        ));
        let request = post(
            wire::turn_action(self.prefix(), &lease.thread, &lease.turn_id, "runs"),
            &serde_json::json!({ "requestId": request_id }),
            headers,
        )?;
        self.write(request).await.map_err(Into::into)
    }

    async fn run(
        &self,
        thread: &ThreadKey,
        turn_id: &str,
        run_id: &str,
    ) -> agentscope_durable::Result<RunView> {
        let sub = format!("runs/{}", wire::encode_segment(run_id));
        let request = get(
            wire::turn_sub(self.prefix(), thread, turn_id, &sub),
            Vec::new(),
            wire::version_headers(),
        );
        self.read(request).await.map_err(Into::into)
    }

    async fn runs(
        &self,
        thread: &ThreadKey,
        turn_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> agentscope_durable::Result<RunPage> {
        wire::page_limit(limit)?;
        let request = get(
            wire::turn_sub(self.prefix(), thread, turn_id, "runs"),
            query(cursor, limit),
            wire::version_headers(),
        );
        self.read(request).await.map_err(Into::into)
    }

    async fn steps(
        &self,
        thread: &ThreadKey,
        turn_id: &str,
        run_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> agentscope_durable::Result<StepPage> {
        wire::page_limit(limit)?;
        let sub = format!("runs/{}/steps", wire::encode_segment(run_id));
        let request = get(
            wire::turn_sub(self.prefix(), thread, turn_id, &sub),
            query(cursor, limit),
            wire::version_headers(),
        );
        self.read(request).await.map_err(Into::into)
    }

    async fn model_calls(
        &self,
        thread: &ThreadKey,
        turn_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> agentscope_durable::Result<ModelCallPage> {
        wire::page_limit(limit)?;
        let request = get(
            wire::turn_sub(self.prefix(), thread, turn_id, "model-calls"),
            query(cursor, limit),
            wire::version_headers(),
        );
        self.read(request).await.map_err(Into::into)
    }

    async fn journal(
        &self,
        lease: &Lease,
        after: u64,
        limit: usize,
    ) -> agentscope_durable::Result<Vec<Value>> {
        wire::page_limit(limit)?;
        let request = get(
            wire::turn_sub(self.prefix(), &lease.thread, &lease.turn_id, "journal"),
            vec![
                ("after".to_string(), after.to_string()),
                ("limit".to_string(), limit.to_string()),
            ],
            wire::lease_headers(lease),
        );
        self.read(request).await.map_err(Into::into)
    }

    async fn read_journal(
        &self,
        thread: &ThreadKey,
        after: u64,
        limit: usize,
    ) -> agentscope_durable::Result<Vec<Value>> {
        wire::page_limit(limit)?;
        let request = get(
            wire::thread_action(self.prefix(), thread, "journal"),
            vec![
                ("after".to_string(), after.to_string()),
                ("limit".to_string(), limit.to_string()),
            ],
            wire::version_headers(),
        );
        self.read(request).await.map_err(Into::into)
    }

    async fn execution(
        &self,
        thread: &ThreadKey,
        execution_id: &str,
    ) -> agentscope_durable::Result<ToolExecution> {
        let request = get(
            wire::execution_path(self.prefix(), thread, execution_id),
            Vec::new(),
            wire::version_headers(),
        );
        self.read(request).await.map_err(Into::into)
    }

    async fn begin_dispatch(
        &self,
        lease: &Lease,
        execution_id: &str,
    ) -> agentscope_durable::Result<ToolExecution> {
        let (_, mut headers) = self.leased(lease);
        headers.push((
            HEADER_CONTENT_TYPE.to_string(),
            wire::CONTENT_TYPE_JSON.to_string(),
        ));
        let request = post(
            wire::turn_action(self.prefix(), &lease.thread, &lease.turn_id, "dispatch"),
            &serde_json::json!({ "executionId": execution_id }),
            headers,
        )?;
        self.write(request).await.map_err(Into::into)
    }

    async fn accept_result(
        &self,
        thread: &ThreadKey,
        execution_id: &str,
        result: ExecutionResult,
    ) -> agentscope_durable::Result<ToolExecution> {
        let request = post(
            wire::execution_action(self.prefix(), thread, execution_id, "result"),
            &serde_json::json!({ "executionId": execution_id, "result": result }),
            json_headers(),
        )?;
        self.write(request).await.map_err(Into::into)
    }

    async fn ready(
        &self,
        selectors: &[HarnessSelector],
        limit: usize,
    ) -> agentscope_durable::Result<Vec<ThreadSpec>> {
        wire::page_limit(limit)?;
        let request = post(
            wire::ready_path(self.prefix()),
            &serde_json::json!({ "selectors": selectors, "limit": limit }),
            json_headers(),
        )?;
        self.read(request).await.map_err(Into::into)
    }

    async fn checkpoints(
        &self,
        thread: &ThreadKey,
        after: u64,
        limit: usize,
    ) -> agentscope_durable::Result<Vec<u64>> {
        wire::page_limit(limit)?;
        let request = get(
            wire::checkpoints_path(self.prefix(), thread),
            vec![
                ("after".to_string(), after.to_string()),
                ("limit".to_string(), limit.to_string()),
            ],
            wire::version_headers(),
        );
        self.read(request).await.map_err(Into::into)
    }

    async fn checkpoint(
        &self,
        thread: &ThreadKey,
        position: u64,
    ) -> agentscope_durable::Result<CheckpointView> {
        let request = get(
            wire::checkpoint_path(self.prefix(), thread, position),
            Vec::new(),
            wire::version_headers(),
        );
        self.read(request).await.map_err(Into::into)
    }

    async fn events(
        &self,
        session: &SessionKey,
        cursor: Option<&str>,
        limit: usize,
    ) -> agentscope_durable::Result<EventPage> {
        let request = events_request(self.prefix(), session, cursor, limit)?;
        self.read(request).await.map_err(Into::into)
    }

    async fn events_with(
        &self,
        session: &SessionKey,
        cursor: Option<&str>,
        limit: usize,
        options: EventStreamOptions,
    ) -> agentscope_durable::Result<EventPage> {
        let request = events_request_with(self.prefix(), session, cursor, limit, options.deltas)?;
        self.read(request).await.map_err(Into::into)
    }

    async fn snapshot(&self, session: &SessionKey) -> agentscope_durable::Result<Snapshot> {
        let request = get(
            wire::snapshot_path(self.prefix(), session),
            Vec::new(),
            wire::version_headers(),
        );
        self.read(request).await.map_err(Into::into)
    }

    async fn prune_events(
        &self,
        session: &SessionKey,
        keep_newest: u64,
    ) -> agentscope_durable::Result<u64> {
        let request = post(
            wire::events_prune_path(self.prefix(), session),
            &serde_json::json!({ "keepNewest": keep_newest }),
            json_headers(),
        )?;
        self.write(request).await.map_err(Into::into)
    }

    async fn resources(
        &self,
        lease: &Lease,
        request: ResourceRequest,
    ) -> agentscope_durable::Result<Vec<ResourceValue>> {
        let (_, headers) = self.leased(lease);
        let mut headers = headers;
        headers.push((
            HEADER_CONTENT_TYPE.to_string(),
            wire::CONTENT_TYPE_JSON.to_string(),
        ));
        let request = post(
            wire::resources_path(self.prefix(), &lease.thread),
            &request,
            headers,
        )?;
        self.write(request).await.map_err(Into::into)
    }

    async fn delegate(
        &self,
        lease: &Lease,
        execution_id: &str,
        request_id: &str,
        target: DelegationTarget,
    ) -> agentscope_durable::Result<DelegationView> {
        let (_, mut headers) = self.leased(lease);
        headers.push((
            HEADER_CONTENT_TYPE.to_string(),
            wire::CONTENT_TYPE_JSON.to_string(),
        ));
        let request = post(
            wire::turn_action(self.prefix(), &lease.thread, &lease.turn_id, "delegate"),
            &serde_json::json!({
                "executionId": execution_id,
                "requestId": request_id,
                "target": target,
            }),
            headers,
        )?;
        self.write(request).await.map_err(Into::into)
    }

    async fn restore(&self, restoration: Restoration) -> agentscope_durable::Result<ThreadView> {
        let request = post(
            wire::restore_path(self.prefix(), &restoration.target.key.session),
            &restoration,
            json_headers(),
        )?;
        self.write(request).await.map_err(Into::into)
    }
}
