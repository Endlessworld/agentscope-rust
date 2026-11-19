//! Transactional backend: a session state machine over one `RecordStore` scope.
//!
//! Every mutating call is a single session transaction. A [`Lease`] fences writers; a
//! `request_id`/`commit_id`/`execution_id` authenticates everything else. The only clock
//! is `Txn::now()`.

pub(crate) mod branch;
pub(crate) mod commit;
pub(crate) mod compact;
pub(crate) mod identity;
pub(crate) mod model;
pub(crate) mod query;
pub(crate) mod resource;
pub(crate) mod tools;
pub(crate) mod turn;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// How many granted expiries the memo below remembers before it is cleared.
///
/// The entries are a few dozen bytes each and nothing depends on them, so the bound only has to
/// keep a process that claims thousands of turns from growing a map forever.
const GRANTED_MEMO_ENTRIES: usize = 64;

/// A lease extension this process observed, and the local instant it was observed at.
///
/// `remaining` is measured on the *store's* clock (the `now` of the transaction that granted it),
/// `since` is a local monotonic instant, and together they answer "how much of the lease is left
/// now" without ever comparing the two clocks. Comparing them is what the memo used to do, and it
/// is wrong in both directions for a store on another host: a server clock running ahead makes a
/// fresh lease look nearly expired (one redundant write) while one running behind makes a lapsed
/// lease look fresh — which skips the renewal that keeps the fence and costs the turn its lease.
struct Grant {
    remaining_millis: i64,
    since: Instant,
}

impl Grant {
    fn remaining(&self) -> i64 {
        let elapsed = self.since.elapsed().as_millis().min(i64::MAX as u128) as i64;
        self.remaining_millis.saturating_sub(elapsed)
    }
}

/// Backend over any [`RecordStore`]. Use [`TxnBackend::in_memory`] for tests and
/// [`TxnBackend::with_store`] for a durable store.
pub struct TxnBackend {
    store: Arc<dyn RecordStore>,
    /// The lease this process last observed, keyed by session scope and thread epoch.
    ///
    /// A lease extension that is already in force needs neither a read nor a write, and one
    /// session document is the whole conversation: skipping the transaction is what keeps a
    /// journal flush from re-reading (and re-parsing) tens of megabytes just to confirm an
    /// expiry it wrote itself a moment ago. Only an optimisation — a miss asks the store, which
    /// is the behaviour without the memo — and a stale value can only be *older* than the store's
    /// (nothing in this process shortens a lease it granted), so the worst case is one redundant
    /// write, never a lease that lapses unextended.
    granted: Mutex<HashMap<(String, u64), Grant>>,
}

