//! Journaled model adapter boundary, mirroring `SessionModels` / `SessionModelPolicy`.
//!
//! The ReAct loop calls a model through the middleware chain, but auxiliary calls (structured
//! output, summaries, adapters) also reach a provider. Java closes that gap by wrapping every
//! model in a `Recorded` decorator, so the journal sees the **final adapter boundary**
//! (`request/prepared` → `model/dispatch` → `model/chunk`* → `model/end`) regardless of which
//! caller issued the request. This module is that decorator.
//!
//! Not migrated: Java's `modelForCall` fallback switch (`switchOnFirst` picking a fallback
//! model when the primary fails). The Rust core has no fallback-model configuration, so there
//! is nothing to switch to and no `model/retry` fact is produced here.

use std::sync::Arc;

use futures::stream::{BoxStream, StreamExt};

use super::SessionRecorder;
use crate::agent::RuntimeContext;
use crate::message::{
    ChatUsage, ContentBlock, Msg, TextBlock, ThinkingBlock, ToolResultBlock, ToolUseBlock,
};
use crate::middleware::ModelCallInput;
use crate::model::{ChatResponse, GenerateOptions, Model, ToolSchema};

/// Runtime-context key carrying the active [`SessionModelPolicy`].
pub const MODEL_POLICY_CONTEXT_KEY: &str = "agentscope.session.modelPolicy";

/// Optional hosted admission/accounting policy, shared with inherited child contexts.
///
/// Mirrors Java `SessionModelPolicy`: the host can admit or throttle a call before it is
/// dispatched ([`before_call`](Self::before_call)) and account for it afterwards
/// ([`after_call`](Self::after_call)), with the journal's `modelCallId` as the correlation id.
pub trait SessionModelPolicy: Send + Sync {
    /// Called before the call is prepared, i.e. before `request/prepared` is written.
    fn before_call(&self, call_id: &str, model: &str, ctx: &RuntimeContext);

    /// Called once the call reached a terminal status (`completed` / `failed`), after
    /// `model/end` was attempted.
    fn after_call(
        &self,
        call_id: &str,
        model: &str,
        status: &str,
        usage: Option<&ChatUsage>,
        ctx: &RuntimeContext,
    );
}

/// [`RuntimeContext::extension`] carrier for a boxed [`SessionModelPolicy`].
///
/// Extensions store `T: Any` values, and a bare `Arc<dyn SessionModelPolicy>` cannot be
/// downcast (`dyn Trait` is neither `Sized` nor `Any`), so the policy travels inside this
/// concrete handle and is unwrapped by [`policy_from_context`].
#[derive(Clone)]
pub struct SessionModelPolicyHandle(pub Arc<dyn SessionModelPolicy>);

impl std::fmt::Debug for SessionModelPolicyHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionModelPolicyHandle(..)")
    }
}

/// Returns the policy attached to `ctx`, if any.
pub fn policy_from_context(ctx: &RuntimeContext) -> Option<Arc<dyn SessionModelPolicy>> {
    ctx.extension::<SessionModelPolicyHandle>(MODEL_POLICY_CONTEXT_KEY)
        .map(|handle| Arc::clone(&handle.0))
}

/// Namespace for the journaled model boundary (Java `SessionModels`).
pub struct SessionModels;

impl SessionModels {
    /// Wraps `model` so every call is journaled while `ctx` carries a [`SessionRecorder`].
    ///
    /// Returns `model` unchanged when no recorder is attached, so un-journaled executions pay
    /// nothing. Wrap each model **once**: the decorator is indistinguishable from a plain
    /// model (the `Model` trait has no downcast hook), so wrapping twice would record twice.
    pub fn wrap(
        model: Arc<dyn Model>,
        ctx: &RuntimeContext,
        purpose: impl Into<String>,
    ) -> Arc<dyn Model> {
        wrap(model, ctx, purpose)
    }
}

