//! The DashScope [`Model`] implementation.
//!
//! Calls the DashScope native generation / multimodal-generation endpoints
//! through an injected [`HttpTransport`], which keeps the model fully
//! unit-testable without network access.

use std::sync::Arc;

use futures::stream::BoxStream;
use futures::StreamExt;

use agentscope_core::model::catalog::{descriptor_for, ModelDescriptor};
use agentscope_core::{ChatResponse, Error, GenerateOptions, Model, Msg, Result, ToolSchema};

use crate::common::{HttpRequestSpec, SharedTransport};
use crate::dashscope::converter::{
    build_request, is_multimodal_model, to_dashscope_messages_with_options, DEFAULT_BASE_URL,
    MULTIMODAL_GENERATION_ENDPOINT, TEXT_GENERATION_ENDPOINT,
};
use crate::dashscope::credential::EndpointType;
use crate::dashscope::dto::{DashScopeResponse, DashScopeSearchOptions};
use crate::dashscope::encryption::{
    self, DashScopeCryptoProvider, EncryptionConfig, EncryptionContext,
};
use crate::dashscope::parser::{parse_response, DashScopeStreamIncrementer};

/// Public keys endpoint used for request encryption.
pub const PUBLIC_KEYS_ENDPOINT: &str = "/api/v1/public-keys/latest";

/// A chat model backed by the DashScope (Alibaba Cloud) native API.
pub struct DashScopeChatModel {
    model_name: String,
    api_key: String,
    base_url: String,
    endpoint_type: EndpointType,
    default_options: GenerateOptions,
    transport: SharedTransport,
    stream: bool,
    enable_thinking: bool,
    context_window_size: usize,
    descriptor: Option<Arc<ModelDescriptor>>,
    /// `Some(prompt)` switches to `DashScopeMultiAgentFormatter`: agent turns
    /// collapse into one `<history>` user message, tool sequences stay intact.
    multi_agent_prompt: Option<String>,
    /// RSA public key for request encryption (mirrors
    /// `DashScopeChatModel.Builder#enableEncrypt` + `publicKey*`).
    encryption: Option<EncryptionConfig>,
    /// `enableEncrypt` flag: when set without an explicit key, the public key is
    /// fetched from [`PUBLIC_KEYS_ENDPOINT`] on the first encrypted request.
    enable_encrypt: bool,
    /// Endpoint (relative to `base_url`) the public key is fetched from.
    public_keys_endpoint: String,
    /// Public key fetched lazily for `enable_encrypt` (cached after the first fetch).
    resolved_encryption: tokio::sync::OnceCell<EncryptionConfig>,
    /// Crypto primitives used when `encryption` is set.
    crypto: Arc<dyn DashScopeCryptoProvider>,
    /// Web search (`enable_search` + `search_options`), mirroring Java's
    /// `enableSearch` model/provider option.
    enable_search: bool,
    search_options: Option<DashScopeSearchOptions>,
    /// Built-in code interpreter (`enable_code_interpreter`).
    enable_code_interpreter: bool,
    /// User-supplied patterns forcing the multimodal endpoint
    /// (Java `multimodalModelPatterns`), on top of the built-in table.
    multimodal_patterns: Vec<String>,
    /// Native structured output capability bits (Java `nativeStructuredOutput` /
    /// `nativeStructuredOutputWithTools`), on by default.
    native_structured_output: bool,
    native_structured_output_with_tools: bool,
}

impl DashScopeChatModel {
    /// Creates a model with the given name, API key and optional base URL.
    pub fn new(
        model_name: impl Into<String>,
        api_key: impl Into<String>,
        base_url: Option<String>,
        transport: SharedTransport,
    ) -> Self {
        let model_name = model_name.into();
        let configured_base_url = base_url.clone();
        let base_url = configured_base_url
            .clone()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let descriptor = descriptor_for(
            Some("dashscope"),
            &model_name,
            configured_base_url.as_deref(),
        );
        let context_window_size = descriptor
            .as_ref()
            .map(|descriptor| descriptor.context_window)
            .unwrap_or(0);
        Self {
            model_name,
            api_key: api_key.into(),
            base_url,
            endpoint_type: EndpointType::Auto,
            default_options: GenerateOptions::default(),
            transport,
            stream: true,
            enable_thinking: false,
            context_window_size,
            descriptor,
            multi_agent_prompt: None,
            encryption: None,
            enable_encrypt: false,
            public_keys_endpoint: PUBLIC_KEYS_ENDPOINT.to_string(),
            resolved_encryption: tokio::sync::OnceCell::new(),
            crypto: Arc::new(super::crypto::AesRsaCryptoProvider),
            enable_search: false,
            search_options: None,
            enable_code_interpreter: false,
            multimodal_patterns: Vec::new(),
            native_structured_output: true,
            native_structured_output_with_tools: true,
        }
    }

    /// Switches the adapter to the multi-agent formatter, mirroring Java's
    /// `DashScopeChatModel` + `DashScopeMultiAgentFormatter` combination.
    pub fn with_multi_agent_formatter(mut self, prompt: Option<String>) -> Self {
        self.multi_agent_prompt = prompt;
        self
    }

    /// Whether the multi-agent formatter is active.
    pub fn is_multi_agent(&self) -> bool {
        self.multi_agent_prompt.is_some()
    }

