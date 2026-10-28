//! `JevClient` — the System One HTTP call, mirroring Java
//! `io.agentscope.extensions.judge.jev.JevClient`.
//!
//! The pipeline is the same as Java's `systemOne`:
//!
//! 1. validate the request (`validate_request`, argument errors are never retried),
//! 2. substitute the effective model (`request.model` wins unless null/blank),
//! 3. `POST {base}/v1/systemone` with `Authorization: Bearer <key>` and the serialized request,
//! 4. per-attempt timeout, HTTP status mapping, parsing, then `validate_result`,
//! 5. retry only retryable failures, with `initialBackoff * 2^n` capped at `8x`.
//!
//! Reactor's `Mono`/`Schedulers.boundedElastic()` and its `jitter(0.2)` have no counterpart here;
//! the call is an `async fn` driven by the caller's runtime and the backoff is deterministic
//! (see the tracker's execution record for both known deviations).

use std::sync::Arc;
use std::time::Duration;

use crate::error::JevError;
use crate::http::{
    backoff_ms, JudgeHttpRequest, JudgeHttpResponse, JudgeHttpTransport, ReqwestJudgeTransport,
};

use super::dto::{SystemOneRequest, SystemOneResult};
use super::validation::{validate_request, validate_result};
use super::{
    api_key_from_env, error_message, system_one_url, RetryPolicy, DEFAULT_BASE_URL, DEFAULT_MODEL,
    DEFAULT_TIMEOUT_MILLIS,
};

/// Statuses [`JevClient`] retries.
///
/// Java `JevClient.httpException` retries only `429`, `529` and `5xx`. The port additionally
/// retries `408` (request timeout) and `409` (optimistic-concurrency conflict), which gateways
/// in front of the API emit for transient failures; the module-level retry filter is otherwise
/// identical. Decisions keeps the Java set verbatim (`429`/`5xx`), see
/// [`crate::openai_decisions::is_retryable_status`].
#[must_use]
pub fn is_retryable_status(status_code: u16) -> bool {
    matches!(status_code, 408 | 409 | 429 | 529) || status_code >= 500
}

/// Client for TypeSafe's System One endpoint.
#[derive(Clone)]
pub struct JevClient {
    api_key: String,
    base_url: String,
    model: String,
    transport: Arc<dyn JudgeHttpTransport>,
    retry_policy: RetryPolicy,
    timeout: Duration,
}

