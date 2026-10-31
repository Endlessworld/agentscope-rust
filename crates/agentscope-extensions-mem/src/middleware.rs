//! Framework assembly seam for the vendor long-term-memory adapters.
//!
//! Java drove long-term memory from the agent loop through
//! `StaticLongTermMemoryHook` (a `Hook`) plus the deprecated
//! `ReActAgent.Builder.longTermMemory*` setters. Both the hook system and the
//! `LongTermMemory` interface were removed in 2.0.0, and the hook's own javadoc says to
//! "reimplement as a `MiddlewareBase` if required". This module is that reimplementation,
//! so a host no longer has to hand-write the call sites:
//!
//! ```text
//! let memory = Arc::new(Mem0LongTermMemory::new(client, config)?);
//! let mut chain = MiddlewareChain::new();
//! chain.add(Box::new(MemLongTermMemoryMiddleware::new(memory)));
//! let agent = HarnessAgent::builder().model(model).middleware(chain).build();
//! ```
//!
//! It mirrors the two things `StaticLongTermMemoryHook` did:
//!
//! * [`Middleware::on_reasoning`] — retrieve the memories relevant to the latest USER
//!   message and append them as a `long_term_memory` USER message wrapped in
//!   `<long_term_memory>` tags (`handlePreCall` + `LongTermMemoryTools.wrap`). The
//!   injected message is request-scoped: it is never persisted to `AgentState.context`,
//!   exactly like the Java hook, which only rewrote `event.getInputMessages()`.
//! * [`Middleware::on_agent`] — once the turn has finished, record the conversation
//!   history (`AgentState.context`, Java `Memory#getMessages()` in `handlePostCall`)
//!   through [`LongTermMemoryBackend::record_lossy`].
//!
//! Both directions swallow failures (warn + continue). That is the hook's contract
//! (`onErrorResume` on both branches): a memory outage must never abort an agent turn or
//! surface as a failed reply.
//!
//! Differences from the Java hook, all documented rather than accidental:
//!
//! * Java registered the hook on `PreCallEvent`/`PostCallEvent`. The Rust equivalent of
//!   `PreCallEvent` (`on_pre_call`) cannot rewrite the model request, so the retrieval runs
//!   at `on_reasoning` — the first point that can (Java's `PreReasoningEvent` equivalent).
//!   Java injected once per call, so only the first iteration of a turn injects; the
//!   retrieval is cached per `RuntimeContext::run_id` so a later iteration of the same
//!   turn neither searches again nor appends a second USER message after a tool result
//!   (which strict providers reject).
//! * Java's optional `asyncRecord` mode (fire-and-forget on a bounded scheduler) is not
//!   ported: the record runs after the turn has produced its reply, so the caller already
//!   has the answer; blocking the (already finished) call for one more request is simpler
//!   than owning a background scheduler, and it keeps the failure visible in the log.
//! * Log lines go through `tracing::warn!` (like the crate's other `*_lossy` paths)
//!   instead of SLF4J.

use std::sync::{Arc, Mutex};

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::{Msg, MsgRole};
use agentscope_core::middleware::{
    AgentInput, EventStream, ExtensionPoint, Middleware, Next, ReasoningInput,
};
use async_trait::async_trait;
use futures::StreamExt;

use crate::bailian::memory::BailianLongTermMemory;
use crate::mem0::memory::Mem0LongTermMemory;
use crate::reme::memory::ReMeLongTermMemory;

/// Name given to the injected memory message
/// (Java `Msg.builder().name("long_term_memory")`).
pub const LONG_TERM_MEMORY_MESSAGE_NAME: &str = "long_term_memory";

/// Prefix of the wrapped memory text (Java `LongTermMemoryTools.wrap`, verbatim).
pub const LONG_TERM_MEMORY_PREFIX: &str = "Below is content retrieved from the long-term memory associated with the current user, Please extract useful information from it in the context of the current conversation.\n<long_term_memory>\n";

/// Suffix of the wrapped memory text (Java `LongTermMemoryTools.wrap`).
pub const LONG_TERM_MEMORY_SUFFIX: &str = "\n</long_term_memory>";

/// Wraps retrieved memory text exactly like `LongTermMemoryTools.wrap`.
pub fn wrap_long_term_memory(text: &str) -> String {
    format!("{LONG_TERM_MEMORY_PREFIX}{text}{LONG_TERM_MEMORY_SUFFIX}")
}

/// Builds the request-scoped message that carries retrieved memories.
///
/// Java appends a `MsgRole.USER` message named `long_term_memory` holding one text
/// block; it is appended after the caller's input rather than replacing anything.
pub fn long_term_memory_message(text: &str) -> Msg {
    Msg::builder()
        .role(MsgRole::User)
        .name(LONG_TERM_MEMORY_MESSAGE_NAME)
        .text_content(wrap_long_term_memory(text))
        .build()
}

