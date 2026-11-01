//! Real HTTP [`NacosAiService`] (no official Rust Nacos AI SDK exists).
//!
//! The Java module gets its client from `AiFactory.createAiService`; this crate inverted the
//! dependency into the [`NacosAiService`] trait. Until now only
//! [`crate::service::InMemoryNacosAiService`] implemented it, so embedders had to write the
//! HTTP layer themselves. This module closes that gap:
//!
//! * [`NacosHttpAiService`] implements the whole trait over the Nacos **v1 AI OpenAPI**
//!   (`/v1/ai/agent`, `/v1/ai/agent/endpoint`, `/v1/ai/prompt`, `/v1/ai/skill`);
//! * requests go through the [`NacosHttpTransport`] seam, so everything is testable offline
//!   with a recording/fake transport (no `httpmock` and no network in tests);
//! * [`ReqwestNacosTransport`] is the real client, behind the `http-client` feature, so the
//!   default build stays dependency-free.
//!
//! ## Auth and namespace
//!
//! * **Auth (NACOS-03)**: with `username`/`password` configured the service logs in through
//!   `POST /v1/auth/login`, caches `accessToken` and refreshes it early (90% of `tokenTtl`);
//!   `401`/`403` triggers exactly one re-login and a replay of the original request. A static
//!   `access_token` is used when no credentials are configured, and as the fallback when a
//!   login attempt fails.
//! * **Namespace (NACOS-04/16)**: `NacosHttpConfig::namespace` is required and sent as
//!   `namespaceId` on **every** request (read and write); an empty value means "server default"
//!   and is omitted. [`NacosHttpAiService::for_namespace`] gives a scoped view for another
//!   namespace.
//!
//! ## Endpoint assumptions
//!
//! Nacos instances differ in prefix (`/nacos`) and in how strictly the AI OpenAPI is
//! versioned, so every path is configurable through [`NacosHttpConfig`] while defaulting to
//! the Nacos 3.x layout. Responses are accepted both bare (`{...card...}`) and wrapped in the
//! Nacos envelope (`{"code":0,"data":{...}}`), and a `404` maps to `Ok(None)` for the
//! `get_*` methods (absent) and to [`AiServiceError::not_found`] for the `subscribe_*` ones.
//! An envelope with a non-zero `code` is a business rejection even when the status is `200`
//! (NACOS-11): reads and writes both surface it as [`AiServiceError::Business`].
//!
//! ## Subscriptions
//!
//! Nacos pushes updates over gRPC; this HTTP client cannot hold such a stream, so
//! `subscribe_*` performs the initial fetch, remembers the listener and exposes
//! [`NacosHttpAiService::poll_once`] for change detection. An embedder that needs push-like
//! behaviour drives `poll_once` from its own loop, or lets
//! [`NacosHttpAiService::polling`] (also exposed as `spawn_polling` with the `http-client`
//! feature) do it. [`NacosHttpAiService::unsubscribe_prompt`] /
//! [`NacosHttpAiService::unsubscribe_agent_card`] / [`NacosHttpAiService::close`] prune the
//! subscription table (NACOS-12).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use tracing::warn;

use crate::a2a::model::{NacosAgentCard, NacosAgentEndpoint};
use crate::service::{
    AiServiceError, AiServiceResult, AgentCardListener, NacosAiService, NacosPrompt, PromptListener,
    SkillZip,
};

/// A single HTTP request produced by [`NacosHttpAiService`].
///
/// The URL already carries the query string (`?a=b&c=d`), so a transport does not need to
/// know anything about Nacos parameters and a fake transport can assert on the URL directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub body: Option<Vec<u8>>,
    pub content_type: Option<String>,
}

impl HttpRequest {
    /// Builds a `GET` request.
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: "GET".to_string(),
            url: url.into(),
            body: None,
            content_type: None,
        }
    }

    /// Builds a `POST` request with a JSON body.
    pub fn post_json(url: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            method: "POST".to_string(),
            url: url.into(),
            body: Some(body),
            content_type: Some("application/json".to_string()),
        }
    }
}

/// A minimal HTTP response (status + raw body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Builds a response.
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }

    /// Whether the status is `2xx`.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body decoded as UTF-8 (lossy).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// The HTTP seam used by [`NacosHttpAiService`].
///
/// Implementations only move bytes: parsing, error mapping and subscription bookkeeping live
/// in the service, so the same logic is exercised by the offline tests.
#[async_trait]
pub trait NacosHttpTransport: Send + Sync {
    /// Executes `request` and returns the raw response.
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, String>;
}

/// The Nacos default namespace id, used when an embedder wants the server default.
pub const DEFAULT_NAMESPACE: &str = "public";

/// Token TTL assumed when the login response carries no `tokenTtl` (Nacos default: 5 hours).
pub const DEFAULT_TOKEN_TTL: std::time::Duration = std::time::Duration::from_secs(18_000);

/// Ratio of `tokenTtl` after which the token is refreshed early (NACOS-03).
pub const DEFAULT_TOKEN_REFRESH_RATIO: f64 = 0.9;

/// Total attempts made for a retryable failure (Nacos SDK default: one retry on top of the
/// first try).
pub const DEFAULT_RETRY_MAX_ATTEMPTS: u32 = 3;

/// Backoff before the first retry (doubles on every further attempt).
pub const DEFAULT_RETRY_INITIAL_BACKOFF: std::time::Duration =
    std::time::Duration::from_millis(200);

/// Upper bound of the exponential backoff.
pub const DEFAULT_RETRY_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);

/// Retry policy for **retryable** failures (`Transport`, `429`, `5xx`), mirroring the Nacos
/// SDK, which retries a failed HTTP call a few times before surfacing the error (NACOS-06).
///
/// Only failures classified as retryable by [`AiServiceError::is_retryable`] are retried: a
/// Nacos business code or an undecodable payload is terminal on the first try. Retried calls
/// are safe by construction — every Nacos AI endpoint is keyed by agent/prompt/skill name, so a
/// repeated `releaseAgentCard`/`registerAgentEndpoint` writes the same state again.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryPolicy {
    /// Total attempts, including the first one (`1` disables retrying).
    pub max_attempts: u32,
    /// Backoff before the first retry.
    pub initial_backoff: std::time::Duration,
    /// Cap applied to the exponential growth.
    pub max_backoff: std::time::Duration,
    /// Growth factor applied per attempt.
    pub multiplier: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_RETRY_MAX_ATTEMPTS,
            initial_backoff: DEFAULT_RETRY_INITIAL_BACKOFF,
            max_backoff: DEFAULT_RETRY_MAX_BACKOFF,
            multiplier: 2.0,
        }
    }
}

impl RetryPolicy {
    /// A policy that never retries.
    pub const fn none() -> Self {
        Self {
            max_attempts: 1,
            initial_backoff: std::time::Duration::ZERO,
            max_backoff: std::time::Duration::ZERO,
            multiplier: 1.0,
        }
    }

    /// Builds a policy with exponential backoff (factor `2.0`).
    pub fn new(
        max_attempts: u32,
        initial_backoff: std::time::Duration,
        max_backoff: std::time::Duration,
    ) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            initial_backoff,
            max_backoff,
            multiplier: 2.0,
        }
    }

    /// The backoff to wait **after** `attempt` (1-based) failed.
    ///
    /// Kept as a pure function so the schedule is unit-testable without sleeping.
    pub fn backoff(&self, attempt: u32) -> std::time::Duration {
        if attempt == 0 {
            return std::time::Duration::ZERO;
        }
        let multiplier = if self.multiplier.is_finite() && self.multiplier >= 1.0 {
            self.multiplier
        } else {
            1.0
        };
        let factor = multiplier.powi(attempt.saturating_sub(1).min(32) as i32);
        let backoff =
            std::time::Duration::from_secs_f64(self.initial_backoff.as_secs_f64() * factor);
        backoff.min(self.max_backoff)
    }

    /// Whether another attempt is allowed after `attempt` (1-based) already failed.
    pub fn should_retry(&self, attempt: u32) -> bool {
        attempt < self.max_attempts.max(1)
    }
}