    /// Sets the default options merged under every request.
    pub fn with_default_options(mut self, options: GenerateOptions) -> Self {
        self.default_options = options;
        self
    }

    /// Sets the default streaming mode (overridable per request).
    pub fn with_stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    /// Forces a specific endpoint type.
    pub fn with_endpoint_type(mut self, endpoint_type: EndpointType) -> Self {
        self.endpoint_type = endpoint_type;
        self
    }

    /// Enables thinking mode by default.
    pub fn with_enable_thinking(mut self, enable_thinking: bool) -> Self {
        self.enable_thinking = enable_thinking;
        self
    }

    /// Enables web search (`parameters.enable_search`), mirroring Java's
    /// `DashScopeChatModel.Builder#enableSearch(true)`.
    pub fn with_enable_search(mut self, enable: bool) -> Self {
        self.enable_search = enable;
        self
    }

    /// Sets the web-search strategy (`parameters.search_options`).
    pub fn with_search_options(mut self, options: DashScopeSearchOptions) -> Self {
        self.search_options = Some(options);
        self
    }

    /// Enables the built-in code interpreter (`parameters.enable_code_interpreter`).
    pub fn with_enable_code_interpreter(mut self, enable: bool) -> Self {
        self.enable_code_interpreter = enable;
        self
    }

    /// Forces the multimodal endpoint for model ids matching any of `patterns`
    /// (Java `DashScopeHttpClient.Builder#multimodalModelPatterns`).
    ///
    /// Matching is a case-insensitive substring test, so an unanchored Java
    /// pattern such as `qwen-vl` behaves identically.
    pub fn with_multimodal_patterns(mut self, patterns: Vec<String>) -> Self {
        self.multimodal_patterns = patterns;
        self
    }

    /// Sets the native structured-output capability bits (Java
    /// `nativeStructuredOutput` / `nativeStructuredOutputWithTools`).
    pub fn with_native_structured_output(mut self, enabled: bool) -> Self {
        self.native_structured_output = enabled;
        self
    }

    /// Sets whether native structured output is honoured alongside tools.
    pub fn with_native_structured_output_with_tools(mut self, enabled: bool) -> Self {
        self.native_structured_output_with_tools = enabled;
        self
    }

    /// Sets the reported context window size in tokens.
    pub fn with_context_window_size(mut self, size: usize) -> Self {
        if size > 0 {
            self.context_window_size = size;
        }
        self
    }

    /// The resolved base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Whether this model talks to the multimodal endpoint.
    pub fn requires_multimodal_api(&self) -> bool {
        match self.endpoint_type {
            EndpointType::Multimodal => true,
            EndpointType::Text => false,
            EndpointType::Auto => {
                is_multimodal_model(&self.model_name) || self.matches_multimodal_pattern()
            }
        }
    }

    /// Whether the model id matches a user-supplied multimodal pattern.
    fn matches_multimodal_pattern(&self) -> bool {
        let model = self.model_name.to_lowercase();
        self.multimodal_patterns.iter().any(|pattern| {
            let pattern = pattern.trim().to_lowercase();
            !pattern.is_empty() && model.contains(&pattern)
        })
    }

