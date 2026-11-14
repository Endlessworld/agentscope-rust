//! Transactional backend: a session state machine over one `RecordStore` scope.
//!
//! Every mutating call is a single session transaction. A [`Lease`] fences writers; a
//! `request_id`/`commit_id`/`execution_id` authenticates everything else. The only clock
//! is `Txn::now()`.

pub(crate) mod commit;
pub(crate) mod identity;
pub(crate) mod model;
pub(crate) mod query;
pub(crate) mod tools;
pub(crate) mod turn;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::session_backend::{BackendCapabilities, SessionBackend};
use crate::store::memory::InMemoryStore;
use crate::store::{RecordStore, Txn};
use crate::types::*;
use crate::{json, DurableError, Result};

use model::*;

/// Backend over any [`RecordStore`]. Use [`TxnBackend::in_memory`] for tests and
/// [`TxnBackend::with_store`] for a durable store.
pub struct TxnBackend {
    store: Arc<dyn RecordStore>,
}

impl TxnBackend {
    pub fn in_memory() -> Self {
        Self {
            store: Arc::new(InMemoryStore::new()),
        }
    }

    pub fn with_store(store: Arc<dyn RecordStore>) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &Arc<dyn RecordStore> {
        &self.store
    }
}

impl Default for TxnBackend {
    fn default() -> Self {
        Self::in_memory()
    }
}

// ---------------------------------------------------------------- helpers

pub(super) fn with_session<T>(
    store: &dyn RecordStore,
    session: &SessionKey,
    work: impl FnOnce(&mut dyn Txn) -> Result<T>,
) -> Result<T> {
    crate::store::txn(store, &session.storage_key(), work)
}

pub(super) fn store_json<T: Serialize>(txn: &mut dyn Txn, part: &str, value: &T) -> Result<()> {
    txn.put(part, Some(serde_json::to_value(value)?))
}

pub(super) fn load_json<T: DeserializeOwned>(txn: &dyn Txn, part: &str) -> Result<Option<T>> {
    match txn.get(part)? {
        Some(value) => Ok(Some(serde_json::from_value(value)?)),
        None => Ok(None),
    }
}

pub(super) fn digest_of<T: Serialize>(value: &T) -> Result<String> {
    Ok(json::hash(&serde_json::to_value(value)?))
}

pub(super) fn load_state(txn: &dyn Txn) -> Result<Option<SessionState>> {
    load_json::<SessionState>(txn, ROOT)
}

pub(super) fn save_state(txn: &mut dyn Txn, state: &SessionState) -> Result<()> {
    store_json(txn, ROOT, state)
}

pub(super) fn require_state(txn: &dyn Txn) -> Result<SessionState> {
    load_state(txn)?.ok_or_else(|| DurableError::not_found("Session does not exist"))
}

pub(super) fn load_thread(txn: &dyn Txn, thread_id: &str) -> Result<Option<ThreadData>> {
    load_json::<ThreadData>(txn, &thread_part(thread_id, "definition"))
}

pub(super) fn save_thread(txn: &mut dyn Txn, thread: &ThreadData) -> Result<()> {
    store_json(
        txn,
        &thread_part(&thread.definition.key.thread_id, "definition"),
        thread,
    )
}

pub(super) fn require_thread(txn: &dyn Txn, key: &ThreadKey) -> Result<ThreadData> {
    let thread = load_thread(txn, &key.thread_id)?
        .ok_or_else(|| DurableError::not_found("Thread does not exist"))?;
    if thread.definition.key != *key {
        return Err(DurableError::conflict("Thread key changed"));
    }
    Ok(thread)
}

pub(super) fn load_turn(txn: &dyn Txn, turn_id: &str) -> Result<Option<TurnData>> {
    load_json::<TurnData>(txn, &turn_part(turn_id))
}

pub(super) fn save_turn(txn: &mut dyn Txn, turn: &TurnData) -> Result<()> {
    store_json(txn, &turn_part(&turn.turn_id), turn)
}

pub(super) fn require_turn(txn: &dyn Txn, turn_id: &str) -> Result<TurnData> {
    load_turn(txn, turn_id)?.ok_or_else(|| DurableError::not_found("Turn does not exist"))
}

