//! State-checkpoint middleware.
//!
//! Persists the agent's full [`AgentState`] (conversation history, task bag,
//! plan-mode flag, …) to an [`AgentStateStore`] after every run, and restores
//! it once on the first run of the process. This turns the state store into a
//! true checkpoint: a restarted process that rebuilds the harness agent with the
//! same `(user_id, session_id)` resumes the conversation from where it left off.
//!
//! Mirrors the checkpoint/resume idea conceptually; the storage layer is the
//! core [`AgentStateStore`] (e.g. [`JsonFileAgentStateStore`]).
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Error;
use agentscope_core::event::AgentEvent;
use agentscope_core::middleware::{AgentInput, EventStream, Middleware, Next, ReasoningInput};
use agentscope_core::state::{AgentState, AgentStateStore, ConflictPolicy, UNVERSIONED};
use futures::Stream;

/// Key under which the full `AgentState` is persisted in the store.
pub const AGENT_STATE_KEY: &str = "agent_state";

/// Per-slot bookkeeping: the last store version this process observed, plus the
/// context length at load time (`ConflictPolicy::AppendMerge` needs it to know
/// which messages this turn added).
#[derive(Default)]
struct SlotState {
    version: i64,
    loaded_context: usize,
}

/// Middleware that snapshots `AgentState` to a persistent store.
///
/// The snapshot is taken after the inner agent run drains (so the final
/// state — including the latest assistant reply — is persisted), and restored
/// only on the first run so a fresh process can resume prior conversation
/// history. Restore is keyed by the live state's own `(user_id, session_id)`,
/// so callers must configure a stable session id for restart-resume to work.
pub struct StateCheckpointMiddleware {
    store: Arc<dyn AgentStateStore>,
    /// Guards the one-time restore from storage on the first run of the process.
    restored: Arc<AtomicBool>,
    /// Last store version observed per `(user, session)` slot on versioning backends,
    /// together with the context length at load time.
    slots: Arc<Mutex<HashMap<String, SlotState>>>,
    /// How a write that loses the CAS race is resolved. Mirrors Java
    /// `ReActAgent.Builder.conflictPolicy` (default `OVERWRITE`).
    policy: ConflictPolicy,
}

impl StateCheckpointMiddleware {
    /// Creates a checkpoint middleware backed by the given state store.
    pub fn new(store: Arc<dyn AgentStateStore>) -> Self {
        Self::with_conflict_policy(store, ConflictPolicy::default())
    }

    /// Creates a checkpoint middleware with an explicit conflict policy.
    pub fn with_conflict_policy(
        store: Arc<dyn AgentStateStore>,
        policy: ConflictPolicy,
    ) -> Self {
        Self {
            store,
            restored: Arc::new(AtomicBool::new(false)),
            slots: Arc::new(Mutex::new(HashMap::new())),
            policy,
        }
    }

    /// The configured conflict policy.
    pub fn conflict_policy(&self) -> ConflictPolicy {
        self.policy
    }
}

/// Slot key mirroring the Java `slotKey`: `(userId or anon) + "/" + sessionId`.
fn slot_key(user_id: Option<&str>, session_id: &str) -> String {
    let user = match user_id {
        Some(u) if !u.trim().is_empty() => u,
        _ => "__anon__",
    };
    format!("{user}/{session_id}")
}

/// How a save resolves a conflict.
#[derive(Clone, Copy)]
enum SaveMode {
    /// Apply the configured [`ConflictPolicy`]. Only the turn-end save may do this: it is
    /// the authoritative write.
    Resolve(ConflictPolicy),
    /// Report the conflict and leave the durable state alone.
    ///
    /// In-flight (per-iteration) checkpoints use this. They exist to cap data loss on an
    /// abrupt exit, not to decide ownership of the slot — and the agent loop keeps writing
    /// back its own state copy, so a merge adopted mid-iteration would be clobbered anyway.
    BestEffort,
}