/// Free-function form of [`SessionModels::wrap`].
pub fn wrap(
    model: Arc<dyn Model>,
    ctx: &RuntimeContext,
    purpose: impl Into<String>,
) -> Arc<dyn Model> {
    if SessionRecorder::from_context(ctx).is_none() {
        return model;
    }
    Arc::new(Recorded {
        delegate: model,
        ctx: ctx.clone(),
        purpose: purpose.into(),
    })
}

/// Coalesces streamed deltas before journaling.
///
/// Providers emit text/thinking one token or one SSE event at a time. Recording every
/// delta as its own committed event made the journal orders of magnitude larger than the
/// conversation and turned a long stream into thousands of commits. This buffer emits one
/// merged `model/chunk` per 4 KiB / 64 deltas / 1 second, while the consumer still
/// receives every original delta unchanged.
struct MergedChunk {
    id: String,
    text: String,
    thinking: String,
    tool_calls: Vec<ToolUseBlock>,
    tool_results: Vec<ToolResultBlock>,
    other: Vec<ContentBlock>,
    chunks: usize,
    bytes: usize,
    usage: ChatUsage,
    finish_reason: Option<String>,
    since: std::time::Instant,
}

impl Default for MergedChunk {
    fn default() -> Self {
        Self {
            id: String::new(),
            text: String::new(),
            thinking: String::new(),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            other: Vec::new(),
            chunks: 0,
            bytes: 0,
            usage: ChatUsage::default(),
            finish_reason: None,
            since: std::time::Instant::now(),
        }
    }
}

impl MergedChunk {
    fn push(&mut self, chunk: &ChatResponse) {
        if self.id.is_empty() {
            self.id = chunk.id.clone();
        }
        if chunk.usage.total_tokens() > 0 {
            self.usage = chunk.usage;
        }
        if chunk.finish_reason.is_some() {
            self.finish_reason = chunk.finish_reason.clone();
        }
        self.chunks += 1;
        for block in &chunk.content {
            self.bytes += serde_json::to_string(block)
                .map(|json| json.len())
                .unwrap_or(0);
            match block {
                ContentBlock::Text(text) => self.text.push_str(&text.text),
                ContentBlock::Thinking(thinking) => self.thinking.push_str(&thinking.thinking),
                ContentBlock::ToolUse(call) => self.tool_calls.push(call.clone()),
                ContentBlock::ToolResult(result) => self.tool_results.push(result.clone()),
                other => self.other.push(other.clone()),
            }
        }
    }

    fn should_flush(&self) -> bool {
        self.chunks >= 64
            || self.bytes >= 4 * 1024
            || self.since.elapsed() >= std::time::Duration::from_secs(1)
    }

    fn into_response(&mut self) -> ChatResponse {
        let mut content = Vec::new();
        if !self.thinking.is_empty() {
            content.push(ContentBlock::Thinking(ThinkingBlock::new(std::mem::take(
                &mut self.thinking,
            ))));
        }
        if !self.text.is_empty() {
            content.push(ContentBlock::Text(TextBlock::new(std::mem::take(
                &mut self.text,
            ))));
        }
        for call in self.tool_calls.drain(..) {
            content.push(ContentBlock::ToolUse(call));
        }
        for result in self.tool_results.drain(..) {
            content.push(ContentBlock::ToolResult(result));
        }
        content.extend(self.other.drain(..));
        let response = ChatResponse::new(
            self.id.clone(),
            content,
            self.usage,
            None,
            self.finish_reason.take(),
        );
        self.chunks = 0;
        self.bytes = 0;
        self.usage = ChatUsage::default();
        self.since = std::time::Instant::now();
        response
    }
}

async fn record_merged_chunk(
    recorder: &SessionRecorder,
    call_id: &str,
    purpose: &str,
    merged: &mut MergedChunk,
) -> crate::error::Result<()> {
    if merged.chunks == 0 {
        return Ok(());
    }
    let merged_chunks = merged.chunks;
    let response = merged.into_response();
    recorder
        .record(
            "model/chunk",
            &serde_json::json!({
                "modelCallId": call_id,
                "purpose": purpose,
                "chunk": response,
                "mergedChunks": merged_chunks,
            }),
        )
        .await?;
    Ok(())
}