impl std::fmt::Debug for JevClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JevClient")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("retry_policy", &self.retry_policy)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl JevClient {
    /// Starts building a client (`JevClient.builder()`).
    pub fn builder() -> JevClientBuilder {
        JevClientBuilder::default()
    }

    /// The trimmed, validated base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The configured default model.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The per-attempt timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The configured retry policy.
    pub fn retry_policy(&self) -> RetryPolicy {
        self.retry_policy
    }

    /// The System One endpoint derived from the base URL (`JevClient.systemOneUrl`).
    pub fn system_one_url(&self) -> String {
        system_one_url(&self.base_url)
    }

    /// Sends one System One request and returns the validated result.
    ///
    /// Validation failures are [`JevError::InvalidArgument`] and never reach the transport;
    /// response-contract failures are [`JevError::Protocol`] and are never retried.
    pub async fn system_one(
        &self,
        request: &SystemOneRequest,
    ) -> Result<SystemOneResult, JevError> {
        validate_request(request)?;
        let payload = self.payload(request);

        let mut retry_index = 0u32;
        loop {
            match self.attempt(&payload).await {
                Ok(result) => return Ok(result),
                Err(error) => {
                    if !error.is_retryable() || retry_index >= self.retry_policy.max_retries {
                        return Err(error);
                    }
                    let delay = backoff_ms(self.retry_policy.initial_backoff_ms, retry_index);
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    retry_index += 1;
                }
            }
        }
    }

    /// Serializes the request with the effective model applied (`JevClient.execute`).
    fn payload(&self, request: &SystemOneRequest) -> SystemOneRequest {
        let effective_model = match request.model.as_deref() {
            Some(model) if !model.trim().is_empty() => model.to_string(),
            _ => self.model.clone(),
        };
        SystemOneRequest {
            state: request.state.clone(),
            model: Some(effective_model),
            questions: request.questions.clone(),
        }
    }

    /// One HTTP attempt, including status mapping, parsing and contract validation.
    async fn attempt(&self, payload: &SystemOneRequest) -> Result<SystemOneResult, JevError> {
        let body = serde_json::to_string(payload).map_err(|error| {
            JevError::protocol(format!("Failed to serialize System One request: {error}"))
        })?;
        let http_request = JudgeHttpRequest::post_json(self.system_one_url(), body)
            .with_header("Authorization", format!("Bearer {}", self.api_key))
            .with_header("Accept", "application/json");

        let response =
            match tokio::time::timeout(self.timeout, self.transport.execute(http_request)).await {
                Err(_) => return Err(self.timeout_error()),
                Ok(Err(error)) => {
                    return Err(JevError::protocol_with_http(
                        "System One transport failed",
                        error.status_code,
                        error.response_body,
                        error.retryable,
                    ))
                }
                Ok(Ok(response)) => response,
            };

        if !response.is_success() {
            return Err(http_error(&response));
        }

        // Java's `readValue` yields `null` for a literal `null` body and then rejects it with a
        // dedicated message, so parse into `Option` first to keep the same taxonomy.
        let parsed: Option<SystemOneResult> =
            serde_json::from_str(&response.body).map_err(|_| unparseable_response(&response))?;
        let Some(result) = parsed else {
            return Err(JevError::protocol_with_http(
                "System One response must not be null",
                Some(response.status_code),
                Some(response.body.clone()),
                false,
            ));
        };

        validate_result(payload, &result)?;
        Ok(result)
    }

    fn timeout_error(&self) -> JevError {
        JevError::protocol_with_http(
            format!(
                "System One request timed out after {}ms",
                self.timeout.as_millis()
            ),
            None,
            None,
            true,
        )
    }
}

/// Maps a non-2xx response onto a [`JevError`] (`JevClient.httpException`).
fn http_error(response: &JudgeHttpResponse) -> JevError {
    let message = error_message(Some(response.body.as_str()))
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| {
            format!(
                "System One request failed with HTTP {}",
                response.status_code
            )
        });
    JevError::protocol_with_http(
        message,
        Some(response.status_code),
        Some(response.body.clone()),
        is_retryable_status(response.status_code),
    )
}

/// The 2xx-but-unparseable case (`JevClient.execute`'s `JsonProcessingException` branch).
fn unparseable_response(response: &JudgeHttpResponse) -> JevError {
    JevError::protocol_with_http(
        "Failed to parse System One response",
        Some(response.status_code),
        Some(response.body.clone()),
        false,
    )
}

/// Builder for [`JevClient`] (`JevClient.Builder`).
#[derive(Clone)]
pub struct JevClientBuilder {
    api_key: Option<String>,
    base_url: String,
    model: String,
    transport: Option<Arc<dyn JudgeHttpTransport>>,
    retry_policy: RetryPolicy,
    timeout: Duration,
}

impl Default for JevClientBuilder {
    fn default() -> Self {
        Self {
            api_key: api_key_from_env(),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            transport: None,
            retry_policy: RetryPolicy::defaults(),
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MILLIS),
        }
    }
}

impl JevClientBuilder {
    /// Sets the API key (defaults to `TYPESAFE_API_KEY`, then `JEV_API_KEY`).
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Sets the base URL (defaults to [`DEFAULT_BASE_URL`]).
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Sets the default model (defaults to [`DEFAULT_MODEL`]).
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Injects the HTTP transport (the production default is [`ReqwestJudgeTransport`]).
    pub fn transport(mut self, transport: Arc<dyn JudgeHttpTransport>) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Sets the retry policy (defaults to [`RetryPolicy::defaults`]).
    pub fn retry_policy(mut self, retry_policy: RetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self
    }