/// Compare-and-swap write of the current state, resolving a lost race according to
/// `mode`.
///
/// `AppendMerge` needs to know which messages this process has not yet committed, so
/// `loaded_context` is refreshed after every successful write.
async fn save_state_snapshot(
    store: Arc<dyn AgentStateStore>,
    slots: Arc<Mutex<HashMap<String, SlotState>>>,
    state: Arc<std::sync::Mutex<AgentState>>,
    mode: SaveMode,
) -> Option<Error> {
    let (uid, sid, local) = {
        let g = state.lock().unwrap();
        (g.user_id().map(str::to_string), g.session_id().to_string(), g.clone())
    };
    let slot = slot_key(uid.as_deref(), &sid);
    let (expected, loaded_context) = {
        let guard = slots.lock().unwrap();
        match guard.get(&slot) {
            Some(entry) => (entry.version, entry.loaded_context),
            None => (UNVERSIONED, 0),
        }
    };
    let value = serde_json::to_value(&local).unwrap_or_default();
    let written = store
        .save_if_version(uid.as_deref(), &sid, AGENT_STATE_KEY, value.clone(), expected)
        .await;
    if written != UNVERSIONED {
        record_write(&slots, &slot, written, local.context().len());
        return None;
    }
    // Non-versioning backends report UNVERSIONED by contract and already wrote.
    if !store.supports_versioning() {
        return None;
    }
    let policy = match mode {
        SaveMode::Resolve(policy) => policy,
        SaveMode::BestEffort => {
            tracing::debug!(session_id = %sid, "agent_state CAS conflict — checkpoint skipped");
            return None;
        }
    };
    match policy {
        ConflictPolicy::Overwrite => {
            let overwritten = store
                .save_if_version(uid.as_deref(), &sid, AGENT_STATE_KEY, value.clone(), UNVERSIONED)
                .await;
            if overwritten != UNVERSIONED {
                record_write(&slots, &slot, overwritten, local.context().len());
            } else {
                store.save(uid.as_deref(), &sid, AGENT_STATE_KEY, value).await;
            }
            tracing::warn!(session_id = %sid, "agent_state CAS conflict — OVERWRITE applied");
            None
        }
        ConflictPolicy::Fail => {
            tracing::warn!(session_id = %sid, "agent_state CAS conflict — FAIL");
            Some(concurrent_modification(&uid, &sid, expected))
        }
        ConflictPolicy::AppendMerge => {
            append_merge(store, slots, slot, uid, sid, &state, local, loaded_context).await
        }
    }
}

/// The error a `Fail` policy surfaces when a save loses the race.
fn concurrent_modification(user_id: &Option<String>, session_id: &str, expected: i64) -> Error {
    Error::ConcurrentSessionModification {
        user_id: user_id.clone().unwrap_or_else(|| "__anon__".to_string()),
        session_id: session_id.to_string(),
        key: AGENT_STATE_KEY.to_string(),
        expected_version: expected,
    }
}

/// Reloads the durable baseline, appends this process's uncommitted messages, and
/// retries the CAS once. A second loss surfaces as a conflict rather than looping.
async fn append_merge(
    store: Arc<dyn AgentStateStore>,
    slots: Arc<Mutex<HashMap<String, SlotState>>>,
    slot: String,
    uid: Option<String>,
    sid: String,
    state: &Arc<Mutex<AgentState>>,
    local: AgentState,
    loaded_context: usize,
) -> Option<Error> {
    let latest = store.get_versioned(uid.as_deref(), &sid, AGENT_STATE_KEY).await;
    let mut baseline = latest
        .value
        .as_ref()
        .and_then(|v| serde_json::from_value::<AgentState>(v.clone()).ok())
        .unwrap_or_else(|| baseline_state(&local));
    // Append only what the store has not seen yet; a compaction turn that rewrote
    // history would be corrupted here, which is why this policy is opt-in.
    if loaded_context < local.context().len() {
        baseline
            .context_mut()
            .extend_from_slice(&local.context()[loaded_context..]);
    }
    baseline.set_permission_context(local.permission_context().clone());
    let merged = store
        .save_if_version(
            uid.as_deref(),
            &sid,
            AGENT_STATE_KEY,
            serde_json::to_value(&baseline).unwrap_or_default(),
            latest.version,
        )
        .await;
    if merged == UNVERSIONED {
        tracing::warn!(session_id = %sid, "agent_state CAS conflict — APPEND_MERGE lost again");
        return Some(concurrent_modification(&uid, &sid, latest.version));
    }
    record_write(&slots, &slot, merged, baseline.context().len());
    // Adopt the merged baseline so the next save starts from what is now durable
    // instead of re-deriving it from a stale local copy (Java `stateCache.put`).
    *state.lock().unwrap() = baseline;
    tracing::info!(session_id = %sid, "agent_state CAS conflict — APPEND_MERGE succeeded");
    None
}