/// Turns admitted to a thread, in admission order.
pub(super) fn queue_ids(txn: &dyn Txn, thread_id: &str) -> Result<Vec<String>> {
    let parts = txn.keys(&queue_prefix(thread_id), None, 10_000)?;
    let mut ids = Vec::with_capacity(parts.len());
    for part in parts {
        if let Some(id) = load_json::<String>(txn, &part)? {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// The turn a writer would work on: the first non-terminal one in admission order.
pub(super) fn current_turn(txn: &dyn Txn, thread_id: &str) -> Result<Option<TurnData>> {
    for turn_id in queue_ids(txn, thread_id)? {
        let turn = require_turn(txn, &turn_id)?;
        if !turn.status.terminal() {
            return Ok(Some(turn));
        }
    }
    Ok(None)
}

fn waiting_satisfied(turn: &TurnData, key: &str) -> bool {
    turn.signals.values().any(|signal| {
        signal.key == key
            && !turn.consumed.contains(&signal.id)
            && !turn.withdrawn.contains(&signal.id)
    })
}

/// When this thread wants a writer. `-1` removes it from the readiness index.
pub(super) fn ready_at(thread: &ThreadData, turn: Option<&TurnData>, now: i64) -> i64 {
    let Some(turn) = turn else {
        return -1;
    };
    if thread.status != Lifecycle::Open || turn.status == TurnStatus::Paused {
        return -1;
    }
    if let Some(owner) = &turn.owner {
        if !owner.is_empty() && turn.expires_at > now {
            return turn.expires_at;
        }
    }
    if turn.cancel || turn.status == TurnStatus::Pausing {
        return turn.next_attempt_at;
    }
    let mut ready = turn.next_attempt_at;
    if let Some(recovery) = &turn.recovery {
        if !recovery.retry_allowed() {
            return ready;
        }
        if let Some(retry_at) = recovery.retry_at {
            ready = ready.max(retry_at);
        }
    }
    if let Some(waiting) = &turn.waiting {
        if !waiting_satisfied(turn, &waiting.key) {
            if waiting.deadline == 0 {
                return -1;
            }
            ready = ready.max(waiting.deadline);
        }
    }
    ready
}

pub(super) fn schedule_thread(
    txn: &mut dyn Txn,
    thread: &ThreadData,
    turn: Option<&TurnData>,
    now: i64,
) -> Result<()> {
    let ready = ready_at(thread, turn, now);
    let definition = &thread.definition;
    let payload = Some(serde_json::to_value(definition)?);
    let thread_id = definition.key.thread_id.clone();
    let all_queue = format!("thread:{}", definition.harness());
    txn.schedule(&all_queue, &thread_id, ready, payload.clone())?;
    let selector = HarnessSelector::exact(definition)?;
    let versioned = format!("thread-version:{}", digest_of(&selector)?);
    txn.schedule(&versioned, &thread_id, ready, payload)
}

pub(super) fn cursor_of(state: &SessionState, sequence: u64) -> String {
    format!("{}:{sequence}", state.session.key.storage_key())
}

pub(super) fn parse_cursor(state: &SessionState, cursor: &str) -> Result<u64> {
    let (scope, sequence) = cursor
        .rsplit_once(':')
        .ok_or_else(|| DurableError::invalid("Invalid or foreign event cursor"))?;
    if scope != state.session.key.storage_key() {
        return Err(DurableError::invalid("Invalid or foreign event cursor"));
    }
    let sequence: u64 = sequence
        .parse()
        .map_err(|_| DurableError::invalid("Invalid or foreign event cursor"))?;
    if sequence > state.sequence {
        return Err(DurableError::invalid("Invalid or foreign event cursor"));
    }
    Ok(sequence)
}

/// Appends one public event, idempotent by event id.
#[allow(clippy::too_many_arguments)]
pub(super) fn append_event(
    txn: &mut dyn Txn,
    state: &mut SessionState,
    thread: &ThreadKey,
    turn_id: &str,
    run_id: Option<&str>,
    step_id: Option<&str>,
    event_id: &str,
    event_type: &str,
    data: Value,
) -> Result<()> {
    let identity = format!("{turn_id}\u{0}{event_type}\u{0}{data}");
    let digest = digest_of(&identity)?;
    let known = load_json::<String>(txn, &event_id_part(event_id))?;
    match known {
        Some(existing) if existing == digest => return Ok(()),
        Some(_) => return Err(DurableError::conflict("Event identity changed")),
        None => {}
    }
    state.sequence += 1;
    let sequence = state.sequence;
    let event = PublicEvent {
        schema_version: "1".to_string(),
        event_id: event_id.to_string(),
        session: thread.session.clone(),
        thread_id: thread.thread_id.clone(),
        turn_id: turn_id.to_string(),
        run_id: run_id.map(str::to_string),
        step_id: step_id.map(str::to_string),
        sequence: sequence.to_string(),
        cursor: cursor_of(state, sequence),
        event_type: event_type.to_string(),
        data,
    };
    store_json(txn, &event_part(sequence), &event)?;
    store_json(txn, &event_id_part(event_id), &digest)
}

pub(super) fn assert_owned(turn: &TurnData, lease: &Lease, now: i64) -> Result<()> {
    let owned = turn.owner.as_deref() == Some(lease.owner.as_str())
        && turn.epoch == lease.epoch
        && turn.expires_at > now
        && !turn.status.terminal()
        && turn.turn_id == lease.turn_id;
    if !owned {
        return Err(DurableError::fenced(
            "Execution ownership expired or changed",
        ));
    }
    Ok(())
}

pub(super) fn ttl_millis(ttl: Duration) -> Result<i64> {
    let millis = ttl.as_millis();
    if millis == 0 {
        return Err(DurableError::invalid("Lease ttl must be positive"));
    }
    Ok(millis.min(i64::MAX as u128) as i64)
}

pub(super) fn clamp_limit(limit: usize) -> Result<usize> {
    if limit == 0 || limit > 1000 {
        return Err(DurableError::invalid("Page limit must be 1..=1000"));
    }
    Ok(limit)
}

// ---------------------------------------------------------------- SessionBackend

#[async_trait]
impl SessionBackend for TxnBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            shared_across_hosts: self.store.shared_across_hosts(),
            ..BackendCapabilities::default()
        }
    }

    async fn create(&self, spec: SessionSpec) -> Result<SessionView> {
        identity::create(self.store.as_ref(), spec)
    }

    async fn session(&self, key: &SessionKey) -> Result<SessionView> {
        identity::session(self.store.as_ref(), key)
    }

    async fn close_session(&self, key: &SessionKey) -> Result<SessionView> {
        identity::close_session(self.store.as_ref(), key)
    }

    async fn create_thread(&self, spec: ThreadSpec) -> Result<ThreadView> {
        identity::create_thread(self.store.as_ref(), spec)
    }

    async fn thread(&self, key: &ThreadKey) -> Result<ThreadView> {
        identity::thread(self.store.as_ref(), key)
    }

    async fn thread_state(&self, key: &ThreadKey) -> Result<ThreadStateView> {
        query::thread_state(self.store.as_ref(), key)
    }

    async fn close_thread(&self, key: &ThreadKey) -> Result<ThreadView> {
        identity::close_thread(self.store.as_ref(), key)
    }

    async fn threads(
        &self,
        session: &SessionKey,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ThreadPage> {
        identity::threads(self.store.as_ref(), session, cursor, limit)
    }

    async fn submit(
        &self,
        thread: &ThreadKey,
        request_id: &str,
        input: Value,
    ) -> Result<TurnReceipt> {
        turn::submit(self.store.as_ref(), thread, request_id, input)
    }

    async fn admission(
        &self,
        session: &SessionKey,
        request_id: &str,
    ) -> Result<Option<TurnReceipt>> {
        turn::admission(self.store.as_ref(), session, request_id)
    }

    async fn turn(&self, thread: &ThreadKey, turn_id: &str) -> Result<TurnView> {
        query::turn(self.store.as_ref(), thread, turn_id)
    }

    async fn turns(
        &self,
        thread: &ThreadKey,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<TurnPage> {
        query::turns(self.store.as_ref(), thread, cursor, limit)
    }

    async fn history(
        &self,
        thread: &ThreadKey,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<HistoryPage> {
        query::history(self.store.as_ref(), thread, cursor, limit)
    }

    async fn control(&self, thread: &ThreadKey, command: ControlCommand) -> Result<CommandReceipt> {
        turn::control(self.store.as_ref(), thread, command)
    }

    async fn signal(&self, thread: &ThreadKey, turn_id: &str, signal: Signal) -> Result<()> {
        turn::signal(self.store.as_ref(), thread, turn_id, signal)
    }

    async fn claim(&self, thread: &ThreadKey, owner: &str, ttl: Duration) -> Result<Option<Lease>> {
        turn::claim(self.store.as_ref(), thread, owner, ttl)
    }

    async fn renew(&self, lease: &Lease, ttl: Duration) -> Result<()> {
        turn::renew(self.store.as_ref(), lease, ttl)
    }

    async fn release(&self, lease: &Lease, delay: Duration) -> Result<()> {
        turn::release(self.store.as_ref(), lease, delay)
    }

    async fn frame(&self, lease: &Lease) -> Result<Frame> {
        turn::frame(self.store.as_ref(), lease)
    }

    async fn record_failure(&self, lease: &Lease, failure: Failure) -> Result<RecoveryView> {
        turn::record_failure(self.store.as_ref(), lease, failure)
    }

    async fn commit(&self, lease: &Lease, commit: Commit) -> Result<CommitReceipt> {
        commit::commit(self.store.as_ref(), lease, commit)
    }

    async fn receipt(
        &self,
        thread: &ThreadKey,
        turn_id: &str,
        commit_id: &str,
    ) -> Result<Option<CommitReceipt>> {
        commit::receipt(self.store.as_ref(), thread, turn_id, commit_id)
    }

    async fn prepare_run(&self, lease: &Lease, request_id: &str) -> Result<RunView> {
        tools::prepare_run(self.store.as_ref(), lease, request_id)
    }

    async fn run(&self, thread: &ThreadKey, turn_id: &str, run_id: &str) -> Result<RunView> {
        tools::run(self.store.as_ref(), thread, turn_id, run_id)
    }

    async fn runs(
        &self,
        thread: &ThreadKey,
        turn_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<RunPage> {
        tools::runs(self.store.as_ref(), thread, turn_id, cursor, limit)
    }

    async fn steps(
        &self,
        thread: &ThreadKey,
        turn_id: &str,
        run_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<StepPage> {
        tools::steps(self.store.as_ref(), thread, turn_id, run_id, cursor, limit)
    }

    async fn journal(&self, lease: &Lease, after: u64, limit: usize) -> Result<Vec<Value>> {
        commit::journal(self.store.as_ref(), lease, after, limit)
    }

    async fn read_journal(
        &self,
        thread: &ThreadKey,
        after: u64,
        limit: usize,
    ) -> Result<Vec<Value>> {
        query::read_journal(self.store.as_ref(), thread, after, limit)
    }

    async fn execution(&self, thread: &ThreadKey, execution_id: &str) -> Result<ToolExecution> {
        tools::execution(self.store.as_ref(), thread, execution_id)
    }

    async fn begin_dispatch(&self, lease: &Lease, execution_id: &str) -> Result<ToolExecution> {
        tools::begin_dispatch(self.store.as_ref(), lease, execution_id)
    }

    async fn accept_result(
        &self,
        thread: &ThreadKey,
        execution_id: &str,
        result: ExecutionResult,
    ) -> Result<ToolExecution> {
        tools::accept_result(self.store.as_ref(), thread, execution_id, result)
    }

    async fn ready(&self, selectors: &[HarnessSelector], limit: usize) -> Result<Vec<ThreadSpec>> {
        query::ready(self.store.as_ref(), selectors, limit)
    }

    async fn checkpoints(&self, thread: &ThreadKey, after: u64, limit: usize) -> Result<Vec<u64>> {
        query::checkpoints(self.store.as_ref(), thread, after, limit)
    }

    async fn checkpoint(&self, thread: &ThreadKey, position: u64) -> Result<CheckpointView> {
        query::checkpoint(self.store.as_ref(), thread, position)
    }

    async fn events(
        &self,
        session: &SessionKey,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<EventPage> {
        query::events(self.store.as_ref(), session, cursor, limit)
    }

    async fn snapshot(&self, session: &SessionKey) -> Result<Snapshot> {
        query::snapshot(self.store.as_ref(), session)
    }
}