/// Configuration for [`NacosHttpAiService`].
#[derive(Debug, Clone, PartialEq)]
pub struct NacosHttpConfig {
    /// Base URL including the context path, e.g. `http://127.0.0.1:8848/nacos`.
    pub base_url: String,
    /// A static Nacos `accessToken` (mirrors `Properties(accessToken)`).
    ///
    /// Used when no `username`/`password` credentials are configured, and as the fallback
    /// value when a login attempt fails (NACOS-03).
    pub access_token: Option<String>,
    /// Nacos username used to log in through [`Self::auth_path`] (NACOS-03).
    pub username: Option<String>,
    /// Nacos password used to log in through [`Self::auth_path`] (NACOS-03).
    pub password: Option<String>,
    /// Login path, defaulting to `/v1/auth/login`.
    pub auth_path: String,
    /// Ratio of `tokenTtl` after which the token is refreshed early (default
    /// [`DEFAULT_TOKEN_REFRESH_RATIO`]: 90%).
    pub token_refresh_ratio: f64,
    /// Nacos namespace (`namespaceId`).
    ///
    /// **Configuration-level and required** (NACOS-16 ruling A): the service is bound to one
    /// namespace, like a Java `AiService` built from `Properties(namespace)`, and every request
    /// carries it. An empty value means "use the server default namespace" and is omitted from
    /// the query string. Per-call override: [`NacosHttpAiService::for_namespace`].
    pub namespace: String,
    /// AgentCard path.
    pub agent_path: String,
    /// Agent endpoint path.
    pub agent_endpoint_path: String,
    /// Prompt path.
    pub prompt_path: String,
    /// Skill ZIP path.
    pub skill_path: String,
    /// Retry policy for retryable failures (NACOS-06); defaults to
    /// [`RetryPolicy::default`] (`3` attempts, `200ms` doubling, capped at `2s`).
    pub retry: RetryPolicy,
}

impl NacosHttpConfig {
    /// Configuration for `namespace` with the Nacos 3.x default paths.
    ///
    /// `namespace` is required; pass [`DEFAULT_NAMESPACE`] for the Nacos default namespace.
    pub fn new(base_url: impl Into<String>, namespace: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            access_token: None,
            username: None,
            password: None,
            auth_path: "/v1/auth/login".to_string(),
            token_refresh_ratio: DEFAULT_TOKEN_REFRESH_RATIO,
            namespace: namespace.into(),
            agent_path: "/v1/ai/agent".to_string(),
            agent_endpoint_path: "/v1/ai/agent/endpoint".to_string(),
            prompt_path: "/v1/ai/prompt".to_string(),
            skill_path: "/v1/ai/skill".to_string(),
            retry: RetryPolicy::default(),
        }
    }

    /// Sets the static `accessToken` query parameter.
    pub fn with_access_token(mut self, token: impl Into<String>) -> Self {
        self.access_token = Some(token.into());
        self
    }

    /// Configures the `username`/`password` pair used to log in and refresh the token.
    ///
    /// Mirrors the Nacos SDK, which logs in when the properties carry credentials.
    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }

    /// Sets the namespace (overriding the one given to [`Self::new`]).
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = namespace.into();
        self
    }

    /// Sets the early-refresh ratio applied to `tokenTtl` (clamped to `0.0..=1.0`).
    pub fn with_token_refresh_ratio(mut self, ratio: f64) -> Self {
        self.token_refresh_ratio = ratio;
        self
    }

    /// Sets the login path.
    pub fn with_auth_path(mut self, path: impl Into<String>) -> Self {
        self.auth_path = path.into();
        self
    }

    /// Overrides the retry policy applied to retryable failures (NACOS-06).
    ///
    /// Use [`RetryPolicy::none`] to fail on the first attempt.
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Whether a login can be attempted (both credential fields are set).
    pub fn has_credentials(&self) -> bool {
        self.username.is_some() && self.password.is_some()
    }
}

/// How long a freshly issued token is reused before it is refreshed early.
///
/// Kept as a free function so the 90% policy is unit-testable without sleeping.
fn refresh_deadline(ttl: std::time::Duration, ratio: f64) -> std::time::Duration {
    let ratio = if ratio.is_finite() {
        ratio.clamp(0.0, 1.0)
    } else {
        DEFAULT_TOKEN_REFRESH_RATIO
    };
    ttl.mul_f64(ratio)
}

/// Upper bound of the polling backoff, mirroring the model-call retry cap.
pub const MAX_POLLING_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);

/// The delay before the next [`NacosHttpAiService::polling`] tick after `failures` consecutive
/// failures (NACOS-06).
///
/// A healthy service polls every `interval`; a failing one backs off exponentially and is capped
/// at [`MAX_POLLING_BACKOFF`], so a broken connection does not hammer the server. Kept as a pure
/// function so the schedule is unit-testable without sleeping.
pub fn polling_backoff(
    interval: std::time::Duration,
    failures: u32,
) -> std::time::Duration {
    crate::service::exponential_backoff(interval, failures, MAX_POLLING_BACKOFF)
}

/// Cached credential-based access token (NACOS-03).
#[derive(Default)]
struct AuthState {
    token: Option<String>,
    /// Instant after which the token is refreshed early (ratio of `tokenTtl`).
    refresh_at: Option<std::time::Instant>,
}

/// A prompt subscription kept by [`NacosHttpAiService`].
struct SubscribedPrompt {
    version: Option<String>,
    label: Option<String>,
    listener: PromptListener,
    last: Option<NacosPrompt>,
}

/// Subscription bookkeeping (mirrors the listener registration a Nacos client performs).
#[derive(Default)]
struct Subscriptions {
    card_listeners: HashMap<String, AgentCardListener>,
    card_last: HashMap<String, NacosAgentCard>,
    prompt_listeners: HashMap<String, SubscribedPrompt>,
}

/// A [`NacosAiService`] over the Nacos v1 AI OpenAPI.
pub struct NacosHttpAiService {
    config: NacosHttpConfig,
    transport: Arc<dyn NacosHttpTransport>,
    subscriptions: Mutex<Subscriptions>,
    /// Shared so [`NacosHttpAiService::for_namespace`] views reuse one login.
    auth: Arc<Mutex<AuthState>>,
}

impl std::fmt::Debug for NacosHttpAiService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NacosHttpAiService")
            .field("base_url", &self.config.base_url)
            .finish_non_exhaustive()
    }
}

impl NacosHttpAiService {
    /// Builds the service over a transport.
    pub fn new(config: NacosHttpConfig, transport: Arc<dyn NacosHttpTransport>) -> Self {
        Self {
            config,
            transport,
            subscriptions: Mutex::new(Subscriptions::default()),
            auth: Arc::new(Mutex::new(AuthState::default())),
        }
    }

    /// A view of this service that addresses `namespace` instead of the configured one.
    ///
    /// This is the **method-level optional override** of NACOS-16: the returned service shares
    /// the transport, the login cache and the configuration, but every request it builds
    /// carries `namespaceId=<namespace>`. Its subscription bookkeeping is independent, so it
    /// never mixes cards/prompts of two namespaces.
    pub fn for_namespace(&self, namespace: impl Into<String>) -> NacosHttpAiService {
        let mut config = self.config.clone();
        config.namespace = namespace.into();
        Self {
            config,
            transport: Arc::clone(&self.transport),
            subscriptions: Mutex::new(Subscriptions::default()),
            auth: Arc::clone(&self.auth),
        }
    }

    /// The configuration this service was built with.
    pub fn config(&self) -> &NacosHttpConfig {
        &self.config
    }

    /// Fires every listener whose subscribed value changed. Returns the number fired.
    ///
    /// This is the HTTP stand-in for a Nacos gRPC push: call it from a timer (see
    /// `spawn_polling` with the `http-client` feature) or from tests.
    pub async fn poll_once(&self) -> AiServiceResult<usize> {
        let cards: Vec<String> = self
            .subscriptions
            .lock()
            .unwrap()
            .card_listeners
            .keys()
            .cloned()
            .collect();
        let mut fired = 0usize;
        for name in cards {
            let Some(fetched) = self.fetch_agent_card(&name, None).await? else {
                continue;
            };
            let listener = {
                let mut guard = self.subscriptions.lock().unwrap();
                if guard.card_last.get(&name) == Some(&fetched) {
                    continue;
                }
                guard.card_last.insert(name.clone(), fetched.clone());
                guard.card_listeners.get(&name).cloned()
            };
            if let Some(listener) = listener {
                listener(&name, &fetched);
                fired += 1;
            }
        }

        let prompts: Vec<(String, Option<String>, Option<String>)> = self
            .subscriptions
            .lock()
            .unwrap()
            .prompt_listeners
            .iter()
            .map(|(key, sub)| (key.clone(), sub.version.clone(), sub.label.clone()))
            .collect();
        for (key, version, label) in prompts {
            let fetched = self
                .fetch_prompt(&key, version.as_deref(), label.as_deref())
                .await?;
            let listener = {
                let mut guard = self.subscriptions.lock().unwrap();
                let Some(sub) = guard.prompt_listeners.get_mut(&key) else {
                    continue;
                };
                if sub.last == fetched {
                    continue;
                }
                sub.last = fetched.clone();
                Some(sub.listener.clone())
            };
            if let (Some(listener), Some(prompt)) = (listener, fetched) {
                listener(&key, &prompt);
                fired += 1;
            }
        }
        Ok(fired)
    }