/// Records the version a successful write produced, so the next CAS starts from it and
/// `AppendMerge` knows which messages are already durable.
fn record_write(
    slots: &Arc<Mutex<HashMap<String, SlotState>>>,
    slot: &str,
    version: i64,
    context_len: usize,
) {
    slots.lock().unwrap().insert(
        slot.to_string(),
        SlotState {
            version,
            loaded_context: context_len,
        },
    );
}

/// Baseline for `AppendMerge` when the store holds nothing: identity from the local
/// state, empty history (the caller appends the new messages).
fn baseline_state(local: &AgentState) -> AgentState {
    let mut baseline = AgentState::default();
    baseline.session_id = local.session_id.clone();
    baseline.user_id = local.user_id.clone();
    baseline
}

impl Middleware for StateCheckpointMiddleware {
    fn on_agent(
        &self,
        ctx: &RuntimeContext,
        input: AgentInput,
        next: Next<AgentInput>,
    ) -> EventStream {
        // No shared per-agent state handle (ad-hoc context) -> nothing to snapshot.
        let Some(state) = ctx.state.clone() else {
            return next(input);
        };
        let store = Arc::clone(&self.store);
        let restored = Arc::clone(&self.restored);
        let slots = Arc::clone(&self.slots);

        // Pre: restore the checkpoint once, before the inner run starts.
        let pre = {
            let store = Arc::clone(&store);
            let slots = Arc::clone(&slots);
            let state = Arc::clone(&state);
            Box::pin(async move {
                if restored.swap(true, Ordering::SeqCst) {
                    return;
                }
                let (uid, sid) = {
                    let g = state.lock().unwrap();
                    (g.user_id().map(str::to_string), g.session_id().to_string())
                };
                let loaded = store
                    .get_versioned(uid.as_deref(), &sid, AGENT_STATE_KEY)
                    .await;
                if let Some(saved) = loaded.value.clone().and_then(|v| serde_json::from_value::<AgentState>(v).ok()) {
                    let mut g = state.lock().unwrap();
                    *g = saved;
                }
                if store.supports_versioning() {
                    let loaded_context = state.lock().unwrap().context().len();
                    slots.lock().unwrap().insert(
                        slot_key(uid.as_deref(), &sid),
                        SlotState {
                            version: loaded.version,
                            // The restored history is the baseline `AppendMerge` compares
                            // against, so record its length here.
                            loaded_context,
                        },
                    );
                }
            }) as Pin<Box<dyn Future<Output = ()> + Send>>
        };

        let inner = next(input);

        // Post: persist the final state after the run drains. A `Fail` policy resolves to
        // the conflict error, which `AsyncGate` surfaces as a stream error rather than
        // letting the turn end as if the save succeeded.
        let post = {
            let store = Arc::clone(&store);
            let slots = Arc::clone(&slots);
            let state = Arc::clone(&state);
            let policy = self.policy;
            Box::pin(async move {
                save_state_snapshot(store, slots, state, SaveMode::Resolve(policy)).await
            }) as Pin<Box<dyn Future<Output = Option<Error>> + Send>>
        };

        Box::pin(AsyncGate::new(inner, pre, post))
    }

