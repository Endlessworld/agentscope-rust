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
//! ## Endpoint assumptions
//!
//! Nacos instances differ in prefix (`/nacos`) and in how strictly the AI OpenAPI is
//! versioned, so every path is configurable through [`NacosHttpConfig`] while defaulting to
//! the Nacos 3.x layout. Responses are accepted both bare (`{...card...}`) and wrapped in the
//! Nacos envelope (`{"code":0,"data":{...}}`), and a `404` maps to `Ok(None)` for the
//! `get_*` methods (absent) and to [`AiServiceError::not_found`] for the `subscribe_*` ones.
//!
//! ## Subscriptions
//!
//! Nacos pushes updates over gRPC; this HTTP client cannot hold such a stream, so
//! `subscribe_*` performs the initial fetch, remembers the listener and exposes
//! [`NacosHttpAiService::poll_once`] for change detection. An embedder that needs push-like
//! behaviour drives `poll_once` from its own loop (or `spawn_polling` from the `http-client`
//! feature).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;

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

/// Configuration for [`NacosHttpAiService`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NacosHttpConfig {
    /// Base URL including the context path, e.g. `http://127.0.0.1:8848/nacos`.
    pub base_url: String,
    /// Nacos `accessToken` query parameter, appended to every request when set.
    pub access_token: Option<String>,
    /// Nacos namespace (`namespaceId`), sent on the mutating calls.
    pub namespace: Option<String>,
    /// AgentCard path.
    pub agent_path: String,
    /// Agent endpoint path.
    pub agent_endpoint_path: String,
    /// Prompt path.
    pub prompt_path: String,
    /// Skill ZIP path.
    pub skill_path: String,
}

impl NacosHttpConfig {
    /// Configuration with the Nacos 3.x default paths.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            access_token: None,
            namespace: None,
            agent_path: "/v1/ai/agent".to_string(),
            agent_endpoint_path: "/v1/ai/agent/endpoint".to_string(),
            prompt_path: "/v1/ai/prompt".to_string(),
            skill_path: "/v1/ai/skill".to_string(),
        }
    }

    /// Sets the `accessToken` query parameter.
    pub fn with_access_token(mut self, token: impl Into<String>) -> Self {
        self.access_token = Some(token.into());
        self
    }

    /// Sets the namespace used by the mutating calls.
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }
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

    // ---- internals ----

    /// Builds a full URL with the query parameters (and the `accessToken`, when configured).
    fn url(&self, path: &str, query: &[(&str, String)]) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        let mut pairs: Vec<String> = query
            .iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(key, value)| format!("{}={}", key, urlencode(value)))
            .collect();
        if let Some(token) = &self.config.access_token {
            pairs.push(format!("accessToken={}", urlencode(token)));
        }
        if pairs.is_empty() {
            format!("{base}{path}")
        } else {
            format!("{base}{path}?{}", pairs.join("&"))
        }
    }

    async fn send(&self, request: HttpRequest) -> AiServiceResult<HttpResponse> {
        self.transport
            .execute(request)
            .await
            .map_err(|e| AiServiceError::new(500, format!("nacos http transport failed: {e}")))
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
        let value: Value = serde_json::from_slice(&response.body).map_err(|e| {
            AiServiceError::new(500, format!("invalid nacos response: {e}"))
        })?;
        Ok(Some(unwrap_envelope(value)))
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
        let card = serde_json::from_value::<NacosAgentCard>(value).map_err(|e| {
            AiServiceError::new(500, format!("invalid agent card payload: {e}"))
        })?;
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
        set_as_latest: bool,
    ) -> AiServiceResult<()> {
        let url = self.url(
            &self.config.agent_path,
            &[
                ("setAsLatest", set_as_latest.to_string()),
                (
                    "namespaceId",
                    self.config.namespace.clone().unwrap_or_default(),
                ),
            ],
        );
        let body = serde_json::to_vec(card)
            .map_err(|e| AiServiceError::new(AiServiceError::INVALID_PARAM, e.to_string()))?;
        let response = self.send(HttpRequest::post_json(url, body)).await?;
        if !response.is_success() {
            return Err(error_from_response(&response));
        }
        Ok(())
    }

    async fn register_agent_endpoint(
        &self,
        agent_name: &str,
        endpoint: &NacosAgentEndpoint,
    ) -> AiServiceResult<()> {
        let url = self.url(
            &self.config.agent_endpoint_path,
            &[
                ("agentName", agent_name.to_string()),
                (
                    "namespaceId",
                    self.config.namespace.clone().unwrap_or_default(),
                ),
            ],
        );
        let body = serde_json::to_vec(endpoint)
            .map_err(|e| AiServiceError::new(AiServiceError::INVALID_PARAM, e.to_string()))?;
        let response = self.send(HttpRequest::post_json(url, body)).await?;
        if !response.is_success() {
            return Err(error_from_response(&response));
        }
        Ok(())
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
fn unwrap_envelope(value: Value) -> Value {
    match value {
        Value::Object(mut map) => {
            let has_code = map.contains_key("code") || map.contains_key("success");
            if has_code {
                if let Some(data) = map.remove("data") {
                    return data;
                }
            }
            Value::Object(map)
        }
        other => other,
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

/// Maps an error response onto [`AiServiceError`], preserving a Nacos `code`/`message` body.
fn error_from_response(response: &HttpResponse) -> AiServiceError {
    let text = response.text();
    let parsed: Option<Value> = serde_json::from_str(&text).ok();
    let code = parsed
        .as_ref()
        .and_then(|v| v.get("code"))
        .and_then(|v| v.as_i64())
        .unwrap_or(i64::from(response.status));
    let message = parsed
        .as_ref()
        .and_then(|v| v.get("message"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| text.trim().to_string());
    AiServiceError::new(code, message)
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
    use crate::service::InMemoryNacosAiService;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
            NacosHttpConfig::new("http://nacos:8848/nacos/").with_access_token("tok en"),
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
            "http://nacos:8848/nacos/v1/ai/agent?agentName=remote&version=1.2&accessToken=tok%20en"
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
        assert!(error.message.contains("invalid agent card payload"));
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
        service.release_agent_card(&card, true).await.unwrap();

        let request = &transport.requests()[0];
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.url,
            "http://nacos:8848/nacos/v1/ai/agent?setAsLatest=true&accessToken=tok%20en"
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
        let error = service.release_agent_card(&card, false).await.unwrap_err();
        assert_eq!(error.code, 12001);
        assert_eq!(error.message, "invalid param");
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
            NacosHttpConfig::new("http://nacos:8848/nacos").with_namespace("dev"),
            transport,
        );
        assert_eq!(
            service.url("/v1/ai/prompt", &[("promptKey", "k".into()), ("label", String::new())]),
            "http://nacos:8848/nacos/v1/ai/prompt?promptKey=k"
        );
    }
}