/// The recorded adapter boundary: journals each dispatch and forwards the chunks.
struct Recorded {
    delegate: Arc<dyn Model>,
    ctx: RuntimeContext,
    purpose: String,
}

impl Model for Recorded {
    fn stream(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: GenerateOptions,
    ) -> BoxStream<'_, crate::error::Result<ChatResponse>> {
        // Resolved at subscribe time (Java resolves it inside `Flux.defer` too), so a recorder
        // attached after the wrap still records and a detached one stops recording.
        let Some(recorder) = SessionRecorder::from_context(&self.ctx) else {
            return self.delegate.stream(messages, tools, options);
        };

        let delegate = Arc::clone(&self.delegate);
        let ctx = self.ctx.clone();
        let purpose = self.purpose.clone();
        // `model_name` is borrowed from the delegate, which may be moved into the generator, so
        // take an owned copy first.
        let model_name = delegate.model_name().to_string();

        Box::pin(async_stream::stream! {
            // A call id per dispatch: retries and parallel calls never share one.
            let call_id = uuid::Uuid::new_v4().to_string().replace('-', "");

            // Admission runs *before* the request is prepared (Java order).
            if let Some(policy) = policy_from_context(&ctx) {
                policy.before_call(&call_id, &model_name, &ctx);
            }

            // The recorder checkpoints `before_model`; hand it a snapshot because a guard
            // cannot be held across an await inside a `Send` stream.
            let state = ctx.state.as_ref().map(|state| {
                state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
            });
            let input = ModelCallInput::new(
                messages.clone(),
                tools.clone(),
                options.clone(),
                Arc::clone(&delegate),
            );
            if let Err(error) = recorder
                .prepare_model(&input, &call_id, &purpose, state.as_ref())
                .await
            {
                yield Err(error);
                return;
            }

            let mut last: Option<ChatResponse> = None;
            let mut failure: Option<crate::error::Error> = None;
            let mut merged = MergedChunk::default();
            let mut inner = delegate.stream(messages, tools, options);
            while let Some(item) = inner.next().await {
                match item {
                    Ok(chunk) => {
                        // The consumer still receives every delta; the journal only keeps
                        // coalesced deltas so a long stream does not become thousands of
                        // committed events.
                        merged.push(&chunk);
                        if merged.should_flush() {
                            if let Err(error) =
                                record_merged_chunk(&recorder, &call_id, &purpose, &mut merged)
                                    .await
                            {
                                failure = Some(error);
                                break;
                            }
                        }
                        last = Some(chunk.clone());
                        yield Ok(chunk);
                    }
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }

            // Flush the trailing merged delta, including a partial response before a
            // provider failure. A persistence failure is never retried into the journal.
            if !matches!(failure, Some(crate::error::Error::Session(_))) {
                if let Err(error) =
                    record_merged_chunk(&recorder, &call_id, &purpose, &mut merged).await
                {
                    if failure.is_none() {
                        failure = Some(error);
                    }
                }
            }

            let status = match &failure {
                None => "completed",
                // Java distinguishes a cancelled subscription from a provider failure.
                Some(crate::error::Error::Cancelled(_)) => "cancelled",
                Some(_) => "failed",
            };
            // Mirrors Java `SessionLogException.causedBy(error) ? Mono.empty()`: a failure to
            // persist is never itself recorded — that could loop on the same broken journal and
            // hide the original cause.
            let persistence_failed = matches!(failure, Some(crate::error::Error::Session(_)));
            if !persistence_failed {
                let mut payload = serde_json::Map::new();
                payload.insert("modelCallId".to_string(), call_id.clone().into());
                payload.insert("status".to_string(), status.into());
                payload.insert("model".to_string(), model_name.clone().into());
                payload.insert("purpose".to_string(), purpose.clone().into());
                if let Some(last) = &last {
                    payload.insert(
                        "usage".to_string(),
                        serde_json::to_value(last.usage).unwrap_or(serde_json::Value::Null),
                    );
                    // `finish_reason` is a plain `Option<String>`, so it is journaled (Java does
                    // the same); only richer provider-specific reasons would need omitting.
                    if let Some(reason) = &last.finish_reason {
                        payload.insert("finishReason".to_string(), reason.clone().into());
                    }
                }
                if let Some(error) = &failure {
                    // A coarse category, not the message: the payload is durable and may carry
                    // provider text or user content.
                    payload.insert("errorType".to_string(), error_category(error).into());
                }
                if let Err(error) = recorder
                    .record("model/end", &serde_json::Value::Object(payload))
                    .await
                {
                    if failure.is_none() {
                        failure = Some(error);
                    }
                }
            }

            if let Some(policy) = policy_from_context(&ctx) {
                policy.after_call(
                    &call_id,
                    &model_name,
                    status,
                    last.as_ref().map(|response| &response.usage),
                    &ctx,
                );
            }

            // A recording failure is a stream failure: the call must never look successful
            // after its facts were lost.
            if let Some(error) = failure {
                yield Err(error);
            }
        })
    }