    /// Stops pushing changes for `prompt_key`, returning whether a subscription was removed.
    ///
    /// Nacos has no unsubscribe call (the Java module relies on disposing the whole `AiService`),
    /// so this prunes the local subscription table only — after which [`Self::poll_once`] no
    /// longer re-fetches the key (NACOS-12).
    pub fn unsubscribe_prompt(&self, prompt_key: &str) -> bool {
        self.subscriptions
            .lock()
            .unwrap()
            .prompt_listeners
            .remove(prompt_key)
            .is_some()
    }

    /// Stops pushing changes for `agent_name`; see [`Self::unsubscribe_prompt`] (NACOS-12).
    pub fn unsubscribe_agent_card(&self, agent_name: &str) -> bool {
        let mut guard = self.subscriptions.lock().unwrap();
        let removed = guard.card_listeners.remove(agent_name).is_some();
        guard.card_last.remove(agent_name);
        removed
    }

    /// Drops every subscription, mirroring the disposal of a Nacos `AiService` client
    /// (NACOS-12).
    ///
    /// The service stays usable: a later `subscribe_*` call registers again.
    pub fn close(&self) {
        let mut guard = self.subscriptions.lock().unwrap();
        guard.card_listeners.clear();
        guard.card_last.clear();
        guard.prompt_listeners.clear();
    }

    /// Drives [`Self::poll_once`] every `interval` until the returned handle is aborted.
    ///
    /// This is the HTTP stand-in for the Nacos client's gRPC push stream: subscribed AgentCards /
    /// prompts are re-fetched and changed values are pushed to the listeners. A failed tick is
    /// logged and retried with exponential backoff ([`polling_backoff`], capped at
    /// [`MAX_POLLING_BACKOFF`]); a successful tick resets the schedule. `interval` is injectable,
    /// so the loop is testable with millisecond ticks (NACOS-06/13).
    pub fn polling(self: &Arc<Self>, interval: std::time::Duration) -> tokio::task::JoinHandle<()> {
        let service = Arc::clone(self);
        tokio::spawn(async move {
            let mut failures = 0u32;
            loop {
                match service.poll_once().await {
                    Ok(_) => failures = 0,
                    Err(error) => {
                        failures = failures.saturating_add(1);
                        warn!(error = %error, failures, "nacos ai service poll failed");
                    }
                }
                let delay = polling_backoff(interval, failures);
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
            }
        })
    }

    // ---- internals ----

    /// Builds a full URL with the query parameters plus the configured `namespaceId`.
    ///
    /// The `accessToken` is **not** part of this URL: it is injected per attempt by
    /// [`Self::execute_with_token`], so a refreshed token is always the one sent.
    fn url(&self, path: &str, query: &[(&str, String)]) -> String {
        self.build_url(path, query, true)
    }

    /// Builds a URL, optionally without the `namespaceId` (used by the auth endpoint, which is
    /// namespace independent).
    fn build_url(&self, path: &str, query: &[(&str, String)], with_namespace: bool) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        let mut pairs: Vec<String> = query
            .iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(key, value)| format!("{}={}", key, urlencode(value)))
            .collect();
        if with_namespace && !self.config.namespace.is_empty() {
            pairs.push(format!("namespaceId={}", urlencode(&self.config.namespace)));
        }
        if pairs.is_empty() {
            format!("{base}{path}")
        } else {
            format!("{base}{path}?{}", pairs.join("&"))
        }
    }

    /// Executes `request`, retrying **retryable** failures per [`NacosHttpConfig::retry`]
    /// (NACOS-06).
    ///
    /// The last attempt's outcome is returned unchanged, so a caller still maps a final `5xx`
    /// to its own error; terminal failures (business code, decode error, `4xx`) are returned
    /// immediately.
    async fn send(&self, request: HttpRequest) -> AiServiceResult<HttpResponse> {
        let mut attempt = 1u32;
        loop {
            let outcome = self.send_once(&request).await;
            let retryable = match &outcome {
                Ok(response) => crate::service::is_retryable_status(response.status),
                Err(error) => error.is_retryable(),
            };
            if !retryable || !self.config.retry.should_retry(attempt) {
                return outcome;
            }
            let backoff = self.config.retry.backoff(attempt);
            warn!(
                attempt,
                backoff_ms = backoff.as_millis() as u64,
                url = %request.url,
                "Nacos request failed with a retryable error, retrying"
            );
            if !backoff.is_zero() {
                tokio::time::sleep(backoff).await;
            }
            attempt += 1;
        }
    }

    /// Executes `request` once, injecting the current access token.
    ///
    /// When credentials are configured and the server rejects the token (`401`/`403`), the
    /// token is refreshed once and the original request is replayed — mirroring the Nacos SDK,
    /// which re-authenticates on a token expiry rejection (NACOS-03).
    async fn send_once(&self, request: &HttpRequest) -> AiServiceResult<HttpResponse> {
        let token = self.access_token().await?;
        let response = self.execute_with_token(request, token.as_deref()).await?;
        if matches!(response.status, 401 | 403) && self.config.has_credentials() {
            warn!(status = response.status, "Nacos rejected the accessToken, logging in again");
            self.invalidate_token();
            if let Some(refreshed) = self.access_token().await? {
                return self.execute_with_token(request, Some(&refreshed)).await;
            }
        }
        Ok(response)
    }

    /// Sends `request` with `accessToken` appended to its query string.
    async fn execute_with_token(
        &self,
        request: &HttpRequest,
        token: Option<&str>,
    ) -> AiServiceResult<HttpResponse> {
        let mut request = request.clone();
        if let Some(token) = token.filter(|token| !token.is_empty()) {
            request.url = append_query(&request.url, "accessToken", token);
        }
        self.transport
            .execute(request)
            .await
            .map_err(|e| AiServiceError::transport(format!("nacos http transport failed: {e}")))
    }

    /// The access token to send: the cached/refreshed credential token when credentials are
    /// configured, else the static [`NacosHttpConfig::access_token`].
    async fn access_token(&self) -> AiServiceResult<Option<String>> {
        if !self.config.has_credentials() {
            return Ok(self.config.access_token.clone());
        }
        {
            let guard = self.auth.lock().unwrap();
            if let (Some(token), Some(refresh_at)) = (&guard.token, guard.refresh_at) {
                if std::time::Instant::now() < refresh_at {
                    return Ok(Some(token.clone()));
                }
            }
        }
        match self.login().await {
            Ok((token, ttl)) => {
                let refresh_at = std::time::Instant::now()
                    + refresh_deadline(ttl, self.config.token_refresh_ratio);
                let mut guard = self.auth.lock().unwrap();
                guard.token = Some(token.clone());
                guard.refresh_at = Some(refresh_at);
                Ok(Some(token))
            }
            Err(error) => {
                // Credential login is unavailable (auth disabled server-side, bad password,
                // transport failure): fall back to the statically configured token.
                warn!(error = %error, "Nacos login failed, falling back to the static accessToken");
                self.invalidate_token();
                Ok(self.config.access_token.clone())
            }
        }
    }

    /// Logs in through `/v1/auth/login`, returning `(accessToken, tokenTtl)`.
    ///
    /// Nacos answers `{"accessToken":"..","tokenTtl":18000,"globalAdmin":false}`; a missing
    /// `tokenTtl` falls back to [`DEFAULT_TOKEN_TTL`] (the Nacos server default of 5 hours).
    async fn login(&self) -> AiServiceResult<(String, std::time::Duration)> {
        let url = self.build_url(
            &self.config.auth_path,
            &[
                ("username", self.config.username.clone().unwrap_or_default()),
                ("password", self.config.password.clone().unwrap_or_default()),
            ],
            false,
        );
        // Nacos reads `username`/`password` from the (query) parameters; a body-less POST keeps
        // the request compatible with instances that only accept form/query credentials.
        let request = HttpRequest {
            method: "POST".to_string(),
            url,
            body: None,
            content_type: None,
        };
        let response = self.execute_with_token(&request, None).await?;
        if !response.is_success() {
            return Err(error_from_response(&response));
        }
        let value: Value = serde_json::from_slice(&response.body)
            .map_err(|e| AiServiceError::serde(format!("invalid nacos login response: {e}")))?;
        let value = unwrap_envelope(value)?;
        let token = value
            .get("accessToken")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| AiServiceError::serde("nacos login response carried no accessToken"))?;
        let ttl = value
            .get("tokenTtl")
            .and_then(Value::as_f64)
            .filter(|ttl| *ttl > 0.0)
            .map(std::time::Duration::from_secs_f64)
            .unwrap_or(DEFAULT_TOKEN_TTL);
        Ok((token.to_string(), ttl))
    }

    /// Drops the cached credential token so the next call logs in again.
    fn invalidate_token(&self) {
        let mut guard = self.auth.lock().unwrap();
        guard.token = None;
        guard.refresh_at = None;
    }

    /// Runs a `GET` and returns the (envelope-unwrapped) JSON body.
    async fn get_json(&self, url: String) -> AiServiceResult<Option<Value>> {
        let response = self.send(HttpRequest::get(url)).await?;
        if response.status == 404 {
            return Ok(None);
        }
        if !response.is_success() {
            return Err(error_from_response(&response));
        }
        let value: Value = serde_json::from_slice(&response.body)
            .map_err(|e| AiServiceError::serde(format!("invalid nacos response: {e}")))?;
        Ok(Some(unwrap_envelope(value)?))
    }

    async fn fetch_agent_card(
        &self,
        agent_name: &str,
        version: Option<&str>,
    ) -> AiServiceResult<Option<NacosAgentCard>> {
        let url = self.url(
            &self.config.agent_path,
            &[
                ("agentName", agent_name.to_string()),
                ("version", version.unwrap_or_default().to_string()),
            ],
        );
        let Some(value) = self.get_json(url).await? else {
            return Ok(None);
        };
        let card = serde_json::from_value::<NacosAgentCard>(value)
            .map_err(|e| AiServiceError::serde(format!("invalid agent card payload: {e}")))?;
        Ok(Some(card))
    }

    async fn fetch_prompt(
        &self,
        prompt_key: &str,
        version: Option<&str>,
        label: Option<&str>,
    ) -> AiServiceResult<Option<NacosPrompt>> {
        let url = self.url(
            &self.config.prompt_path,
            &[
                ("promptKey", prompt_key.to_string()),
                ("version", version.unwrap_or_default().to_string()),
                ("label", label.unwrap_or_default().to_string()),
            ],
        );
        let Some(value) = self.get_json(url).await? else {
            return Ok(None);
        };
        Ok(parse_prompt(&value))
    }
}

