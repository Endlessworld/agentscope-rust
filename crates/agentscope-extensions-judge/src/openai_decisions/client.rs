//! `OpenAIDecisionsClient` — the Decisions HTTP call, mirroring Java
//! `io.agentscope.extensions.judge.openaidecisions.OpenAIDecisionsClient`.
//!
//! Java wraps the official OpenAI SDK (blocking call on `boundedElastic`, SDK retries disabled,
//! wrapper retries opt-in). The port keeps the same observable behaviour over the shared
//! [`JudgeHttpTransport`](crate::http::JudgeHttpTransport) seam:
//!
//! * `POST {base}/decisions` with `Authorization: Bearer <key>` and a `type`-tagged question
//!   array ([`decision_body`](super::protocol::decision_body)),
//! * one attempt per call by default ([`DecisionsRetryPolicy::disabled`]), opt-in exponential
//!   backoff otherwise,
//! * a per-call timeout, then sanitized errors (the provider body never reaches the caller),
//! * every answer re-validated against its question before it is returned.

use std::sync::Arc;
use std::time::Duration;

use crate::http::{
    backoff_ms, JudgeHttpRequest, JudgeHttpTransport, ReqwestJudgeTransport, JSON_CONTENT_TYPE,
};

use super::dto::{DecisionRequest, DecisionResult};
use super::error::DecisionsError;
use super::protocol::{decision_body, parse_decision_response};
use super::{
    DECISIONS_ENDPOINT, DEFAULT_BASE_URL, DEFAULT_INITIAL_BACKOFF_MILLIS, DEFAULT_MAX_RETRIES,
    DEFAULT_MODEL, DEFAULT_TIMEOUT_MILLIS, OPENAI_API_KEY_ENV_VAR,
};

/// Statuses [`OpenAIDecisionsClient`] retries (`429` or `5xx`, exactly like Java).
///
/// Unlike [`crate::jev::is_retryable_status`] this deliberately excludes `408`/`409`: Java's
/// suite asserts `retryable == (status == 429 || status >= 500)` for this client.
#[must_use]
pub fn is_retryable_status(status_code: u16) -> bool {
    status_code == 429 || status_code >= 500
}

/// Bounded retries for transient Decisions failures, in addition to the first attempt
/// (`OpenAIDecisionsRetryPolicy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecisionsRetryPolicy {
    /// Maximum retries after the first attempt.
    pub max_retries: u32,
    /// Initial backoff before the first retry.
    pub initial_backoff_ms: u64,
}

impl DecisionsRetryPolicy {
    /// Creates a policy, rejecting a zero backoff
    /// (`require maxRetries >= 0 and positive initialBackoff`).
    pub fn new(max_retries: u32, initial_backoff_ms: u64) -> Result<Self, DecisionsError> {
        if initial_backoff_ms == 0 {
            return Err(DecisionsError::invalid(
                "require maxRetries >= 0 and positive initialBackoff",
            ));
        }
        Ok(Self {
            max_retries,
            initial_backoff_ms,
        })
    }

    /// `OpenAIDecisionsRetryPolicy.defaults()`: 2 retries after 500ms.
    pub fn defaults() -> Self {
        Self {
            max_retries: DEFAULT_MAX_RETRIES,
            initial_backoff_ms: DEFAULT_INITIAL_BACKOFF_MILLIS,
        }
    }

    /// `OpenAIDecisionsRetryPolicy.disabled()`: a single attempt, the client default.
    pub fn disabled() -> Self {
        Self {
            max_retries: 0,
            initial_backoff_ms: DEFAULT_INITIAL_BACKOFF_MILLIS,
        }
    }

    /// The maximum backoff (Java caps at `initialBackoff * 8`).
    pub fn max_backoff_ms(&self) -> u64 {
        self.initial_backoff_ms.saturating_mul(8)
    }
}