    /// The endpoint path selected for this model.
    pub fn endpoint_path(&self) -> &'static str {
        if self.requires_multimodal_api() {
            MULTIMODAL_GENERATION_ENDPOINT
        } else {
            TEXT_GENERATION_ENDPOINT
        }
    }

    /// Builds the URL + body for a single request.
    ///
    /// Fails when request encryption is enabled and the encryption step cannot
    /// be performed (see [`crate::dashscope::encryption`]); this never silently
    /// downgrades to a plaintext request.
    pub fn build_http_request(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: &GenerateOptions,
        stream: bool,
    ) -> Result<HttpRequestSpec> {
        self.build_http_request_with_context(messages, tools, options, stream)
            .map(|(spec, _)| spec)
    }

    /// Builds the request and returns the encryption context needed to decrypt
    /// the response (`None` when encryption is disabled).
    ///
    /// Synchronous: an `enableEncrypt` model whose public key has not been fetched yet
    /// returns an error here instead of sending plaintext. Use
    /// [`Self::build_http_request_with_context_async`] to resolve the key on demand.
    pub fn build_http_request_with_context(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: &GenerateOptions,
        stream: bool,
    ) -> Result<(HttpRequestSpec, Option<EncryptionContext>)> {
        let config =
            match (&self.encryption, self.enable_encrypt) {
                (Some(config), _) => Some(config.clone()),
                (None, true) => return Err(Error::Model(
                    "DashScope request encryption is enabled (enableEncrypt=true) but no public \
                     key has been resolved; await `resolve_encryption_config()` (or supply a key \
                     with `with_encryption_config`) before building the request — refusing to \
                     send a plaintext request"
                        .to_string(),
                )),
                (None, false) => None,
            };
        self.build_with_encryption_config(messages, tools, options, stream, config.as_ref())
    }

    /// Builds the request after resolving (and caching) the public key when
    /// `enableEncrypt` is set without an explicit key.
    ///
    /// This is the path the model itself uses, so `enableEncrypt` is honoured end to
    /// end: the key is fetched from [`Self::public_keys_endpoint`] on the first call and
    /// a fetch failure is an error, never a plaintext fallback.
    pub async fn build_http_request_with_context_async(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: &GenerateOptions,
        stream: bool,
    ) -> Result<(HttpRequestSpec, Option<EncryptionContext>)> {
        let config = self.resolve_encryption_config().await?;
        self.build_with_encryption_config(messages, tools, options, stream, config.as_ref())
    }

    fn build_with_encryption_config(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: &GenerateOptions,
        stream: bool,
        config: Option<&EncryptionConfig>,
    ) -> Result<(HttpRequestSpec, Option<EncryptionContext>)> {
        let force_multimodal = self.requires_multimodal_api();
        let ds_messages = match &self.multi_agent_prompt {
            Some(prompt) => super::converter::to_multi_agent_messages(
                messages,
                force_multimodal,
                prompt,
                options,
            ),
            None => to_dashscope_messages_with_options(messages, force_multimodal, options),
        };
        let mut request = build_request(
            &self.model_name,
            ds_messages,
            stream,
            options,
            &self.default_options,
            tools,
            options.tool_choice.as_ref(),
            self.enable_thinking,
        );
        // Provider knobs that live on the model (Java `DashScopeChatModel` builder
        // fields) are written into `parameters` here, so they are never confused
        // with per-call `additional_body_params`.
        if let Some(params) = request.parameters.as_mut() {
            if self.enable_search {
                params.enable_search = Some(true);
            }
            if let Some(search_options) = self.search_options.clone() {
                params.search_options = Some(search_options);
            }
            if self.enable_code_interpreter {
                params.enable_code_interpreter = Some(true);
            }
        }
        let mut body = serde_json::to_value(&request).unwrap_or(serde_json::Value::Null);

        // Encrypt the `input` member when configured (mirrors
        // `DashScopeHttpClient#encryptRequestBodyIfNeeded`).
        let mut encryption_context = None;
        if let Some(config) = config {
            encryption_context =
                encryption::encrypt_request_body(&mut body, config, self.crypto.as_ref())?;
        }

        let mut spec =
            HttpRequestSpec::new(format!("{}{}", self.base_url, self.endpoint_path()), body)
                .streaming(stream)
                .provider("dashscope")
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Content-Type", "application/json");
        if stream {
            spec = spec.header("X-DashScope-SSE", "enable");
        }
        if let (Some(config), Some(context)) = (config, &encryption_context) {
            spec = spec.header(
                encryption::ENCRYPTION_KEY_HEADER,
                encryption::encryption_header_value(&config.public_key_id, context)?,
            );
        }
        for (name, value) in &options.additional_headers {
            spec = spec.header(name.clone(), value.clone());
        }
        Ok((spec, encryption_context))
    }

    /// Installs the RSA public key used for request encryption plus the crypto
    /// provider that performs AES-GCM / RSA operations.
    ///
    /// Providing a key implies `enable_encrypt` (Java's `publicKeyId`/`publicKey`
    /// constructor pair): requests are encrypted from now on.
    ///
    /// The provider seam exists so callers can inject an alternative
    /// implementation (e.g. an HSM-backed one); the bundled
    /// [`AesRsaCryptoProvider`](super::crypto::AesRsaCryptoProvider) is what
    /// [`with_encryption_config`](Self::with_encryption_config) installs.
    pub fn with_encryption(
        mut self,
        config: EncryptionConfig,
        provider: Arc<dyn DashScopeCryptoProvider>,
    ) -> Self {
        self.encryption = Some(config);
        self.enable_encrypt = true;
        self.crypto = provider;
        self
    }

    /// Enables request encryption with the bundled
    /// [`AesRsaCryptoProvider`](super::crypto::AesRsaCryptoProvider)
    /// (AES-256-GCM + RSA PKCS#1 v1.5, matching `DashScopeEncryptionUtils`).
    pub fn with_encryption_config(mut self, config: EncryptionConfig) -> Self {
        self.encryption = Some(config);
        self.enable_encrypt = true;
        self.crypto = Arc::new(super::crypto::AesRsaCryptoProvider);
        self
    }

    /// Sets the `enableEncrypt` flag (Java
    /// `DashScopeChatModel.Builder#enableEncrypt`).
    ///
    /// When enabled **without** an explicit public key, the key is fetched from
    /// [`Self::public_keys_endpoint`] on the first request (see
    /// [`Self::resolve_encryption_config`]). A failed fetch is an error: the model
    /// never silently sends a plaintext request.
    pub fn with_encryption_enabled(mut self, enable: bool) -> Self {
        self.enable_encrypt = enable;
        self
    }

    /// Overrides the public-key endpoint (default [`PUBLIC_KEYS_ENDPOINT`],
    /// resolved against `base_url`).
    pub fn with_public_keys_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.public_keys_endpoint = endpoint.into();
        self
    }

    /// Whether request encryption is enabled (`enableEncrypt` or an explicit key).
    pub fn is_encryption_enabled(&self) -> bool {
        self.enable_encrypt || self.encryption.is_some()
    }

    /// Resolves the encryption configuration that will be used for requests.
    ///
    /// Order (mirrors Java `build()`):
    /// 1. an explicit [`with_encryption_config`](Self::with_encryption_config) key wins;
    /// 2. otherwise, when `enableEncrypt` is set, the public key is fetched once from
    ///    `{base_url}{public_keys_endpoint}` and cached;
    /// 3. when encryption is disabled entirely, returns `Ok(None)`.
    ///
    /// Fetch/parse failures are returned as errors — never downgraded to plaintext.
    pub async fn resolve_encryption_config(&self) -> Result<Option<EncryptionConfig>> {
        if let Some(config) = &self.encryption {
            return Ok(Some(config.clone()));
        }
        if !self.enable_encrypt {
            return Ok(None);
        }
        let fetched = self
            .resolved_encryption
            .get_or_try_init(|| async { self.fetch_public_key().await })
            .await?;
        Ok(Some(fetched.clone()))
    }

    /// Fetches `{base_url}{public_keys_endpoint}` and parses the public key pair.
    async fn fetch_public_key(&self) -> Result<EncryptionConfig> {
        let url = format!("{}{}", self.base_url, self.public_keys_endpoint);
        let headers = vec![
            (
                "Authorization".to_string(),
                format!("Bearer {}", self.api_key),
            ),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];
        let body = self
            .transport
            .get_json(&url, &headers, "dashscope")
            .await
            .map_err(|e| {
                Error::Model(format!(
                    "DashScope request encryption is enabled (enableEncrypt=true) but fetching \
                     the public key from {url} failed: {e}"
                ))
            })?;
        EncryptionConfig::from_public_key_response(&body)
    }
}