#[async_trait]
impl NacosAiService for NacosHttpAiService {
    async fn release_agent_card(
        &self,
        card: &NacosAgentCard,
        endpoint_type: &str,
        set_as_latest: bool,
    ) -> AiServiceResult<()> {
        let url = self.url(
            &self.config.agent_path,
            &[
                ("endpointType", endpoint_type.to_string()),
                ("setAsLatest", set_as_latest.to_string()),
            ],
        );
        let body = serde_json::to_vec(card)
            .map_err(|e| AiServiceError::new(AiServiceError::INVALID_PARAM, e.to_string()))?;
        let response = self.send(HttpRequest::post_json(url, body)).await?;
        check_write_response(&response)
    }

    async fn register_agent_endpoint(
        &self,
        agent_name: &str,
        endpoint: &NacosAgentEndpoint,
    ) -> AiServiceResult<()> {
        let url = self.url(
            &self.config.agent_endpoint_path,
            &[("agentName", agent_name.to_string())],
        );
        let body = serde_json::to_vec(endpoint)
            .map_err(|e| AiServiceError::new(AiServiceError::INVALID_PARAM, e.to_string()))?;
        let response = self.send(HttpRequest::post_json(url, body)).await?;
        check_write_response(&response)
    }

    async fn get_agent_card(
        &self,
        agent_name: &str,
        version: Option<&str>,
    ) -> AiServiceResult<Option<NacosAgentCard>> {
        self.fetch_agent_card(agent_name, version).await
    }

    async fn subscribe_agent_card(
        &self,
        agent_name: &str,
        listener: AgentCardListener,
    ) -> AiServiceResult<NacosAgentCard> {
        let card = self
            .fetch_agent_card(agent_name, None)
            .await?
            .ok_or_else(|| {
                AiServiceError::not_found(format!("agent card not found: {agent_name}"))
            })?;
        let mut guard = self.subscriptions.lock().unwrap();
        guard
            .card_listeners
            .insert(agent_name.to_string(), listener);
        guard.card_last.insert(agent_name.to_string(), card.clone());
        Ok(card)
    }

    async fn subscribe_prompt(
        &self,
        prompt_key: &str,
        version: Option<&str>,
        label: Option<&str>,
        listener: PromptListener,
    ) -> AiServiceResult<Option<NacosPrompt>> {
        let prompt = self.fetch_prompt(prompt_key, version, label).await?;
        let mut guard = self.subscriptions.lock().unwrap();
        guard.prompt_listeners.insert(
            prompt_key.to_string(),
            SubscribedPrompt {
                version: version.map(str::to_string),
                label: label.map(str::to_string),
                listener,
                last: prompt.clone(),
            },
        );
        Ok(prompt)
    }

    async fn download_skill_zip(&self, skill_name: &str) -> AiServiceResult<SkillZip> {
        self.download_skill(skill_name, &[]).await
    }

    async fn download_skill_zip_by_version(
        &self,
        skill_name: &str,
        version: &str,
    ) -> AiServiceResult<SkillZip> {
        self.download_skill(skill_name, &[("version", version.to_string())])
            .await
    }

    async fn download_skill_zip_by_label(
        &self,
        skill_name: &str,
        label: &str,
    ) -> AiServiceResult<SkillZip> {
        self.download_skill(skill_name, &[("label", label.to_string())])
            .await
    }
}

impl NacosHttpAiService {
    async fn download_skill(
        &self,
        skill_name: &str,
        extra: &[(&str, String)],
    ) -> AiServiceResult<SkillZip> {
        let mut query: Vec<(&str, String)> = vec![("skillName", skill_name.to_string())];
        query.extend(extra.iter().cloned());
        let url = self.url(&self.config.skill_path, &query);
        let response = self.send(HttpRequest::get(url)).await?;
        if response.status == 404 {
            return Err(AiServiceError::not_found(format!(
                "skill not found: {skill_name}"
            )));
        }
        if !response.is_success() {
            return Err(error_from_response(&response));
        }
        if response.body.is_empty() {
            return Err(AiServiceError::not_found(format!(
                "skill package is empty: {skill_name}"
            )));
        }
        Ok(response.body)
    }
}

/// Unwraps the Nacos `{"code":0,"data":...}` envelope (bare payloads pass through).
///
/// A non-zero `code` is a Nacos **business** rejection: Nacos answers `HTTP 200` in that case,
/// so it must be turned into an error here, otherwise a refused write would look successful and
/// a refused read would be misreported as a decode failure (NACOS-11).
fn unwrap_envelope(value: Value) -> AiServiceResult<Value> {
    match value {
        Value::Object(mut map) => {
            let code = map.get("code").and_then(Value::as_i64);
            if let Some(code) = code.filter(|code| *code != 0) {
                let message = map
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("nacos business error {code}"));
                return Err(AiServiceError::new(code, message));
            }
            let has_envelope = code.is_some() || map.contains_key("success");
            if has_envelope {
                if let Some(data) = map.remove("data") {
                    return Ok(data);
                }
            }
            Ok(Value::Object(map))
        }
        other => Ok(other),
    }
}