/// The never-fail operations [`MemLongTermMemoryMiddleware`] needs from a backend.
///
/// This is deliberately narrower than the removed Java `LongTermMemory` interface
/// (tracker §4.3.1 `MEM-D01`: the crate does **not** resurrect it): it exposes only the
/// `*_lossy` variants the Java hook observes through `onErrorResume`, and every vendor
/// adapter in this crate implements it.
///
/// `Mem0` and `ReMe` have no inherent `record_lossy` (their Java `record` propagates
/// failures), yet the *hook* swallowed record errors on all three backends — so their
/// [`LongTermMemoryBackend::record_lossy`] swallow here, at the assembly seam, which is
/// exactly where Java swallowed them too.
#[async_trait]
pub trait LongTermMemoryBackend: Send + Sync {
    /// Retrieves the memories relevant to `msg`; returns `""` (never fails) on error.
    async fn retrieve_lossy(&self, msg: &Msg) -> String;

    /// Records `msgs`; never fails.
    async fn record_lossy(&self, msgs: &[Msg]);
}

#[async_trait]
impl LongTermMemoryBackend for Mem0LongTermMemory {
    async fn retrieve_lossy(&self, msg: &Msg) -> String {
        Mem0LongTermMemory::retrieve_lossy(self, msg).await
    }

    async fn record_lossy(&self, msgs: &[Msg]) {
        if let Err(err) = Mem0LongTermMemory::record(self, msgs).await {
            tracing::warn!(error = %err, "Failed to record Mem0 memory");
        }
    }
}

#[async_trait]
impl LongTermMemoryBackend for BailianLongTermMemory {
    async fn retrieve_lossy(&self, msg: &Msg) -> String {
        BailianLongTermMemory::retrieve_lossy(self, msg).await
    }

    async fn record_lossy(&self, msgs: &[Msg]) {
        BailianLongTermMemory::record_lossy(self, msgs).await
    }
}

#[async_trait]
impl LongTermMemoryBackend for ReMeLongTermMemory {
    async fn retrieve_lossy(&self, msg: &Msg) -> String {
        ReMeLongTermMemory::retrieve_lossy(self, msg).await
    }

    async fn record_lossy(&self, msgs: &[Msg]) {
        if let Err(err) = ReMeLongTermMemory::record(self, msgs).await {
            tracing::warn!(error = %err, "Failed to record ReMe memory");
        }
    }
}

/// Mounts a [`LongTermMemoryBackend`] into the agent loop — the `MiddlewareBase`
/// replacement `StaticLongTermMemoryHook`'s deprecation note asks for.
///
/// Register it through `HarnessAgentBuilder::middleware`; see the module docs for the
/// exact mapping onto the Java hook.
#[derive(Clone)]
pub struct MemLongTermMemoryMiddleware {
    backend: Arc<dyn LongTermMemoryBackend>,
    /// One-entry cache of the last retrieval, keyed by `RuntimeContext::run_id`.
    ///
    /// `on_reasoning` runs once per iteration, but Java retrieved once per *call*. The
    /// cache keeps that contract without an unbounded map: a different run id simply
    /// replaces the entry.
    cache: Arc<Mutex<Option<(String, String)>>>,
}

impl MemLongTermMemoryMiddleware {
    /// Creates the middleware for `backend`.
    pub fn new(backend: Arc<dyn LongTermMemoryBackend>) -> Self {
        Self {
            backend,
            cache: Arc::new(Mutex::new(None)),
        }
    }

    /// The wrapped backend.
    pub fn backend(&self) -> &Arc<dyn LongTermMemoryBackend> {
        &self.backend
    }

    /// The memories cached for `key`, when the current call already retrieved them.
    pub fn cached(&self, key: &str) -> Option<String> {
        let cache = self.cache.lock().unwrap();
        cache
            .as_ref()
            .filter(|(cached_key, _)| cached_key == key)
            .map(|(_, text)| text.clone())
    }

    fn store(&self, key: &str, text: &str) {
        *self.cache.lock().unwrap() = Some((key.to_string(), text.to_string()));
    }

    /// Forgets the cached retrieval so the next reasoning step retrieves again.
    pub fn invalidate(&self) {
        *self.cache.lock().unwrap() = None;
    }

    /// The conversation to record (`AgentState.context`, Java `Memory#getMessages()`).
    ///
    /// `RuntimeContext::state` is the live per-agent handle, so once the agent stream has
    /// ended it holds the whole turn (input, assistant replies, tool results). Returns
    /// `None` for an ad-hoc context without state (tool calls, some tests).
    fn conversation(&self, ctx: &RuntimeContext) -> Option<Vec<Msg>> {
        let state = ctx.state.as_ref()?;
        let context = state.lock().unwrap().context().to_vec();
        (!context.is_empty()).then_some(context)
    }
}

impl Middleware for MemLongTermMemoryMiddleware {
    /// Java `StaticLongTermMemoryHook.priority()`; higher runs earlier.
    fn order(&self) -> i32 {
        50
    }

    fn active_points(&self) -> Vec<ExtensionPoint> {
        vec![ExtensionPoint::OnAgent, ExtensionPoint::OnReasoning]
    }

