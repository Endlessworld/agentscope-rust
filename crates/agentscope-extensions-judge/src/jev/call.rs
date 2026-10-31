//! The injectable System One call seam shared by every JEV orchestration layer.
//!
//! Java threads a `Function<SystemOneRequest, Mono<SystemOneResult>>` through the evaluation,
//! application, evidence, routing and supervision packages so that offline tests can answer with
//! fixed results while production wires [`JevClient::system_one`]. Reactor's `Mono` becomes a
//! boxed `Future`; the failure type is [`CallFailure`] because two of Java's call sites need the
//! billable `model`/`usage` of a reply that failed contract validation
//! (`JevTextBackend.InvalidReply`).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;

use crate::error::JevError;

use super::client::JevClient;
use super::dto::{SystemOneRequest, SystemOneResult, Usage};

/// A boxed, `Send` future returned by a [`Caller`] or [`TextTransport`].
///
/// The lifetime is `'static` so that the closures can own everything they need (a [`JevClient`]
/// clone, a scripted response list, ...).
pub type BoxedFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// One System One call, resolved asynchronously.
pub type CallFuture = BoxedFuture<Result<SystemOneResult, CallFailure>>;

/// A caller seam: `SystemOneRequest` -> boxed future.
pub type Caller = Arc<dyn Fn(SystemOneRequest) -> CallFuture + Send + Sync>;

/// One raw chat-completion call used by [`crate::jev::evaluation::JevTextBackend`]:
/// the request body in, the raw model reply out.
pub type TextFuture = BoxedFuture<Result<String, CallFailure>>;

/// A chat-completion transport seam (Java `Function<String, Mono<String>>`).
pub type TextTransport = Arc<dyn Fn(String) -> TextFuture + Send + Sync>;

/// Why an injected caller produced no usable [`SystemOneResult`].
///
/// Java distinguishes `JevTextBackend.InvalidReply` (a malformed model reply that still carries
/// billable usage) from every other exception; two consumers use that distinction to avoid
/// fabricating a zero cost.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum CallFailure {
    /// A reply that violated the response contract, but whose model/usage are still known.
    InvalidReply {
        /// The model the backend reported (Java prefixes chat models with `llm:`).
        model: String,
        /// The billable usage, when the reply reported usable counters.
        usage: Option<Usage>,
    },
    /// Transport, HTTP, validation or timeout problem.
    Backend {
        /// Human readable message.
        message: String,
    },
}

impl CallFailure {
    /// Creates a backend failure.
    pub fn backend(message: impl Into<String>) -> Self {
        Self::Backend {
            message: message.into(),
        }
    }

    /// Creates an invalid-reply failure.
    pub fn invalid_reply(model: impl Into<String>, usage: Option<Usage>) -> Self {
        Self::InvalidReply {
            model: model.into(),
            usage,
        }
    }

    /// The reported model, when the failure came from a parseable reply.
    pub fn model(&self) -> Option<&str> {
        match self {
            Self::InvalidReply { model, .. } => Some(model),
            Self::Backend { .. } => None,
        }
    }

    /// The billable usage, when the failure came from a parseable reply.
    pub fn usage(&self) -> Option<Usage> {
        match self {
            Self::InvalidReply { usage, .. } => *usage,
            Self::Backend { .. } => None,
        }
    }

    /// Whether this failure is an invalid reply (Java `instanceof InvalidReply`).
    pub fn is_invalid_reply(&self) -> bool {
        matches!(self, Self::InvalidReply { .. })
    }

    /// The message, for error reporting.
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidReply { .. } => "INVALID_TEXT_MODEL_RESPONSE",
            Self::Backend { message } => message,
        }
    }
}

impl std::fmt::Display for CallFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for CallFailure {}

impl From<JevError> for CallFailure {
    fn from(error: JevError) -> Self {
        Self::backend(error.message().to_string())
    }
}

/// Adapts a [`JevClient`] to the [`Caller`] seam.
pub fn caller_from_client(client: JevClient) -> Caller {
    Arc::new(move |request| {
        let client = client.clone();
        Box::pin(async move { client.system_one(&request).await.map_err(CallFailure::from) })
    })
}