    fn model_name(&self) -> &str {
        self.delegate.model_name()
    }

    fn supports_native_structured_output(&self) -> bool {
        self.delegate.supports_native_structured_output()
    }

    fn supports_native_structured_output_with_tools(&self) -> bool {
        self.delegate.supports_native_structured_output_with_tools()
    }

    fn supports_tool_choice_specific(&self) -> bool {
        self.delegate.supports_tool_choice_specific()
    }

    fn context_window_size(&self) -> usize {
        self.delegate.context_window_size()
    }
}

/// A coarse, non-sensitive error category for `model/end.errorType`.
///
/// Java stores the exception class name; a category name keeps the same diagnostic value
/// without persisting provider messages (which may embed request content).
fn error_category(error: &crate::error::Error) -> &'static str {
    match error {
        crate::error::Error::Json(_) => "json",
        crate::error::Error::Io(_) => "io",
        crate::error::Error::Http(_) => "http",
        crate::error::Error::HttpStatus { .. } => "http_status",
        crate::error::Error::Model(_) => "model",
        crate::error::Error::Tool(_) => "tool",
        crate::error::Error::ToolSuspended(_) => "tool_suspended",
        crate::error::Error::Agent(_) => "agent",
        crate::error::Error::Interrupted(_) => "interrupted",
        crate::error::Error::Cancelled(_) => "cancelled",
        crate::error::Error::Session(_) => "session",
        crate::error::Error::ActionObservation(_) => "action_observation",
        crate::error::Error::ConcurrentSessionModification { .. } => {
            "concurrent_session_modification"
        }
        crate::error::Error::ShuttingDown(_) => "shutting_down",
        crate::error::Error::Other(_) => "other",
        crate::error::Error::CompositeAgent(_) => "composite_agent",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures::stream::BoxStream;
    use futures::StreamExt;

    use super::*;
    use crate::message::{ContentBlock, TextBlock};
    use crate::session::{
        AtomicSessionStorage, JournalSessionLog, MemorySessionStorage, SessionLog, SessionRecorder,
        RECORDER_CONTEXT_KEY,
    };

    /// A model that yields fixed chunks and optionally fails at the end.
    struct StubModel {
        name: String,
        chunks: Vec<ChatResponse>,
        fail: bool,
    }

    impl StubModel {
        fn new(name: &str, chunks: Vec<ChatResponse>) -> Self {
            Self {
                name: name.to_string(),
                chunks,
                fail: false,
            }
        }

        fn failing(name: &str, chunks: Vec<ChatResponse>) -> Self {
            Self {
                fail: true,
                ..Self::new(name, chunks)
            }
        }
    }

    impl Model for StubModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, crate::error::Result<ChatResponse>> {
            let mut items: Vec<crate::error::Result<ChatResponse>> =
                self.chunks.iter().cloned().map(Ok).collect();
            if self.fail {
                items.push(Err(crate::error::Error::Model("boom".to_string())));
            }
            Box::pin(futures::stream::iter(items))
        }

        fn model_name(&self) -> &str {
            &self.name
        }
    }

    fn chunk(id: &str, text: &str, usage: ChatUsage) -> ChatResponse {
        ChatResponse::new(
            id,
            vec![ContentBlock::Text(TextBlock::new(text))],
            usage,
            None,
            Some("stop".to_string()),
        )
    }

    fn usage(input: u64, output: u64) -> ChatUsage {
        ChatUsage {
            input_tokens: input,
            output_tokens: output,
            ..ChatUsage::default()
        }
    }

    async fn journal() -> Arc<dyn SessionLog> {
        let storage: Arc<dyn AtomicSessionStorage> = Arc::new(MemorySessionStorage::new());
        Arc::new(JournalSessionLog::new(storage, "s1"))
    }

    fn context(recorder: &Arc<SessionRecorder>) -> RuntimeContext {
        RuntimeContext::new().with_extension_arc(RECORDER_CONTEXT_KEY, Arc::clone(recorder))
    }

    async fn events(log: &dyn SessionLog) -> Vec<crate::session::SessionEvent> {
        let head = log.head().await.unwrap();
        log.scan(0, head.seq).await.unwrap()
    }

    #[tokio::test]
    async fn records_the_adapter_boundary_like_java() {
        let log = journal().await;
        let recorder = Arc::new(
            SessionRecorder::open(Arc::clone(&log), Some("t1"))
                .await
                .unwrap(),
        );
        let mut ctx = context(&recorder);
        ctx.state = Some(Arc::new(Mutex::new(
            crate::state::AgentState::builder()
                .session_id("s1")
                .add_message(Msg::user("hi"))
                .build(),
        )));

        let model: Arc<dyn Model> = Arc::new(StubModel::new(
            "stub",
            vec![
                chunk("c1", "hel", usage(3, 0)),
                chunk("c1", "lo", usage(3, 2)),
            ],
        ));
        let wrapped = SessionModels::wrap(model, &ctx, "REASONING");
        assert_eq!(wrapped.model_name(), "stub");
        let streamed: Vec<_> = wrapped
            .stream(
                vec![Msg::user("hi")],
                Vec::new(),
                GenerateOptions::default(),
            )
            .collect()
            .await;
        assert_eq!(streamed.len(), 2);
        assert!(streamed.iter().all(|item| item.is_ok()), "{streamed:?}");

        let facts = events(&*log).await;
        let types: Vec<&str> = facts.iter().map(|e| e.event_type.as_str()).collect();
        assert!(types.contains(&"state/checkpoint"), "{types:?}");
        assert!(types.contains(&"request/prepared"), "{types:?}");
        assert!(types.contains(&"model/dispatch"), "{types:?}");
        let chunk_events: Vec<_> = facts
            .iter()
            .filter(|event| event.event_type == "model/chunk")
            .collect();
        assert_eq!(chunk_events.len(), 1, "chunks must be coalesced: {types:?}");
        let merged = chunk_events[0].data().unwrap();
        assert_eq!(merged["mergedChunks"], 2);
        assert!(
            merged["chunk"]["content"].to_string().contains("hello"),
            "{merged}"
        );
        assert!(types.contains(&"model/end"), "{types:?}");

        // One call id correlates every fact of this dispatch.
        let prepared = facts
            .iter()
            .find(|e| e.event_type == "request/prepared")
            .unwrap();
        let prepared = prepared.data().unwrap();
        let call_id = prepared["modelCallId"].as_str().unwrap().to_string();
        assert!(!call_id.is_empty());
        assert_eq!(prepared["purpose"], "REASONING");
        assert_eq!(prepared["model"], "stub");
        assert_eq!(prepared["messages"][0]["role"], "User");

        let dispatch = facts
            .iter()
            .find(|e| e.event_type == "model/dispatch")
            .unwrap();
        let dispatch = dispatch.data().unwrap();
        assert_eq!(dispatch["modelCallId"], call_id.as_str());
        assert_eq!(dispatch["attemptId"], call_id.as_str());

        let end = facts.iter().find(|e| e.event_type == "model/end").unwrap();
        let end = end.data().unwrap();
        assert_eq!(end["modelCallId"], call_id.as_str());
        assert_eq!(end["status"], "completed");
        assert_eq!(end["model"], "stub");
        assert_eq!(end["purpose"], "REASONING");
        assert_eq!(end["usage"]["inputTokens"], 3);
        assert_eq!(end["finishReason"], "stop");
        assert!(end.get("errorType").is_none());

        recorder.close().await;
    }

    #[tokio::test]
    async fn without_a_recorder_the_model_is_returned_unchanged() {
        let model: Arc<dyn Model> = Arc::new(StubModel::new("stub", Vec::new()));
        let wrapped = SessionModels::wrap(Arc::clone(&model), &RuntimeContext::new(), "REASONING");
        assert!(Arc::ptr_eq(&model, &wrapped));
    }

    #[tokio::test]
    async fn a_failing_model_records_failed_and_propagates_the_error() {
        let log = journal().await;
        let recorder = Arc::new(
            SessionRecorder::open(Arc::clone(&log), Some("t1"))
                .await
                .unwrap(),
        );
        let ctx = context(&recorder);

        let model: Arc<dyn Model> = Arc::new(StubModel::failing(
            "stub",
            vec![chunk("c1", "partial", usage(1, 1))],
        ));
        let wrapped = SessionModels::wrap(model, &ctx, "SUMMARY");
        let streamed: Vec<_> = wrapped
            .stream(
                vec![Msg::user("hi")],
                Vec::new(),
                GenerateOptions::default(),
            )
            .collect()
            .await;
        assert_eq!(streamed.len(), 2, "one chunk then the failure");
        assert!(streamed[0].is_ok());
        assert!(matches!(streamed[1], Err(crate::error::Error::Model(_))));

        let facts = events(&*log).await;
        let end = facts
            .iter()
            .find(|e| e.event_type == "model/end")
            .expect("model/end is recorded for failures too");
        let end = end.data().unwrap();
        assert_eq!(end["status"], "failed");
        assert_eq!(end["errorType"], "model");
        assert_eq!(end["usage"]["inputTokens"], 1, "the last chunk's usage");
        assert!(
            !end.to_string().contains("boom"),
            "the message must not leak into the journal: {end}"
        );

        recorder.close().await;
    }

    #[tokio::test]
    async fn the_policy_sees_before_and_after_with_the_same_call_id() {
        #[derive(Default)]
        struct RecordingPolicy {
            seen: Mutex<Vec<String>>,
        }

        impl SessionModelPolicy for RecordingPolicy {
            fn before_call(&self, call_id: &str, model: &str, _ctx: &RuntimeContext) {
                self.seen
                    .lock()
                    .unwrap()
                    .push(format!("before:{call_id}:{model}"));
            }

            fn after_call(
                &self,
                call_id: &str,
                model: &str,
                status: &str,
                usage: Option<&ChatUsage>,
                _ctx: &RuntimeContext,
            ) {
                let total = usage.map(|usage| usage.total_tokens()).unwrap_or(0);
                self.seen
                    .lock()
                    .unwrap()
                    .push(format!("after:{call_id}:{model}:{status}:{total}"));
            }
        }

        let log = journal().await;
        let recorder = Arc::new(
            SessionRecorder::open(Arc::clone(&log), Some("t1"))
                .await
                .unwrap(),
        );
        let policy = Arc::new(RecordingPolicy::default());
        let dynamic: Arc<dyn SessionModelPolicy> = policy.clone();
        let ctx = context(&recorder)
            .with_extension(MODEL_POLICY_CONTEXT_KEY, SessionModelPolicyHandle(dynamic));

        let model: Arc<dyn Model> =
            Arc::new(StubModel::new("stub", vec![chunk("c1", "hi", usage(2, 3))]));
        let wrapped = SessionModels::wrap(model, &ctx, "REASONING");
        let _: Vec<_> = wrapped
            .stream(
                vec![Msg::user("hi")],
                Vec::new(),
                GenerateOptions::default(),
            )
            .collect()
            .await;

        let seen = policy.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen[0].starts_with("before:"), "{seen:?}");
        let call_id = seen[0].split(':').nth(1).unwrap().to_string();
        assert_eq!(seen[1], format!("after:{call_id}:stub:completed:5"));

        recorder.close().await;
    }
}