    /// Sets the per-attempt timeout (defaults to [`DEFAULT_TIMEOUT_MILLIS`]).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Validates the configuration and builds the client (`JevClient.validateClient`).
    pub fn build(self) -> Result<JevClient, JevError> {
        let api_key = self
            .api_key
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| JevError::invalid("apiKey must not be blank"))?;
        let base_url = normalize_base_url(&self.base_url)?;
        if self.model.trim().is_empty() {
            return Err(JevError::invalid("model must not be blank"));
        }
        if self.timeout.is_zero() {
            return Err(JevError::invalid("timeout must be positive"));
        }
        let transport = match self.transport {
            Some(transport) => transport,
            None => Arc::new(ReqwestJudgeTransport::new().map_err(|error| {
                JevError::protocol(format!("System One transport failed: {}", error.message))
            })?),
        };
        Ok(JevClient {
            api_key,
            base_url,
            model: self.model,
            transport,
            retry_policy: self.retry_policy,
            timeout: self.timeout,
        })
    }
}

/// Trims, strips trailing slashes and validates the base URL (`JevClient.normalizeBaseUrl` +
/// the `validateClient` URI checks).
///
/// Unlike the Decisions client this keeps Java's laxity: credentials, query and fragment are
/// not rejected.
fn normalize_base_url(value: &str) -> Result<String, JevError> {
    let mut normalized = value.trim();
    while normalized.len() > 1 && normalized.ends_with('/') {
        normalized = &normalized[..normalized.len() - 1];
    }
    let url = reqwest::Url::parse(normalized)
        .map_err(|_| JevError::invalid("baseUrl must be a valid URI"))?;
    let scheme = url.scheme();
    let absolute_http = scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
    if !absolute_http || url.host_str().map(str::is_empty).unwrap_or(true) {
        return Err(JevError::invalid("baseUrl must be an absolute http(s) URL"));
    }
    Ok(normalized.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::http::testing::{HangingTransport, ScriptedTransport};
    use crate::http::JudgeTransportError;
    use crate::jev::dto::{
        Answer, ChoiceAnswer, ChoiceQuestion, JsonMap, NoulAnswer, NoulCriteria, NoulQuestion,
        Question, Usage,
    };
    use crate::jev::validate_response;

    fn request() -> SystemOneRequest {
        let mut criteria = JsonMap::new();
        criteria.insert("billing".to_string(), serde_json::json!("Payments"));
        criteria.insert("technical".to_string(), serde_json::json!("Bugs"));
        SystemOneRequest::builder()
            .state(serde_json::json!({ "message": "My payouts are failing" }))
            .question(
                "team",
                Question::Choice(ChoiceQuestion::new(
                    "Which team should handle this?",
                    criteria,
                )),
            )
            .build()
    }

    fn success_body() -> String {
        serde_json::json!({
            "model": "jev-1.13.0",
            "answers": {
                "team": {
                    "type": "choice",
                    "choice": "technical",
                    "probabilities": { "billing": 0.2, "technical": 0.8 },
                    "confidence": 0.77
                }
            },
            "usage": { "input_tokens": 100, "output_tokens": 20 }
        })
        .to_string()
    }

    fn transport_with(
        responses: Vec<Result<JudgeHttpResponse, JudgeTransportError>>,
    ) -> Arc<ScriptedTransport> {
        Arc::new(ScriptedTransport::new(responses))
    }

    fn client(transport: Arc<dyn JudgeHttpTransport>) -> JevClient {
        JevClient::builder()
            .api_key("test-key")
            .base_url("http://typesafe.test")
            .transport(transport)
            .retry_policy(RetryPolicy::new(2, 1).unwrap())
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn sends_the_validated_request_and_parses_the_response() {
        let transport = transport_with(vec![Ok(JudgeHttpResponse::new(200, success_body()))]);
        let result = client(transport.clone())
            .system_one(&request())
            .await
            .unwrap();

        let sent = transport.last_request();
        assert_eq!(sent.url, "http://typesafe.test/v1/systemone");
        assert_eq!(sent.header("Authorization"), Some("Bearer test-key"));
        assert_eq!(sent.header("Content-Type"), Some("application/json"));
        assert_eq!(sent.header("Accept"), Some("application/json"));

        let body: serde_json::Value = serde_json::from_str(&sent.body).unwrap();
        assert_eq!(body["state"]["message"], "My payouts are failing");
        assert_eq!(body["model"], "jev-latest");
        assert_eq!(body["questions"]["team"]["type"], "choice");

        assert_eq!(result.model.as_deref(), Some("jev-1.13.0"));
        assert_eq!(result.usage, Some(Usage::new(100, 20)));
        assert_eq!(
            result.answers.as_ref().unwrap()["team"],
            Answer::Choice(ChoiceAnswer {
                choice: Some("technical".to_string()),
                probabilities: Some(BTreeMap::from([
                    ("billing".to_string(), 0.2),
                    ("technical".to_string(), 0.8),
                ])),
                confidence: Some(0.77),
            })
        );
    }

    #[tokio::test]
    async fn a_request_model_overrides_the_configured_default() {
        let transport = transport_with(vec![Ok(JudgeHttpResponse::new(200, success_body()))]);
        let mut overridden = request();
        overridden.model = Some("jev-1.14.0".to_string());
        client(transport.clone())
            .system_one(&overridden)
            .await
            .unwrap();

        let body: serde_json::Value = serde_json::from_str(&transport.last_request().body).unwrap();
        assert_eq!(body["model"], "jev-1.14.0");
    }

    #[tokio::test]
    async fn a_blank_request_model_falls_back_to_the_configured_default() {
        let transport = transport_with(vec![Ok(JudgeHttpResponse::new(200, success_body()))]);
        let mut blank = request();
        blank.model = Some("   ".to_string());
        client(transport.clone()).system_one(&blank).await.unwrap();

        let body: serde_json::Value = serde_json::from_str(&transport.last_request().body).unwrap();
        assert_eq!(body["model"], "jev-latest");
    }

    #[tokio::test]
    async fn maps_a_non_retryable_http_error_and_sends_it_once() {
        let transport = transport_with(vec![Ok(JudgeHttpResponse::new(
            401,
            "{\"message\":\"invalid key\"}",
        ))]);
        let error = client(transport.clone())
            .system_one(&request())
            .await
            .unwrap_err();

        assert_eq!(error.message(), "invalid key");
        assert_eq!(error.status_code(), Some(401));
        assert_eq!(error.response_body(), Some("{\"message\":\"invalid key\"}"));
        assert!(!error.is_retryable());
        assert_eq!(transport.call_count(), 1);
    }

    #[tokio::test]
    async fn falls_back_to_the_status_message_when_the_body_has_no_known_field() {
        let transport = transport_with(vec![Ok(JudgeHttpResponse::new(400, "{\"other\":1}"))]);
        let error = client(transport).system_one(&request()).await.unwrap_err();
        assert_eq!(error.message(), "System One request failed with HTTP 400");
        assert!(!error.is_retryable());
    }

    #[tokio::test]
    async fn retries_a_rate_limited_call_then_succeeds() {
        let transport = transport_with(vec![
            Ok(JudgeHttpResponse::new(429, "{\"message\":\"slow down\"}")),
            Ok(JudgeHttpResponse::new(200, success_body())),
        ]);
        let result = client(transport.clone())
            .system_one(&request())
            .await
            .unwrap();
        assert!(result.answers.is_some());
        assert_eq!(transport.call_count(), 2);
    }

    #[tokio::test]
    async fn retries_overloaded_until_the_retry_budget_is_exhausted() {
        let transport = Arc::new(ScriptedTransport::repeating(Ok(JudgeHttpResponse::new(
            529,
            "{\"message\":\"overloaded\"}",
        ))));
        let error = client(transport.clone())
            .system_one(&request())
            .await
            .unwrap_err();

        assert!(error.message().contains("overloaded"));
        assert!(error.is_retryable());
        assert_eq!(transport.call_count(), 3, "first attempt plus two retries");
    }

    #[tokio::test]
    async fn retries_gateway_timeouts_and_conflicts() {
        for status in [408u16, 409] {
            let transport = transport_with(vec![
                Ok(JudgeHttpResponse::new(status, "")),
                Ok(JudgeHttpResponse::new(200, success_body())),
            ]);
            client(transport.clone())
                .system_one(&request())
                .await
                .unwrap();
            assert_eq!(transport.call_count(), 2, "status {status} should retry");
        }
    }

    #[tokio::test]
    async fn does_not_retry_client_errors() {
        for status in [400u16, 401, 403, 404, 422, 428] {
            let transport = Arc::new(ScriptedTransport::repeating(Ok(JudgeHttpResponse::new(
                status, "",
            ))));
            assert!(client(transport.clone())
                .system_one(&request())
                .await
                .is_err());
            assert_eq!(transport.call_count(), 1, "status {status} must not retry");
        }
    }

    #[tokio::test]
    async fn forwards_transport_failures_with_their_retry_hint() {
        let transport = Arc::new(ScriptedTransport::repeating(Err(JudgeTransportError::new(
            "connection reset",
            Some(502),
            Some("gateway".to_string()),
            true,
        ))));
        let error = client(transport.clone())
            .system_one(&request())
            .await
            .unwrap_err();

        assert_eq!(error.message(), "System One transport failed");
        assert_eq!(error.status_code(), Some(502));
        assert_eq!(error.response_body(), Some("gateway"));
        assert!(error.is_retryable());
        assert_eq!(transport.call_count(), 3);
    }

    #[tokio::test]
    async fn times_out_a_slow_transport() {
        let client = JevClient::builder()
            .api_key("test-key")
            .base_url("http://typesafe.test")
            .transport(Arc::new(HangingTransport))
            .retry_policy(RetryPolicy::new(0, 1).unwrap())
            .timeout(Duration::from_millis(10))
            .build()
            .unwrap();
        let error = client.system_one(&request()).await.unwrap_err();
        assert_eq!(error.message(), "System One request timed out after 10ms");
        assert!(error.is_retryable());
        assert_eq!(error.status_code(), None);
    }

    #[tokio::test]
    async fn rejects_an_invalid_request_before_touching_the_transport() {
        let transport = transport_with(vec![]);
        let error = client(transport.clone())
            .system_one(&SystemOneRequest::builder().build())
            .await
            .unwrap_err();
        assert!(error.is_invalid_argument());
        assert_eq!(error.message(), "state must not be null");
        assert_eq!(transport.call_count(), 0);
    }

    #[tokio::test]
    async fn rejects_a_literal_null_response() {
        let transport = transport_with(vec![Ok(JudgeHttpResponse::new(200, "null"))]);
        let error = client(transport.clone())
            .system_one(&request())
            .await
            .unwrap_err();
        assert_eq!(error.message(), "System One response must not be null");
        assert_eq!(error.status_code(), Some(200));
        assert!(!error.is_retryable());
        assert_eq!(transport.call_count(), 1);
    }

    #[tokio::test]
    async fn rejects_an_unparseable_response_without_retrying() {
        let transport = transport_with(vec![Ok(JudgeHttpResponse::new(200, "{private"))]);
        let error = client(transport.clone())
            .system_one(&request())
            .await
            .unwrap_err();
        assert_eq!(error.message(), "Failed to parse System One response");
        assert_eq!(error.response_body(), Some("{private"));
        assert!(!error.is_retryable());
        assert_eq!(transport.call_count(), 1);
    }

    #[tokio::test]
    async fn validates_the_response_contract_without_retrying() {
        let body = serde_json::json!({
            "answers": { "team": { "type": "choice", "choice": "billing" } },
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        })
        .to_string();
        let transport = transport_with(vec![Ok(JudgeHttpResponse::new(200, body))]);
        let error = client(transport.clone())
            .system_one(&request())
            .await
            .unwrap_err();
        assert_eq!(
            error.message(),
            "System One response model must not be blank"
        );
        assert!(!error.is_retryable());
        assert_eq!(transport.call_count(), 1);
    }

    #[test]
    fn retryable_statuses_are_the_java_set_plus_gateway_timeouts() {
        for status in [408u16, 409, 429, 529, 500, 503, 599] {
            assert!(is_retryable_status(status), "status {status}");
        }
        for status in [200u16, 400, 401, 403, 404, 422, 428] {
            assert!(!is_retryable_status(status), "status {status}");
        }
    }

    #[test]
    fn builder_rejects_invalid_configuration() {
        let build = |configure: &dyn Fn(JevClientBuilder) -> JevClientBuilder| {
            configure(JevClient::builder()).build().unwrap_err()
        };

        assert_eq!(
            build(&|builder| builder.api_key("")).message(),
            "apiKey must not be blank"
        );
        assert_eq!(
            build(&|builder| builder.api_key("k").base_url("not-a-url")).message(),
            "baseUrl must be a valid URI"
        );
        assert_eq!(
            build(&|builder| builder.api_key("k").base_url("file:///tmp/x")).message(),
            "baseUrl must be an absolute http(s) URL"
        );
        assert_eq!(
            build(&|builder| builder.api_key("k").model(" ")).message(),
            "model must not be blank"
        );
        assert_eq!(
            build(&|builder| builder.api_key("k").timeout(Duration::ZERO)).message(),
            "timeout must be positive"
        );
    }

    #[test]
    fn base_url_normalization_matches_java() {
        let build = |base_url: &str| {
            JevClient::builder()
                .api_key("k")
                .base_url(base_url)
                .transport(Arc::new(ScriptedTransport::new(vec![])))
                .build()
                .unwrap()
        };
        assert_eq!(
            build("http://typesafe.test/").base_url(),
            "http://typesafe.test"
        );
        assert_eq!(
            build("  http://typesafe.test/v1/  ").system_one_url(),
            "http://typesafe.test/v1/systemone"
        );
        assert_eq!(
            build("http://typesafe.test/v1/systemone").system_one_url(),
            "http://typesafe.test/v1/systemone"
        );
        assert_eq!(build("http://typesafe.test").model(), "jev-latest");
        assert_eq!(
            build("http://typesafe.test").timeout(),
            Duration::from_millis(5_000)
        );
    }

    #[test]
    fn debug_hides_the_api_key() {
        let client = client(Arc::new(ScriptedTransport::new(vec![])));
        let rendered = format!("{client:?}");
        assert!(rendered.contains("http://typesafe.test"));
        assert!(!rendered.contains("test-key"));
    }

    #[test]
    fn validate_response_still_rejects_negative_usage() {
        // Guard rail for the shared evaluation-backend entry point
        // (`JevClient.validateResponse`): the HTTP path uses `validate_result`, which (like
        // Java) leaves usage signs to `validate_response`.
        let result = SystemOneResult {
            model: Some("jev-1.13.0".to_string()),
            answers: Some(BTreeMap::from([(
                "urgent".to_string(),
                Answer::Noul(NoulAnswer { noul: Some(0.5) }),
            )])),
            usage: Some(Usage::new(-1, 0)),
        };
        let request = SystemOneRequest::builder()
            .state(serde_json::json!({ "a": 1 }))
            .question(
                "urgent",
                Question::Noul(NoulQuestion::new(
                    "Is it urgent?",
                    Some(NoulCriteria::new("yes", "no")),
                )),
            )
            .build();
        // `validate_result` accepts it (Java parity)...
        assert!(crate::jev::validate_result(&request, &result).is_ok());
        // ...while `validate_response` rejects it.
        assert_eq!(
            validate_response(&request, &result).unwrap_err().message(),
            "System One usage must not be negative"
        );
    }
}
