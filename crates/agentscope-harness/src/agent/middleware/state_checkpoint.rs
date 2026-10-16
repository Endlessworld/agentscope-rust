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
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use agentscope_core::agent::RuntimeContext;
use agentscope_core::event::AgentEvent;
use agentscope_core::middleware::{AgentInput, EventStream, Middleware, Next, ReasoningInput};
use agentscope_core::state::{AgentState, AgentStateStore};
use futures::Stream;

/// Key under which the full `AgentState` is persisted in the store.
pub const AGENT_STATE_KEY: &str = "agent_state";

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
}

impl StateCheckpointMiddleware {
    /// Creates a checkpoint middleware backed by the given state store.
    pub fn new(store: Arc<dyn AgentStateStore>) -> Self {
        Self {
            store,
            restored: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Serializes the shared agent state and writes it under the checkpoint key.
async fn save_state_snapshot(
    store: Arc<dyn AgentStateStore>,
    state: Arc<std::sync::Mutex<AgentState>>,
) {
    let (uid, sid, value) = {
        let g = state.lock().unwrap();
        (
            g.user_id().map(str::to_string),
            g.session_id().to_string(),
            serde_json::to_value(&*g).unwrap_or_default(),
        )
    };
    store.save(uid.as_deref(), &sid, AGENT_STATE_KEY, value).await;
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

        // Pre: restore the checkpoint once, before the inner run starts.
        let pre = {
            let store = Arc::clone(&store);
            let state = Arc::clone(&state);
            Box::pin(async move {
                if restored.swap(true, Ordering::SeqCst) {
                    return;
                }
                let (uid, sid) = {
                    let g = state.lock().unwrap();
                    (g.user_id().map(str::to_string), g.session_id().to_string())
                };
                if let Some(value) = store.get(uid.as_deref(), &sid, AGENT_STATE_KEY).await {
                    if let Ok(saved) = serde_json::from_value::<AgentState>(value) {
                        let mut g = state.lock().unwrap();
                        *g = saved;
                    }
                }
            }) as Pin<Box<dyn Future<Output = ()> + Send>>
        };

        let inner = next(input);

        // Post: persist the final state after the run drains.
        let post = {
            let store = Arc::clone(&store);
            let state = Arc::clone(&state);
            Box::pin(async move {
                let (uid, sid, value) = {
                    let g = state.lock().unwrap();
                    let uid = g.user_id().map(str::to_string);
                    let sid = g.session_id().to_string();
                    let value = serde_json::to_value(&*g).unwrap_or_default();
                    (uid, sid, value)
                };
                store
                    .save(uid.as_deref(), &sid, AGENT_STATE_KEY, value)
                    .await;
            }) as Pin<Box<dyn Future<Output = ()> + Send>>
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
        let post = Box::pin(save_state_snapshot(Arc::clone(&self.store), Arc::clone(&state)))
            as Pin<Box<dyn Future<Output = ()> + Send>>;
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
    post: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    phase: Phase,
}

enum Phase {
    Pre(Pin<Box<dyn Future<Output = ()> + Send>>),
    Mid,
    Post(Pin<Box<dyn Future<Output = ()> + Send>>),
    Done,
}

impl<S> AsyncGate<S> {
    fn new(
        inner: S,
        pre: Pin<Box<dyn Future<Output = ()> + Send>>,
        post: Pin<Box<dyn Future<Output = ()> + Send>>,
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
                    Poll::Ready(()) => return Poll::Ready(None),
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