    fn on_agent(
        &self,
        ctx: &RuntimeContext,
        input: AgentInput,
        next: Next<AgentInput>,
    ) -> EventStream {
        let this = self.clone();
        let ctx = ctx.clone();
        // Fallback when no state handle is attached: recording this turn's input is
        // better than recording nothing.
        let input_messages = input.messages.clone();
        // A new call owns the cache slot.
        this.invalidate();
        let downstream = next(input);
        let tail = async move {
            let conversation = this.conversation(&ctx).unwrap_or(input_messages);
            if !conversation.is_empty() {
                this.backend.record_lossy(&conversation).await;
            }
            Vec::new()
        };
        Box::pin(
            downstream.chain(
                futures::stream::once(tail)
                    .map(futures::stream::iter)
                    .flatten(),
            ),
        )
    }

    fn on_reasoning(
        &self,
        ctx: &RuntimeContext,
        input: ReasoningInput,
        next: Next<ReasoningInput>,
    ) -> EventStream {
        // Java `extractLastUserMessageIndex`: the most recent USER message is the query.
        let Some(query) = last_user_message(&input.messages) else {
            return next(input);
        };
        let key = ctx.run_id().unwrap_or_default().to_string();
        // Java injected the memory once per call (`PreCallEvent`). Injecting again on a
        // later iteration would append a second USER message right after a tool result,
        // which strict providers (Anthropic) reject; the cache keeps the search cost at
        // one request per turn while leaving later iterations untouched.
        if self.cached(&key).is_some() {
            return next(input);
        }
        let this = self.clone();
        Box::pin(
            futures::stream::once(async move {
                let text = this.backend.retrieve_lossy(&query).await;
                // Cache even an empty result: a failed retrieval must not be retried on
                // every iteration of the same turn (Java retrieved once and moved on).
                this.store(&key, &text);
                next(inject(input, &text))
            })
            .flatten(),
        )
    }
}

/// The most recent USER message, mirroring `extractLastUserMessageIndex`.
fn last_user_message(messages: &[Msg]) -> Option<Msg> {
    messages
        .iter()
        .rev()
        .find(|message| message.role() == MsgRole::User)
        .cloned()
}

/// Appends the memory message to the reasoning request; an empty retrieval is a no-op.
fn inject(input: ReasoningInput, memory_text: &str) -> ReasoningInput {
    if memory_text.trim().is_empty() {
        return input;
    }
    let ReasoningInput {
        mut messages,
        tools,
        options,
    } = input;
    messages.push(long_term_memory_message(memory_text));
    ReasoningInput {
        messages,
        tools,
        options,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_matches_java_long_term_memory_tools() {
        assert_eq!(
            wrap_long_term_memory("likes oolong tea"),
            concat!(
                "Below is content retrieved from the long-term memory associated with the ",
                "current user, Please extract useful information from it in the context of ",
                "the current conversation.\n<long_term_memory>\nlikes oolong tea\n",
                "</long_term_memory>"
            )
        );
    }

    #[test]
    fn memory_message_is_a_named_user_text_message() {
        let msg = long_term_memory_message("m1");
        assert_eq!(msg.role(), MsgRole::User);
        assert_eq!(msg.name(), Some(LONG_TERM_MEMORY_MESSAGE_NAME));
        assert_eq!(msg.text_content(), wrap_long_term_memory("m1"));
    }

    #[test]
    fn last_user_message_skips_trailing_assistant_messages() {
        let messages = vec![
            Msg::user("first"),
            Msg::assistant("answer"),
            Msg::user("second"),
            Msg::assistant("answer again"),
        ];
        assert_eq!(
            last_user_message(&messages).unwrap().text_content(),
            "second"
        );
        assert!(last_user_message(&[Msg::assistant("only")]).is_none());
    }

    #[test]
    fn inject_appends_and_ignores_an_empty_retrieval() {
        let input = ReasoningInput::new(vec![Msg::user("q")], Vec::new(), Default::default());
        let untouched = inject(input.clone(), "   ");
        assert_eq!(untouched.messages.len(), 1);

        let injected = inject(input, "m1");
        assert_eq!(injected.messages.len(), 2);
        assert_eq!(
            injected.messages[1].name(),
            Some(LONG_TERM_MEMORY_MESSAGE_NAME)
        );
    }

    #[test]
    fn the_cache_is_keyed_by_run_id() {
        let middleware = MemLongTermMemoryMiddleware::new(Arc::new(NoopBackend));
        assert!(middleware.cached("run-1").is_none());
        middleware.store("run-1", "m1");
        assert_eq!(middleware.cached("run-1").as_deref(), Some("m1"));
        assert!(middleware.cached("run-2").is_none());
        middleware.invalidate();
        assert!(middleware.cached("run-1").is_none());
    }

    struct NoopBackend;

    #[async_trait]
    impl LongTermMemoryBackend for NoopBackend {
        async fn retrieve_lossy(&self, _msg: &Msg) -> String {
            String::new()
        }

        async fn record_lossy(&self, _msgs: &[Msg]) {}
    }
}