impl Model for DashScopeChatModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: GenerateOptions,
    ) -> BoxStream<'_, Result<ChatResponse>> {
        let options = crate::common::clamp_max_tokens(options, self.max_output_tokens());
        let stream_enabled = options.stream.unwrap_or(self.stream);
        Box::pin(async_stream::stream! {
            let (spec, encryption_context) =
                match self
                    .build_http_request_with_context_async(
                        messages,
                        tools,
                        &options,
                        stream_enabled,
                    )
                    .await
                {
                    Ok(built) => built,
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                };
            if stream_enabled {
                let mut frames = self.transport.post_json_stream(spec);
                let mut incrementer = DashScopeStreamIncrementer::new();
                while let Some(frame) = frames.next().await {
                    let mut value = match frame {
                        Ok(value) => value,
                        Err(e) => {
                            yield Err(e);
                            return;
                        }
                    };
                    if let Some(context) = &encryption_context {
                        encryption::decrypt_response_body(
                            &mut value,
                            context,
                            self.crypto.as_ref(),
                        );
                    }
                    let response: DashScopeResponse = match serde_json::from_value(value) {
                        Ok(response) => response,
                        Err(e) => {
                            yield Err(Error::from(e));
                            return;
                        }
                    };
                    if let Some(code) = response.code.clone() {
                        yield Err(crate::common::error::ModelError::from_parts(
                            "dashscope",
                            0,
                            response.message.clone().unwrap_or_default(),
                            None,
                            Some(code),
                        )
                        .to_core());
                        return;
                    }
                    if let Some(chunk) = incrementer.accumulate(&response) {
                        yield Ok(chunk);
                    }
                }
                if let Some(tail) = incrementer.into_final() {
                    yield Ok(tail);
                }
            } else {
                match self.transport.post_json(spec).await {
                    Ok(mut value) => {
                        if let Some(context) = &encryption_context {
                            encryption::decrypt_response_body(
                                &mut value,
                                context,
                                self.crypto.as_ref(),
                            );
                        }
                        match serde_json::from_value::<DashScopeResponse>(value) {
                            Ok(response) => {
                                if let Some(code) = response.code.clone() {
                                    yield Err(crate::common::error::ModelError::from_parts(
                                        "dashscope",
                                        0,
                                        response.message.clone().unwrap_or_default(),
                                        None,
                                        Some(code),
                                    )
                                    .to_core());
                                } else {
                                    yield Ok(parse_response(&response));
                                }
                            }
                            Err(e) => yield Err(Error::from(e)),
                        }
                    }
                    Err(e) => yield Err(e),
                }
            }
        })
    }

    fn model_name(&self) -> &str {
        &self.model_name
    }

    fn supports_native_structured_output(&self) -> bool {
        self.native_structured_output
    }

    fn supports_native_structured_output_with_tools(&self) -> bool {
        self.native_structured_output_with_tools
    }

    fn supports_tool_choice_specific(&self) -> bool {
        true
    }

    fn context_window_size(&self) -> usize {
        self.context_window_size
    }

    fn max_output_tokens(&self) -> usize {
        self.descriptor
            .as_ref()
            .map(|descriptor| descriptor.max_output)
            .unwrap_or(0)
    }

    fn descriptor(&self) -> Option<&ModelDescriptor> {
        self.descriptor.as_deref()
    }
}