/// Runs `operation` under the execution's mode and budget.
///
/// Mirrors `JevExecution.execute`: `OFF` never runs the operation and reports `SKIPPED`/`DISABLED`,
/// a timeout becomes `ERROR`/`TIMEOUT` and any other failure becomes
/// `ERROR`/`CALL_OR_RESPONSE_ERROR`. The observation record is handed to the configured
/// [`Observer`](super::execution::Observer) (no `RuntimeContext` is in scope here; use
/// [`execute_guarded_with_context`] to forward one). Dropping the returned future before it
/// completes reports `CANCELLED` (`JevExecution.execute`'s `doOnCancel`).
pub async fn execute_guarded<T, F>(
    execution: &super::execution::JevExecution,
    operation: F,
) -> super::execution::Decision<T>
where
    F: Future<Output = Result<super::execution::Decision<T>, JevError>>,
{
    execute_guarded_with_context(execution, None, operation).await
}

/// [`execute_guarded`] with the [`RuntimeContext`] handed to the observer
/// (`JevExecution.execute(ctx, operation)`).
pub async fn execute_guarded_with_context<T, F>(
    execution: &super::execution::JevExecution,
    context: Option<&RuntimeContext>,
    operation: F,
) -> super::execution::Decision<T>
where
    F: Future<Output = Result<super::execution::Decision<T>, JevError>>,
{
    use super::execution::Mode;
    let mut observation = Observation::new(execution, context);
    let decision = if execution.mode() == Mode::Off {
        execution.disabled_decision()
    } else {
        match tokio::time::timeout(execution.budget(), operation).await {
            Ok(Ok(decision)) => decision,
            Ok(Err(_error)) => super::execution::Decision::error(false),
            Err(_elapsed) => super::execution::Decision::error(true),
        }
    };
    observation.report(decision.status, &decision.reason, &decision.recommendation);
    decision
}

/// Reports a `CANCELLED` record from `Drop` when the future is dropped before finishing.
///
/// This is the ownership-based equivalent of Reactor's `doOnCancel`: a future that is dropped
/// mid-await cannot produce a decision, so the observer still learns that the run was abandoned.
struct Observation<'a> {
    execution: &'a super::execution::JevExecution,
    context: Option<&'a RuntimeContext>,
    started: std::time::Instant,
    reported: bool,
}

impl<'a> Observation<'a> {
    fn new(
        execution: &'a super::execution::JevExecution,
        context: Option<&'a RuntimeContext>,
    ) -> Self {
        Self {
            execution,
            context,
            started: std::time::Instant::now(),
            reported: false,
        }
    }

    fn report(
        &mut self,
        status: super::execution::Status,
        reason: &str,
        recommendation: &std::collections::BTreeMap<String, String>,
    ) {
        self.reported = true;
        self.execution.observe(
            self.context,
            status,
            reason,
            self.started.elapsed(),
            recommendation,
        );
    }
}

