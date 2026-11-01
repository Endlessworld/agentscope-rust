//! Trinity training backend: DTOs, HTTP client and model adapter.
//!
//! Rust port of `io.agentscope.core.training.backend`. Trinity's Chat API is
//! OpenAI-compatible, so it is reached through a normal chat model; this module
//! only wraps the Trinity-specific Feedback and Commit APIs plus a thin adapter
//! that collects the `msg_id`s emitted by Trinity responses.

use std::sync::Arc;

use futures::StreamExt;
use serde::Serialize;

use agentscope_core::error::{Error, Result};
use agentscope_core::message::Msg;
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};

pub mod dto {
    //! Request/response payloads for the Trinity APIs.
    use serde::{Deserialize, Serialize};

    /// Trinity Commit API request, mirroring `CommitRequest`.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub struct CommitRequest {
        #[serde(skip_serializing_if = "Option::is_none")]
        pub task_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub run_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub time_threshold: Option<i64>,
    }

    impl CommitRequest {
        /// Returns a new builder.
        pub fn builder() -> CommitRequestBuilder {
            CommitRequestBuilder::default()
        }
    }

    /// Builder for [`CommitRequest`].
    #[derive(Debug, Clone, Default)]
    pub struct CommitRequestBuilder {
        task_id: Option<String>,
        run_id: Option<String>,
        time_threshold: Option<i64>,
    }

    impl CommitRequestBuilder {
        pub fn task_id(mut self, task_id: impl Into<String>) -> Self {
            self.task_id = Some(task_id.into());
            self
        }
        pub fn run_id(mut self, run_id: impl Into<String>) -> Self {
            self.run_id = Some(run_id.into());
            self
        }
        pub fn time_threshold(mut self, time_threshold: i64) -> Self {
            self.time_threshold = Some(time_threshold);
            self
        }
        pub fn build(self) -> CommitRequest {
            CommitRequest {
                task_id: self.task_id,
                run_id: self.run_id,
                time_threshold: self.time_threshold,
            }
        }
    }

    /// Trinity Feedback API request, mirroring `FeedbackRequest`.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub struct FeedbackRequest {
        #[serde(skip_serializing_if = "Option::is_none")]
        pub msg_ids: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub reward: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub task_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub run_id: Option<String>,
    }

    impl FeedbackRequest {
        /// Returns a new builder.
        pub fn builder() -> FeedbackRequestBuilder {
            FeedbackRequestBuilder::default()
        }
    }

    /// Builder for [`FeedbackRequest`].
    #[derive(Debug, Clone, Default)]
    pub struct FeedbackRequestBuilder {
        msg_ids: Option<Vec<String>>,
        reward: Option<f64>,
        task_id: Option<String>,
        run_id: Option<String>,
    }

    impl FeedbackRequestBuilder {
        pub fn msg_ids<I, S>(mut self, msg_ids: I) -> Self
        where
            I: IntoIterator<Item = S>,
            S: Into<String>,
        {
            self.msg_ids = Some(msg_ids.into_iter().map(Into::into).collect());
            self
        }
        pub fn reward(mut self, reward: f64) -> Self {
            self.reward = Some(reward);
            self
        }
        pub fn task_id(mut self, task_id: impl Into<String>) -> Self {
            self.task_id = Some(task_id.into());
            self
        }
        pub fn run_id(mut self, run_id: impl Into<String>) -> Self {
            self.run_id = Some(run_id.into());
            self
        }
        pub fn build(self) -> FeedbackRequest {
            FeedbackRequest {
                msg_ids: self.msg_ids,
                reward: self.reward,
                task_id: self.task_id,
                run_id: self.run_id,
            }
        }
    }

    /// Trinity common status response, mirroring `StatusResponse`.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub struct StatusResponse {
        #[serde(default)]
        pub status: String,
        #[serde(default)]
        pub message: String,
    }

    impl StatusResponse {
        /// Creates a new status response.
        pub fn new(status: impl Into<String>, message: impl Into<String>) -> Self {
            Self {
                status: status.into(),
                message: message.into(),
            }
        }

        /// The status string.
        pub fn status(&self) -> &str {
            &self.status
        }

        /// The status message.
        pub fn message(&self) -> &str {
            &self.message
        }

        /// Whether the status equals `success` (case-insensitive).
        pub fn is_success(&self) -> bool {
            self.status.eq_ignore_ascii_case("success")
        }
    }
}