/// A transport factory so callers can construct models without referencing
/// `reqwest` directly.
pub fn default_transport() -> SharedTransport {
    Arc::new(crate::common::ReqwestTransport::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::MemoryTransport;
    use agentscope_core::{ContentBlock, Msg};

    fn model_with(transport: MemoryTransport, name: &str) -> DashScopeChatModel {
        DashScopeChatModel::new(name, "sk-test", None, Arc::new(transport))
    }

    async fn collect(
        model: &DashScopeChatModel,
        options: GenerateOptions,
    ) -> Vec<Result<ChatResponse>> {
        model
            .stream(vec![Msg::user("hi")], Vec::new(), options)
            .collect()
            .await
    }

    #[tokio::test]
    async fn non_streaming_request_hits_text_endpoint() {
        let transport = MemoryTransport::with_response(serde_json::json!({
            "request_id": "req-1",
            "output": { "choices": [{ "finish_reason": "stop", "message": { "role": "assistant", "content": "Hi there" } }] },
            "usage": { "input_tokens": 4, "output_tokens": 7 }
        }));
        let model = model_with(transport.clone(), "qwen-plus");
        let out = collect(
            &model,
            GenerateOptions {
                stream: Some(false),
                ..Default::default()
            },
        )
        .await;

        assert_eq!(out.len(), 1);
        let response = out[0].as_ref().unwrap();
        assert!(matches!(&response.content[0], ContentBlock::Text(t) if t.text == "Hi there"));
        assert_eq!(response.usage.output_tokens, 7);

        let spec = transport.last_request().unwrap();
        assert_eq!(
            spec.url,
            "https://dashscope.aliyuncs.com/api/v1/services/aigc/text-generation/generation"
        );
        assert!(!spec.stream);
        assert_eq!(spec.body["model"], serde_json::json!("qwen-plus"));
        assert_eq!(
            spec.body["input"]["messages"][0]["role"],
            serde_json::json!("user")
        );
        assert!(spec
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "Bearer sk-test"));
    }

    #[tokio::test]
    async fn multimodal_model_hits_multimodal_endpoint() {
        let transport = MemoryTransport::with_response(serde_json::json!({
            "request_id": "req-2",
            "output": { "choices": [{ "message": { "content": "seen" } }] }
        }));
        let model = model_with(transport.clone(), "qwen-vl-plus");
        let _ = collect(
            &model,
            GenerateOptions {
                stream: Some(false),
                ..Default::default()
            },
        )
        .await;

        let spec = transport.last_request().unwrap();
        assert_eq!(
            spec.url,
            "https://dashscope.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation"
        );
    }

    #[tokio::test]
    async fn streaming_yields_incremental_chunks_then_flushes() {
        let transport = MemoryTransport::with_stream(vec![
            serde_json::json!({ "request_id": "r", "output": { "choices": [{ "message": { "content": "Hel" } }] } }),
            serde_json::json!({ "output": { "choices": [{ "message": { "content": "lo" } }] } }),
            serde_json::json!({
                "output": { "choices": [{ "finish_reason": "stop", "message": { "content": "" } }] },
                "usage": { "input_tokens": 2, "output_tokens": 3 }
            }),
        ]);
        let model = model_with(transport.clone(), "qwen-plus");
        let out = collect(&model, GenerateOptions::default()).await;

        assert_eq!(out.len(), 3);
        assert!(
            matches!(&out[0].as_ref().unwrap().content[0], ContentBlock::Text(t) if t.text == "Hel")
        );
        assert!(
            matches!(&out[1].as_ref().unwrap().content[0], ContentBlock::Text(t) if t.text == "lo")
        );
        assert_eq!(out[2].as_ref().unwrap().usage.output_tokens, 3);
        let spec = transport.last_request().unwrap();
        assert!(spec.stream);
        assert!(spec
            .headers
            .iter()
            .any(|(k, v)| k == "X-DashScope-SSE" && v == "enable"));
    }

    #[tokio::test]
    async fn error_payload_surfaces_as_model_error() {
        let transport = MemoryTransport::with_response(serde_json::json!({
            "code": "InvalidApiKey",
            "message": "bad key"
        }));
        let model = model_with(transport, "qwen-plus");
        let out = collect(
            &model,
            GenerateOptions {
                stream: Some(false),
                ..Default::default()
            },
        )
        .await;
        let err = out[0].as_ref().unwrap_err().to_string();
        assert!(err.contains("InvalidApiKey"), "unexpected error: {err}");
    }

    #[test]
    fn endpoint_type_override_wins_over_auto_detection() {
        let model = DashScopeChatModel::new(
            "qwen-vl-plus",
            "sk",
            None,
            Arc::new(MemoryTransport::default()),
        )
        .with_endpoint_type(EndpointType::Text);
        assert!(!model.requires_multimodal_api());
        assert_eq!(model.endpoint_path(), TEXT_GENERATION_ENDPOINT);

        let forced = DashScopeChatModel::new(
            "qwen-plus",
            "sk",
            None,
            Arc::new(MemoryTransport::default()),
        )
        .with_endpoint_type(EndpointType::Multimodal);
        assert!(forced.requires_multimodal_api());
    }

    #[test]
    fn search_and_code_interpreter_knobs_reach_parameters() {
        let model = DashScopeChatModel::new(
            "qwen-plus",
            "sk",
            None,
            Arc::new(MemoryTransport::default()),
        )
        .with_enable_search(true)
        .with_search_options(DashScopeSearchOptions {
            forced_search: Some(true),
            ..Default::default()
        })
        .with_enable_code_interpreter(true);
        let spec = model
            .build_http_request(
                vec![Msg::user("hi")],
                Vec::new(),
                &GenerateOptions::default(),
                false,
            )
            .unwrap();
        assert_eq!(spec.body["parameters"]["enable_search"], serde_json::json!(true));
        assert_eq!(
            spec.body["parameters"]["search_options"]["forced_search"],
            serde_json::json!(true)
        );
        assert_eq!(
            spec.body["parameters"]["enable_code_interpreter"],
            serde_json::json!(true)
        );
    }

    #[test]
    fn user_multimodal_patterns_force_the_multimodal_endpoint() {
        let model = DashScopeChatModel::new(
            "acme-vision-7b",
            "sk",
            None,
            Arc::new(MemoryTransport::default()),
        );
        assert!(!model.requires_multimodal_api());

        let model = model.with_multimodal_patterns(vec!["Vision".to_string()]);
        assert!(model.requires_multimodal_api());
        assert_eq!(model.endpoint_path(), MULTIMODAL_GENERATION_ENDPOINT);

        // An explicit text endpoint still wins over the user pattern.
        let text = DashScopeChatModel::new(
            "acme-vision-7b",
            "sk",
            None,
            Arc::new(MemoryTransport::default()),
        )
        .with_endpoint_type(EndpointType::Text)
        .with_multimodal_patterns(vec!["vision".to_string()]);
        assert!(!text.requires_multimodal_api());
    }

    #[test]
    fn native_structured_output_capabilities_follow_the_options() {
        let model = DashScopeChatModel::new(
            "qwen-plus",
            "sk",
            None,
            Arc::new(MemoryTransport::default()),
        );
        assert!(model.supports_native_structured_output());
        assert!(model.supports_native_structured_output_with_tools());

        let model = model
            .with_native_structured_output(false)
            .with_native_structured_output_with_tools(false);
        assert!(!model.supports_native_structured_output());
        assert!(!model.supports_native_structured_output_with_tools());
    }

    #[test]
    fn capabilities_reported() {
        let model = DashScopeChatModel::new(
            "qwen-plus",
            "sk",
            None,
            Arc::new(MemoryTransport::default()),
        )
        .with_context_window_size(131072);
        assert_eq!(model.model_name(), "qwen-plus");
        assert_eq!(model.context_window_size(), 131072);
        assert!(model.supports_native_structured_output());
    }

    // ── Encryption + multi-agent wiring ────────────────────────────────────

    /// Deterministic XOR "cipher" so the encryption protocol can be tested
    /// end-to-end without a crypto dependency.
    #[derive(Debug)]
    struct XorProvider;

    impl crate::dashscope::encryption::DashScopeCryptoProvider for XorProvider {
        fn generate_aes_key(&self) -> [u8; crate::dashscope::encryption::AES_KEY_SIZE] {
            [5u8; crate::dashscope::encryption::AES_KEY_SIZE]
        }

        fn generate_iv(&self) -> [u8; crate::dashscope::encryption::GCM_IV_LENGTH] {
            [1u8; crate::dashscope::encryption::GCM_IV_LENGTH]
        }

        fn aes_gcm_encrypt(&self, key: &[u8], _iv: &[u8], plaintext: &str) -> Result<String> {
            use base64::Engine as _;
            let bytes: Vec<u8> = plaintext
                .bytes()
                .enumerate()
                .map(|(i, b)| b ^ key[i % key.len()])
                .collect();
            Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
        }

        fn aes_gcm_decrypt(&self, key: &[u8], _iv: &[u8], ciphertext: &str) -> Result<String> {
            use base64::Engine as _;
            let raw = base64::engine::general_purpose::STANDARD
                .decode(ciphertext.as_bytes())
                .map_err(|e| Error::Model(e.to_string()))?;
            let bytes: Vec<u8> = raw
                .iter()
                .enumerate()
                .map(|(i, b)| b ^ key[i % key.len()])
                .collect();
            String::from_utf8(bytes).map_err(|e| Error::Model(e.to_string()))
        }

        fn rsa_encrypt(&self, _public_key_b64: &str, plaintext_b64: &str) -> Result<String> {
            Ok(format!("wrapped({plaintext_b64})"))
        }
    }

    #[tokio::test]
    async fn encryption_encrypts_input_and_decrypts_output() {
        let provider = Arc::new(XorProvider);
        // Build the response with the same fake cipher: `output` is a string.
        let plaintext = serde_json::json!({
            "choices": [{ "finish_reason": "stop", "message": { "role": "assistant", "content": "secret" } }]
        })
        .to_string();
        let encrypted_output =
            crate::dashscope::encryption::DashScopeCryptoProvider::aes_gcm_encrypt(
                provider.as_ref(),
                &[5u8; 32],
                &[1u8; 12],
                &plaintext,
            )
            .unwrap();
        let transport = MemoryTransport::with_response(serde_json::json!({
            "request_id": "req-enc",
            "output": encrypted_output,
            "usage": { "input_tokens": 1, "output_tokens": 2 }
        }));
        let model = model_with(transport.clone(), "qwen-plus").with_encryption(
            EncryptionConfig::new("key-1", "PUBKEY").unwrap(),
            provider.clone(),
        );
        assert!(model.is_encryption_enabled());

        let out = collect(
            &model,
            GenerateOptions {
                stream: Some(false),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out.len(), 1);
        assert!(
            matches!(&out[0].as_ref().unwrap().content[0], ContentBlock::Text(t) if t.text == "secret")
        );

        let spec = transport.last_request().unwrap();
        // `input` was replaced by the ciphertext, the header carries the wrapped key.
        assert!(spec.body["input"].is_string(), "{}", spec.body);
        let header = spec
            .headers
            .iter()
            .find(|(k, _)| k == crate::dashscope::encryption::ENCRYPTION_KEY_HEADER)
            .map(|(_, v)| v.clone())
            .expect("encryption header present");
        let header: serde_json::Value = serde_json::from_str(&header).unwrap();
        assert_eq!(header["public_key_id"], serde_json::json!("key-1"));
        assert!(header["encrypt_key"]
            .as_str()
            .unwrap()
            .starts_with("wrapped("));
    }

    #[tokio::test]
    async fn encryption_without_a_provider_fails_instead_of_sending_plaintext() {
        let transport = MemoryTransport::with_response(serde_json::json!({}));
        let model = model_with(transport.clone(), "qwen-plus").with_encryption(
            EncryptionConfig::new("key-1", "PUBKEY").unwrap(),
            Arc::new(crate::dashscope::encryption::UnavailableCryptoProvider),
        );
        let out = collect(
            &model,
            GenerateOptions {
                stream: Some(false),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out.len(), 1);
        let err = out[0].as_ref().unwrap_err().to_string();
        assert!(err.contains("no DashScopeCryptoProvider"), "{err}");
        // Nothing was sent: a failed encryption must not fall back to plaintext.
        assert!(transport.last_request().is_none());
    }

    /// Full wire path with the bundled `AesRsaCryptoProvider`: the request body
    /// carries ciphertext, the header carries the RSA-wrapped AES key, and the
    /// matching private key unwraps it and decrypts the body back to the
    /// original `input` JSON — exactly what the DashScope gateway does.
    #[tokio::test]
    async fn default_crypto_provider_encrypts_the_wire_body_and_header() {
        use crate::dashscope::crypto::test_support;
        let transport = MemoryTransport::with_response(serde_json::json!({
            "request_id": "req-enc-real",
            "output": { "choices": [{ "finish_reason": "stop", "message": { "role": "assistant", "content": "ok" } }] },
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        }));
        let model = model_with(transport.clone(), "qwen-plus").with_encryption_config(
            EncryptionConfig::new("key-1", test_support::RSA_PUBLIC_KEY_B64).unwrap(),
        );
        assert!(model.is_encryption_enabled());

        let out = collect(
            &model,
            GenerateOptions {
                stream: Some(false),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out.len(), 1);
        assert!(
            out[0].is_ok(),
            "{}",
            out[0]
                .as_ref()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
        );

        let spec = transport.last_request().unwrap();
        // The plaintext `input` object is gone: ciphertext went on the wire.
        let encrypted_input = spec.body["input"]
            .as_str()
            .expect("input replaced by AES-GCM ciphertext")
            .to_string();
        assert!(serde_json::from_str::<serde_json::Value>(&encrypted_input).is_err());

        let header_value = spec
            .headers
            .iter()
            .find(|(k, _)| k == crate::dashscope::encryption::ENCRYPTION_KEY_HEADER)
            .map(|(_, v)| v.clone())
            .expect("X-DashScope-EncryptionKey present");
        let header: serde_json::Value = serde_json::from_str(&header_value).unwrap();
        assert_eq!(header["public_key_id"], serde_json::json!("key-1"));

        // Server-side unwrap: RSA private key -> base64(raw AES key) -> AES-GCM.
        let aes_key = test_support::unwrap_aes_key(header["encrypt_key"].as_str().unwrap());
        assert_eq!(aes_key.len(), crate::dashscope::encryption::AES_KEY_SIZE);
        let iv = test_support::decode_b64(header["iv"].as_str().unwrap());
        assert_eq!(iv.len(), crate::dashscope::encryption::GCM_IV_LENGTH);

        let provider = crate::dashscope::crypto::AesRsaCryptoProvider::new();
        let plaintext = provider
            .aes_gcm_decrypt(&aes_key, &iv, &encrypted_input)
            .expect("wire body decrypts with the wrapped key");
        let parsed: serde_json::Value = serde_json::from_str(&plaintext).unwrap();
        assert_eq!(parsed["messages"][0]["content"], serde_json::json!("hi"));
    }

    // ── enableEncrypt: public-key fetch + fail-fast ─────────────────────────

    /// `enableEncrypt=false` (the default) must leave the request untouched: the
    /// `input` member stays a plain object and no encryption header is sent.
    #[tokio::test]
    async fn enable_encrypt_false_sends_the_plaintext_body() {
        let transport = MemoryTransport::with_response(serde_json::json!({
            "request_id": "req-plain",
            "output": { "choices": [{ "finish_reason": "stop", "message": { "role": "assistant", "content": "ok" } }] }
        }));
        let model = model_with(transport.clone(), "qwen-plus").with_encryption_enabled(false);
        assert!(!model.is_encryption_enabled());

        let out = collect(
            &model,
            GenerateOptions {
                stream: Some(false),
                ..Default::default()
            },
        )
        .await;
        assert!(out[0].is_ok());

        let spec = transport.last_request().unwrap();
        assert!(spec.body["input"].is_object(), "{}", spec.body);
        assert!(spec
            .headers
            .iter()
            .all(|(k, _)| k != crate::dashscope::encryption::ENCRYPTION_KEY_HEADER));
        assert!(transport.last_get_url().is_none(), "no key fetch happened");
    }

    /// `enableEncrypt=true` without a key must fail loudly instead of sending a
    /// plaintext request (a failed public-key fetch is an error, not a fallback).
    #[tokio::test]
    async fn enable_encrypt_without_a_public_key_fails_instead_of_sending_plaintext() {
        let transport = MemoryTransport::with_response(serde_json::json!({
            "output": { "choices": [{ "finish_reason": "stop", "message": { "role": "assistant", "content": "ok" } }] }
        }));
        let model = model_with(transport.clone(), "qwen-plus").with_encryption_enabled(true);

        let out = collect(
            &model,
            GenerateOptions {
                stream: Some(false),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out.len(), 1);
        let err = out[0].as_ref().unwrap_err().to_string();
        assert!(err.contains("enableEncrypt=true"), "{err}");
        assert!(err.contains("public key"), "{err}");
        // The fetch was attempted exactly against the documented endpoint…
        assert_eq!(
            transport.last_get_url().as_deref(),
            Some("https://dashscope.aliyuncs.com/api/v1/public-keys/latest")
        );
        // …and no chat request was sent.
        assert!(transport.last_request().is_none());
    }

    /// The synchronous builder must refuse to build a request for an
    /// `enableEncrypt` model whose key has not been resolved yet.
    #[test]
    fn sync_request_builder_refuses_to_send_plaintext_when_encryption_is_on() {
        let model = DashScopeChatModel::new(
            "qwen-plus",
            "sk",
            None,
            Arc::new(MemoryTransport::default()),
        )
        .with_encryption_enabled(true);

        let err = model
            .build_http_request_with_context(
                vec![Msg::user("hi")],
                Vec::new(),
                &GenerateOptions::default(),
                false,
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("enableEncrypt=true"), "{err}");
    }

    /// `enableEncrypt=true` *with* the public-key endpoint wired up: the key is
    /// fetched once, then every request body is encrypted with it.
    #[tokio::test]
    async fn enable_encrypt_fetches_the_public_key_and_encrypts_the_body() {
        use crate::dashscope::crypto::test_support;

        let transport = MemoryTransport {
            get_response: Some(serde_json::json!({
                "code": null,
                "data": {
                    "public_key_id": "fetched-key",
                    "public_key": test_support::RSA_PUBLIC_KEY_B64,
                }
            })),
            ..MemoryTransport::with_response(serde_json::json!({
                "request_id": "req-enc-fetched",
                "output": { "choices": [{ "finish_reason": "stop", "message": { "role": "assistant", "content": "ok" } }] },
                "usage": { "input_tokens": 1, "output_tokens": 1 }
            }))
        };
        let model = model_with(transport.clone(), "qwen-plus").with_encryption_enabled(true);
        assert!(model.is_encryption_enabled());

        for round in 0..2 {
            let out = collect(
                &model,
                GenerateOptions {
                    stream: Some(false),
                    ..Default::default()
                },
            )
            .await;
            assert!(
                out[0].is_ok(),
                "round {round}: {}",
                out[0]
                    .as_ref()
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_default()
            );
        }

        let spec = transport.last_request().unwrap();
        let encrypted_input = spec.body["input"]
            .as_str()
            .expect("input replaced by AES-GCM ciphertext")
            .to_string();
        assert!(serde_json::from_str::<serde_json::Value>(&encrypted_input).is_err());
        let header: serde_json::Value = serde_json::from_str(
            spec.headers
                .iter()
                .find(|(k, _)| k == crate::dashscope::encryption::ENCRYPTION_KEY_HEADER)
                .map(|(_, v)| v.as_str())
                .expect("encryption header present"),
        )
        .unwrap();
        assert_eq!(header["public_key_id"], serde_json::json!("fetched-key"));

        // The fetched key is cached: the second request reuses it instead of re-fetching.
        assert!(transport.last_get_url().is_some());
        assert_eq!(
            model
                .resolve_encryption_config()
                .await
                .unwrap()
                .unwrap()
                .public_key_id,
            "fetched-key"
        );
    }

    #[test]
    fn multi_agent_formatter_is_applied_to_the_request_body() {
        let model = DashScopeChatModel::new(
            "qwen-plus",
            "sk",
            None,
            Arc::new(MemoryTransport::default()),
        )
        .with_multi_agent_formatter(Some("PROMPT".to_string()));
        assert!(model.is_multi_agent());

        let messages = vec![
            Msg::builder()
                .role(agentscope_core::MsgRole::Assistant)
                .name("Alice")
                .text_content("hi")
                .build(),
            Msg::tool("call-1", "search", "42"),
        ];
        let (spec, _) = model
            .build_http_request_with_context(
                messages,
                Vec::new(),
                &GenerateOptions::default(),
                false,
            )
            .unwrap();
        let messages = &spec.body["input"]["messages"];
        assert_eq!(
            messages[0]["content"],
            serde_json::json!("PROMPT<history>\nAlice: hi\n</history>")
        );
        assert_eq!(messages[1]["role"], serde_json::json!("tool"));
    }
}
