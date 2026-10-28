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
/// `ERROR`/`CALL_OR_RESPONSE_ERROR`. Java reports an observation record as a side effect; the
/// record is available from [`super::execution::JevExecution::record`] for callers that observe.
pub async fn execute_guarded<T, F>(execution: &super::execution::JevExecution, operation: F) -> super::execution::Decision<T>
where
    F: Future<Output = Result<super::execution::Decision<T>, JevError>>,
{
    use super::execution::{Decision, Mode};
    if execution.mode() == Mode::Off {
        return execution.disabled_decision();
    }
    match tokio::time::timeout(execution.budget(), operation).await {
        Ok(Ok(decision)) => decision,
        Ok(Err(_error)) => Decision::error(false),
        Err(_elapsed) => Decision::error(true),
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