impl Default for DecisionsRetryPolicy {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Trims, validates and normalizes the API base URL (`OpenAIDecisionsClient.apiBase`).
///
/// Trailing slashes are removed, a trailing `/decisions` is removed (so a full endpoint yields
/// the same URL), and credentials, query and fragment are rejected because they would leak into
/// the URL the caller logs.
pub fn normalize_decision_base_url(value: &str) -> Result<String, DecisionsError> {
    let mut base_url = value.trim();
    while base_url.ends_with('/') {
        base_url = &base_url[..base_url.len() - 1];
    }
    let url =
        reqwest::Url::parse(base_url).map_err(|_| DecisionsError::invalid("invalid baseUrl"))?;
    let scheme = url.scheme();
    let absolute_http = scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
    if !absolute_http
        || url.host_str().map(str::is_empty).unwrap_or(true)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(DecisionsError::invalid(
            "baseUrl must be an absolute http(s) URL without credentials, query or fragment",
        ));
    }
    let normalized = base_url
        .strip_suffix(DECISIONS_ENDPOINT)
        .unwrap_or(base_url);
    Ok(normalized.to_string())
}

/// Client for the provider's Decisions endpoint.
#[derive(Clone)]
pub struct OpenAIDecisionsClient {
    transport: Arc<dyn JudgeHttpTransport>,
    api_key: Option<String>,
    base_url: String,
    model: String,
    timeout: Duration,
    retry_policy: DecisionsRetryPolicy,
}

impl std::fmt::Debug for OpenAIDecisionsClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAIDecisionsClient")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("timeout", &self.timeout)
            .field("retry_policy", &self.retry_policy)
            .finish_non_exhaustive()
    }
}

impl OpenAIDecisionsClient {
    /// Starts building a client (`OpenAIDecisionsClient.builder()`).
    pub fn builder() -> OpenAIDecisionsClientBuilder {
        OpenAIDecisionsClientBuilder::default()
    }

    /// The normalized base URL.
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
    pub fn retry_policy(&self) -> DecisionsRetryPolicy {
        self.retry_policy
    }

    /// The endpoint derived from the base URL (`{base}/decisions`).
    pub fn decisions_url(&self) -> String {
        format!("{}{DECISIONS_ENDPOINT}", self.base_url)
    }