use dto::{CommitRequest, FeedbackRequest, StatusResponse};

/// Trinity backend client, mirroring `TrinityClient`.
///
/// Only wraps the Trinity-specific Feedback and Commit APIs; chat is handled by
/// a normal OpenAI-compatible model.
#[derive(Clone)]
pub struct TrinityClient {
    base_url: String,
    http: reqwest::Client,
    timeout: std::time::Duration,
}

impl TrinityClient {
    /// Returns a new builder.
    pub fn builder() -> TrinityClientBuilder {
        TrinityClientBuilder::default()
    }

    /// The configured base URL.
    pub fn endpoint(&self) -> &str {
        &self.base_url
    }

    /// Submits reward feedback, mirroring `feedback`.
    pub async fn feedback(&self, request: &FeedbackRequest) -> Result<()> {
        let url = format!("{}/feedback", self.base_url);
        self.post_status(&url, request).await?;
        Ok(())
    }

    /// Triggers a training commit, mirroring `commit`.
    pub async fn commit(&self, request: &CommitRequest) -> Result<()> {
        let url = format!("{}/commit", self.base_url);
        self.post_status(&url, request).await?;
        Ok(())
    }

    async fn post_status<T: Serialize>(&self, url: &str, body: &T) -> Result<StatusResponse> {
        let resp = self
            .http
            .post(url)
            .timeout(self.timeout)
            .json(body)
            .send()
            .await
            .map_err(|e| Error::Http(format!("Trinity request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(Error::http_status(
                status.as_u16(),
                format!("Trinity API failed: {status}"),
            ));
        }

        let status_response: StatusResponse = resp
            .json()
            .await
            .map_err(|e| Error::Http(format!("Trinity response decode failed: {e}")))?;
        Ok(status_response)
    }
}

/// Builder for [`TrinityClient`], mirroring `TrinityClient.Builder`.
#[derive(Debug, Clone)]
pub struct TrinityClientBuilder {
    endpoint: Option<String>,
    timeout: std::time::Duration,
}

impl Default for TrinityClientBuilder {
    fn default() -> Self {
        Self {
            endpoint: None,
            timeout: std::time::Duration::from_secs(300),
        }
    }
}

impl TrinityClientBuilder {
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }
    pub fn timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }
    /// Builds the client, requiring a non-empty endpoint.
    pub fn build(self) -> Result<TrinityClient> {
        let endpoint = self
            .endpoint
            .filter(|e| !e.is_empty())
            .ok_or_else(|| Error::Agent("endpoint is required".to_string()))?;
        Ok(TrinityClient {
            base_url: endpoint,
            http: reqwest::Client::new(),
            timeout: self.timeout,
        })
    }
}

/// Lightweight adapter that wraps a chat model and records each response `id`
/// into a [`crate::runner::RunExecutionContext`], mirroring `TrinityModelAdapter`.
///
/// Composition is used rather than re-implementing a model: the adapter delegates
/// to an inner [`Model`] and taps its stream to collect `msg_id`s.
pub struct TrinityModelAdapter {
    delegate: Arc<dyn Model>,
    execution_context: Option<Arc<crate::runner::RunExecutionContext>>,
    model_name: String,
}

impl TrinityModelAdapter {
    /// Returns a new builder.
    pub fn builder() -> TrinityModelAdapterBuilder {
        TrinityModelAdapterBuilder::default()
    }

    /// The model name reported by the wrapped model.
    pub fn get_model_name(&self) -> &str {
        &self.model_name
    }

    /// The execution context that receives collected `msg_id`s (if any).
    pub fn get_execution_context(&self) -> Option<&Arc<crate::runner::RunExecutionContext>> {
        self.execution_context.as_ref()
    }
}