impl Drop for Observation<'_> {
    fn drop(&mut self) {
        if !self.reported {
            self.report(
                super::execution::Status::Cancelled,
                super::execution::reason::CANCELLED,
                &std::collections::BTreeMap::new(),
            );
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Mutex;

    use super::*;

    /// An in-memory [`Caller`] that replays prepared results in order.
    pub(crate) struct ScriptedCaller {
        responses: Mutex<Vec<Result<SystemOneResult, CallFailure>>>,
        requests: Mutex<Vec<SystemOneRequest>>,
    }

    impl ScriptedCaller {
        pub(crate) fn new(responses: Vec<Result<SystemOneResult, CallFailure>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                requests: Mutex::new(Vec::new()),
            }
        }

        pub(crate) fn as_caller(self: &Arc<Self>) -> Caller {
            let shared = Arc::clone(self);
            Arc::new(move |request| {
                let shared = Arc::clone(&shared);
                Box::pin(async move {
                    shared.requests.lock().unwrap().push(request);
                    let mut responses = shared.responses.lock().unwrap();
                    if responses.is_empty() {
                        return Err(CallFailure::backend("script exhausted"));
                    }
                    responses.remove(0)
                })
            })
        }

        pub(crate) fn requests(&self) -> Vec<SystemOneRequest> {
            self.requests.lock().unwrap().clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::execution::Decision;
    use super::*;

    #[test]
    fn call_failure_exposes_java_invalid_reply_context() {
        let usage = Usage::new(7, 3);
        let invalid = CallFailure::invalid_reply("llm:test", Some(usage));
        assert!(invalid.is_invalid_reply());
        assert_eq!(invalid.model(), Some("llm:test"));
        assert_eq!(invalid.usage(), Some(usage));
        assert_eq!(invalid.message(), "INVALID_TEXT_MODEL_RESPONSE");

        let backend = CallFailure::backend("boom");
        assert!(!backend.is_invalid_reply());
        assert_eq!(backend.model(), None);
        assert_eq!(backend.usage(), None);
        assert_eq!(backend.to_string(), "boom");
    }

    #[test]
    fn jev_errors_become_backend_failures() {
        let failure = CallFailure::from(JevError::invalid("state must not be null"));
        assert_eq!(failure, CallFailure::backend("state must not be null"));
    }

    #[tokio::test]
    async fn execute_guarded_maps_modes_budgets_and_errors() {
        use super::super::execution::{Decision, JevExecution, Mode, Options, Status};
        use std::time::Duration;

        let off = JevExecution::new("x", Options::disabled());
        let skipped: Decision<u8> = execute_guarded(&off, async { Ok(Decision::decided(1)) }).await;
        assert_eq!(skipped.status, Status::Skipped);

        let shadow = JevExecution::new(
            "x",
            Options::new(Mode::Shadow, Duration::from_millis(50), "v1").unwrap(),
        );
        let decided: Decision<u8> = execute_guarded(&shadow, async { Ok(Decision::decided(2)) }).await;
        assert_eq!(decided.status, Status::Decided);
        assert_eq!(decided.value, Some(2));

        let failed: Decision<u8> =
            execute_guarded(&shadow, async { Err(JevError::invalid("nope")) }).await;
        assert_eq!(failed.status, Status::Error);
        assert_eq!(failed.reason, super::super::execution::reason::CALL_OR_RESPONSE_ERROR);

        let slow: Decision<u8> = execute_guarded(&shadow, async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(Decision::decided(3))
        })
        .await;
        assert_eq!(slow.status, Status::Error);
        assert_eq!(slow.reason, super::super::execution::reason::TIMEOUT);
    }

    /// A shared sink of observation records plus an observer that appends to it.
    type Seen = Arc<std::sync::Mutex<Vec<super::super::execution::Record>>>;

    fn recording_observer() -> (super::super::execution::Observer, Seen) {
        let seen: Seen = Arc::default();
        let shared = Arc::clone(&seen);
        let observer: super::super::execution::Observer =
            Arc::new(move |_context, record: &super::super::execution::Record| {
                shared.lock().unwrap().push(record.clone());
            });
        (observer, seen)
    }

    /// JUDGE-02: every run reports exactly one record — including `SKIPPED` in `OFF` and
    /// `ERROR`/`TIMEOUT`, whose elapsed time covers the whole budget
    /// (`JevExecution.execute`'s `doOnNext`).
    #[tokio::test]
    async fn the_observer_receives_decided_timeout_and_skipped_runs() {
        use super::super::execution::{reason, JevExecution, Mode, Options, Status};
        use std::time::Duration;

        let (observer, seen) = recording_observer();
        let execution = JevExecution::new(
            "quality",
            Options::new(Mode::Enforce, Duration::from_millis(30), "v2")
                .unwrap()
                .with_observer(Arc::clone(&observer)),
        );
        let decided: Decision<u8> =
            execute_guarded(&execution, async { Ok(Decision::decided(1)) }).await;
        assert_eq!(decided, Decision::decided(1));

        let timed_out: Decision<u8> = execute_guarded(&execution, async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(Decision::decided(2))
        })
        .await;
        assert_eq!(timed_out.reason, reason::TIMEOUT);

        let off = JevExecution::new(
            "quality",
            Options::disabled().with_observer(observer),
        );
        let skipped: Decision<u8> =
            execute_guarded(&off, async { Ok(Decision::decided(3)) }).await;
        assert_eq!(skipped.status, Status::Skipped);

        let records = seen.lock().unwrap().clone();
        assert_eq!(records.len(), 3);

        assert_eq!(records[0].purpose, "quality");
        assert_eq!(records[0].version, "v2");
        assert_eq!(records[0].mode, Mode::Enforce);
        assert_eq!(records[0].status, Status::Decided);
        assert_eq!(records[0].reason, reason::DECIDED);
        assert!(records[0].recommendation.is_empty());

        assert_eq!(records[1].status, Status::Error);
        assert_eq!(records[1].reason, reason::TIMEOUT);
        assert!(records[1].elapsed >= Duration::from_millis(30));

        assert_eq!(records[2].status, Status::Skipped);
        assert_eq!(records[2].reason, reason::DISABLED);
        assert_eq!(records[2].mode, Mode::Off);
    }

    /// JUDGE-02: the supplied [`RuntimeContext`] reaches the observer
    /// (`JevExecution.execute(ctx, …)`).
    #[tokio::test]
    async fn the_observer_receives_the_supplied_runtime_context() {
        use super::super::execution::{JevExecution, Mode, Options};
        use std::time::Duration;

        let session: Arc<std::sync::Mutex<Option<String>>> = Arc::default();
        let shared = Arc::clone(&session);
        let observer: super::super::execution::Observer = Arc::new(move |context, _record| {
            *shared.lock().unwrap() = context.and_then(|c| c.session_id()).map(str::to_string);
        });
        let execution = JevExecution::new(
            "routing",
            Options::new(Mode::Shadow, Duration::from_millis(50), "v1")
                .unwrap()
                .with_observer(observer),
        );
        let ctx = RuntimeContext::new().with_session_id("s-observer");
        let decision: Decision<u8> = execute_guarded_with_context(&execution, Some(&ctx), async {
            Ok(Decision::decided(1))
        })
        .await;
        assert_eq!(decision.status, super::super::execution::Status::Decided);
        assert_eq!(session.lock().unwrap().as_deref(), Some("s-observer"));
    }

    /// JUDGE-02: dropping the future mid-await reports `CANCELLED` (`doOnCancel`).
    #[tokio::test]
    async fn dropping_the_future_reports_a_cancelled_run() {
        use super::super::execution::{reason, JevExecution, Mode, Options, Status};
        use std::time::Duration;

        let (observer, seen) = recording_observer();
        let execution = JevExecution::new(
            "evidence",
            Options::new(Mode::Shadow, Duration::from_secs(5), "v1")
                .unwrap()
                .with_observer(observer),
        );
        {
            let mut future = Box::pin(execute_guarded(&execution, async {
                tokio::time::sleep(Duration::from_millis(400)).await;
                Ok(Decision::decided(1u8))
            }));
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut future)
                    .await
                    .is_err(),
                "the operation must still be in flight"
            );
        }

        let records = seen.lock().unwrap().clone();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].purpose, "evidence");
        assert_eq!(records[0].status, Status::Cancelled);
        assert_eq!(records[0].reason, reason::CANCELLED);
        assert!(records[0].recommendation.is_empty());
    }

    /// JUDGE-02: a panicking observer must not change the decision
    /// (`JevExecution.observe` swallows `RuntimeException`).
    #[tokio::test]
    async fn a_panicking_observer_does_not_change_the_decision() {
        use super::super::execution::{JevExecution, Mode, Options};
        use std::time::Duration;

        let observer: super::super::execution::Observer =
            Arc::new(|_context, _record| panic!("observer must not change execution"));
        let execution = JevExecution::new(
            "guardrail",
            Options::new(Mode::Enforce, Duration::from_millis(50), "v1")
                .unwrap()
                .with_observer(observer),
        );
        let decision: Decision<u8> =
            execute_guarded(&execution, async { Ok(Decision::decided(7)) }).await;
        assert_eq!(decision, Decision::decided(7));
    }

    #[tokio::test]
    async fn caller_from_client_uses_the_injected_transport() {
        use crate::http::testing::ScriptedTransport;
        use crate::http::{JudgeHttpResponse, JudgeHttpTransport};
        use std::collections::BTreeMap;

        let body = r#"{"model":"jev-latest","answers":{"q":{"type":"noul","noul":0.9}},"usage":{"input_tokens":1,"output_tokens":2}}"#;
        let transport = ScriptedTransport::new(vec![Ok(JudgeHttpResponse::new(200, body))]);
        let client = JevClient::builder()
            .api_key("k")
            .transport(Arc::new(transport) as Arc<dyn JudgeHttpTransport>)
            .build()
            .unwrap();
        let caller = caller_from_client(client);
        let request = SystemOneRequest::builder()
            .state(serde_json::json!({}))
            .question(
                "q",
                super::super::dto::Question::Noul(super::super::dto::NoulQuestion::new(
                    serde_json::json!("is it true?"),
                    None,
                )),
            )
            .build();
        let result = caller(request).await.unwrap();
        assert_eq!(result.model.as_deref(), Some("jev-latest"));
        let answers: &BTreeMap<String, super::super::dto::Answer> =
            result.answers.as_ref().unwrap();
        assert_eq!(answers.len(), 1);
    }
}