impl TxnBackend {
    pub fn in_memory() -> Self {
        Self {
            store: Arc::new(InMemoryStore::new()),
            granted: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_store(store: Arc<dyn RecordStore>) -> Self {
        Self {
            store,
            granted: Mutex::new(HashMap::new()),
        }
    }

    fn lease_key(lease: &Lease) -> (String, u64) {
        (lease.thread.session.storage_key(), lease.epoch)
    }

    /// Records how long the store said was left on the lease, as of now.
    fn remember(&self, lease: &Lease, remaining_millis: i64) {
        if let Ok(mut granted) = self.granted.lock() {
            // Bounded: one entry per claim, and a long-lived process claims once per turn. A map
            // that grows past this is cleared wholesale — an empty memo only means the next renew
            // asks the store, which is the correct behaviour anyway.
            if granted.len() >= GRANTED_MEMO_ENTRIES {
                granted.clear();
            }
            granted.insert(
                Self::lease_key(lease),
                Grant {
                    remaining_millis,
                    since: Instant::now(),
                },
            );
        }
    }

    fn forget(&self, lease: &Lease) {
        if let Ok(mut granted) = self.granted.lock() {
            granted.remove(&Self::lease_key(lease));
        }
    }

    /// Whether the extension `ttl` requests is already in force.
    fn already_granted(&self, lease: &Lease, ttl: Duration) -> bool {
        let ttl = ttl.as_millis().min(i64::MAX as u128) as i64;
        let Ok(granted) = self.granted.lock() else {
            return false;
        };
        let Some(grant) = granted.get(&Self::lease_key(lease)) else {
            return false;
        };
        // The headroom rule itself lives in `turn::renew`; the memo only answers it with the
        // store's own measurement of what is left.
        grant.remaining() > turn::renew_headroom(ttl)
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

/// Runs `work` in a read-only transaction of one session scope.
///
/// Every read-side query is a pure read, so the store may answer from the document it read the
/// previous time instead of reading the file again; see [`RecordStore::read_transaction`].
pub(super) fn with_session_read<T>(
    store: &dyn RecordStore,
    session: &SessionKey,
    work: impl FnOnce(&mut dyn Txn) -> Result<T>,
) -> Result<T> {
    crate::store::txn_read(store, &session.storage_key(), work)
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
    Ok(json::hash(&serde_json::to_value(value)?)?)
}

/// Whether a persisted digest still describes `value`.
///
/// Digests written before the RFC 8785 switch are accepted under the legacy rendering too, so
/// sessions, admissions, receipts and failures persisted by an older build keep verifying.
/// New digests are always RFC 8785.
pub(super) fn digest_matches<T: Serialize>(stored: &str, value: &T) -> Result<bool> {
    Ok(json::matches(stored, &serde_json::to_value(value)?))
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

/// How many turn ids one read of a thread queue returns.
///
/// The queue is read in pages so that a conversation is never cut off at a fixed size: a single
/// read used to stop at ten thousand turns, after which the newest turn was invisible to
/// [`current_turn`], `ready_at` answered `-1` and the thread stopped being scheduled while
/// `submit` kept succeeding.
const QUEUE_PAGE: usize = 1_000;

/// How many turns one thread may hold before the backend refuses to walk its queue.
///
/// Some bound has to exist — walking a queue of unknown length inside one transaction is the read
/// amplification this store is trying to avoid — but it is reported, never applied silently: a
/// caller that hits it gets an `INVALID` failure instead of a thread that quietly stops running.
const QUEUE_MAX_TURNS: usize = 1_000_000;

fn queue_over_limit(turns: usize) -> DurableError {
    DurableError::invalid(format!(
        "thread queue holds more than {QUEUE_MAX_TURNS} turns ({turns} read)"
    ))
}

/// One page of a thread's turn queue: the ids in admission order, and the key to continue after
/// (`None` when the queue ends here).
fn queue_page(
    txn: &dyn Txn,
    thread_id: &str,
    after: Option<&str>,
) -> Result<(Vec<String>, Option<String>)> {
    // One id more than the page tells whether another page exists without a second round trip.
    let mut parts = txn.keys(&queue_prefix(thread_id), after, QUEUE_PAGE + 1)?;
    let more = parts.len() > QUEUE_PAGE;
    if more {
        parts.truncate(QUEUE_PAGE);
    }
    let next = if more { parts.last().cloned() } else { None };
    let mut ids = Vec::with_capacity(parts.len());
    for part in parts {
        if let Some(id) = load_json::<String>(txn, &part)? {
            ids.push(id);
        }
    }
    Ok((ids, next))
}

/// Turns admitted to a thread, in admission order.
///
/// Paged rather than windowed: the only limit is [`QUEUE_MAX_TURNS`], and reaching it is an
/// error. A cap that is applied silently is how a long conversation stops being scheduled without
/// anybody noticing.
pub(super) fn queue_ids(txn: &dyn Txn, thread_id: &str) -> Result<Vec<String>> {
    let mut ids: Vec<String> = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let (page, next) = queue_page(txn, thread_id, after.as_deref())?;
        if ids.len() + page.len() > QUEUE_MAX_TURNS {
            return Err(queue_over_limit(ids.len() + page.len()));
        }
        ids.extend(page);
        match next {
            Some(next) => after = Some(next),
            None => return Ok(ids),
        }
    }
}

/// How many keys one [`scan_keys`] page reads.
///
/// Scans used to ask for a hundred thousand keys in a single call: the page was materialized in
/// full, and a prefix with more than that was silently truncated — a caller then worked on a
/// prefix it believed it had seen completely. Paging bounds the memory of one step and removes
/// the ceiling.
const SCAN_PAGE: usize = 1_000;

/// Walks every key under `prefix` in order, one page at a time.
///
/// `visit` may read and write through the transaction and returns `false` to stop the walk (a
/// scan bounded by a watermark, for instance). Deletions are safe: the next page is asked for
/// with the last key this one returned as its cursor, and a key that is already gone only makes
/// the store start further along.
pub(super) fn scan_keys(
    txn: &mut dyn Txn,
    prefix: &str,
    start: Option<&str>,
    mut visit: impl FnMut(&mut dyn Txn, &str) -> Result<bool>,
) -> Result<()> {
    let mut after = start.map(str::to_string);
    loop {
        let page = txn.keys(prefix, after.as_deref(), SCAN_PAGE)?;
        let full = page.len() == SCAN_PAGE;
        let Some(last) = page.last().cloned() else {
            return Ok(());
        };
        for part in &page {
            if !visit(&mut *txn, part)? {
                return Ok(());
            }
        }
        if !full {
            return Ok(());
        }
        after = Some(last);
    }
}

/// The turn a writer would work on: the first non-terminal one in admission order.
///
/// Starts at the thread's materialized prefix of settled turns (see [`advance_settled`]): a
/// conversation whose finished front has been materialized costs one page and one turn read,
/// however many turns it already holds.
pub(super) fn current_turn(txn: &dyn Txn, thread: &ThreadData) -> Result<Option<TurnData>> {
    current_turn_after(txn, &thread.definition.key.thread_id, thread.settled)
}

/// [`current_turn`] for a caller that knows only the id, and so starts at `settled`.
///
/// `settled` is only ever a *lower bound* on the finished front: starting below it re-reads turns
/// that are already known to be terminal, starting above it would skip work. A caller without
/// the thread record passes `0` and pays the full scan.
pub(super) fn current_turn_after(
    txn: &dyn Txn,
    thread_id: &str,
    settled: u64,
) -> Result<Option<TurnData>> {
    let mut after = (settled > 0).then(|| queue_part(thread_id, settled));
    let mut read = 0usize;
    loop {
        let (page, next) = queue_page(txn, thread_id, after.as_deref())?;
        read += page.len();
        // The bound is on the whole queue, not just the part scanned here: it exists to stop a
        // walk of unknown length inside one transaction, and it is reported, never applied
        // silently. The prefix does not raise it.
        if read.saturating_add(usize::try_from(settled).unwrap_or(usize::MAX)) > QUEUE_MAX_TURNS {
            return Err(queue_over_limit(
                read.saturating_add(usize::try_from(settled).unwrap_or(usize::MAX)),
            ));
        }
        for turn_id in page {
            let turn = require_turn(txn, &turn_id)?;
            if !turn.status.terminal() {
                return Ok(Some(turn));
            }
        }
        match next {
            Some(next) => after = Some(next),
            None => return Ok(None),
        }
    }
}

/// How many queue slots one write may materialize into [`ThreadData::settled`].
///
/// A commit settles the head of the unsettled suffix, so the walk below stops at the first live
/// turn and normally costs one read. The cap bounds the *upgrade* case: a thread written before
/// the prefix was materialized starts at zero and would otherwise walk its whole finished history
/// inside one transaction (and one lock). Readers scan from the prefix and stay correct while it
/// catches up, so a few commits are enough, and the prefix never overshoots.
const SETTLED_WALK: u64 = 64;

/// Advances the materialized prefix of settled turns as far as the queue agrees.
///
/// `settled` counts leading queue slots whose turn is terminal. Because a turn only becomes
/// terminal through a commit (or a cancellation of a turn no writer holds), advancing it exactly
/// when this write is about to store the thread record costs no extra transaction: the prefix
/// rides the document this commit rewrites anyway.
///
/// Only a prefix ever advances: the walk stops at the first non-terminal turn, so a turn that
/// settles out of order (a queued turn cancelled behind a running one) does not push the prefix
/// past work that is still pending. Returns how many slots it materialized, for tests.
pub(super) fn advance_settled(txn: &dyn Txn, thread: &mut ThreadData) -> Result<u64> {
    let thread_id = thread.definition.key.thread_id.clone();
    let mut advanced = 0;
    while advanced < SETTLED_WALK {
        let part = queue_part(&thread_id, thread.settled + 1);
        let Some(turn_id) = load_json::<String>(txn, &part)? else {
            break;
        };
        let Some(turn) = load_turn(txn, &turn_id)? else {
            break;
        };
        if !turn.status.terminal() {
            break;
        }
        thread.settled += 1;
        advanced += 1;
    }
    Ok(advanced)
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
    // A settled turn is not work. Leaving it in the index is what made every settle cost one more
    // claim — and one more rewrite of the whole session document — that could only find out there
    // is nothing to do, because `current_turn` skips terminal turns anyway.
    if thread.status != Lifecycle::Open
        || turn.status == TurnStatus::Paused
        || turn.status.terminal()
    {
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
    // The work id is the whole *thread* key, not its bare id: one queue is shared by every session
    // of a harness, and a thread id (`main`) is unique within its session, not across them. With
    // the bare id two sessions of the same harness wrote the same readiness record and the later
    // write replaced the earlier one, so the replaced session was scheduled nowhere: `submit` kept
    // succeeding, but no tick ever found that thread again.
    let work_id = definition.key.storage_key();
    let all_queue = format!("thread:{}", definition.harness());
    txn.schedule(&all_queue, &work_id, ready, payload.clone())?;
    let selector = HarnessSelector::exact(definition)?;
    let versioned = format!("thread-version:{}", digest_of(&selector)?);
    txn.schedule(&versioned, &work_id, ready, payload)
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
        let claimed = turn::claim(self.store.as_ref(), thread, owner, ttl)?;
        if let Some((lease, remaining)) = &claimed {
            self.remember(lease, *remaining);
        }
        Ok(claimed.map(|(lease, _)| lease))
    }

    async fn renew(&self, lease: &Lease, ttl: Duration) -> Result<()> {
        if self.already_granted(lease, ttl) {
            return Ok(());
        }
        let remaining = turn::renew(self.store.as_ref(), lease, ttl)?;
        self.remember(lease, remaining);
        Ok(())
    }

    async fn release(&self, lease: &Lease, delay: Duration) -> Result<()> {
        turn::release(self.store.as_ref(), lease, delay)
            // Forgotten on both paths: a failed release either left the lease to expire (the memo
            // then only under-reports the time left) or the caller retries it.
            .map(|()| self.forget(lease))
    }

    async fn frame(&self, lease: &Lease) -> Result<Frame> {
        turn::frame(self.store.as_ref(), lease)
    }

    async fn control_state(&self, lease: &Lease) -> Result<TurnControl> {
        turn::control_state(self.store.as_ref(), lease)
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

    async fn model_calls(
        &self,
        thread: &ThreadKey,
        turn_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ModelCallPage> {
        tools::model_calls(self.store.as_ref(), thread, turn_id, cursor, limit)
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

    async fn prune_events(&self, session: &SessionKey, keep_newest: u64) -> Result<u64> {
        query::prune_events(self.store.as_ref(), session, keep_newest)
    }

    async fn resources(
        &self,
        lease: &Lease,
        request: ResourceRequest,
    ) -> Result<Vec<ResourceValue>> {
        resource::apply(self.store.as_ref(), lease, request)
    }

    async fn delegate(
        &self,
        lease: &Lease,
        execution_id: &str,
        request_id: &str,
        target: DelegationTarget,
    ) -> Result<DelegationView> {
        branch::delegate(self.store.as_ref(), lease, execution_id, request_id, target)
    }

    async fn restore(&self, restoration: Restoration) -> Result<ThreadView> {
        branch::restore(self.store.as_ref(), restoration)
    }
}

#[cfg(test)]
mod queue_tests {
    use super::*;

    /// The queue is walked in pages, so its length is not the schedule's length.
    ///
    /// It used to be read with one fixed window of ten thousand ids: the ten-thousand-and-first
    /// turn was invisible to `current_turn`, `ready_at` answered `-1` and the thread was never
    /// scheduled again — while `submit` kept succeeding, so nothing reported the stall.
    #[test]
    fn a_queue_longer_than_one_page_still_finds_the_active_turn() -> Result<()> {
        let store = InMemoryStore::new();
        let thread = "thread-with-a-long-queue";
        let total = QUEUE_PAGE + 3;
        crate::store::txn(&store, "session", |txn: &mut dyn Txn| -> Result<()> {
            for index in 0..total {
                let turn_id = format!("turn-{index:05}");
                txn.put(
                    &queue_part(thread, index as u64),
                    Some(serde_json::json!(turn_id)),
                )?;
                let mut turn = TurnData::new(turn_id, thread.to_string(), Value::Null, 0);
                // Everything but the newest turn is finished, so only that one is work.
                if index + 1 < total {
                    turn.status = TurnStatus::Succeeded;
                }
                save_turn(txn, &turn)?;
            }
            Ok(())
        })?;
        let ids = crate::store::txn(&store, "session", |txn: &mut dyn Txn| {
            queue_ids(txn, thread)
        })?;
        assert_eq!(ids.len(), total, "every admitted turn is listed");
        let current = crate::store::txn(&store, "session", |txn: &mut dyn Txn| {
            current_turn_after(txn, thread, 0)
        })?;
        assert_eq!(
            current.map(|turn| turn.turn_id),
            Some(format!("turn-{:05}", total - 1)),
            "the turn past the first page is the one to run"
        );
        Ok(())
    }

    /// A settled turn is not work, and saying so is what removes it from the index.
    ///
    /// `ready_at` used to answer with the time the turn settled, so every settle left the thread
    /// scheduled: the next scan claimed it (a whole-document write) only to find that
    /// `current_turn` skips terminal turns and unindex it again.
    #[test]
    fn a_settled_turn_asks_for_no_writer() {
        let definition = ThreadSpec {
            key: ThreadKey::new(SessionKey::new("ns", "s").expect("session"), "main")
                .expect("thread"),
            binding: ThreadBinding::new("harness-a", "def-1", "state-1").expect("binding"),
        };
        let thread = ThreadData::new(definition);
        let mut turn = TurnData::new("turn-1".to_string(), "main".to_string(), Value::Null, 0);
        turn.next_attempt_at = 0;
        for status in [TurnStatus::Queued, TurnStatus::Running, TurnStatus::Waiting] {
            turn.status = status;
            assert!(
                ready_at(&thread, Some(&turn), 10) >= 0,
                "{status:?} still wants a writer"
            );
        }
        for status in [
            TurnStatus::Succeeded,
            TurnStatus::Failed,
            TurnStatus::Cancelled,
        ] {
            turn.status = status;
            assert_eq!(
                ready_at(&thread, Some(&turn), 10),
                -1,
                "{status:?} is not work"
            );
        }
    }

    fn thread_record(id: &str) -> ThreadData {
        let definition = ThreadSpec {
            key: ThreadKey::new(SessionKey::new("ns", "s").expect("session"), id).expect("thread"),
            binding: ThreadBinding::new("harness-a", "def-1", "state-1").expect("binding"),
        };
        ThreadData::new(definition)
    }

    /// Writes a thread record plus one turn per status, in admission order, the way `submit` does.
    fn seed(store: &InMemoryStore, id: &str, statuses: &[TurnStatus]) -> Result<ThreadData> {
        let mut record = thread_record(id);
        record.admitted = statuses.len() as u64;
        crate::store::txn(store, "session", |txn: &mut dyn Txn| -> Result<()> {
            save_thread(txn, &record)?;
            for (index, status) in statuses.iter().enumerate() {
                let turn_id = format!("turn-{index:05}");
                store_json(txn, &queue_part(id, index as u64 + 1), &turn_id)?;
                let mut turn = TurnData::new(turn_id, id.to_string(), Value::Null, 0);
                turn.status = *status;
                save_turn(txn, &turn)?;
            }
            Ok(())
        })?;
        Ok(record)
    }

    fn advance(store: &InMemoryStore, record: &mut ThreadData) -> Result<u64> {
        crate::store::txn(store, "session", |txn: &mut dyn Txn| {
            advance_settled(txn, record)
        })
    }

    /// Every field a reader can compare, so "the same turn" means the same turn.
    fn assert_same_turn(left: &TurnData, right: &TurnData) {
        assert_eq!(left.turn_id, right.turn_id);
        assert_eq!(left.status, right.status);
        assert_eq!(left.revision, right.revision);
        assert_eq!(left.invocation_count, right.invocation_count);
        assert_eq!(left.tool_order, right.tool_order);
        assert_eq!(left.run_order, right.run_order);
        assert_eq!(left.output, right.output);
        assert_eq!(left.error, right.error);
    }

    /// The prefix is materialized where the turn ends: three finished turns and a live one leave
    /// `settled` at three, ending the fourth moves it to four, and an empty queue materializes
    /// nothing.
    #[test]
    fn the_settled_prefix_advances_as_the_front_of_the_queue_ends() -> Result<()> {
        let store = InMemoryStore::new();
        let mut record = seed(
            &store,
            "thread-settling",
            &[
                TurnStatus::Succeeded,
                TurnStatus::Failed,
                TurnStatus::Cancelled,
                TurnStatus::Queued,
            ],
        )?;
        assert_eq!(advance(&store, &mut record)?, 3, "three finished turns");
        assert_eq!(record.settled, 3);
        // The live turn ends: the prefix takes it and stops at the end of the queue.
        crate::store::txn(&store, "session", |txn: &mut dyn Txn| -> Result<()> {
            let mut turn = require_turn(txn, "turn-00003")?;
            turn.status = TurnStatus::Succeeded;
            save_turn(txn, &turn)
        })?;
        assert_eq!(advance(&store, &mut record)?, 1);
        assert_eq!(record.settled, 4);
        assert_eq!(
            advance(&store, &mut record)?,
            0,
            "nothing left to materialize"
        );
        assert_eq!(record.settled, 4);
        Ok(())
    }

    /// A turn that ends out of order (a queued turn cancelled behind a running one) must not push
    /// the prefix past work that is still pending: the walk only counts a *prefix*.
    #[test]
    fn a_turn_that_ends_out_of_order_does_not_move_the_prefix() -> Result<()> {
        let store = InMemoryStore::new();
        let mut record = seed(
            &store,
            "thread-out-of-order",
            &[
                TurnStatus::Running,
                TurnStatus::Cancelled,
                TurnStatus::Queued,
            ],
        )?;
        assert_eq!(advance(&store, &mut record)?, 0);
        assert_eq!(record.settled, 0, "the head is still work");
        Ok(())
    }

    /// A thread record written before the prefix existed has no field; it must load as "nothing
    /// materialized" and answer exactly what it answered before: a scan from the start.
    #[test]
    fn a_thread_without_the_materialized_prefix_answers_as_before() -> Result<()> {
        let store = InMemoryStore::new();
        let record = seed(
            &store,
            "thread-legacy",
            &[
                TurnStatus::Succeeded,
                TurnStatus::Succeeded,
                TurnStatus::Queued,
            ],
        )?;
        // Rewrite the thread record the way an older build stored it: no `settled` at all.
        let mut legacy = serde_json::to_value(&record)?;
        legacy
            .as_object_mut()
            .expect("a thread record is an object")
            .remove("settled");
        crate::store::txn(&store, "session", |txn: &mut dyn Txn| -> Result<()> {
            txn.put(&thread_part("thread-legacy", "definition"), Some(legacy))?;
            Ok(())
        })?;
        let key = ThreadKey::new(SessionKey::new("ns", "s")?, "thread-legacy")?;
        let loaded = crate::store::txn(&store, "session", |txn: &mut dyn Txn| {
            require_thread(txn, &key)
        })?;
        assert_eq!(loaded.settled, 0, "an old record materializes nothing");
        let current = crate::store::txn(&store, "session", |txn: &mut dyn Txn| {
            current_turn(txn, &loaded)
        })?
        .expect("the queued turn is work");
        assert_eq!(current.turn_id, "turn-00002");
        assert_eq!(current.status, TurnStatus::Queued);
        Ok(())
    }

    /// The prefix is a starting cursor, not a different answer: the turn the prefix finds is the
    /// turn a full scan of the same queue finds, field by field.
    ///
    /// The scan also becomes O(1) in the length of the finished front, which is the point of
    /// materializing it. A page holds [`QUEUE_PAGE`] ids, so a full walk of a front longer than
    /// that costs one `keys()` per page *plus* one turn read per finished turn, while the
    /// prefixed walk asks for one page starting at the live turn.
    #[test]
    fn a_settled_prefix_finds_the_same_turn_as_a_full_scan() -> Result<()> {
        let store = InMemoryStore::new();
        let finished = 3 * QUEUE_PAGE;
        let mut statuses = vec![TurnStatus::Succeeded; finished];
        statuses.push(TurnStatus::Queued);
        let mut record = seed(&store, "thread-with-a-front", &statuses)?;
        // One write materializes at most `SETTLED_WALK` slots, so a long history catches up over a
        // few writes instead of walking itself inside one transaction.
        while advance(&store, &mut record)? > 0 {}
        assert_eq!(record.settled, finished as u64, "the whole front");

        crate::traffic::reset_read_stats();
        let prefixed = crate::store::txn(&store, "session", |txn: &mut dyn Txn| {
            current_turn(txn, &record)
        })?
        .expect("a live turn");
        let after = crate::traffic::read_stats();

        crate::traffic::reset_read_stats();
        let full = crate::store::txn(&store, "session", |txn: &mut dyn Txn| {
            current_turn_after(txn, "thread-with-a-front", 0)
        })?
        .expect("a live turn");
        let before = crate::traffic::read_stats();

        assert_same_turn(&prefixed, &full);
        // One page is asked for as `QUEUE_PAGE + 1` keys (the extra one says whether a next page
        // exists), so a full walk of the front costs a key per finished turn plus one per page.
        assert_eq!(
            after.keys_returned, 1,
            "the prefix starts the page at the live turn"
        );
        assert_eq!(
            before.keys_returned,
            finished as u64 + 1 + (finished / QUEUE_PAGE) as u64,
            "a full scan walks the finished front"
        );
        Ok(())
    }

    /// A compaction rewrites the same document the prefix lives in, and must neither move it nor
    /// change the answer derived from it.
    #[test]
    fn a_compaction_keeps_the_prefix_and_the_answer_it_produces() -> Result<()> {
        let store = InMemoryStore::new();
        let session = SessionKey::new("ns", "compact-settled")?;
        identity::create(
            &store,
            SessionSpec {
                key: session.clone(),
                primary_binding: ThreadBinding::new("harness-a", "def-1", "state-1")?,
                metadata: Value::Null,
            },
        )?;
        let key = ThreadKey::new(session.clone(), "main")?;
        let scope = session.storage_key();
        let record = crate::store::txn(&store, &scope, |txn: &mut dyn Txn| {
            let mut record = require_thread(txn, &key)?;
            for (index, status) in [
                TurnStatus::Succeeded,
                TurnStatus::Succeeded,
                TurnStatus::Queued,
            ]
            .iter()
            .enumerate()
            {
                let turn_id = format!("turn-{index:05}");
                store_json(txn, &queue_part("main", index as u64 + 1), &turn_id)?;
                let mut turn = TurnData::new(turn_id, "main".to_string(), Value::Null, 0);
                turn.status = *status;
                save_turn(txn, &turn)?;
            }
            record.admitted = 3;
            advance_settled(txn, &mut record)?;
            save_thread(txn, &record)?;
            Ok(record)
        })?;
        assert_eq!(record.settled, 2);
        let before = crate::store::txn(&store, &scope, |txn: &mut dyn Txn| {
            current_turn(txn, &record)
        })?
        .expect("the queued turn is work");

        let retained = crate::store::txn(&store, &scope, |txn: &mut dyn Txn| {
            let mut state = require_state(txn)?;
            let thread = require_thread(txn, &key)?;
            compact::run(txn, &mut state, &thread, 1)?;
            let retained = state.retained_after;
            save_state(txn, &state)?;
            Ok(retained)
        })?;
        assert!(retained > 0, "the compaction pruned the event log");
        let after = crate::store::txn(&store, &scope, |txn: &mut dyn Txn| {
            let record = require_thread(txn, &key)?;
            assert_eq!(record.settled, 2, "the compaction left the prefix alone");
            current_turn(txn, &record)
        })?
        .expect("the queued turn is still work");
        assert_same_turn(&before, &after);
        Ok(())
    }
}

#[cfg(test)]
mod prune_tests {
    use super::*;

    /// Pruning drops the event but keeps its id marker: a replay of the same commit must stay a
    /// no-op instead of appending the pruned event again under a new sequence.
    #[test]
    fn a_pruned_event_keeps_its_id_marker_so_a_replay_cannot_resurrect_it() -> Result<()> {
        let store = InMemoryStore::new();
        let session = SessionKey::new("ns", "prune-event")?;
        identity::create(
            &store,
            SessionSpec {
                key: session.clone(),
                primary_binding: ThreadBinding::new("harness-a", "def-1", "state-1")?,
                metadata: Value::Null,
            },
        )?;
        let key = ThreadKey::new(session.clone(), "main")?;
        let scope = session.storage_key();
        let sequence = crate::store::txn(&store, &scope, |txn: &mut dyn Txn| -> Result<u64> {
            let mut state = require_state(txn)?;
            let base = state.sequence;
            append_event(
                txn,
                &mut state,
                &key,
                "turn-1",
                None,
                None,
                "event-1",
                "test",
                Value::Null,
            )?;
            let sequence = state.sequence;
            assert_eq!(sequence, base + 1);
            let dropped = compact::prune(txn, &mut state, 0)?;
            assert!(dropped > 0, "the prune dropped the event");
            assert!(load_json::<PublicEvent>(txn, &event_part(sequence))?.is_none());
            assert!(
                load_json::<String>(txn, &event_id_part("event-1"))?.is_some(),
                "the id marker outlives the pruned event"
            );
            // Replaying the same commit is a no-op: the marker still answers for the event.
            append_event(
                txn,
                &mut state,
                &key,
                "turn-1",
                None,
                None,
                "event-1",
                "test",
                Value::Null,
            )?;
            assert_eq!(
                state.sequence, sequence,
                "the pruned event is not appended again"
            );
            assert!(load_json::<PublicEvent>(txn, &event_part(sequence))?.is_none());
            // A different identity under the same id is still rejected.
            let conflict = append_event(
                txn,
                &mut state,
                &key,
                "turn-1",
                None,
                None,
                "event-1",
                "test",
                Value::String("changed".to_string()),
            );
            assert!(conflict.is_err(), "the marker still guards the identity");
            save_state(txn, &state)?;
            Ok(state.sequence)
        })?;
        assert!(sequence > 0);
        Ok(())
    }
}