impl Model for TrinityModelAdapter {
    fn stream(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, Result<ChatResponse>> {
        // Java `TrinityModelAdapter.Builder#build` constructs the delegate with
        // `.stream(false)` — "Trinity doesn't support streaming, force disable".
        // Rust delegates to an arbitrary `Model`, so the same constraint is applied to
        // the options handed to it (a delegate that honours `options.stream` will send a
        // non-streaming request).
        let mut options = options;
        options.stream = Some(false);

        let ctx = self.execution_context.clone();
        // With a non-streaming backend one id arrives per call. A delegate that still
        // yields N chunks repeats the same id; Java would have recorded it once, so
        // collapse consecutive duplicates.
        let mut last_recorded: Option<String> = None;
        let inner = self.delegate.stream(messages, tools, options);
        Box::pin(inner.map(move |item| {
            if let Ok(resp) = &item {
                if !resp.id.is_empty() && last_recorded.as_deref() != Some(resp.id.as_str()) {
                    last_recorded = Some(resp.id.clone());
                    match &ctx {
                        Some(context) => context.add_msg_id(resp.id.clone()),
                        None => tracing::warn!(
                            "No execution context available, msg_id not recorded: {}",
                            resp.id
                        ),
                    }
                }
            }
            item
        }))
    }

    fn model_name(&self) -> &str {
        &self.model_name
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

    fn max_output_tokens(&self) -> usize {
        self.delegate.max_output_tokens()
    }

    fn descriptor(&self) -> Option<&agentscope_core::model::ModelDescriptor> {
        self.delegate.descriptor()
    }
}

/// Builder for [`TrinityModelAdapter`].
///
/// Mirrors the Java builder: the adapter either wraps an already-constructed
/// delegate [`Model`] (Rust-native composition, handy for tests) or builds its
/// own OpenAI-compatible model from `base_url`/`model_name`/`api_key`, exactly
/// like the Java implementation does with `OpenAIChatModel.builder()`.
#[derive(Clone)]
pub struct TrinityModelAdapterBuilder {
    delegate: Option<Arc<dyn Model>>,
    base_url: Option<String>,
    model_name: Option<String>,
    api_key: Option<String>,
    execution_context: Option<Arc<crate::runner::RunExecutionContext>>,
}

impl Default for TrinityModelAdapterBuilder {
    fn default() -> Self {
        Self {
            delegate: None,
            base_url: None,
            model_name: None,
            // Trinity deployments usually do not require a real key.
            api_key: Some("dummy".to_string()),
            execution_context: None,
        }
    }
}

impl TrinityModelAdapterBuilder {
    /// Sets the inner model to wrap (mutually exclusive with `base_url`).
    pub fn delegate(mut self, delegate: Arc<dyn Model>) -> Self {
        self.delegate = Some(delegate);
        self
    }

    /// Sets the Trinity OpenAI-compatible base URL (e.g. `{endpoint}/v1`).
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// Sets the training model name/path (defaults to `training-model`).
    pub fn model_name(mut self, model_name: impl Into<String>) -> Self {
        self.model_name = Some(model_name.into());
        self
    }

    /// Sets the Trinity API key (defaults to `dummy`).
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Sets the run execution context that receives collected `msg_id`s.
    pub fn execution_context(mut self, ctx: Arc<crate::runner::RunExecutionContext>) -> Self {
        self.execution_context = Some(ctx);
        self
    }

    /// Builds the adapter.
    ///
    /// When no delegate is supplied, an OpenAI-compatible model is created from
    /// `base_url`/`model_name`/`api_key`, mirroring the Java builder.
    pub fn build(self) -> Result<TrinityModelAdapter> {
        let TrinityModelAdapterBuilder {
            delegate,
            base_url,
            model_name,
            api_key,
            execution_context,
        } = self;

        let (delegate, model_name): (Arc<dyn Model>, String) = match delegate {
            Some(d) => {
                let name = model_name.unwrap_or_else(|| d.model_name().to_string());
                (d, name)
            }
            None => {
                let base_url = base_url.filter(|u| !u.is_empty()).ok_or_else(|| {
                    Error::Agent(
                        "TrinityModelAdapter requires either a delegate model or a base_url"
                            .to_string(),
                    )
                })?;
                let name = model_name.unwrap_or_else(|| "training-model".to_string());
                let model = agentscope_extensions_model::OpenAiChatModel::new(
                    name.clone(),
                    api_key,
                    Some(base_url),
                )?;
                (Arc::new(model), name)
            }
        };

        Ok(TrinityModelAdapter {
            delegate,
            execution_context,
            model_name,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::ChatUsage;
    use futures::stream;

    #[test]
    fn commit_request_snake_case_roundtrip() {
        let req = CommitRequest::builder()
            .task_id("t1")
            .run_id("0")
            .time_threshold(100)
            .build();
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["task_id"], "t1");
        assert_eq!(json["run_id"], "0");
        assert_eq!(json["time_threshold"], 100);
        let back: CommitRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn feedback_request_snake_case_roundtrip() {
        let req = FeedbackRequest::builder()
            .msg_ids(["m1", "m2"])
            .reward(0.8)
            .task_id("t1")
            .run_id("1")
            .build();
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["msg_ids"][0], "m1");
        assert_eq!(json["reward"], 0.8);
        let back: FeedbackRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn status_response_success() {
        let s = StatusResponse::new("success", "OK");
        assert!(s.is_success());
        assert!(!StatusResponse::new("error", "bad").is_success());
        assert!(StatusResponse::new("SUCCESS", "ok").is_success());
    }

    #[test]
    fn client_requires_endpoint() {
        assert!(TrinityClient::builder().build().is_err());
        assert!(TrinityClient::builder().endpoint("").build().is_err());
        let c = TrinityClient::builder()
            .endpoint("http://localhost:8080")
            .build()
            .unwrap();
        assert_eq!(c.endpoint(), "http://localhost:8080");
    }

    struct FakeModel;

    impl Model for FakeModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<ToolSchema>,
            _options: GenerateOptions,
        ) -> futures::stream::BoxStream<'_, Result<ChatResponse>> {
            let r = ChatResponse::new("resp-42", vec![], ChatUsage::default(), None, None);
            Box::pin(stream::once(async move { Ok(r) }))
        }
        fn model_name(&self) -> &str {
            "fake"
        }
    }

    /// Delegate advertising every capability the adapter must forward (TRAIN-06).
    struct CapableModel {
        descriptor: agentscope_core::model::ModelDescriptor,
    }

    impl Model for CapableModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<ToolSchema>,
            _options: GenerateOptions,
        ) -> futures::stream::BoxStream<'_, Result<ChatResponse>> {
            Box::pin(stream::empty())
        }
        fn model_name(&self) -> &str {
            "capable"
        }
        fn supports_native_structured_output(&self) -> bool {
            true
        }
        fn supports_native_structured_output_with_tools(&self) -> bool {
            true
        }
        fn supports_tool_choice_specific(&self) -> bool {
            true
        }
        fn context_window_size(&self) -> usize {
            131_072
        }
        fn max_output_tokens(&self) -> usize {
            8_192
        }
        fn descriptor(&self) -> Option<&agentscope_core::model::ModelDescriptor> {
            Some(&self.descriptor)
        }
    }

    #[test]
    fn adapter_forwards_delegate_capabilities() {
        use agentscope_core::model::{ModelKind, ModelStatus};
        let descriptor = agentscope_core::model::ModelDescriptor {
            key: "demo/model".to_string(),
            provider: "demo".to_string(),
            id: "model".to_string(),
            name: "Demo".to_string(),
            description: None,
            family: None,
            kind: ModelKind::Chat,
            context_window: 131_072,
            max_output: 8_192,
            input_modalities: Default::default(),
            output_modalities: Default::default(),
            attachment: false,
            reasoning: false,
            reasoning_options: vec![],
            interleaved_field: None,
            tool_call: true,
            structured_output: Some(true),
            temperature: true,
            status: ModelStatus::Active,
            open_weights: false,
            cost: None,
            canonical: None,
            use_cases: Default::default(),
            fingerprint: 0,
        };
        let adapter = TrinityModelAdapter::builder()
            .delegate(Arc::new(CapableModel { descriptor }))
            .build()
            .unwrap();
        // Shadow runs copy the adapter into a cloned agent; every capability bit has to
        // survive that hop or the clone degrades to the trait defaults.
        assert!(adapter.supports_native_structured_output());
        assert!(adapter.supports_native_structured_output_with_tools());
        assert!(adapter.supports_tool_choice_specific());
        assert_eq!(adapter.context_window_size(), 131_072);
        assert_eq!(adapter.max_output_tokens(), 8_192);
        assert_eq!(
            adapter.descriptor().map(|d| d.key.as_str()),
            Some("demo/model")
        );
    }

    #[tokio::test]
    async fn adapter_collects_msg_ids() {
        use crate::runner::RunExecutionContext;
        let ctx = Arc::new(RunExecutionContext::create("task-1", "0"));
        let adapter = TrinityModelAdapter::builder()
            .delegate(Arc::new(FakeModel))
            .execution_context(ctx.clone())
            .build()
            .unwrap();
        let mut s = adapter.stream(vec![], vec![], GenerateOptions::default());
        while let Some(item) = s.next().await {
            item.unwrap();
        }
        assert_eq!(ctx.get_msg_ids(), vec!["resp-42".to_string()]);
        assert_eq!(adapter.get_model_name(), "fake");
        assert!(adapter.get_execution_context().is_some());
    }

    /// Records the options it receives and emits `chunks` responses sharing one id.
    struct RecordingModel {
        received: Arc<std::sync::Mutex<Option<GenerateOptions>>>,
        chunks: usize,
        id: &'static str,
    }

    impl Model for RecordingModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<ToolSchema>,
            options: GenerateOptions,
        ) -> futures::stream::BoxStream<'_, Result<ChatResponse>> {
            *self.received.lock().unwrap() = Some(options);
            let id = self.id.to_string();
            let responses: Vec<Result<ChatResponse>> = (0..self.chunks)
                .map(|_| {
                    Ok(ChatResponse::new(
                        id.clone(),
                        vec![],
                        ChatUsage::default(),
                        None,
                        None,
                    ))
                })
                .collect();
            Box::pin(stream::iter(responses))
        }

        fn model_name(&self) -> &str {
            "recording"
        }
    }

    // Java: `TrinityModelAdapter` builds the delegate with `.stream(false)` —
    // "Trinity doesn't support streaming, force disable".
    #[tokio::test]
    async fn adapter_forces_stream_off() {
        let received = Arc::new(std::sync::Mutex::new(None));
        let adapter = TrinityModelAdapter::builder()
            .delegate(Arc::new(RecordingModel {
                received: received.clone(),
                chunks: 1,
                id: "resp-1",
            }))
            .build()
            .unwrap();
        let mut s = adapter.stream(
            vec![],
            vec![],
            GenerateOptions {
                stream: Some(true),
                ..Default::default()
            },
        );
        while let Some(item) = s.next().await {
            item.unwrap();
        }
        let opts = received.lock().unwrap().take().expect("options captured");
        assert_eq!(opts.stream, Some(false));
    }

    // A streaming delegate repeats one id per chunk; Java records one id per call.
    #[tokio::test]
    async fn adapter_records_duplicate_chunk_ids_once() {
        use crate::runner::RunExecutionContext;
        let ctx = Arc::new(RunExecutionContext::create("task-1", "0"));
        let adapter = TrinityModelAdapter::builder()
            .delegate(Arc::new(RecordingModel {
                received: Arc::new(std::sync::Mutex::new(None)),
                chunks: 3,
                id: "resp-9",
            }))
            .execution_context(ctx.clone())
            .build()
            .unwrap();
        let mut s = adapter.stream(vec![], vec![], GenerateOptions::default());
        while let Some(item) = s.next().await {
            item.unwrap();
        }
        assert_eq!(ctx.get_msg_ids(), vec!["resp-9".to_string()]);
    }

    #[test]
    fn adapter_requires_delegate_or_base_url() {
        assert!(TrinityModelAdapter::builder().build().is_err());
        assert!(TrinityModelAdapter::builder().base_url("").build().is_err());
    }

    #[test]
    fn adapter_builds_model_from_base_url() {
        // Mirrors the Java `builder().baseUrl(...).modelName(...).apiKey(...)` path:
        // the adapter constructs its own OpenAI-compatible model.
        let adapter = TrinityModelAdapter::builder()
            .base_url("http://trinity.local:8010/v1")
            .model_name("Qwen2.5-0.5B-Instruct")
            .api_key("dummy")
            .execution_context(Arc::new(crate::runner::RunExecutionContext::create(
                "t", "0",
            )))
            .build()
            .unwrap();
        assert_eq!(adapter.get_model_name(), "Qwen2.5-0.5B-Instruct");
        assert!(adapter.get_execution_context().is_some());
    }

    // --- Offline mock HTTP server for TrinityClient request construction ---------

    fn header_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n")
    }

    fn content_length(head: &str) -> usize {
        for line in head.lines() {
            if let Some((k, v)) = line.split_once(':') {
                if k.eq_ignore_ascii_case("content-length") {
                    return v.trim().parse().unwrap_or(0);
                }
            }
        }
        0
    }

    /// Accepts a single request and returns `(request_line, body)`, replying with a
    /// success [`StatusResponse`]. Runs entirely on the loopback interface.
    fn spawn_mock_server() -> (String, std::thread::JoinHandle<(String, String)>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 2048];
            loop {
                let n = stream.read(&mut tmp).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(end) = header_end(&buf) {
                    let head = String::from_utf8_lossy(&buf[..end]).to_string();
                    if buf.len() >= end + 4 + content_length(&head) {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&buf).to_string();
            let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
            let request_line = head.lines().next().unwrap_or("").to_string();
            let response_body = r#"{"status":"success","message":"ok"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
            (request_line, body.to_string())
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn client_feedback_posts_snake_case_body() {
        use crate::backend::dto::FeedbackRequest;
        let (url, handle) = spawn_mock_server();
        let client = TrinityClient::builder().endpoint(url).build().unwrap();
        let req = FeedbackRequest::builder()
            .msg_ids(["m1", "m2"])
            .reward(0.75)
            .task_id("task-x")
            .run_id("1")
            .build();
        client.feedback(&req).await.unwrap();
        let (line, body) = handle.join().unwrap();
        assert!(line.starts_with("POST /feedback "), "unexpected: {line}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["msg_ids"][0], "m1");
        assert_eq!(v["msg_ids"][1], "m2");
        assert_eq!(v["reward"], 0.75);
        assert_eq!(v["task_id"], "task-x");
        assert_eq!(v["run_id"], "1");
        assert!(v.get("msgIds").is_none());
    }

    #[tokio::test]
    async fn client_commit_posts_snake_case_body() {
        use crate::backend::dto::CommitRequest;
        let (url, handle) = spawn_mock_server();
        let client = TrinityClient::builder().endpoint(url).build().unwrap();
        let req = CommitRequest::builder()
            .task_id("task-y")
            .run_id("2")
            .time_threshold(100)
            .build();
        client.commit(&req).await.unwrap();
        let (line, body) = handle.join().unwrap();
        assert!(line.starts_with("POST /commit "), "unexpected: {line}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["task_id"], "task-y");
        assert_eq!(v["run_id"], "2");
        assert_eq!(v["time_threshold"], 100);
    }

    #[tokio::test]
    async fn client_reports_non_2xx() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut tmp = [0u8; 2048];
            let _ = stream.read(&mut tmp);
            let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
        });
        let client = TrinityClient::builder()
            .endpoint(format!("http://{addr}"))
            .build()
            .unwrap();
        let err = client
            .commit(&CommitRequest::builder().build())
            .await
            .unwrap_err();
        let _ = handle.join();
        assert!(format!("{err}").contains("500"), "error was: {err}");
    }

    // --- Additional offline coverage -------------------------------------------------

    #[test]
    fn commit_request_omits_unset_fields() {
        let req = CommitRequest::builder().build();
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("task_id").is_none());
        assert!(json.get("run_id").is_none());
        assert!(json.get("time_threshold").is_none());
        // An explicit JSON null decodes to `None` (Jackson parity).
        let back: CommitRequest = serde_json::from_str(r#"{"task_id":null}"#).unwrap();
        assert!(back.task_id.is_none());
        assert_eq!(back, CommitRequest::default());
    }

    #[test]
    fn feedback_request_omits_unset_reward() {
        let req = FeedbackRequest::builder().msg_ids(["m1"]).build();
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("reward").is_none());
        assert!(json.get("task_id").is_none());
        assert_eq!(json["msg_ids"][0], "m1");
    }

    #[test]
    fn status_response_deserializes_and_defaults() {
        let s: StatusResponse = serde_json::from_str(r#"{"status":"success"}"#).unwrap();
        assert!(s.is_success());
        assert_eq!(s.message(), "");
        assert_eq!(s.status(), "success");
        // Missing fields fall back to defaults; default status is not success.
        let s = StatusResponse::default();
        assert!(!s.is_success());
        let s: StatusResponse = serde_json::from_str("{}").unwrap();
        assert!(!s.is_success());
    }

    #[test]
    fn trinity_client_builder_honours_timeout() {
        let c = TrinityClient::builder()
            .endpoint("http://h")
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        assert_eq!(c.endpoint(), "http://h");
    }

    #[tokio::test]
    async fn adapter_without_execution_context_is_ok() {
        // Java logs a warning and keeps streaming when the context is absent.
        let adapter = TrinityModelAdapter::builder()
            .delegate(Arc::new(FakeModel))
            .build()
            .unwrap();
        assert!(adapter.get_execution_context().is_none());
        let mut s = adapter.stream(vec![], vec![], GenerateOptions::default());
        let mut count = 0;
        while let Some(item) = s.next().await {
            item.unwrap();
            count += 1;
        }
        assert_eq!(count, 1);
    }
}