    /// Sends one request and returns the validated result (`OpenAIDecisionsClient.decide`).
    pub async fn decide(
        &self,
        request: &DecisionRequest,
    ) -> Result<DecisionResult, DecisionsError> {
        let model = request.model().unwrap_or(&self.model);
        let body = serde_json::to_string(&decision_body(request, model)).map_err(|error| {
            DecisionsError::invalid(format!("Unable to encode the request: {error}"))
        })?;

        let mut http_request = JudgeHttpRequest::post_json(self.decisions_url(), body)
            .with_header("Content-Type", JSON_CONTENT_TYPE);
        if let Some(api_key) = &self.api_key {
            http_request = http_request.with_header("Authorization", format!("Bearer {api_key}"));
        }

        let mut retry_index = 0u32;
        loop {
            match self.attempt(http_request.clone(), request).await {
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

    async fn attempt(
        &self,
        http_request: JudgeHttpRequest,
        request: &DecisionRequest,
    ) -> Result<DecisionResult, DecisionsError> {
        let response =
            match tokio::time::timeout(self.timeout, self.transport.execute(http_request)).await {
                Err(_) => {
                    return Err(DecisionsError::api(
                        "Decisions request timed out",
                        None,
                        true,
                    ))
                }
                // Sanitized on purpose: the SDK error text can embed the provider body.
                Ok(Err(error)) => {
                    return Err(DecisionsError::api(
                        "Decisions request failed",
                        error.status_code,
                        error.retryable,
                    ))
                }
                Ok(Ok(response)) => response,
            };

        if !response.is_success() {
            return Err(DecisionsError::api(
                format!(
                    "Decisions request failed with HTTP {}",
                    response.status_code
                ),
                Some(response.status_code),
                is_retryable_status(response.status_code),
            ));
        }

        parse_decision_response(request, &response.body)
    }
}

/// Builder for [`OpenAIDecisionsClient`] (`OpenAIDecisionsClient.Builder`).
#[derive(Clone)]
pub struct OpenAIDecisionsClientBuilder {
    api_key: Option<String>,
    base_url: String,
    model: String,
    timeout: Duration,
    retry_policy: DecisionsRetryPolicy,
    transport: Option<Arc<dyn JudgeHttpTransport>>,
}

impl Default for OpenAIDecisionsClientBuilder {
    fn default() -> Self {
        Self {
            api_key: std::env::var(OPENAI_API_KEY_ENV_VAR).ok(),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MILLIS),
            retry_policy: DecisionsRetryPolicy::disabled(),
            transport: None,
        }
    }
}

impl OpenAIDecisionsClientBuilder {
    /// Sets the API key (defaults to `OPENAI_API_KEY`).
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Sets the API base URL, with or without the version prefix or the `/decisions` suffix.
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Sets the default model.
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Sets the per-attempt timeout (defaults to 30s).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the retry policy (defaults to [`DecisionsRetryPolicy::disabled`]).
    pub fn retry_policy(mut self, retry_policy: DecisionsRetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self
    }

    /// Injects the transport; Java's analogue borrows an SDK client whose credentials win.
    ///
    /// With an injected transport the API key is optional: no `Authorization` header is sent
    /// when it is absent.
    pub fn transport(mut self, transport: Arc<dyn JudgeHttpTransport>) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Validates the configuration and builds the client
    /// (`OpenAIDecisionsClient`'s constructor).
    pub fn build(self) -> Result<OpenAIDecisionsClient, DecisionsError> {
        if self.model.trim().is_empty() {
            return Err(DecisionsError::invalid("model must not be blank"));
        }
        if self.timeout.is_zero() {
            return Err(DecisionsError::invalid("timeout must be positive"));
        }
        let base_url = normalize_decision_base_url(&self.base_url)?;
        let api_key = match self.api_key {
            Some(api_key) if api_key.trim().is_empty() => {
                return Err(DecisionsError::invalid("apiKey must be a nonblank value"))
            }
            other => other,
        };
        let transport = match self.transport {
            Some(transport) => transport,
            None => {
                if api_key.is_none() {
                    return Err(DecisionsError::invalid("apiKey must be a nonblank value"));
                }
                Arc::new(ReqwestJudgeTransport::new().map_err(|error| {
                    DecisionsError::api(error.message, error.status_code, error.retryable)
                })?)
            }
        };
        Ok(OpenAIDecisionsClient {
            transport,
            api_key,
            base_url,
            model: self.model,
            timeout: self.timeout,
            retry_policy: self.retry_policy,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::testing::{HangingTransport, ScriptedTransport};
    use crate::http::{JudgeHttpResponse, JudgeTransportError};
    use crate::openai_decisions::dto::{
        DecisionInput, DecisionLevel, DecisionOption, DecisionQuestion, DecisionValue,
    };
    use crate::openai_decisions::protocol::{ANSWER_COUNT_MESSAGE, PARSE_MESSAGE, SUM_MESSAGE};
    use crate::openai_decisions::DecisionAnswer;

    const SUCCESS: &str = r#"{"model":"fixture-model", "answers":[
      {"name":"severity", "type":"score", "score":0.8, "confidence":0.6,
       "probabilities":[{"value":1,"label":"blocked","probability":0.8},
                        {"value":0,"label":"minor","probability":0.2}]},
      {"name":"damaged", "type":"predicate", "probability":0},
      {"name":"route", "type":"choice", "choice":true, "confidence":0.7,
       "probabilities":[{"value":true,"probability":0.9},{"value":"true","probability":0.1}]}
    ], "usage":{"input_tokens":12,"total_tokens":12}}"#;

    fn request() -> DecisionRequest {
        DecisionRequest::builder()
            .input_text("The screen is broken")
            .question(
                "damaged",
                DecisionQuestion::predicate("Is the item damaged?"),
            )
            .question(
                "route",
                DecisionQuestion::choice(
                    "Select",
                    vec![
                        DecisionOption::text("true"),
                        DecisionOption::new(true, Some("Needs repair".to_string())),
                    ],
                )
                .unwrap(),
            )
            .question(
                "severity",
                DecisionQuestion::score(
                    "Rate",
                    vec![
                        DecisionLevel::labeled("minor"),
                        DecisionLevel::labeled("blocked"),
                    ],
                )
                .unwrap(),
            )
            .build()
            .unwrap()
    }

    fn client(transport: Arc<dyn JudgeHttpTransport>) -> OpenAIDecisionsClient {
        OpenAIDecisionsClient::builder()
            .api_key("fixture-key")
            .transport(transport)
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap()
    }

    fn http(
        status: u16,
        body: impl Into<String>,
    ) -> Result<JudgeHttpResponse, JudgeTransportError> {
        Ok(JudgeHttpResponse::new(status, body))
    }

    #[tokio::test]
    async fn posts_the_typed_body_and_maps_every_answer() {
        let transport = Arc::new(ScriptedTransport::new(vec![http(200, SUCCESS)]));
        let result = client(transport.clone()).decide(&request()).await.unwrap();

        let sent = transport.last_request();
        assert_eq!(sent.url, "https://api.openai.com/v1/decisions");
        assert_eq!(sent.header("Authorization"), Some("Bearer fixture-key"));
        assert_eq!(sent.header("Content-Type"), Some(JSON_CONTENT_TYPE));

        let body: serde_json::Value = serde_json::from_str(&sent.body).unwrap();
        assert_eq!(body["input"], "The screen is broken");
        assert_eq!(body["model"], DEFAULT_MODEL);
        assert_eq!(body["questions"][0]["type"], "predicate");
        assert_eq!(body["questions"][1]["choices"][1]["value"], true);
        assert!(body["questions"][1]["choices"][0]
            .get("description")
            .is_none());
        assert_eq!(body["questions"][2]["levels"][0]["label"], "minor");

        assert_eq!(result.model, "fixture-model");
        assert_eq!(result.answers.len(), 3);
        assert_eq!(
            result.answers["damaged"],
            DecisionAnswer::Predicate { probability: 0.0 }
        );
        assert_eq!(
            result.usage.unwrap().input_tokens,
            Some(12),
            "unspecified counters stay absent"
        );
    }

    #[tokio::test]
    async fn honours_a_model_override_and_the_prompt_cache_key() {
        let transport = Arc::new(ScriptedTransport::new(vec![http(200, SUCCESS)]));
        let request = DecisionRequest::builder()
            .input(DecisionInput::text("The screen is broken").unwrap())
            .model("override-model")
            .prompt_cache_key("tenant-42")
            .question(
                "damaged",
                DecisionQuestion::predicate("Is the item damaged?"),
            )
            .question(
                "route",
                DecisionQuestion::choice(
                    "Select",
                    vec![
                        DecisionOption::text("true"),
                        DecisionOption::new(true, Some("Needs repair".to_string())),
                    ],
                )
                .unwrap(),
            )
            .question(
                "severity",
                DecisionQuestion::score(
                    "Rate",
                    vec![
                        DecisionLevel::labeled("minor"),
                        DecisionLevel::labeled("blocked"),
                    ],
                )
                .unwrap(),
            )
            .build()
            .unwrap();
        client(transport.clone()).decide(&request).await.unwrap();

        let body: serde_json::Value = serde_json::from_str(&transport.last_request().body).unwrap();
        assert_eq!(body["model"], "override-model");
        assert_eq!(body["prompt_cache_key"], "tenant-42");
        assert!(body.get("prompt_cache_options").is_none());
    }

    #[test]
    fn base_url_normalization_accepts_version_prefixes_and_full_endpoints() {
        assert_eq!(
            normalize_decision_base_url("http://127.0.0.1:8080/prefix/v1/").unwrap(),
            "http://127.0.0.1:8080/prefix/v1"
        );
        assert_eq!(
            normalize_decision_base_url("http://127.0.0.1:8080/prefix/v1/decisions/").unwrap(),
            "http://127.0.0.1:8080/prefix/v1"
        );
        assert_eq!(
            normalize_decision_base_url("  https://api.openai.com/v1  ").unwrap(),
            "https://api.openai.com/v1"
        );

        let client = OpenAIDecisionsClient::builder()
            .api_key("k")
            .base_url("http://127.0.0.1:1/v1/decisions")
            .transport(Arc::new(ScriptedTransport::new(vec![])))
            .build()
            .unwrap();
        assert_eq!(client.decisions_url(), "http://127.0.0.1:1/v1/decisions");
    }

    #[test]
    fn rejects_unsafe_or_relative_base_urls_without_echoing_secrets() {
        for url in [
            "file:///tmp/a",
            "https://user:secret@example.com/v1",
            "https://example.com/v1?key=secret",
            "https://example.com/v1#secret",
            "invalid",
        ] {
            let error = OpenAIDecisionsClient::builder()
                .api_key("key")
                .base_url(url)
                .build()
                .unwrap_err();
            assert!(error.is_invalid_argument(), "url {url}");
            assert!(!error.message().contains("secret"), "url {url}");
        }
    }

    #[test]
    fn builder_rejects_invalid_configuration() {
        let error = OpenAIDecisionsClient::builder()
            .api_key("")
            .transport(Arc::new(ScriptedTransport::new(vec![])))
            .build()
            .unwrap_err();
        assert_eq!(error.message(), "apiKey must be a nonblank value");

        assert_eq!(
            OpenAIDecisionsClient::builder()
                .api_key("k")
                .model("")
                .build()
                .unwrap_err()
                .message(),
            "model must not be blank"
        );
        assert_eq!(
            OpenAIDecisionsClient::builder()
                .api_key("k")
                .timeout(Duration::ZERO)
                .build()
                .unwrap_err()
                .message(),
            "timeout must be positive"
        );

        // Without an injected transport the key is mandatory.
        let previous = std::env::var(OPENAI_API_KEY_ENV_VAR).ok();
        if previous.is_none() {
            let error = OpenAIDecisionsClient::builder()
                .base_url("https://api.openai.com/v1")
                .build()
                .unwrap_err();
            assert_eq!(error.message(), "apiKey must be a nonblank value");
        }

        let client = OpenAIDecisionsClient::builder()
            .transport(Arc::new(ScriptedTransport::new(vec![])))
            .build()
            .unwrap();
        assert_eq!(client.model(), DEFAULT_MODEL);
        assert_eq!(client.timeout(), Duration::from_millis(30_000));
        assert_eq!(client.retry_policy(), DecisionsRetryPolicy::disabled());
    }

    #[tokio::test]
    async fn an_injected_transport_without_a_key_sends_no_authorization() {
        let transport = Arc::new(ScriptedTransport::new(vec![http(200, SUCCESS)]));
        let client = OpenAIDecisionsClient::builder()
            .transport(transport.clone())
            .build()
            .unwrap();
        client.decide(&request()).await.unwrap();
        assert_eq!(transport.last_request().header("Authorization"), None);
    }

    #[tokio::test]
    async fn exposes_http_status_and_retry_hint_without_retrying_by_default() {
        for status in [400u16, 401, 403, 429, 500, 503] {
            let transport = Arc::new(ScriptedTransport::new(vec![http(
                status,
                "private provider body fixture-secret",
            )]));
            let error = client(transport.clone())
                .decide(&request())
                .await
                .unwrap_err();
            assert_eq!(error.status_code(), Some(status));
            assert_eq!(error.is_retryable(), status == 429 || status >= 500);
            assert_eq!(
                error.message(),
                format!("Decisions request failed with HTTP {status}")
            );
            assert!(!error.message().contains("private"));
            assert_eq!(transport.call_count(), 1);
        }
    }

    #[tokio::test]
    async fn sanitizes_transport_failures() {
        for retryable in [false, true] {
            let transport = Arc::new(ScriptedTransport::repeating(Err(JudgeTransportError::new(
                "fixture-secret",
                None,
                None,
                retryable,
            ))));
            let error = client(transport.clone())
                .decide(&request())
                .await
                .unwrap_err();
            assert_eq!(error.message(), "Decisions request failed");
            assert_eq!(error.is_retryable(), retryable);
            assert!(!error.message().contains("fixture-secret"));
            assert_eq!(error.status_code(), None);
        }
    }

    #[tokio::test]
    async fn times_out_a_slow_transport() {
        let client = OpenAIDecisionsClient::builder()
            .transport(Arc::new(HangingTransport))
            .timeout(Duration::from_millis(10))
            .build()
            .unwrap();
        let error = client.decide(&request()).await.unwrap_err();
        assert_eq!(error.message(), "Decisions request timed out");
        assert!(error.is_retryable());
        assert_eq!(error.status_code(), None);
    }

    #[tokio::test]
    async fn retries_only_when_the_policy_opts_in() {
        let transport = Arc::new(ScriptedTransport::new(vec![
            http(429, ""),
            http(200, SUCCESS),
        ]));
        let client = OpenAIDecisionsClient::builder()
            .transport(transport.clone())
            .retry_policy(DecisionsRetryPolicy::new(1, 1).unwrap())
            .build()
            .unwrap();
        let result = client.decide(&request()).await.unwrap();
        assert_eq!(result.answers.len(), 3);
        assert_eq!(transport.call_count(), 2);

        let transport = Arc::new(ScriptedTransport::repeating(http(500, "")));
        let client = OpenAIDecisionsClient::builder()
            .transport(transport.clone())
            .retry_policy(DecisionsRetryPolicy::defaults())
            .build()
            .unwrap();
        assert_eq!(
            client.decide(&request()).await.unwrap_err().status_code(),
            Some(500)
        );
        assert_eq!(transport.call_count(), 3, "first attempt plus two retries");
    }

    #[tokio::test]
    async fn never_retries_an_invalid_response() {
        let transport = Arc::new(ScriptedTransport::repeating(http(200, "{")));
        let client = OpenAIDecisionsClient::builder()
            .transport(transport.clone())
            .retry_policy(DecisionsRetryPolicy::defaults())
            .build()
            .unwrap();
        let error = client.decide(&request()).await.unwrap_err();
        assert_eq!(error.message(), PARSE_MESSAGE);
        assert!(!error.is_retryable());
        assert_eq!(transport.call_count(), 1);
    }

    #[tokio::test]
    async fn surfaces_out_of_contract_answers() {
        let mut body: serde_json::Value = serde_json::from_str(SUCCESS).unwrap();
        body["answers"].as_array_mut().unwrap().remove(0);
        let transport = Arc::new(ScriptedTransport::new(vec![http(200, body.to_string())]));
        assert_eq!(
            client(transport)
                .decide(&request())
                .await
                .unwrap_err()
                .message(),
            ANSWER_COUNT_MESSAGE
        );

        let mut body: serde_json::Value = serde_json::from_str(SUCCESS).unwrap();
        body["answers"][2]["probabilities"][0]["probability"] = serde_json::json!(0.2);
        let transport = Arc::new(ScriptedTransport::new(vec![http(200, body.to_string())]));
        assert_eq!(
            client(transport)
                .decide(&request())
                .await
                .unwrap_err()
                .message(),
            SUM_MESSAGE
        );
    }

    #[test]
    fn retry_policy_matches_java_constructors() {
        assert_eq!(DecisionsRetryPolicy::defaults().max_retries, 2);
        assert_eq!(DecisionsRetryPolicy::defaults().initial_backoff_ms, 500);
        assert_eq!(DecisionsRetryPolicy::defaults().max_backoff_ms(), 4_000);
        assert_eq!(DecisionsRetryPolicy::disabled().max_retries, 0);
        assert_eq!(
            DecisionsRetryPolicy::default(),
            DecisionsRetryPolicy::disabled()
        );
        assert_eq!(
            DecisionsRetryPolicy::new(1, 0).unwrap_err().message(),
            "require maxRetries >= 0 and positive initialBackoff"
        );
    }

    #[test]
    fn statuses_follow_the_java_predicate() {
        for status in [429u16, 500, 503, 599] {
            assert!(is_retryable_status(status), "status {status}");
        }
        for status in [200u16, 400, 401, 403, 404, 408, 409, 422] {
            assert!(!is_retryable_status(status), "status {status}");
        }
    }

    #[test]
    fn debug_hides_the_api_key() {
        let client = client(Arc::new(ScriptedTransport::new(vec![])));
        let rendered = format!("{client:?}");
        assert!(rendered.contains("https://api.openai.com/v1"));
        assert!(!rendered.contains("fixture-key"));
    }

    #[test]
    fn choice_values_round_trip_as_strings_and_booleans() {
        let question = DecisionQuestion::choice(
            "x",
            vec![DecisionOption::text("a"), DecisionOption::boolean(true)],
        )
        .unwrap();
        let choices = question.choices().unwrap();
        assert_eq!(choices[0].value, DecisionValue::text("a"));
        assert_eq!(choices[1].value, DecisionValue::boolean(true));
    }
}