/// Validates a mutating response: a success status *and* a non-error envelope.
///
/// A plain-text acknowledgement (`ok`) is accepted, mirroring the Nacos v1 endpoints that
/// answer without a JSON body (NACOS-11).
fn check_write_response(response: &HttpResponse) -> AiServiceResult<()> {
    if !response.is_success() {
        return Err(error_from_response(response));
    }
    if response.body.is_empty() {
        return Ok(());
    }
    match serde_json::from_slice::<Value>(&response.body) {
        Ok(value) => unwrap_envelope(value).map(|_| ()),
        Err(_) => Ok(()),
    }
}

/// Builds a [`NacosPrompt`] from a prompt payload (`template` + optional version/label).
fn parse_prompt(value: &Value) -> Option<NacosPrompt> {
    let object = value.as_object()?;
    let template = object.get("template")?.as_str()?.to_string();
    Some(NacosPrompt {
        template,
        version: object
            .get("version")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        label: object
            .get("label")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

/// Maps an error response onto [`AiServiceError`].
///
/// A Nacos error envelope keeps its business `code`/`message` ([`AiServiceError::Business`]);
/// a bare status answer becomes [`AiServiceError::Http`] so the caller can tell a retryable
/// 5xx from a business rejection.
fn error_from_response(response: &HttpResponse) -> AiServiceError {
    let text = response.text();
    let parsed: Option<Value> = serde_json::from_str(&text).ok();
    let message = parsed
        .as_ref()
        .and_then(|v| v.get("message"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| text.trim().to_string());
    match parsed
        .as_ref()
        .and_then(|v| v.get("code"))
        .and_then(Value::as_i64)
    {
        Some(code) => AiServiceError::new(code, message),
        None => AiServiceError::http(response.status, message),
    }
}

/// Appends one query parameter to `url`.
fn append_query(url: &str, key: &str, value: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}{key}={}", urlencode(value))
}

/// Percent-encodes a query value (RFC 3986 unreserved characters pass through).
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(feature = "http-client")]
mod reqwest_transport;
#[cfg(feature = "http-client")]
pub use reqwest_transport::{spawn_polling, ReqwestNacosTransport};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a::discovery::NacosAgentCardResolver;
    use crate::prompt::NacosPromptListener;
    use crate::service::{InMemoryNacosAiService, A2A_ENDPOINT_TYPE_SERVICE};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Records requests and replays a queued response per call.
    struct FakeTransport {
        responses: Mutex<Vec<HttpResponse>>,
        requests: Mutex<Vec<HttpRequest>>,
    }

    impl FakeTransport {
        fn new(responses: Vec<HttpResponse>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses),
                requests: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<HttpRequest> {
            self.requests.lock().unwrap().clone()
        }

        fn push(&self, response: HttpResponse) {
            self.responses.lock().unwrap().push(response);
        }
    }

    #[async_trait]
    impl NacosHttpTransport for FakeTransport {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, String> {
            self.requests.lock().unwrap().push(request);
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Err("no queued response".to_string());
            }
            Ok(responses.remove(0))
        }
    }

    fn service(transport: Arc<FakeTransport>) -> NacosHttpAiService {
        NacosHttpAiService::new(
            NacosHttpConfig::new("http://nacos:8848/nacos/", "public")
                .with_access_token("tok en"),
            transport,
        )
    }

    fn card_json(name: &str) -> String {
        let card = NacosAgentCard::new(name, "http://agent:9000");
        serde_json::to_string(&card).unwrap()
    }

    #[tokio::test]
    async fn get_agent_card_builds_the_v1_url_and_parses_the_card() {
        let transport = FakeTransport::new(vec![HttpResponse::new(200, card_json("remote"))]);
        let service = service(Arc::clone(&transport));

        let card = service
            .get_agent_card("remote", Some("1.2"))
            .await
            .unwrap()
            .expect("a card");
        assert_eq!(card.name, "remote");
        assert_eq!(
            transport.requests()[0].url,
            "http://nacos:8848/nacos/v1/ai/agent?agentName=remote&version=1.2&namespaceId=public&accessToken=tok%20en"
        );
    }

    #[tokio::test]
    async fn an_absent_card_is_none_but_a_broken_payload_is_an_error() {
        let transport = FakeTransport::new(vec![
            HttpResponse::new(404, ""),
            HttpResponse::new(200, "{\"unexpected\": true}"),
        ]);
        let service = service(Arc::clone(&transport));
        assert!(service.get_agent_card("missing", None).await.unwrap().is_none());
        let error = service.get_agent_card("broken", None).await.unwrap_err();
        assert!(error.message().contains("invalid agent card payload"));
    }

    #[tokio::test]
    async fn the_nacos_envelope_is_unwrapped() {
        let payload = format!(
            "{{\"code\":0,\"data\":{}}}",
            card_json("wrapped")
        );
        let transport = FakeTransport::new(vec![HttpResponse::new(200, payload)]);
        let card = service(Arc::clone(&transport))
            .get_agent_card("wrapped", None)
            .await
            .unwrap()
            .expect("a card");
        assert_eq!(card.name, "wrapped");
    }

    #[tokio::test]
    async fn release_agent_card_posts_the_card_and_set_as_latest() {
        let transport = FakeTransport::new(vec![HttpResponse::new(200, "ok")]);
        let service = service(Arc::clone(&transport));
        let card = NacosAgentCard::new("local", "http://agent:9000");
        service
            .release_agent_card(&card, A2A_ENDPOINT_TYPE_SERVICE, true)
            .await
            .unwrap();

        let request = &transport.requests()[0];
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.url,
            "http://nacos:8848/nacos/v1/ai/agent?endpointType=service&setAsLatest=true&namespaceId=public&accessToken=tok%20en"
        );
        assert_eq!(request.content_type.as_deref(), Some("application/json"));
        let posted: NacosAgentCard = serde_json::from_slice(request.body.as_ref().unwrap()).unwrap();
        assert_eq!(posted, card);
    }

    #[tokio::test]
    async fn a_nacos_error_body_keeps_its_code_and_message() {
        let transport = FakeTransport::new(vec![HttpResponse::new(
            400,
            "{\"code\":12001,\"message\":\"invalid param\"}",
        )]);
        let service = service(Arc::clone(&transport));
        let card = NacosAgentCard::new("local", "http://agent:9000");
        let error = service
            .release_agent_card(&card, A2A_ENDPOINT_TYPE_SERVICE, false)
            .await
            .unwrap_err();
        assert_eq!(error.code(), 12001);
        assert_eq!(error.message(), "invalid param");
        assert!(matches!(error, AiServiceError::Business { code: 12001, .. }));
    }

    #[tokio::test]
    async fn register_agent_endpoint_posts_the_endpoint_json() {
        let transport = FakeTransport::new(vec![HttpResponse::new(200, "")]);
        let service = service(Arc::clone(&transport));
        let endpoint = NacosAgentEndpoint {
            transport: "JSONRPC".to_string(),
            address: Some("10.0.0.1".to_string()),
            port: 9000,
            path: Some("/a2a".to_string()),
            support_tls: false,
            version: Some("1".to_string()),
            protocol: Some("http".to_string()),
            query: None,
        };
        service
            .register_agent_endpoint("local", &endpoint)
            .await
            .unwrap();
        let request = &transport.requests()[0];
        assert!(request.url.contains("/v1/ai/agent/endpoint?agentName=local"));
        let posted: NacosAgentEndpoint =
            serde_json::from_slice(request.body.as_ref().unwrap()).unwrap();
        assert_eq!(posted.port, 9000);
    }

    #[tokio::test]
    async fn prompts_are_parsed_and_subscriptions_fire_on_change() {
        let transport = FakeTransport::new(vec![
            HttpResponse::new(200, "{\"template\":\"hello {{name}}\",\"version\":\"1\"}"),
        ]);
        let service = service(Arc::clone(&transport));
        let fired = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&fired);
        let listener: PromptListener = Arc::new(move |key: &str, prompt: &NacosPrompt| {
            assert_eq!(key, "greeting");
            // A push may carry the initial template or a later revision, so only the
            // placeholder contract is stable across pushes.
            assert!(
                prompt.template.contains("{{name}}"),
                "got {}",
                prompt.template
            );
            counter.fetch_add(1, Ordering::SeqCst);
        });

        let prompt = service
            .subscribe_prompt("greeting", Some("1"), None, listener)
            .await
            .unwrap()
            .expect("a prompt");
        assert_eq!(prompt.version.as_deref(), Some("1"));
        assert_eq!(prompt.template, "hello {{name}}");

        // No server-side change: nothing fires.
        transport.push(HttpResponse::new(200, "{\"template\":\"hello {{name}}\",\"version\":\"1\"}"));
        assert_eq!(service.poll_once().await.unwrap(), 0);
        assert_eq!(fired.load(Ordering::SeqCst), 0);

        // A changed template fires the listener once.
        transport.push(HttpResponse::new(200, "{\"template\":\"hi {{name}}\",\"version\":\"2\"}"));
        assert_eq!(service.poll_once().await.unwrap(), 1);
        assert_eq!(fired.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn agent_card_subscriptions_are_polled_for_changes() {
        let transport = FakeTransport::new(vec![HttpResponse::new(200, card_json("remote"))]);
        let service = service(Arc::clone(&transport));
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&seen);
        let listener: AgentCardListener = Arc::new(move |name: &str, card: &NacosAgentCard| {
            sink.lock().unwrap().push(format!("{name}:{}", card.url.clone().unwrap_or_default()));
        });
        service.subscribe_agent_card("remote", listener).await.unwrap();

        transport.push(HttpResponse::new(200, card_json("remote")));
        assert_eq!(service.poll_once().await.unwrap(), 0);

        let mut changed = NacosAgentCard::new("remote", "http://agent:9999");
        changed.version = Some("2".to_string());
        transport.push(HttpResponse::new(200, serde_json::to_string(&changed).unwrap()));
        assert_eq!(service.poll_once().await.unwrap(), 1);
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["remote:http://agent:9999".to_string()]
        );
    }

    #[tokio::test]
    async fn subscribing_to_a_missing_card_is_not_found() {
        let transport = FakeTransport::new(vec![HttpResponse::new(404, "")]);
        let service = service(Arc::clone(&transport));
        let noop: AgentCardListener = Arc::new(|_, _| {});
        let error = service.subscribe_agent_card("gone", noop).await.unwrap_err();
        assert!(error.is_not_found());
    }

    /// NACOS-12: unsubscribing / closing prunes the subscription table, so `poll_once` no longer
    /// re-fetches that key.
    #[tokio::test]
    async fn unsubscribing_and_close_stop_polling_the_key() {
        let transport = FakeTransport::new(vec![
            HttpResponse::new(200, card_json("remote")),
            HttpResponse::new(200, "{\"template\":\"hi\"}"),
            HttpResponse::new(200, card_json("remote")),
            HttpResponse::new(200, card_json("remote")),
        ]);
        let http = Arc::new(service(Arc::clone(&transport)));
        let card_listener: AgentCardListener = Arc::new(|_, _| {});
        let prompt_listener: PromptListener = Arc::new(|_, _| {});
        http.subscribe_agent_card("remote", card_listener)
            .await
            .unwrap();
        http.subscribe_prompt("greet", None, None, prompt_listener)
            .await
            .unwrap();
        assert_eq!(transport.requests().len(), 2);

        assert!(http.unsubscribe_prompt("greet"));
        assert!(!http.unsubscribe_prompt("greet"));

        // The card is still subscribed, so it is re-fetched; the pruned prompt is not.
        assert_eq!(http.poll_once().await.unwrap(), 0);
        let requests = transport.requests();
        assert_eq!(requests.len(), 3);
        assert!(requests[2].url.contains("agentName=remote"));
        assert!(!requests[2].url.contains("promptKey"));

        assert!(http.unsubscribe_agent_card("remote"));
        // Nothing is subscribed now, so `poll_once` issues no request at all (the queue is empty,
        // so a re-fetch would surface as a transport error).
        assert_eq!(http.poll_once().await.unwrap(), 0);
        assert_eq!(transport.requests().len(), 3);

        // Subscribing again after an unsubscribe works, and `close()` drops everything.
        let again: AgentCardListener = Arc::new(|_, _| {});
        http.subscribe_agent_card("remote", again).await.unwrap();
        http.close();
        assert_eq!(http.poll_once().await.unwrap(), 0);
        assert_eq!(transport.requests().len(), 4);
    }

    /// NACOS-13: the polling chain feeds the real consumers — a changed prompt reaches
    /// `NacosPromptListener` and a changed card reaches `NacosAgentCardResolver`, and both then
    /// answer from cache (no extra request).
    #[tokio::test]
    async fn polling_refreshes_the_prompt_listener_and_the_card_resolver() {
        let transport = FakeTransport::new(vec![
            HttpResponse::new(200, card_json("remote")),
            HttpResponse::new(200, "{\"template\":\"hello {{name}}\"}"),
        ]);
        let http = Arc::new(service(Arc::clone(&transport)));
        let nacos: Arc<dyn NacosAiService> = http.clone();
        let resolver = NacosAgentCardResolver::new(Arc::clone(&nacos));
        let listener = NacosPromptListener::new(nacos);

        let mut args = HashMap::new();
        args.insert("name".to_string(), "ada".to_string());
        assert_eq!(
            resolver.resolve_detailed("remote").await.unwrap().url,
            "http://agent:9000"
        );
        assert_eq!(
            listener
                .get_prompt_with_args("greet", Some(&args))
                .await
                .unwrap(),
            "hello ada"
        );
        assert_eq!(transport.requests().len(), 2);

        // Nacos now serves a new card and a new template.
        let mut changed = NacosAgentCard::new("remote", "http://agent:9999");
        changed.version = Some("2".to_string());
        transport.push(HttpResponse::new(200, serde_json::to_string(&changed).unwrap()));
        transport.push(HttpResponse::new(
            200,
            "{\"template\":\"hi {{name}}\",\"version\":\"2\",\"label\":\"prod\"}",
        ));
        assert_eq!(http.poll_once().await.unwrap(), 2);

        // Both consumers see the pushed value without issuing another request.
        assert_eq!(
            resolver.resolve_detailed("remote").await.unwrap().url,
            "http://agent:9999"
        );
        assert_eq!(
            listener
                .get_prompt_with_args("greet", Some(&args))
                .await
                .unwrap(),
            "hi ada"
        );
        assert_eq!(transport.requests().len(), 4);
    }

    /// NACOS-13: `polling` is the built-in timer for `poll_once`, so a host needs no loop.
    #[tokio::test]
    async fn polling_pushes_changes_to_subscribers_until_aborted() {
        let transport = FakeTransport::new(vec![HttpResponse::new(200, card_json("remote"))]);
        let http = Arc::new(service(Arc::clone(&transport)));
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&seen);
        let listener: AgentCardListener = Arc::new(move |name: &str, card: &NacosAgentCard| {
            sink.lock()
                .unwrap()
                .push(format!("{name}:{}", card.url.clone().unwrap_or_default()));
        });
        http.subscribe_agent_card("remote", listener).await.unwrap();

        let mut changed = NacosAgentCard::new("remote", "http://agent:9999");
        changed.version = Some("2".to_string());
        transport.push(HttpResponse::new(200, serde_json::to_string(&changed).unwrap()));

        let handle = http.polling(Duration::from_millis(5));
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["remote:http://agent:9999".to_string()]
        );
    }

    /// NACOS-06/13: the polling schedule backs off exponentially and is capped.
    #[test]
    fn polling_backs_off_exponentially_until_the_cap() {
        let interval = Duration::from_secs(1);
        assert_eq!(polling_backoff(interval, 0), interval);
        assert_eq!(polling_backoff(interval, 1), Duration::from_secs(2));
        assert_eq!(polling_backoff(interval, 3), Duration::from_secs(8));
        // Capped, so a long outage stops growing the delay.
        assert_eq!(polling_backoff(interval, 40), MAX_POLLING_BACKOFF);
        // A sub-second interval is still honoured while healthy.
        let fast = Duration::from_millis(5);
        assert_eq!(polling_backoff(fast, 0), fast);
        assert_eq!(polling_backoff(fast, 1), Duration::from_millis(10));
    }

    #[tokio::test]
    async fn skill_zips_are_downloaded_with_version_or_label() {
        let transport = FakeTransport::new(vec![
            HttpResponse::new(200, vec![0x50, 0x4b, 0x03, 0x04]),
            HttpResponse::new(200, vec![0x50, 0x4b, 0x05, 0x06]),
            HttpResponse::new(404, ""),
        ]);
        let service = service(Arc::clone(&transport));
        assert_eq!(service.download_skill_zip("calc").await.unwrap().len(), 4);
        assert_eq!(
            service
                .download_skill_zip_by_version("calc", "2")
                .await
                .unwrap(),
            vec![0x50, 0x4b, 0x05, 0x06]
        );
        assert!(service.download_skill_zip_by_label("calc", "stable").await.unwrap_err().is_not_found());

        let requests = transport.requests();
        assert!(requests[0].url.contains("skillName=calc"));
        assert!(requests[1].url.contains("version=2"));
        assert!(requests[2].url.contains("label=stable"));
    }

    /// The HTTP service and the in-memory service must agree on the trait surface, so the
    /// in-memory one stays a drop-in double for embedders and tests.
    #[tokio::test]
    async fn the_http_service_is_interchangeable_with_the_in_memory_one() {
        let memory: Arc<dyn NacosAiService> = Arc::new(InMemoryNacosAiService::new());
        let transport = FakeTransport::new(vec![HttpResponse::new(200, card_json("remote"))]);
        let http: Arc<dyn NacosAiService> = Arc::new(service(transport));
        assert_eq!(
            http.get_agent_card("remote", None).await.unwrap().unwrap().name,
            "remote"
        );
        assert!(memory
            .get_agent_card("remote", None)
            .await
            .unwrap()
            .is_none());
    }

    #[test]
    fn urls_skip_blank_parameters() {
        let transport = FakeTransport::new(vec![]);
        let service = NacosHttpAiService::new(
            NacosHttpConfig::new("http://nacos:8848/nacos", "dev"),
            transport,
        );
        assert_eq!(
            service.url("/v1/ai/prompt", &[("promptKey", "k".into()), ("label", String::new())]),
            "http://nacos:8848/nacos/v1/ai/prompt?promptKey=k&namespaceId=dev"
        );
    }

    /// NACOS-04: the read paths (card / prompt / skill) address the configured namespace too.
    #[tokio::test]
    async fn read_paths_carry_the_configured_namespace() {
        let transport = FakeTransport::new(vec![
            HttpResponse::new(200, card_json("remote")),
            HttpResponse::new(200, "{\"template\":\"hi\"}"),
            HttpResponse::new(200, vec![0x50, 0x4b]),
        ]);
        let service = NacosHttpAiService::new(
            NacosHttpConfig::new("http://nacos:8848/nacos", "dev"),
            transport.clone(),
        );
        let listener: PromptListener = Arc::new(|_, _| {});
        service.get_agent_card("remote", None).await.unwrap();
        service
            .subscribe_prompt("greeting", None, None, listener)
            .await
            .unwrap();
        service.download_skill_zip("calc").await.unwrap();

        for request in transport.requests() {
            assert!(
                request.url.contains("namespaceId=dev"),
                "missing namespaceId in {}",
                request.url
            );
        }
    }

    /// NACOS-04/16: an empty namespace means "server default" and is omitted.
    #[tokio::test]
    async fn an_empty_namespace_is_omitted_from_the_query() {
        let transport = FakeTransport::new(vec![HttpResponse::new(200, card_json("remote"))]);
        let service = NacosHttpAiService::new(
            NacosHttpConfig::new("http://nacos:8848/nacos", ""),
            transport.clone(),
        );
        service.get_agent_card("remote", None).await.unwrap();
        assert!(!transport.requests()[0].url.contains("namespaceId"));
    }

    /// NACOS-16: a scoped view addresses another namespace over the same transport.
    #[tokio::test]
    async fn a_scoped_service_addresses_another_namespace() {
        let transport = FakeTransport::new(vec![
            HttpResponse::new(200, card_json("remote")),
            HttpResponse::new(200, card_json("remote")),
        ]);
        let parent = NacosHttpAiService::new(
            NacosHttpConfig::new("http://nacos:8848/nacos", "dev"),
            transport.clone(),
        );
        let scoped = parent.for_namespace("staging");
        scoped.get_agent_card("remote", None).await.unwrap();
        parent.get_agent_card("remote", None).await.unwrap();

        let requests = transport.requests();
        assert!(requests[0].url.contains("namespaceId=staging"));
        assert!(requests[1].url.contains("namespaceId=dev"));
    }

    /// A Nacos login payload (`tokenTtl` in seconds, like the server).
    fn login_json(token: &str) -> String {
        format!("{{\"accessToken\":\"{token}\",\"tokenTtl\":18000}}")
    }

    fn credential_service(
        transport: Arc<FakeTransport>,
        username: &str,
        password: &str,
    ) -> NacosHttpAiService {
        NacosHttpAiService::new(
            NacosHttpConfig::new("http://nacos:8848/nacos", "public")
                .with_credentials(username, password),
            transport,
        )
    }

    /// NACOS-03: credentials log in through `/v1/auth/login`; the token is then reused.
    #[tokio::test]
    async fn credentials_log_in_once_and_the_token_is_reused() {
        let transport = FakeTransport::new(vec![
            HttpResponse::new(200, login_json("t1")),
            HttpResponse::new(200, card_json("remote")),
            HttpResponse::new(200, card_json("remote")),
        ]);
        let service = credential_service(Arc::clone(&transport), "nacos", "s3cret");
        service.get_agent_card("remote", None).await.unwrap();
        service.get_agent_card("remote", None).await.unwrap();

        let requests = transport.requests();
        assert_eq!(requests.len(), 3, "one login for two calls: {requests:?}");
        assert_eq!(requests[0].method, "POST");
        assert_eq!(
            requests[0].url,
            "http://nacos:8848/nacos/v1/auth/login?username=nacos&password=s3cret"
        );
        assert!(requests[1].url.ends_with("accessToken=t1"));
        assert!(requests[2].url.ends_with("accessToken=t1"));
    }

    /// NACOS-03: a rejected token triggers exactly one re-login and the request is replayed.
    #[tokio::test]
    async fn a_rejected_token_is_renewed_once_and_the_request_replayed() {
        let transport = FakeTransport::new(vec![
            HttpResponse::new(200, login_json("t1")),
            HttpResponse::new(401, "{\"code\":403,\"message\":\"token expired\"}"),
            HttpResponse::new(200, login_json("t2")),
            HttpResponse::new(200, card_json("remote")),
        ]);
        let service = credential_service(Arc::clone(&transport), "nacos", "s3cret");
        let card = service
            .get_agent_card("remote", None)
            .await
            .unwrap()
            .expect("a card");
        assert_eq!(card.name, "remote");

        let requests = transport.requests();
        assert_eq!(requests.len(), 4, "login, 401, re-login, replay: {requests:?}");
        assert!(requests[1].url.ends_with("accessToken=t1"));
        assert!(requests[2].url.contains("/v1/auth/login"));
        assert!(requests[3].url.ends_with("accessToken=t2"));
    }

    /// NACOS-03: a token is refreshed at 90% of its TTL (no sleeping needed for the policy).
    #[test]
    fn the_refresh_deadline_is_ninety_percent_of_the_ttl() {
        assert_eq!(
            NacosHttpConfig::new("http://nacos:8848/nacos", "public").token_refresh_ratio,
            DEFAULT_TOKEN_REFRESH_RATIO
        );
        let deadline = refresh_deadline(std::time::Duration::from_secs(18_000), 0.9);
        assert!((deadline.as_secs_f64() - 16_200.0).abs() < 0.001, "{deadline:?}");
        // A non-finite ratio falls back to the default instead of producing a bogus deadline.
        assert_eq!(
            refresh_deadline(std::time::Duration::from_secs(10), f64::NAN),
            std::time::Duration::from_secs(9)
        );
    }

    /// NACOS-03: with a ratio of 0 the cached token is due immediately, so each call re-logs in.
    #[tokio::test]
    async fn an_expired_token_is_refreshed_before_the_request() {
        let transport = FakeTransport::new(vec![
            HttpResponse::new(200, login_json("t1")),
            HttpResponse::new(200, card_json("remote")),
            HttpResponse::new(200, login_json("t2")),
            HttpResponse::new(200, card_json("remote")),
        ]);
        let service = NacosHttpAiService::new(
            NacosHttpConfig::new("http://nacos:8848/nacos", "public")
                .with_credentials("nacos", "s3cret")
                .with_token_refresh_ratio(0.0),
            transport.clone(),
        );
        service.get_agent_card("remote", None).await.unwrap();
        service.get_agent_card("remote", None).await.unwrap();

        let requests = transport.requests();
        assert_eq!(requests.len(), 4);
        assert!(requests[1].url.ends_with("accessToken=t1"));
        assert!(requests[3].url.ends_with("accessToken=t2"));
    }

    /// NACOS-03: when the login itself fails, the static token is used.
    #[tokio::test]
    async fn a_failed_login_falls_back_to_the_static_token() {
        let transport = FakeTransport::new(vec![
            HttpResponse::new(500, "auth is disabled"),
            HttpResponse::new(200, card_json("remote")),
        ]);
        let service = NacosHttpAiService::new(
            NacosHttpConfig::new("http://nacos:8848/nacos", "public")
                .with_credentials("nacos", "s3cret")
                .with_access_token("static"),
            transport.clone(),
        );
        service.get_agent_card("remote", None).await.unwrap();

        let requests = transport.requests();
        assert!(requests[0].url.contains("/v1/auth/login"));
        assert!(requests[1].url.ends_with("accessToken=static"));
    }

    /// NACOS-11: a `200` carrying a business error code is a failure on read and on write.
    #[tokio::test]
    async fn a_business_error_code_in_a_success_response_is_an_error() {
        let transport = FakeTransport::new(vec![
            HttpResponse::new(200, "{\"code\":300,\"message\":\"agent exists\"}"),
            HttpResponse::new(200, "{\"code\":404,\"message\":\"no such agent\"}"),
        ]);
        let service = service(Arc::clone(&transport));
        let card = NacosAgentCard::new("local", "http://agent:9000");
        let write = service
            .release_agent_card(&card, A2A_ENDPOINT_TYPE_SERVICE, false)
            .await
            .unwrap_err();
        assert_eq!(write.code(), 300);
        assert_eq!(write.message(), "agent exists");

        let read = service.get_agent_card("missing", None).await.unwrap_err();
        assert_eq!(read.code(), 404);
        assert!(read.is_not_found());
    }

    /// NACOS-15: the error kind drives the retry decision.
    #[test]
    fn the_error_kind_drives_the_retry_decision() {
        assert!(AiServiceError::transport("timeout").is_retryable());
        assert!(AiServiceError::http(503, "unavailable").is_retryable());
        assert!(AiServiceError::http(429, "slow down").is_retryable());
        assert!(!AiServiceError::http(404, "gone").is_retryable());
        assert!(!AiServiceError::new(300, "agent exists").is_retryable());
        assert!(!AiServiceError::serde("bad json").is_retryable());

        assert_eq!(AiServiceError::transport("timeout").code(), 500);
        assert_eq!(AiServiceError::http(503, "unavailable").code(), 503);
        assert!(AiServiceError::http(404, "gone").is_not_found());
        assert!(AiServiceError::new(AiServiceError::NOT_FOUND, "gone").is_not_found());
    }

    /// NACOS-05: `releaseAgentCard` carries the endpoint type, like
    /// `releaseAgentCard(card, A2A_ENDPOINT_TYPE_SERVICE, setAsLatest)`.
    #[tokio::test]
    async fn the_release_carries_the_endpoint_type() {
        let transport = FakeTransport::new(vec![
            HttpResponse::new(200, "ok"),
            HttpResponse::new(200, "ok"),
        ]);
        let service = service(Arc::clone(&transport));
        let card = NacosAgentCard::new("local", "http://agent:9000");
        service
            .release_agent_card(&card, A2A_ENDPOINT_TYPE_SERVICE, false)
            .await
            .unwrap();
        // The type is a parameter, so a caller can publish another Nacos endpoint type.
        service
            .release_agent_card(&card, "agent", false)
            .await
            .unwrap();

        let requests = transport.requests();
        assert!(
            requests[0].url.contains("endpointType=service&setAsLatest=false"),
            "{}",
            requests[0].url
        );
        assert!(requests[1].url.contains("endpointType=agent"), "{}", requests[1].url);
    }

    /// NACOS-06: a retryable status is retried and the last attempt's result is returned.
    #[tokio::test]
    async fn a_retryable_failure_is_retried_until_it_succeeds() {
        let transport = FakeTransport::new(vec![
            HttpResponse::new(500, "boom"),
            HttpResponse::new(503, "still down"),
            HttpResponse::new(200, card_json("remote")),
        ]);
        let service = NacosHttpAiService::new(
            NacosHttpConfig::new("http://nacos:8848/nacos", "public")
                .with_access_token("tok")
                .with_retry(RetryPolicy::new(3, Duration::ZERO, Duration::ZERO)),
            transport.clone(),
        );

        let card = service
            .get_agent_card("remote", None)
            .await
            .unwrap()
            .expect("a card after two failures");
        assert_eq!(card.name, "remote");
        assert_eq!(transport.requests().len(), 3);
    }

    /// NACOS-06: a terminal failure (business code / `4xx`) is not retried, and the retry
    /// budget is honoured when every attempt fails.
    #[tokio::test]
    async fn terminal_failures_are_not_retried_and_the_budget_is_finite() {
        // `404` is terminal: exactly one request.
        let transport = FakeTransport::new(vec![HttpResponse::new(404, "")]);
        let service = NacosHttpAiService::new(
            NacosHttpConfig::new("http://nacos:8848/nacos", "public")
                .with_retry(RetryPolicy::new(3, Duration::ZERO, Duration::ZERO)),
            transport.clone(),
        );
        assert!(service.get_agent_card("missing", None).await.unwrap().is_none());
        assert_eq!(transport.requests().len(), 1);

        // Every attempt fails with a retryable status: the last response is surfaced.
        let transport = FakeTransport::new(vec![
            HttpResponse::new(500, "boom"),
            HttpResponse::new(500, "boom"),
            HttpResponse::new(500, "boom"),
            HttpResponse::new(200, card_json("unused")),
        ]);
        let service = NacosHttpAiService::new(
            NacosHttpConfig::new("http://nacos:8848/nacos", "public")
                .with_retry(RetryPolicy::new(3, Duration::ZERO, Duration::ZERO)),
            transport.clone(),
        );
        let error = service.get_agent_card("remote", None).await.unwrap_err();
        assert_eq!(error.code(), 500);
        assert!(error.is_retryable());
        assert_eq!(transport.requests().len(), 3);
    }

    /// NACOS-06: a transport failure (e.g. an expired request timeout) is retryable and is
    /// surfaced as [`AiServiceError::Transport`] once the budget is spent.
    #[tokio::test]
    async fn a_transport_timeout_is_retryable() {
        let transport = FakeTransport::new(Vec::new());
        let service = NacosHttpAiService::new(
            NacosHttpConfig::new("http://nacos:8848/nacos", "public")
                .with_retry(RetryPolicy::new(2, Duration::ZERO, Duration::ZERO)),
            transport.clone(),
        );
        let error = service.get_agent_card("remote", None).await.unwrap_err();
        assert!(matches!(error, AiServiceError::Transport { .. }), "{error:?}");
        assert!(error.is_retryable());
        // Two attempts: the fake transport answers "no queued response" (a transport failure).
        assert_eq!(transport.requests().len(), 2);
    }

    /// NACOS-06: the backoff schedule is exponential and capped.
    #[test]
    fn the_retry_policy_backs_off_exponentially_and_stops_at_the_budget() {
        let policy = RetryPolicy::new(
            4,
            Duration::from_millis(100),
            Duration::from_millis(300),
        );
        assert_eq!(policy.backoff(0), Duration::ZERO);
        assert_eq!(policy.backoff(1), Duration::from_millis(100));
        assert_eq!(policy.backoff(2), Duration::from_millis(200));
        // Capped at `max_backoff`.
        assert_eq!(policy.backoff(3), Duration::from_millis(300));
        assert_eq!(policy.backoff(10), Duration::from_millis(300));

        assert!(policy.should_retry(1));
        assert!(policy.should_retry(3));
        assert!(!policy.should_retry(4));

        assert_eq!(RetryPolicy::default().max_attempts, DEFAULT_RETRY_MAX_ATTEMPTS);
        assert!(!RetryPolicy::none().should_retry(1));
    }
}