    /// Per-iteration checkpoint.
    ///
    /// Fires once per reasoning iteration; its post phase runs after the previous
    /// iteration's assistant/tool messages have been published to the shared state
    /// (see `reasoning_phase`). Persisting here caps the loss from an abrupt process
    /// death — `kill -9`, OOM kill or power loss, none of which can run shutdown
    /// code — at the in-flight iteration instead of the whole turn.
    fn on_reasoning(
        &self,
        ctx: &RuntimeContext,
        input: ReasoningInput,
        next: Next<ReasoningInput>,
    ) -> EventStream {
        let Some(state) = ctx.state.clone() else {
            return next(input);
        };
        let inner = next(input);
        let pre = Box::pin(async {}) as Pin<Box<dyn Future<Output = ()> + Send>>;
        let store = Arc::clone(&self.store);
        let slots = Arc::clone(&self.slots);
        let post = Box::pin(async move {
            // Best effort: the turn-end save owns conflict resolution.
            save_state_snapshot(store, slots, state, SaveMode::BestEffort).await
        }) as Pin<Box<dyn Future<Output = Option<Error>> + Send>>;
        Box::pin(AsyncGate::new(inner, pre, post))
    }
}

/// Stream adapter that runs an async `pre` action, then forwards the inner
/// stream's items unchanged, then runs an async `post` action once the inner
/// stream drains. The inner stream is only polled after `pre` completes, and
/// items stream through incrementally (no buffering), so token-by-token
/// streaming from the agent is preserved.
struct AsyncGate<S> {
    inner: S,
    post: Option<Pin<Box<dyn Future<Output = Option<Error>> + Send>>>,
    phase: Phase,
}

enum Phase {
    Pre(Pin<Box<dyn Future<Output = ()> + Send>>),
    Mid,
    Post(Pin<Box<dyn Future<Output = Option<Error>> + Send>>),
    Done,
}

impl<S> AsyncGate<S> {
    fn new(
        inner: S,
        pre: Pin<Box<dyn Future<Output = ()> + Send>>,
        post: Pin<Box<dyn Future<Output = Option<Error>> + Send>>,
    ) -> Self {
        Self {
            inner,
            post: Some(post),
            phase: Phase::Pre(pre),
        }
    }
}

impl<S> Stream for AsyncGate<S>
where
    S: Stream<Item = Result<AgentEvent, agentscope_core::error::Error>> + Unpin,
{
    type Item = Result<AgentEvent, agentscope_core::error::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match std::mem::replace(&mut self.phase, Phase::Done) {
                Phase::Pre(mut fut) => match fut.as_mut().poll(cx) {
                    Poll::Ready(()) => self.phase = Phase::Mid,
                    Poll::Pending => {
                        self.phase = Phase::Pre(fut);
                        return Poll::Pending;
                    }
                },
                Phase::Mid => match Pin::new(&mut self.inner).poll_next(cx) {
                    Poll::Ready(Some(item)) => {
                        self.phase = Phase::Mid;
                        return Poll::Ready(Some(item));
                    }
                    Poll::Ready(None) => {
                        if let Some(post) = self.post.take() {
                            self.phase = Phase::Post(post);
                        } else {
                            return Poll::Ready(None);
                        }
                    }
                    Poll::Pending => {
                        self.phase = Phase::Mid;
                        return Poll::Pending;
                    }
                },
                Phase::Post(mut fut) => match fut.as_mut().poll(cx) {
                    Poll::Ready(None) => return Poll::Ready(None),
                    Poll::Ready(Some(error)) => return Poll::Ready(Some(Err(error))),
                    Poll::Pending => {
                        self.phase = Phase::Post(fut);
                        return Poll::Pending;
                    }
                },
                Phase::Done => return Poll::Ready(None),
            }
        }
    }
}
