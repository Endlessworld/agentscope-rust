//! OpenAI-compatible chat model implemented on top of the open-source
//! `async-openai` crate (v0.41, remote crates.io dependency).
//!
//! This is the first **real** provider: `Model::stream` calls
//! `chat().create_stream()` and maps the SSE `delta` chunks into
//! [`agentscope_core::model::ChatResponse`]/[`ContentBlock`]. Because `async-openai`
//! speaks the OpenAI protocol, this single provider also serves any
//! OpenAI-compatible gateway (DeepSeek/Kimi/GLM/Ollama/... ) by overriding the
//! base URL and API key.

use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::sync::Arc;

use serde::Deserialize;

use async_openai::config::OpenAIConfig;
use async_openai::error::OpenAIError;
use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
    ChatCompletionNamedToolChoice,
    ChatCompletionRequestAssistantMessage, ChatCompletionRequestAssistantMessageContent,
    ChatCompletionRequestMessage, ChatCompletionRequestMessageContentPartAudio,
    ChatCompletionRequestMessageContentPartImage,
    ChatCompletionRequestMessageContentPartText, ChatCompletionRequestSystemMessage,
    ChatCompletionRequestSystemMessageContent, ChatCompletionRequestToolMessage,
    ChatCompletionRequestToolMessageContent, ChatCompletionRequestUserMessage,
    ChatCompletionRequestUserMessageContent, ChatCompletionRequestUserMessageContentPart,
    ChatCompletionStreamOptions, ChatCompletionTool, ChatCompletionToolChoiceOption,
    ChatCompletionTools, CompletionUsage, CreateChatCompletionRequest, FinishReason, FunctionCall,
    FunctionName, FunctionObject, ImageDetail, ImageUrl, InputAudio, InputAudioFormat,
    ReasoningEffort, ResponseFormat as OaiResponseFormat, ResponseFormatJsonSchema,
    ToolChoiceOptions,
};
use async_openai::Client;
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};

use agentscope_core::error::{Error, Result};
use agentscope_core::formatter::{
    convert_tool_result_to_string, extract_text_content, ResponseFormat,
};
use agentscope_core::message::{
    ChatUsage, ContentBlock, ImageDetail as BlockImageDetail, JsonMap, Msg, MsgRole, Source,
    TextBlock, ThinkingBlock, ToolUseBlock,
};
use agentscope_core::model::catalog::{descriptor_for, ModelDescriptor};
use agentscope_core::model::context_windows::resolve_context_window;
use agentscope_core::model::finish_reasons as FinishReasons;
use agentscope_core::model::generate_options::GenerateOptions;
use agentscope_core::model::model::{Model, ModelCapabilities};
use agentscope_core::model::provider::{
    ModelCreationContext, ModelProvider, ModelRegistry, DEFAULT_PROVIDER_ID,
};
use agentscope_core::model::tool_choice::ToolChoice;
use agentscope_core::model::tool_schema::ToolSchema;

use crate::common::error::{ErrorKind, ModelError};
use crate::common::{diagnostics, resolve_capabilities_with, usage, ModelMap};

pub mod compat;

/// Connection-establishment timeout for the underlying HTTP client.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Idle timeout for streaming responses: if no bytes arrive for this long the
/// connection is treated as dead (e.g. the Wi-Fi dropped) so the call errors
/// out and is retried instead of hanging the agent forever.
const STREAM_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// TCP keepalive probe interval, so half-open connections are detected.
const TCP_KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(30);

/// Builds the HTTP client used for all chat completions.
///
/// A stalled network (or a dropped Wi-Fi link) otherwise leaves the SSE stream
/// pending indefinitely, which freezes the agent and its ACP UI. The connect /
/// read timeouts turn such a stall into a retryable transport error.
fn build_http_client() -> reqwest::Client {
    agentscope_core::tls::ensure_crypto_provider();
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(STREAM_READ_TIMEOUT)
        .tcp_keepalive(TCP_KEEPALIVE)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// An OpenAI-compatible chat model backed by `async-openai`.
pub struct OpenAiChatModel {
    client: Client<OpenAIConfig>,
    model_name: String,
    capabilities: ModelCapabilities,
    descriptor: Option<Arc<ModelDescriptor>>,
    /// Whether historical assistant `ThinkingBlock`s must be replayed as the
    /// `reasoning_content` request field. DeepSeek requires this whenever the
    /// request carries tools, or the API returns HTTP 400.
    reasoning_passback: bool,
    /// Model-level default for the JSON-schema `strict` flag, applied only when the
    /// request-level schema leaves `strict` unset. Mirrors Java `strictJsonSchema`.
    strict_json_schema: Option<bool>,
    /// Whether a request-level `ToolChoice::Specific` is honoured. Java enables it
    /// only on the official endpoint (`OpenAIModelProvider#create`); many
    /// OpenAI-compatible gateways accept the field but ignore the forced name.
    supports_tool_choice_specific: bool,
    /// Creation-time options, merged *under* every per-call request.
    ///
    /// Mirrors Java `OpenAIChatModel`: the builder options are merged into the
    /// effective options (`mergeOptions(options, configuredOptions)`), so a
    /// configured `temperature` / `maxTokens` / `responseFormat` /
    /// `additionalBodyParams` / thinking knob reaches the wire even when the
    /// call site only passes the per-request options.
    default_options: GenerateOptions,
}

impl OpenAiChatModel {
    /// Environment variable consulted when no explicit base URL is supplied.
    pub const BASE_URL_ENV: &'static str = "OPENAI_BASE_URL";

    /// Whether the given model id requires `reasoning_content` passback.
    ///
    /// DeepSeek (and its OpenAI-compatible endpoints) hard-require the field to
    /// be echoed back on historical turns when tools are in play. Other
    /// providers ignore it, so this is only enabled for DeepSeek by default.
    pub fn requires_reasoning_passback(model_name: &str) -> bool {
        model_name.to_ascii_lowercase().contains("deepseek")
    }

    /// Overrides the automatic `reasoning_content` passback detection.
    pub fn with_reasoning_passback(mut self, enabled: bool) -> Self {
        self.reasoning_passback = enabled;
        self
    }

    /// Sets the model-level default `strict` flag for JSON-schema structured output.
    /// Only applied when the request-level schema does not specify `strict`.
    pub fn strict_json_schema(mut self, strict: bool) -> Self {
        self.strict_json_schema = Some(strict);
        self
    }

    /// Creates a model from an explicit model id, api key and base URL.
    pub fn new(
        model_name: impl Into<String>,
        api_key: Option<String>,
        base_url: Option<String>,
    ) -> Result<Self> {
        let model_name = model_name.into();
        let base_url_for_catalog = base_url.clone();
        let mut cfg = OpenAIConfig::new();
        if let Some(key) = api_key {
            cfg = cfg.with_api_key(key);
        }
        if let Some(base) = base_url {
            cfg = cfg.with_api_base(base);
        }
        let client = Client::with_config(cfg).with_http_client(build_http_client());
        let descriptor = descriptor_for(None, &model_name, base_url_for_catalog.as_deref());
        let (capabilities, supports_tool_choice_specific) = resolve_model_capabilities(
            &HashMap::new(),
            base_url_for_catalog
                .as_deref()
                .unwrap_or(OFFICIAL_BASE_URL),
            &model_name,
            descriptor.as_deref(),
        );
        let reasoning_passback = descriptor
            .as_ref()
            .is_some_and(|descriptor| descriptor.interleaved_field.is_some())
            || Self::requires_reasoning_passback(&model_name);
        Ok(Self {
            client,
            capabilities,
            descriptor,
            reasoning_passback,
            strict_json_schema: None,
            supports_tool_choice_specific,
            default_options: GenerateOptions::default(),
            model_name,
        })
    }

    /// Creates a model from a [`ModelCreationContext`].
    pub fn from_context(ctx: &ModelCreationContext) -> Result<Self> {
        let model_name = if ctx.model.is_empty() {
            ctx.options
                .model_name
                .clone()
                .unwrap_or_else(|| "gpt-4o".to_string())
        } else {
            ctx.model.clone()
        };

        let mut cfg = OpenAIConfig::new();
        if let Some(key) = &ctx.api_key {
            cfg = cfg.with_api_key(key);
        }
        // Explicit context value, else `OPENAI_BASE_URL`, else the SDK default.
        let base_url = crate::common::resolve_base_url(
            ctx.base_url.clone(),
            Self::BASE_URL_ENV,
            "https://api.openai.com/v1",
        );
        let base_url_for_catalog = base_url.clone();
        cfg = cfg.with_api_base(base_url);
        // Merge per-request additional headers into the client config.
        for (k, v) in &ctx.options.additional_headers {
            let name = reqwest::header::HeaderName::from_str(k)
                .map_err(|e| Error::Model(format!("invalid header name '{k}': {e}")))?;
            cfg = cfg
                .with_header(name.clone(), v.as_str())
                .map_err(|e| Error::Model(format!("failed to set header '{k}': {e}")))?;
        }
        let client = Client::with_config(cfg).with_http_client(build_http_client());
        let descriptor = ctx.catalog.clone().or_else(|| {
            descriptor_for(
                Some(&ctx.provider_id),
                &model_name,
                Some(&base_url_for_catalog),
            )
        });
        let (capabilities, supports_tool_choice_specific) = resolve_model_capabilities(
            &ctx.options.additional_body_params,
            &base_url_for_catalog,
            &model_name,
            descriptor.as_deref(),
        );
        let reasoning_passback = descriptor
            .as_ref()
            .is_some_and(|descriptor| descriptor.interleaved_field.is_some())
            || Self::requires_reasoning_passback(&model_name);
        let strict_json_schema = ctx
            .options
            .additional_body_params
            .get(STRICT_JSON_SCHEMA_OPTION)
            .and_then(|v| v.as_bool());
        // Creation-time options are merged *under* every per-call request
        // (Java `OpenAIChatModel#doStream0` → `mergeOptions(options, configuredOptions)`).
        // `strictJsonSchema` and the capability flags were already consumed into
        // model-level state above and are provider knobs, not wire fields
        // (MODEL-06 / MODEL-09), so they must not be re-sent through
        // `additional_body_params`.
        let mut default_options = ctx.options.clone();
        for key in [
            STRICT_JSON_SCHEMA_OPTION,
            NATIVE_STRUCTURED_OUTPUT_OPTION,
            NATIVE_STRUCTURED_OUTPUT_WITH_TOOLS_OPTION,
            SUPPORTS_TOOL_CHOICE_SPECIFIC_OPTION,
            CONTEXT_WINDOW_SIZE_OPTION,
        ] {
            default_options.additional_body_params.remove(key);
        }
        if default_options.stream.is_none() {
            default_options.stream = ctx.stream;
        }
        Ok(Self {
            client,
            capabilities,
            descriptor,
            reasoning_passback,
            strict_json_schema,
            supports_tool_choice_specific,
            default_options,
            model_name,
        })
    }

    /// Sets the creation-time options merged under every request.
    ///
    /// Per-call options always win; this only fills the gaps (mirrors Java's
    /// `OpenAIChatModel.Builder#defaultOptions`).
    pub fn with_default_options(mut self, options: GenerateOptions) -> Self {
        self.default_options = options;
        self
    }

    /// The creation-time options merged under every request.
    pub fn default_options(&self) -> &GenerateOptions {
        &self.default_options
    }

    /// Access the underlying async-openai client (e.g. for retry middleware).
    pub fn client(&self) -> &Client<OpenAIConfig> {
        &self.client
    }

    /// The model id that will reach the wire for a given per-call option set.
    fn effective_model_name(&self, options: &GenerateOptions) -> String {
        options
            .model_name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .or_else(|| {
                self.default_options
                    .model_name
                    .clone()
                    .filter(|name| !name.trim().is_empty())
            })
            .unwrap_or_else(|| self.model_name.clone())
    }

    /// Builds the serialized Chat Completions body for one call.
    ///
    /// Creation-time options ([`Self::with_default_options`], populated from
    /// `ModelCreationContext.options`) are merged **under** the per-call options,
    /// exactly like Java's `mergeOptions(options, configuredOptions)`. Extracted
    /// from [`Model::stream`] so the merge can be asserted without a network.
    pub fn prepare_body(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: GenerateOptions,
    ) -> (serde_json::Value, bool) {
        let mut options = GenerateOptions::merge_options(&options, &self.default_options);
        if let Some(max_output) = self.descriptor.as_ref().map(|d| d.max_output) {
            if max_output > 0 {
                let max_output = max_output.min(u32::MAX as usize) as u32;
                options.max_tokens = options.max_tokens.map(|value| value.min(max_output));
                options.max_completion_tokens = options
                    .max_completion_tokens
                    .map(|value| value.min(max_output));
            }
        }
        let model_name = self.effective_model_name(&options);
        // Vendor-specific body params (e.g. `enable_thinking`) must reach the
        // wire: `CreateChatCompletionRequest` has no extension field, so the
        // typed request is serialized to a JSON object and the extra params
        // are merged in before the BYOT call.
        // `multiAgentFormatter` is a provider-side selector consumed by
        // `build_request`, never a wire field: drop it from the pass-through
        // extras so OpenAI-compatible backends never see it.
        // `strictJsonSchema` is a provider knob, never a wire field: a per-call
        // value overrides the model-level default (MODEL-09) and is then dropped
        // so it cannot leak through `additional_body_params` either.
        let strict_json_schema = options
            .additional_body_params
            .remove(STRICT_JSON_SCHEMA_OPTION)
            .and_then(|value| value.as_bool())
            .or(self.strict_json_schema);
        let mut extra_body = options.additional_body_params.clone();
        extra_body.remove(crate::common::MULTI_AGENT_FORMATTER_KEY);
        // `thinking_budget` has no typed field in `async-openai`; Java's
        // `OpenAIRequest#thinkingBudget` sends it as a top-level wire field.
        let thinking_budget = options.thinking_budget;
        // Automatic prompt-cache marking is opt-in per request(`cache_control`).
        let cache_control = options.cache_control;
        // `async-openai` only models the known effort levels; a vendor value it
        // cannot represent is passed through verbatim (Java
        // `OpenAIChatFormatter#applyOptions` sets the string as-is) instead of
        // being silently dropped.
        let unknown_reasoning_effort = options
            .reasoning_effort
            .clone()
            .filter(|value| to_reasoning_effort(value).is_none());
        // Capture the transport mode before `options` is moved into the builder.
        // `None` keeps the historical streaming default.
        let streaming = options.stream.unwrap_or(true);
        let capabilities = resolve_capabilities_with(&model_name, self.descriptor.as_deref());
        let (request, extras) = build_request(
            model_name.clone(),
            messages,
            tools,
            options,
            self.reasoning_passback,
            strict_json_schema,
            Some(capabilities),
        );
        // Serialize eagerly; on failure fall back to the typed request so the
        // call still proceeds (this can only fail on non-string map keys).
        let mut body: serde_json::Value = match serde_json::to_value(&request) {
            Ok(v) => v,
            Err(_) => serde_json::to_value(&request).unwrap_or(serde_json::Value::Null),
        };
        if let Some(budget) = thinking_budget {
            if let Some(object) = body.as_object_mut() {
                object.insert("thinking_budget".to_string(), serde_json::Value::from(budget));
            }
        }
        if let Some(effort) = unknown_reasoning_effort {
            if let Some(object) = body.as_object_mut() {
                object.insert(
                    "reasoning_effort".to_string(),
                    serde_json::Value::String(effort),
                );
            }
        }
        if !extra_body.is_empty() {
            if let Some(object) = body.as_object_mut() {
                for (key, value) in extra_body {
                    object.insert(key, value);
                }
            }
        }
        // `async-openai` cannot model DeepSeek's `reasoning_content` field, so it
        // is injected into the serialized assistant messages here. The typed
        // conversion is not 1:1 (a tool message may fan out to N wire messages),
        // hence the parallel `reasoning` vector recorded at build time.
        if self.reasoning_passback {
            inject_reasoning_content(&mut body, &extras.reasoning);
        }
        // `reasoning_details` / `thought_signature` round-trip (MODEL-05): the typed
        // `async-openai` messages cannot express either field.
        inject_reasoning_details(&mut body, &extras.reasoning_details);
        inject_tool_signatures(&mut body, &extras.tool_signatures);
        // Prompt caching: message-level metadata directives first, then the
        // automatic policy when the caller opted in.
        apply_cache_control(&mut body, &extras.cache_directives, cache_control);
        // Vendor-specific sanitising (DeepSeek/GLM/Kimi/MiniMax). Runs last so it
        // sees both `additional_body_params` and the injected reasoning fields.
        crate::openai::compat::apply_vendor_rules(&model_name, &mut body);
        (body, streaming)
    }
}

#[async_trait::async_trait]
impl Model for OpenAiChatModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: GenerateOptions,
    ) -> BoxStream<'_, Result<agentscope_core::model::ChatResponse>> {
        let client = self.client.clone();
        if let Some(descriptor) = &self.descriptor {
            if !descriptor.input_modalities.contains("image")
                && messages.iter().any(has_image_block)
            {
                let message = format!(
                    "model '{}' does not accept image input; switch to a vision-capable model",
                    self.model_name
                );
                return futures::stream::once(async move { Err(Error::Model(message)) }).boxed();
            }
        }
        let (body, streaming) = self.prepare_body(messages, tools, options);

        // Non-streaming path. `GenerateOptions::stream == Some(false)` is what
        // OpenAI-compatible backends that do not speak SSE expect (e.g. the
        // Trinity-RFT adapter in Java calls `.stream(false)`); sending
        // `stream: true` there is a protocol mismatch. The whole completion
        // arrives as a single JSON object, so it is mapped to one
        // `ChatResponse` and wrapped in a one-element stream to keep the trait
        // shape unchanged.
        //
        // `create_byot` is used instead of `create` so the response can be
        // decoded into [`OpenAiCompletionResponse`], which (like the streaming
        // chunk type) preserves the non-standard `reasoning_content` field that
        // `CreateChatCompletionResponse` would silently drop.
        if !streaming {
            let response = futures::stream::once(async move {
                let completion = client
                    .chat()
                    .create_byot::<_, OpenAiCompletionResponse>(body)
                    .await
                    .map_err(map_openai_error)?;
                Ok::<_, Error>(completion_to_response(completion))
            });
            return Box::pin(response);
        }

        // create_stream_byot lets us deserialize each SSE chunk into a custom
        // type that also captures `reasoning_content`, so thinking deltas can be
        // streamed incrementally instead of being swallowed by the default
        // `CreateChatCompletionStreamResponse` (which has no reasoning field).
        let setup = futures::stream::once(async move {
            client
                .chat()
                .create_stream_byot::<_, OpenAiStreamChunk>(body)
                .await
                .map_err(map_openai_error)
        });
        let chunks = setup
            .map_ok(|s| s.map(|item| item.map_err(map_openai_error)))
            .try_flatten()
            .boxed();

        // Yield one incremental `ChatResponse` per SSE chunk: text/thinking
        // deltas are emitted immediately (token-by-token), while tool-call
        // fragments are accumulated and flushed as a single block when the
        // stream ends. This preserves true streaming for consumers that render
        // `TextBlockDelta`/`ThinkingBlockDelta` events as they arrive.
        let stream = futures::stream::unfold((chunks, StreamIncrementer::default()), |state| {
            let (mut chunks, mut acc) = state;
            async move {
                loop {
                    match chunks.next().await {
                        Some(Ok(chunk)) => {
                            if let Some(resp) = acc.accumulate(&chunk) {
                                return Some((Ok(resp), (chunks, acc)));
                            }
                            // No incremental content in this chunk; keep polling.
                        }
                        Some(Err(e)) => return Some((Err(e), (chunks, acc))),
                        None => {
                            let final_resp = acc.into_final();
                            match final_resp {
                                Some(resp) => {
                                    return Some((Ok(resp), (chunks, StreamIncrementer::default())))
                                }
                                None => return None,
                            }
                        }
                    }
                }
            }
        });

        Box::pin(stream)
    }

    fn model_name(&self) -> &str {
        &self.model_name
    }

    fn context_window_size(&self) -> usize {
        self.capabilities.context_window_size
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

    fn supports_native_structured_output(&self) -> bool {
        self.capabilities.native_structured_output
    }

    fn supports_native_structured_output_with_tools(&self) -> bool {
        self.capabilities.native_structured_output_with_tools
    }

    fn supports_tool_choice_specific(&self) -> bool {
        self.supports_tool_choice_specific
    }
}

/// Provider option key carrying the model-level JSON-schema `strict` default.
/// Consumed by [`OpenAiChatModel::from_context`], never sent on the wire.
const STRICT_JSON_SCHEMA_OPTION: &str = "strictJsonSchema";

/// Provider option keys mirroring Java
/// `OpenAIModelProviderSupport#applyAdvancedOptions`. They are provider knobs,
/// never wire fields (the JSON body is built from the typed request).
const NATIVE_STRUCTURED_OUTPUT_OPTION: &str = "nativeStructuredOutput";
const NATIVE_STRUCTURED_OUTPUT_WITH_TOOLS_OPTION: &str = "nativeStructuredOutputWithTools";
const SUPPORTS_TOOL_CHOICE_SPECIFIC_OPTION: &str = "supportsToolChoiceSpecific";
const CONTEXT_WINDOW_SIZE_OPTION: &str = "contextWindowSize";

/// Base URL of the official OpenAI endpoint (`OpenAIClient.DEFAULT_BASE_URL_WITH_VERSION`).
const OFFICIAL_BASE_URL: &str = "https://api.openai.com/v1";

/// Resolves the capability flags carried by the creation-time options.
///
/// Defaults mirror Java `OpenAIModelProvider#create` + `OpenAIChatModel.Builder#build`:
/// native structured output is on, its with-tools variant follows it,
/// `ToolChoice::Specific` is honoured only on the official endpoint, and the
/// context window falls back to the catalog lookup.
fn resolve_model_capabilities(
    additional_body_params: &HashMap<String, serde_json::Value>,
    base_url: &str,
    model_name: &str,
    descriptor: Option<&ModelDescriptor>,
) -> (ModelCapabilities, bool) {
    let flag = |key: &str| {
        additional_body_params
            .get(key)
            .and_then(|value| value.as_bool())
    };
    let native_structured_output = flag(NATIVE_STRUCTURED_OUTPUT_OPTION).unwrap_or(true);
    let native_structured_output_with_tools =
        flag(NATIVE_STRUCTURED_OUTPUT_WITH_TOOLS_OPTION).unwrap_or(native_structured_output);
    let context_window_size = additional_body_params
        .get(CONTEXT_WINDOW_SIZE_OPTION)
        .and_then(|value| value.as_u64())
        .filter(|size| *size > 0)
        .map(|size| size as usize)
        .unwrap_or_else(|| {
            descriptor
                .and_then(|descriptor| descriptor.effective_context())
                .unwrap_or_else(|| resolve_context_window(model_name))
        });
    let supports_tool_choice_specific = flag(SUPPORTS_TOOL_CHOICE_SPECIFIC_OPTION)
        .unwrap_or_else(|| base_url.trim_end_matches('/') == OFFICIAL_BASE_URL);
    (
        ModelCapabilities::new()
            .context_window_size(context_window_size)
            .native_structured_output(native_structured_output)
            .native_structured_output_with_tools(native_structured_output_with_tools),
        supports_tool_choice_specific,
    )
}

/// Maximum number of `cache_control` markers a provider accepts per request
/// (Java `OpenAIBaseFormatter#MAX_CACHE_MARKERS`).
pub const MAX_CACHE_MARKERS: usize = 4;

/// Wire field carrying a prompt-cache breakpoint marker.
const CACHE_CONTROL_FIELD: &str = "cache_control";

/// Provider that registers [`OpenAiChatModel`] under the `openai` provider id.
/// Provider for OpenAI and OpenAI-compatible gateways.
///
/// Beyond constructing [`OpenAiChatModel`] it owns two host-configurable name
/// rules that Java applies *before* the request is built:
///
/// * [`ModelMap`] — chained alias mapping (`gpt-4` → `gpt-4.1`), mirroring the
///   Java `ModelMap`; a cycle is reported instead of looping.
/// * reasoning suffix — `-thinking-4096` / `-high` are stripped from the wire
///   model id and turned into `thinking_budget` / `reasoning_effort`.
#[derive(Debug, Default, Clone)]
pub struct OpenAiProvider {
    model_map: Option<ModelMap>,
    reasoning_suffix: bool,
}

impl OpenAiProvider {
    /// Creates a provider without name rules.
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs a model-name mapping table (applied before anything else).
    pub fn with_model_map(mut self, map: ModelMap) -> Self {
        self.model_map = Some(map);
        self
    }

    /// Enables the `-thinking-<n>` / `-<effort>` suffix convention.
    ///
    /// Mirrors Java's reasoning-suffix handling: the suffix never reaches the
    /// wire, it only seeds the per-request thinking knobs.
    pub fn with_reasoning_suffix(mut self, enabled: bool) -> Self {
        self.reasoning_suffix = enabled;
        self
    }

    /// Resolves the mapped model id and strips the reasoning suffix.
    fn normalize_context(&self, mut ctx: ModelCreationContext) -> Result<ModelCreationContext> {
        if let Some(map) = &self.model_map {
            ctx.model = map
                .resolve(&ctx.model)
                .map_err(|error| Error::Model(error.to_string()))?;
        }
        if self.reasoning_suffix {
            let (base, hint) = crate::common::strip_reasoning_suffix(&ctx.model);
            if let Some(hint) = hint {
                ctx.model = base;
                ctx.enable_thinking = true;
                if ctx.options.thinking_budget.is_none() {
                    ctx.options.thinking_budget = hint.budget_tokens;
                }
                if ctx.options.reasoning_effort.is_none() {
                    ctx.options.reasoning_effort = hint.effort;
                }
            }
        }
        Ok(ctx)
    }
}

impl ModelProvider for OpenAiProvider {
    fn provider_id(&self) -> &str {
        DEFAULT_PROVIDER_ID
    }
    fn supports(&self, _model: &str) -> bool {
        // OpenAI-compatible gateway: accepts any model id; the base URL may be
        // redirected to DeepSeek/Kimi/GLM/Ollama etc.
        true
    }
    fn create(&self, ctx: ModelCreationContext) -> Result<Arc<dyn Model>> {
        let ctx = self.normalize_context(ctx)?;
        let model = OpenAiChatModel::from_context(&ctx)?;
        Ok(Arc::new(model))
    }
}

/// Builds a registry pre-populated with the default OpenAI-compatible provider.
pub fn default_registry() -> ModelRegistry {
    let mut registry = ModelRegistry::new();
    registry.register(Arc::new(OpenAiProvider::new()));
    registry
}

fn map_openai_error(e: OpenAIError) -> Error {
    match e {
        OpenAIError::ApiError(a) => {
            let api = a.api_error;
            ModelError::from_parts(
                "openai",
                a.status_code.as_u16(),
                api.message,
                api.r#type,
                api.code,
            )
            .to_core()
        }
        // Transport failures (connection reset, a dropped Wi-Fi link, a read
        // timeout) must be retryable so the model call is re-established once
        // the network returns instead of hanging or failing the whole turn.
        OpenAIError::Reqwest(e) => {
            let mut err = ModelError::new("openai", 0, e.to_string());
            err.kind = if e.is_timeout() {
                ErrorKind::Timeout
            } else {
                ErrorKind::Network
            };
            err.to_core()
        }
        OpenAIError::StreamError(e) => Error::Http(format!("stream error: {e}")),
        other => Error::Model(format!("{other}")),
    }
}

#[allow(deprecated)] // async-openai marks `seed` deprecated; we keep it for parity.
fn build_request(
    model: String,
    messages: Vec<Msg>,
    tools: Vec<ToolSchema>,
    options: GenerateOptions,
    reasoning_passback: bool,
    strict_json_schema: Option<bool>,
    capabilities: Option<crate::common::ModelCapabilities>,
) -> (CreateChatCompletionRequest, MessageExtras) {
    // Multi-agent formatter: opt-in per call via
    // `additional_body_params["multiAgentFormatter"]` (`true` = default prompt,
    // string = custom prompt), the same selector `ollama` / `dashscope` /
    // `responses` use. Java parity: `OpenAIMultiAgentFormatter` +
    // `OpenAIConversationMerger`. The selector is consumed here so it never
    // reaches the wire.
    let mut options = options;
    let messages = match crate::common::take_multi_agent_prompt(&mut options) {
        Some(prompt) => crate::common::merge_multi_agent_history(messages, &prompt),
        None => messages,
    };

    let request_tools = to_request_tools(tools);
    let has_tools = request_tools.is_some();

    let RequestMessages {
        messages: request_messages,
        reasoning,
        reasoning_details,
        cache_directives,
        tool_signatures,
    } = to_request_messages(messages, reasoning_passback);
    // `None` keeps the historical streaming default (TRAINING/agent loop relies
    // on it); `Some(false)` must reach the wire as a plain non-streaming
    // completion, which also means `stream_options` is meaningless and must be
    // omitted entirely.
    let streaming = options.stream.unwrap_or(true);
    let mut req = CreateChatCompletionRequest {
        model,
        messages: request_messages,
        stream: Some(streaming),
        stream_options: if streaming {
            Some(ChatCompletionStreamOptions {
                include_usage: Some(true),
                include_obfuscation: None,
            })
        } else {
            None
        },
        tools: request_tools,
        ..Default::default()
    };

    // Resolve per-model capabilities so unsupported parameters are never sent
    // (reasoning models 400 on temperature/top_p/penalties/seed; several
    // gateways only accept `max_tokens`, not `max_completion_tokens`).
    let caps = capabilities.unwrap_or_else(|| crate::common::resolve_capabilities(&req.model));
    if !caps.supports_stream_options {
        req.stream_options = None;
    }

    if let Some(tc) = &options.tool_choice {
        if has_tools {
            req.tool_choice = to_tool_choice(tc);
        } else {
            diagnostics::warn(
                "tool_choice_omitted",
                "tool_choice",
                "tool_choice was omitted because the request declares no tools",
            );
        }
    }
    if let Some(t) = options.temperature {
        if caps.supports_sampling {
            req.temperature = Some(t as f32);
        } else {
            diagnostics::info(
                "sampling_unsupported",
                "temperature",
                format!(
                    "model '{}' does not accept temperature; parameter dropped",
                    req.model
                ),
            );
        }
    }
    if let Some(p) = options.top_p {
        if caps.supports_sampling {
            req.top_p = Some(p as f32);
        }
    }
    let max_tokens = options.max_completion_tokens.or(options.max_tokens);
    if let Some(n) = max_tokens {
        if caps.use_max_completion_tokens {
            req.max_completion_tokens = Some(n);
        } else {
            req.max_tokens = Some(n);
        }
    }
    if let Some(f) = options.frequency_penalty {
        if caps.supports_penalties {
            req.frequency_penalty = Some(f as f32);
        }
    }
    if let Some(p) = options.presence_penalty {
        if caps.supports_penalties {
            req.presence_penalty = Some(p as f32);
        }
    }
    if let Some(s) = options.seed {
        if caps.supports_seed {
            req.seed = Some(s);
        }
    }
    if let Some(p) = options.parallel_tool_calls {
        if caps.supports_parallel_tool_calls {
            req.parallel_tool_calls = Some(p);
        }
    }
    if let Some(rc) = options.reasoning_effort.as_deref() {
        req.reasoning_effort = to_reasoning_effort(rc);
    }
    if let Some(rf) = &options.response_format {
        req.response_format = to_response_format(rf, strict_json_schema);
    }
    (req, MessageExtras {
        reasoning,
        reasoning_details,
        cache_directives,
        tool_signatures,
    })
}

/// Bookkeeping recorded while converting core `Msg`s, addressed by wire index.
struct MessageExtras {
    /// The `reasoning_content` to replay for each wire message.
    reasoning: Vec<Option<String>>,
    /// The `reasoning_details` list to replay for each wire message.
    reasoning_details: Vec<Option<serde_json::Value>>,
    /// The `Msg`-level `cache_control` metadata directive of each wire message.
    cache_directives: Vec<Option<bool>>,
    /// `(tool call id, thought_signature)` pairs to replay on the function call.
    tool_signatures: Vec<(String, String)>,
}

/// The result of converting core [`Msg`]s into OpenAI wire messages.
struct RequestMessages {
    /// The typed messages sent to the provider.
    messages: Vec<ChatCompletionRequestMessage>,
    /// Parallel to `messages`: the `reasoning_content` to replay for each wire
    /// message (only populated for assistant turns when passback is enabled).
    reasoning: Vec<Option<String>>,
    /// Parallel to `messages`: the provider `reasoning_details` list of each
    /// assistant turn (OpenRouter / vLLM / Gemini-via-OpenAI).
    reasoning_details: Vec<Option<serde_json::Value>>,
    /// Parallel to `messages`: the message-level cache directive of each wire
    /// message (`true` = the message asked for a marker, `None` = no explicit
    /// preference, so the automatic policy may still mark it).
    cache_directives: Vec<Option<bool>>,
    /// `(tool call id, thought_signature)` pairs collected from tool-use blocks.
    tool_signatures: Vec<(String, String)>,
}

fn has_image_block(msg: &Msg) -> bool {
    msg.content
        .iter()
        .any(|block| matches!(block, ContentBlock::Image(_)))
}

/// Reads the `cache_control` metadata directive of a message.
///
/// Mirrors `OpenAIBaseFormatter#cacheControlDirective`: only booleans count, so
/// a missing or malformed value means no explicit preference.
fn metadata_cache_directive(msg: &Msg) -> Option<bool> {
    msg.metadata
        .get(agentscope_core::metadata_keys::CACHE_CONTROL)
        .and_then(|value| value.as_bool())
}

fn text_content_part(text: impl Into<String>) -> ChatCompletionRequestUserMessageContentPart {
    ChatCompletionRequestUserMessageContentPart::Text(ChatCompletionRequestMessageContentPartText {
        text: text.into(),
    })
}

/// Maps an audio media type to the `input_audio.format` value.
///
/// The API only accepts `wav` / `mp3`; Java's `OpenAIConverterUtils#detectAudioFormat`
/// also yields `opus` / `flac`, which the API rejects, so those degrade to a text
/// reference instead of sending an invalid format.
fn audio_format(media_type: &str) -> Option<InputAudioFormat> {
    let media_type = media_type.to_ascii_lowercase();
    if media_type.contains("wav") {
        Some(InputAudioFormat::Wav)
    } else if media_type.contains("mp3") || media_type.contains("mpeg") {
        Some(InputAudioFormat::Mp3)
    } else {
        None
    }
}

/// `AudioBlock` -> `input_audio` part (base64 sources only; URL audio is not
/// accepted by the API and is sent as a text reference, like Java).
fn audio_content_part(source: &Source) -> ChatCompletionRequestUserMessageContentPart {
    match source {
        Source::Base64(audio) if !audio.data.is_empty() => match audio_format(&audio.media_type) {
            Some(format) => ChatCompletionRequestUserMessageContentPart::InputAudio(
                ChatCompletionRequestMessageContentPartAudio {
                    input_audio: InputAudio {
                        data: audio.data.clone(),
                        format,
                    },
                },
            ),
            None => {
                diagnostics::warn(
                    "audio_unsupported_format",
                    "input_audio",
                    format!(
                        "audio media type '{}' is not accepted by the provider; sent as text",
                        audio.media_type
                    ),
                );
                text_content_part(format!("[Audio - unsupported format: {}]", audio.media_type))
            }
        },
        Source::Base64(_) => text_content_part("[Audio - data missing]"),
        Source::Url(url) if !url.url.is_empty() => {
            diagnostics::warn(
                "audio_url_not_inlined",
                "input_audio",
                "URL-based audio cannot be inlined by input_audio; sent as a text reference",
            );
            text_content_part(format!("[Audio URL: {}]", url.url))
        }
        Source::Url(_) => text_content_part("[Audio URL - missing]"),
        Source::FileId(file) => text_content_part(format!(
            "[Audio - unsupported source: fileid://{}]",
            file.file_id
        )),
    }
}

/// `VideoBlock` -> text reference.
///
/// `async-openai` 0.41 has no `video_url` content part, so the block is degraded
/// to a text reference instead of being dropped silently. Java sends a
/// `video_url` part; the wire difference is recorded in `MODEL__GAP_TRACKER.md`.
fn video_content_part(source: &Source) -> ChatCompletionRequestUserMessageContentPart {
    let reference = match source {
        Source::Url(url) => url.url.clone(),
        Source::Base64(video) if !video.data.is_empty() => {
            format!("data:{};base64,{}", video.media_type, video.data)
        }
        Source::Base64(_) => String::new(),
        Source::FileId(file) => format!("fileid://{}", file.file_id),
    };
    diagnostics::warn(
        "video_not_inlined",
        "video_url",
        "the provider DTO cannot express a video_url part; the video is sent as a text reference",
    );
    if reference.is_empty() {
        text_content_part("[Video - source missing]")
    } else {
        text_content_part(format!("[Video URL: {reference}]"))
    }
}

fn user_message_content(msg: &Msg) -> ChatCompletionRequestUserMessageContent {
    let mut parts = Vec::new();
    // `true` once a non-text block contributed a part: Java's
    // `OpenAIMessageConverter#convertUserMessage` switches to the content-part
    // array exactly when the message carries media (`hasMediaContent`).
    let mut multipart = false;
    for block in &msg.content {
        match block {
            ContentBlock::Text(text) if !text.text.is_empty() => {
                parts.push(text_content_part(text.text.clone()));
            }
            ContentBlock::Image(image) => {
                multipart = true;
                let url = match &image.source {
                    Source::Url(source) => source.url.clone(),
                    Source::Base64(source) => {
                        format!("data:{};base64,{}", source.media_type, source.data)
                    }
                    Source::FileId(source) => {
                        diagnostics::warn(
                            "unsupported_media_type",
                            "content.image",
                            format!(
                                "image source 'fileid://{}' cannot be inlined; sent as a text placeholder",
                                source.file_id
                            ),
                        );
                        parts.push(text_content_part(format!(
                            "[Image - unsupported source: fileid://{}]",
                            source.file_id
                        )));
                        continue;
                    }
                };
                parts.push(ChatCompletionRequestUserMessageContentPart::ImageUrl(
                    ChatCompletionRequestMessageContentPartImage {
                        image_url: ImageUrl {
                            url,
                            // MODEL-17: the caller's `detail` reaches the wire;
                            // `None` keeps the provider default (field omitted).
                            detail: image.detail.map(|detail| match detail {
                                BlockImageDetail::Low => ImageDetail::Low,
                                BlockImageDetail::High => ImageDetail::High,
                                BlockImageDetail::Auto => ImageDetail::Auto,
                            }),
                        },
                    },
                ));
            }
            ContentBlock::Video(video) => {
                multipart = true;
                parts.push(video_content_part(&video.source));
            }
            ContentBlock::Audio(audio) => {
                multipart = true;
                parts.push(audio_content_part(&audio.source));
            }
            // Documents are not accepted by OpenAI: report the dropped content as a
            // text placeholder instead of collapsing the message to empty.
            ContentBlock::Document(d) => {
                let label = d.name.clone().unwrap_or_else(|| d.id.clone());
                diagnostics::warn(
                    "document_not_supported",
                    "content.document",
                    format!(
                        "DocumentBlock is not supported by OpenAI; sending a text placeholder instead of '{label}'"
                    ),
                );
                multipart = true;
                parts.push(text_content_part(format!(
                    "[Document - not supported by OpenAI: {label}]"
                )));
            }
            _ => {}
        }
    }
    if multipart {
        ChatCompletionRequestUserMessageContent::Array(parts)
    } else {
        ChatCompletionRequestUserMessageContent::Text(extract_text_content(msg))
    }
}

fn to_request_messages(messages: Vec<Msg>, reasoning_passback: bool) -> RequestMessages {
    let mut out = Vec::new();
    let mut reasoning = Vec::new();
    let mut reasoning_details = Vec::new();
    let mut cache_directives = Vec::new();
    let mut tool_signatures = Vec::new();
    for msg in messages {
        let cache_directive = metadata_cache_directive(&msg);
        match msg.role {
            MsgRole::System => {
                out.push(ChatCompletionRequestMessage::System(
                    ChatCompletionRequestSystemMessage {
                        content: ChatCompletionRequestSystemMessageContent::Text(
                            extract_text_content(&msg),
                        ),
                        name: None,
                    },
                ));
                reasoning.push(None);
                reasoning_details.push(None);
                cache_directives.push(cache_directive);
            }
            MsgRole::User => {
                out.push(ChatCompletionRequestMessage::User(
                    ChatCompletionRequestUserMessage {
                        content: user_message_content(&msg),
                        name: None,
                    },
                ));
                // Keep `reasoning` 1:1 with the wire messages: the injection step
                // addresses assistant reasoning by message index, so a missing
                // entry here shifts every later assistant turn and drops its
                // `reasoning_content`.
                reasoning.push(None);
                reasoning_details.push(None);
                cache_directives.push(cache_directive);
            }
            MsgRole::Assistant => {
                let text = extract_text_content(&msg);
                let mut tool_calls = Vec::new();
                for tu in msg.tool_use_blocks() {
                    let args = serde_json::to_string(&tu.input).unwrap_or_default();
                    // A provider tool signature (OpenRouter / Gemini-via-OpenAI)
                    // must be echoed back on the same function call, otherwise
                    // the follow-up turn is rejected.
                    if let Some(signature) = tu
                        .metadata
                        .get(ToolUseBlock::METADATA_THOUGHT_SIGNATURE)
                        .and_then(|value| value.as_str())
                        .filter(|signature| !signature.is_empty())
                    {
                        tool_signatures.push((tu.id.clone(), signature.to_string()));
                    }
                    tool_calls.push(ChatCompletionMessageToolCalls::Function(
                        ChatCompletionMessageToolCall {
                            id: tu.id.clone(),
                            function: FunctionCall {
                                name: tu.name.clone(),
                                arguments: args,
                            },
                        },
                    ));
                }
                // Reasoning models (Qwen3, etc.) may yield assistant messages with only
                // thinking blocks (or whose tool_calls were pruned), leaving no text and no
                // tool_calls. Strict OpenAI-compatible APIs reject such a message with
                // "content or tool_calls must be set". Always send `content`, using "" as the
                // fallback �?mirrors Java OpenAIMessageConverter.convertAssistantMessage.
                let content = Some(ChatCompletionRequestAssistantMessageContent::Text(text));
                out.push(ChatCompletionRequestMessage::Assistant(
                    ChatCompletionRequestAssistantMessage {
                        content,
                        tool_calls: if tool_calls.is_empty() {
                            None
                        } else {
                            Some(tool_calls)
                        },
                        ..Default::default()
                    },
                ));
                // DeepSeek requires the assistant turn's reasoning to be echoed
                // back as `reasoning_content`; other providers ignore it. The
                // field must be present (empty string is fine) on *every*
                // assistant turn: a plain-text or synthesized assistant message
                // that omits the key entirely is rejected with HTTP 400 whenever
                // tools are in play.
                reasoning.push(if reasoning_passback {
                    Some(reasoning_text(&msg).unwrap_or_default())
                } else {
                    None
                });
                reasoning_details.push(reasoning_details_of(&msg));
                cache_directives.push(cache_directive);
            }
            MsgRole::Tool => {
                for tr in msg.tool_result_blocks() {
                    let text = convert_tool_result_to_string(&tr.output);
                    if let Some(id) = &tr.id {
                        out.push(ChatCompletionRequestMessage::Tool(
                            ChatCompletionRequestToolMessage {
                                content: ChatCompletionRequestToolMessageContent::Text(text),
                                tool_call_id: id.clone(),
                            },
                        ));
                        reasoning.push(None);
                        reasoning_details.push(None);
                        cache_directives.push(cache_directive);
                    }
                }
            }
        }
    }
    RequestMessages {
        messages: out,
        reasoning,
        reasoning_details,
        cache_directives,
        tool_signatures,
    }
}

/// Concatenates the thinking blocks of a message into the provider's
/// `reasoning_content` string, or `None` when the message has no thinking.
fn reasoning_text(msg: &Msg) -> Option<String> {
    let text = msg
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Thinking(t) => Some(t.thinking.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// The provider `reasoning_details` list recorded on a message's thinking
/// block, or `None` when the message carries none.
fn reasoning_details_of(msg: &Msg) -> Option<serde_json::Value> {
    msg.content.iter().find_map(|block| match block {
        ContentBlock::Thinking(thinking) => thinking
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get(ThinkingBlock::METADATA_REASONING_DETAILS))
            .cloned(),
        _ => None,
    })
}

/// Injects the per-message `reasoning_content` into the serialized request body
/// (`body["messages"][i]["reasoning_content"] = ...`).
///
/// `Some("")` (an assistant turn without thinking) is written as an empty
/// string rather than skipped: DeepSeek rejects requests whose assistant turns
/// omit the key, so the field must always be present when passback is enabled.
/// Messages that are not assistant turns carry `None` and are left untouched.
fn inject_reasoning_content(body: &mut serde_json::Value, reasoning: &[Option<String>]) {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    for (i, entry) in reasoning.iter().enumerate() {
        let Some(text) = entry.as_deref() else {
            continue;
        };
        if let Some(object) = messages.get_mut(i).and_then(|m| m.as_object_mut()) {
            object.insert(
                "reasoning_content".to_string(),
                serde_json::Value::String(text.to_string()),
            );
        }
    }
}

/// Injects the per-message `reasoning_details` list into the serialized request
/// body (`body["messages"][i]["reasoning_details"] = [...]`).
///
/// Replayed regardless of `reasoning_passback`: only gateways that emit the list
/// (OpenRouter / vLLM / Gemini-via-OpenAI) ever see it, and losing it breaks
/// their thinking round-trip.
fn inject_reasoning_details(body: &mut serde_json::Value, details: &[Option<serde_json::Value>]) {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    for (i, entry) in details.iter().enumerate() {
        let Some(details) = entry else {
            continue;
        };
        if let Some(object) = messages.get_mut(i).and_then(|m| m.as_object_mut()) {
            object.insert("reasoning_details".to_string(), details.clone());
        }
    }
}

/// Replays provider tool signatures on the matching function calls
/// (`body["messages"][i]["tool_calls"][j]["function"]["thought_signature"]`).
///
/// `async-openai`'s `FunctionCall` has no such field, and the typed conversion is
/// not 1:1, so the pairs are matched by call id.
fn inject_tool_signatures(body: &mut serde_json::Value, signatures: &[(String, String)]) {
    if signatures.is_empty() {
        return;
    }
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    for message in messages.iter_mut() {
        let Some(tool_calls) = message.get_mut("tool_calls").and_then(|v| v.as_array_mut()) else {
            continue;
        };
        for call in tool_calls.iter_mut() {
            let Some(id) = call.get("id").and_then(|id| id.as_str()) else {
                continue;
            };
            let Some((_, signature)) = signatures.iter().find(|(call_id, _)| call_id == id) else {
                continue;
            };
            if let Some(function) = call.get_mut("function").and_then(|f| f.as_object_mut()) {
                function.insert(
                    "thought_signature".to_string(),
                    serde_json::Value::String(signature.clone()),
                );
            }
        }
    }
}

/// Applies prompt-cache markers to the serialized request body.
///
/// Two passes, mirroring Java `OpenAIMessageConverter#applyCacheControlFromMetadata`
/// followed by `OpenAIBaseFormatter#applyAutomaticCacheControl`:
///
/// 1. a message whose metadata asked for caching (`_cache_control = true`) always
///    gets a marker, regardless of the request-level option;
/// 2. when the caller opted in (`GenerateOptions::cache_control == Some(true)`)
///    the final message and every system message without an explicit directive
///    are marked too, capped at [`MAX_CACHE_MARKERS`] markers per request.
fn apply_cache_control(
    body: &mut serde_json::Value,
    directives: &[Option<bool>],
    automatic: Option<bool>,
) {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    for (index, directive) in directives.iter().enumerate() {
        if *directive != Some(true) {
            continue;
        }
        if let Some(message) = messages.get_mut(index) {
            set_cache_control_on_content(message);
        }
    }
    if automatic != Some(true) || messages.is_empty() {
        return;
    }
    let mut marker_count = count_cache_markers(messages);
    if marker_count > MAX_CACHE_MARKERS {
        diagnostics::warn(
            "cache_markers_over_limit",
            CACHE_CONTROL_FIELD,
            format!(
                "request already carries {marker_count} cache markers; the provider only uses the last {MAX_CACHE_MARKERS}, so automatic markers are skipped"
            ),
        );
        return;
    }
    let mut candidates = vec![messages.len() - 1];
    for (index, message) in messages.iter().enumerate() {
        if message.get("role").and_then(|role| role.as_str()) == Some("system") {
            candidates.push(index);
        }
    }
    for index in candidates {
        if marker_count >= MAX_CACHE_MARKERS {
            break;
        }
        if directives.get(index).copied().flatten().is_some() {
            continue;
        }
        if let Some(message) = messages.get_mut(index) {
            if message_has_cache_marker(message) {
                continue;
            }
            set_cache_control_on_content(message);
            marker_count += 1;
        }
    }
}

/// Places a `cache_control` marker on the final content part of a wire message,
/// converting string content into a single text part when required.
///
/// Mirrors `OpenAIBaseFormatter#setCacheControlOnContent`: an existing marker is
/// preserved (never overwritten).
fn set_cache_control_on_content(message: &mut serde_json::Value) {
    let Some(object) = message.as_object_mut() else {
        return;
    };
    match object.get_mut("content") {
        Some(serde_json::Value::String(text)) => {
            let mut part = serde_json::json!({ "type": "text", "text": text.clone() });
            if let Some(part_object) = part.as_object_mut() {
                part_object.insert(CACHE_CONTROL_FIELD.to_string(), ephemeral_cache_control());
            }
            *object.get_mut("content").expect("content") = serde_json::Value::Array(vec![part]);
        }
        Some(serde_json::Value::Array(parts)) => {
            if let Some(part) = parts.last_mut() {
                if !has_cache_marker(part) {
                    if let Some(part_object) = part.as_object_mut() {
                        part_object
                            .insert(CACHE_CONTROL_FIELD.to_string(), ephemeral_cache_control());
                    }
                }
            }
        }
        _ => {}
    }
}

fn ephemeral_cache_control() -> serde_json::Value {
    serde_json::json!({ "type": "ephemeral" })
}

fn has_cache_marker(part: &serde_json::Value) -> bool {
    part.get(CACHE_CONTROL_FIELD)
        .is_some_and(|value| !value.is_null())
}

fn message_has_cache_marker(message: &serde_json::Value) -> bool {
    message
        .get("content")
        .and_then(|content| content.as_array())
        .is_some_and(|parts| parts.iter().any(has_cache_marker))
}

/// Counts the content parts already carrying a marker.
fn count_cache_markers(messages: &[serde_json::Value]) -> usize {
    messages
        .iter()
        .filter_map(|message| message.get("content").and_then(|c| c.as_array()))
        .flatten()
        .filter(|part| has_cache_marker(part))
        .count()
}

fn to_request_tools(tools: Vec<ToolSchema>) -> Option<Vec<ChatCompletionTools>> {
    if tools.is_empty() {
        return None;
    }
    Some(
        tools
            .into_iter()
            .map(|t| {
                let parameters = if t.parameters.is_empty() {
                    None
                } else {
                    Some(serde_json::Value::Object(t.parameters))
                };
                let description = if t.description.is_empty() {
                    None
                } else {
                    Some(t.description)
                };
                ChatCompletionTools::Function(ChatCompletionTool {
                    function: FunctionObject {
                        name: t.name,
                        description,
                        parameters,
                        strict: t.strict,
                    },
                })
            })
            .collect(),
    )
}

fn to_tool_choice(tc: &ToolChoice) -> Option<ChatCompletionToolChoiceOption> {
    match tc {
        ToolChoice::Auto => Some(ChatCompletionToolChoiceOption::Mode(
            ToolChoiceOptions::Auto,
        )),
        ToolChoice::None => Some(ChatCompletionToolChoiceOption::Mode(
            ToolChoiceOptions::None,
        )),
        ToolChoice::Required => Some(ChatCompletionToolChoiceOption::Mode(
            ToolChoiceOptions::Required,
        )),
        ToolChoice::Specific { tool_name } => Some(ChatCompletionToolChoiceOption::Function(
            ChatCompletionNamedToolChoice {
                function: FunctionName {
                    name: tool_name.clone(),
                },
            },
        )),
    }
}

fn to_reasoning_effort(s: &str) -> Option<ReasoningEffort> {
    match s.trim().to_ascii_lowercase().as_str() {
        "none" => Some(ReasoningEffort::None),
        "minimal" => Some(ReasoningEffort::Minimal),
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        "xhigh" | "x-high" => Some(ReasoningEffort::Xhigh),
        _ => None,
    }
}

fn to_response_format(
    rf: &ResponseFormat,
    strict_default: Option<bool>,
) -> Option<OaiResponseFormat> {
    match rf.format_type.as_str() {
        "json_object" => Some(OaiResponseFormat::JsonObject),
        "json_schema" => {
            let js = rf.json_schema.as_ref()?;
            let name = js
                .name
                .clone()
                .unwrap_or_else(|| "output_schema".to_string());
            // A schema-level `strict` always wins; the model-level default only fills
            // the gap when the request schema leaves it unset (Java `applyStrictJsonSchemaDefault`).
            let strict = js.strict.or(strict_default);
            Some(OaiResponseFormat::JsonSchema {
                json_schema: ResponseFormatJsonSchema {
                    name,
                    schema: serde_json::Value::Object(js.schema.clone()),
                    strict,
                    // MODEL-17: the schema description is forwarded verbatim.
                    description: js.description.clone(),
                },
            })
        }
        _ => None,
    }
}

/// A partial tool call accumulated from streamed fragments keyed by `index`.
#[derive(Default)]
struct ToolCallFragment {
    id: String,
    name: String,
    arguments: String,
    /// OpenRouter / Gemini-via-OpenAI tool signature (`thought_signature`).
    thought_signature: Option<String>,
}

/// A single SSE chunk from an OpenAI-compatible streaming response, decoded with
/// an extra `reasoning_content` field so thinking deltas (e.g. DeepSeek/O1) can
/// be surfaced incrementally. Serde ignores any other unknown fields.
#[derive(Debug, Deserialize)]
struct OpenAiStreamChunk {
    #[serde(default)]
    id: String,
    #[serde(default)]
    choices: Vec<OpenAiStreamChoice>,
    #[serde(default)]
    usage: Option<CompletionUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiStreamChoice {
    #[serde(default)]
    delta: OpenAiStreamDelta,
    #[serde(default)]
    finish_reason: Option<FinishReason>,
}

#[derive(Debug, Deserialize, Default)]
struct OpenAiStreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    /// OpenRouter / vLLM / Gemini-via-OpenAI reasoning detail objects.
    #[serde(default)]
    reasoning_details: Option<serde_json::Value>,
    #[serde(default)]
    refusal: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAiToolCallChunk>>,
}

/// A streamed tool-call fragment.
///
/// `async-openai`'s chunk type drops `thought_signature`, which OpenRouter /
/// Gemini-via-OpenAI send on the function object; it is captured here so it can
/// be replayed (MODEL-05).
#[derive(Debug, Deserialize, Default)]
struct OpenAiToolCallChunk {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<OpenAiFunctionChunk>,
}

#[derive(Debug, Deserialize, Default)]
struct OpenAiFunctionChunk {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
    #[serde(default, alias = "thoughtSignature")]
    thought_signature: Option<String>,
}

impl OpenAiStreamDelta {
    /// Reasoning text carried by this chunk, from either the OpenAI-style
    /// `reasoning_content` field (DeepSeek/O1) or the `reasoning` field used by
    /// some gateways (e.g. opencode zen). Whichever is present wins.
    fn reasoning_text(&self) -> Option<&str> {
        reasoning_text_from(self.reasoning_content.as_deref(), self.reasoning.as_deref())
    }
}

/// Shared reasoning-field resolution for both streaming deltas and
/// non-streaming completions: prefers `reasoning_content` (DeepSeek/O1) and
/// falls back to `reasoning` (some OpenAI-compatible gateways). Empty strings
/// are treated as absent so a present-but-blank field does not emit an empty
/// thinking block.
fn reasoning_text_from<'a>(
    reasoning_content: Option<&'a str>,
    reasoning: Option<&'a str>,
) -> Option<&'a str> {
    if let Some(r) = reasoning_content {
        if !r.is_empty() {
            return Some(r);
        }
    }
    if let Some(r) = reasoning {
        if !r.is_empty() {
            return Some(r);
        }
    }
    None
}

/// Maps OpenAI completion usage onto the core [`ChatUsage`]. Shared by the
/// streaming accumulator and the non-streaming mapper so the two paths cannot
/// drift (`cached_tokens` is a subset of `input_tokens` when reported).
fn usage_from_completion(usage: &CompletionUsage) -> ChatUsage {
    ChatUsage {
        input_tokens: usage.prompt_tokens as u64,
        output_tokens: usage.completion_tokens as u64,
        cached_tokens: usage
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens)
            .unwrap_or(0) as u64,
        ..Default::default()
    }
}

/// Builds a [`ToolUseBlock`] from a fully-assembled tool call. When the
/// arguments are not a JSON object (invalid JSON, or a bare string) the raw
/// text is kept in `content` so it still reaches the event stream instead of
/// being silently dropped.
///
/// `thought_signature` (OpenRouter / Gemini-via-OpenAI) is persisted under
/// [`ToolUseBlock::METADATA_THOUGHT_SIGNATURE`] so later turns can replay it.
fn tool_use_block(
    id: String,
    name: String,
    arguments: &str,
    thought_signature: Option<String>,
) -> ToolUseBlock {
    let input = parse_arguments(arguments);
    let mut tool_use = ToolUseBlock::new(id, name, input);
    if tool_use.input.is_empty() && !arguments.is_empty() {
        tool_use.content = Some(arguments.to_string());
    }
    if let Some(signature) = thought_signature.filter(|signature| !signature.is_empty()) {
        tool_use.metadata.insert(
            ToolUseBlock::METADATA_THOUGHT_SIGNATURE.to_string(),
            serde_json::Value::String(signature),
        );
    }
    tool_use
}

/// Builds a thinking block from a reasoning text and/or `reasoning_details`.
///
/// Mirrors Java's `OpenAIResponseParser` (thinking first: `reasoning_content` +
/// `reasoning_details`): the text is the model's visible reasoning, while the
/// details list is preserved verbatim under
/// [`ThinkingBlock::METADATA_REASONING_DETAILS`] for replay.
fn thinking_block(
    text: Option<&str>,
    details: Option<&serde_json::Value>,
) -> Option<ThinkingBlock> {
    let text = text.filter(|text| !text.is_empty());
    let details = details.filter(|details| !details.is_null());
    if text.is_none() && details.is_none() {
        return None;
    }
    let mut block = ThinkingBlock::new(text.unwrap_or_default());
    if let Some(details) = details {
        block
            .metadata
            .get_or_insert_with(JsonMap::new)
            .insert(
                ThinkingBlock::METADATA_REASONING_DETAILS.to_string(),
                details.clone(),
            );
    }
    Some(block)
}

/// Incremental stream decoder: emits one [`ChatResponse`] per SSE chunk for
/// text/thinking deltas (true token-by-token streaming) while accumulating
/// tool-call fragments, usage and finish reason across chunks. The accumulated
/// tool calls / usage are flushed as a single final [`ChatResponse`] when the
/// underlying stream ends.
#[derive(Default)]
struct StreamIncrementer {
    id: String,
    tool_calls: BTreeMap<u32, ToolCallFragment>,
    usage: Option<ChatUsage>,
    finish_reason: Option<String>,
}

impl StreamIncrementer {
    /// Consumes one chunk, returning a `ChatResponse` carrying only the
    /// incremental text/thinking content of this chunk (or `None` when the chunk
    /// carries no incremental content, e.g. an empty usage-only chunk).
    fn accumulate(
        &mut self,
        chunk: &OpenAiStreamChunk,
    ) -> Option<agentscope_core::model::ChatResponse> {
        if self.id.is_empty() && !chunk.id.is_empty() {
            self.id = chunk.id.clone();
        }
        let mut blocks: Vec<ContentBlock> = Vec::new();
        for choice in &chunk.choices {
            let delta = &choice.delta;
            if let Some(content) = &delta.content {
                if !content.is_empty() {
                    blocks.push(ContentBlock::Text(TextBlock::new(content.clone())));
                }
            }
            if let Some(thinking) = thinking_block(
                delta.reasoning_text(),
                delta.reasoning_details.as_ref(),
            ) {
                blocks.push(ContentBlock::Thinking(thinking));
            }
            if let Some(refusal) = &delta.refusal {
                if !refusal.is_empty() {
                    blocks.push(ContentBlock::Text(TextBlock::new(refusal.clone())));
                }
            }
            if let Some(tool_calls) = &delta.tool_calls {
                for tc in tool_calls {
                    let frag = self.tool_calls.entry(tc.index).or_default();
                    if let Some(id) = &tc.id {
                        frag.id = id.clone();
                    }
                    if let Some(function) = &tc.function {
                        if let Some(name) = &function.name {
                            frag.name = name.clone();
                        }
                        if let Some(args) = &function.arguments {
                            frag.arguments.push_str(args);
                        }
                        if let Some(signature) = &function.thought_signature {
                            frag.thought_signature = Some(signature.clone());
                        }
                    }
                }
            }
            if let Some(fr) = &choice.finish_reason {
                self.finish_reason = Some(finish_reason_string(fr.clone()));
            }
        }
        if let Some(usage) = &chunk.usage {
            let incoming = usage_from_completion(usage);
            self.usage = Some(usage::merge_non_zero(
                &self.usage.unwrap_or_default(),
                &incoming,
            ));
        }
        if blocks.is_empty() {
            None
        } else {
            Some(agentscope_core::model::ChatResponse::new(
                self.id.clone(),
                blocks,
                ChatUsage::default(),
                None,
                None,
            ))
        }
    }

    /// Flushes the accumulated tool calls, usage and finish reason into a final
    /// [`ChatResponse`]. Returns `None` when nothing was accumulated.
    fn into_final(self) -> Option<agentscope_core::model::ChatResponse> {
        let mut blocks: Vec<ContentBlock> = Vec::new();
        for frag in self.tool_calls.values() {
            blocks.push(ContentBlock::ToolUse(tool_use_block(
                frag.id.clone(),
                frag.name.clone(),
                &frag.arguments,
                frag.thought_signature.clone(),
            )));
        }
        if blocks.is_empty() && self.usage.is_none() {
            return None;
        }
        Some(agentscope_core::model::ChatResponse::new(
            self.id,
            blocks,
            self.usage.unwrap_or_default(),
            None,
            self.finish_reason,
        ))
    }
}

fn finish_reason_string(fr: FinishReason) -> String {
    match fr {
        FinishReason::Stop => FinishReasons::STOP.to_string(),
        FinishReason::Length => FinishReasons::LENGTH.to_string(),
        FinishReason::ToolCalls => FinishReasons::TOOL_CALLS.to_string(),
        FinishReason::ContentFilter => "content_filter".to_string(),
        FinishReason::FunctionCall => "function_call".to_string(),
    }
}

fn parse_arguments(args: &str) -> JsonMap {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// A single non-streaming chat completion (`stream: false`).
///
/// Decoded with the extra `reasoning_content` / `reasoning` fields that the
/// stock `CreateChatCompletionResponse` cannot represent, so the non-streaming
/// path surfaces thinking exactly like the streaming path does.
#[derive(Debug, Deserialize)]
struct OpenAiCompletionResponse {
    #[serde(default)]
    id: String,
    #[serde(default)]
    choices: Vec<OpenAiCompletionChoice>,
    #[serde(default)]
    usage: Option<CompletionUsage>,
}

#[derive(Debug, Deserialize, Default)]
struct OpenAiCompletionChoice {
    #[serde(default)]
    message: OpenAiCompletionMessage,
    #[serde(default)]
    finish_reason: Option<FinishReason>,
}

#[derive(Debug, Deserialize, Default)]
struct OpenAiCompletionMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    reasoning_details: Option<serde_json::Value>,
    #[serde(default)]
    refusal: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAiCompletionToolCall>>,
}

/// A non-streaming tool call decoded with the extra `thought_signature` field.
#[derive(Debug, Deserialize, Default)]
struct OpenAiCompletionToolCall {
    #[serde(default)]
    id: String,
    #[serde(default)]
    function: OpenAiCompletionFunction,
}

#[derive(Debug, Deserialize, Default)]
struct OpenAiCompletionFunction {
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: String,
    #[serde(default, alias = "thoughtSignature")]
    thought_signature: Option<String>,
}

/// Maps a non-streaming completion onto the core [`ChatResponse`].
///
/// Field mapping intentionally mirrors [`StreamIncrementer`]: thinking first,
/// then text, then refusal (rendered as text, as on the streaming path), then
/// tool calls. The first choice wins — the agent loop never requests `n > 1`.
/// Custom (non-function) tool calls are skipped because core only models
/// function tools; they are rare and not used by any supported backend.
fn completion_to_response(
    response: OpenAiCompletionResponse,
) -> agentscope_core::model::ChatResponse {
    let mut blocks: Vec<ContentBlock> = Vec::new();
    let mut finish_reason: Option<String> = None;
    for choice in &response.choices {
        let message = &choice.message;
        if let Some(thinking) = thinking_block(
            reasoning_text_from(
                message.reasoning_content.as_deref(),
                message.reasoning.as_deref(),
            ),
            message.reasoning_details.as_ref(),
        ) {
            blocks.push(ContentBlock::Thinking(thinking));
        }
        if let Some(content) = &message.content {
            if !content.is_empty() {
                blocks.push(ContentBlock::Text(TextBlock::new(content.clone())));
            }
        }
        if let Some(refusal) = &message.refusal {
            if !refusal.is_empty() {
                blocks.push(ContentBlock::Text(TextBlock::new(refusal.clone())));
            }
        }
        for call in message.tool_calls.iter().flatten() {
            blocks.push(ContentBlock::ToolUse(tool_use_block(
                call.id.clone(),
                call.function.name.clone(),
                &call.function.arguments,
                call.function.thought_signature.clone(),
            )));
        }
        if let Some(fr) = &choice.finish_reason {
            finish_reason = Some(finish_reason_string(fr.clone()));
        }
    }
    let usage = response
        .usage
        .as_ref()
        .map(usage_from_completion)
        .unwrap_or_default();
    agentscope_core::model::ChatResponse::new(response.id, blocks, usage, None, finish_reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::{
        Base64Source, ContentBlock, ImageBlock, Msg, MsgRole, TextBlock, ToolResultBlock,
        ToolUseBlock,
    };

    fn sample_msg() -> Msg {
        Msg::builder()
            .role(MsgRole::User)
            .text_content("hello")
            .build()
    }

    #[test]
    fn user_message_converts_to_user_role() {
        let msgs = to_request_messages(vec![sample_msg()], false).messages;
        assert_eq!(msgs.len(), 1);
        match &msgs[0] {
            ChatCompletionRequestMessage::User(u) => {
                let text = match &u.content {
                    ChatCompletionRequestUserMessageContent::Text(t) => t.clone(),
                    _ => String::new(),
                };
                assert_eq!(text, "hello");
            }
            _ => panic!("expected user message"),
        }
    }

    #[test]
    fn user_image_becomes_data_url_part() {
        let msg = Msg::user_blocks(vec![
            ContentBlock::Text(TextBlock::new("look")),
            ContentBlock::Image(ImageBlock::new(agentscope_core::message::Source::Base64(
                Base64Source::new("image/png", "iVBORw0KGgo="),
            ))),
        ]);
        let (req, _) = build_request(
            "gpt-4o".into(),
            vec![msg],
            vec![],
            GenerateOptions::default(),
            false,
            None,
            None,
        );
        let body = serde_json::to_value(&req).unwrap();
        assert_eq!(body["messages"][0]["content"][0]["text"], "look");
        assert_eq!(
            body["messages"][0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,iVBORw0KGgo="
        );
    }

    #[test]
    fn multi_agent_formatter_merges_agent_turns_into_one_history_message() {
        // Java `OpenAIMultiAgentFormatter` / `OpenAIConversationMerger`: every
        // run of plain agent messages collapses into one `<history>` user
        // message (only the first run carries the prompt), while system messages
        // and tool sequences keep their ordinary mapping. The selector itself
        // must never reach the wire.
        let named = |text: &str, name: &str| {
            Msg::builder()
                .role(MsgRole::Assistant)
                .name(name)
                .text_content(text)
                .build()
        };
        let messages = vec![
            Msg::system("be helpful"),
            named("hello", "Alice"),
            named("hi there", "Bob"),
            Msg::tool("call-1", "search", "42"),
            named("done", "Alice"),
        ];
        let mut options = GenerateOptions::default();
        options.additional_body_params.insert(
            crate::common::MULTI_AGENT_FORMATTER_KEY.to_string(),
            serde_json::json!(true),
        );

        let (req, _) = build_request(
            "gpt-4o".into(),
            messages,
            vec![],
            options,
            false,
            None,
            None,
        );
        let body = serde_json::to_value(&req).unwrap();
        let wire = body["messages"].as_array().expect("messages array");

        assert_eq!(wire[0]["role"], "system", "system stays individual: {body:?}");
        let first_history = wire[1]["content"].as_str().expect("history text");
        assert!(
            first_history.starts_with(crate::common::DEFAULT_CONVERSATION_HISTORY_PROMPT),
            "the first agent group carries the prompt: {first_history:?}"
        );
        assert!(first_history.contains("Alice: hello"));
        assert!(first_history.contains("Bob: hi there"));
        assert!(first_history.contains(crate::common::HISTORY_START_TAG));
        assert!(first_history.contains(crate::common::HISTORY_END_TAG));

        let last = wire.last().expect("last message");
        assert_eq!(last["role"], "user");
        let last_text = last["content"].as_str().unwrap_or_default();
        assert!(
            last_text.contains("Alice: done"),
            "the run after the tool sequence merges as well: {body:?}"
        );
        assert!(
            !last_text.contains(crate::common::DEFAULT_CONVERSATION_HISTORY_PROMPT),
            "only the first agent group carries the prompt"
        );
        assert!(
            !body.to_string().contains("multiAgentFormatter"),
            "the selector leaked into the body: {body:?}"
        );
    }

    #[test]
    #[allow(deprecated)]
    fn reasoning_model_drops_unsupported_sampling_params() {
        let options = GenerateOptions {
            temperature: Some(0.7),
            top_p: Some(0.9),
            frequency_penalty: Some(0.1),
            seed: Some(7),
            max_tokens: Some(100),
            ..Default::default()
        };
        let (req, _) = build_request(
            "o3-mini".into(),
            vec![sample_msg()],
            vec![],
            options,
            false,
            None,
            None,
        );
        assert!(req.temperature.is_none(), "o3 rejects temperature");
        assert!(req.top_p.is_none(), "o3 rejects top_p");
        assert!(req.frequency_penalty.is_none(), "o3 rejects penalties");
        assert!(req.seed.is_none(), "o3 rejects seed");
        assert_eq!(req.max_completion_tokens, Some(100));
        assert!(req.max_tokens.is_none());
    }

    #[test]
    #[allow(deprecated)]
    fn compatible_gateway_uses_legacy_max_tokens() {
        let options = GenerateOptions {
            max_tokens: Some(64),
            temperature: Some(0.2),
            ..Default::default()
        };
        let (req, _) = build_request(
            "deepseek-chat".into(),
            vec![sample_msg()],
            vec![],
            options,
            false,
            None,
            None,
        );
        assert_eq!(req.max_tokens, Some(64));
        assert!(req.max_completion_tokens.is_none());
        assert_eq!(req.temperature, Some(0.2), "deepseek accepts temperature");
    }

    #[test]
    fn creation_time_options_reach_the_request_body() {
        // MODEL-02: `ModelCreationContext.options` must be merged *under* each
        // request (Java `mergeOptions(options, configuredOptions)`), otherwise the
        // builder-configured knobs (and the reasoning suffix that seeds
        // `thinking_budget`) never reach the wire.
        let mut defaults = GenerateOptions::builder()
            .temperature(0.3)
            .max_tokens(128)
            .thinking_budget(1024)
            .build();
        defaults
            .additional_body_params
            .insert("enable_thinking".to_string(), serde_json::json!(true));
        defaults.additional_body_params.insert(
            STRICT_JSON_SCHEMA_OPTION.to_string(),
            serde_json::json!(true),
        );
        let ctx = ModelCreationContext::new("openai", "gateway-chat")
            .api_key("sk-test")
            .base_url("https://example.test/v1")
            .options(defaults);
        let model = OpenAiChatModel::from_context(&ctx).expect("model");

        let (body, streaming) =
            model.prepare_body(vec![sample_msg()], vec![], GenerateOptions::default());
        assert!(streaming, "the historical streaming default is kept");
        let temperature = body["temperature"].as_f64().expect("temperature");
        assert!((temperature - 0.3).abs() < 1e-6, "body: {body:?}");
        assert_eq!(body["max_completion_tokens"], serde_json::json!(128));
        assert_eq!(body["thinking_budget"], serde_json::json!(1024));
        assert_eq!(body["enable_thinking"], serde_json::json!(true));
        assert!(
            body.get(STRICT_JSON_SCHEMA_OPTION).is_none(),
            "provider knob leaked into the wire body: {body:?}"
        );
    }

    #[test]
    fn provider_capability_defaults_match_java() {
        // MODEL-06: Java `OpenAIModelProvider#create` defaults — native structured
        // output on, its with-tools variant following it, and `ToolChoice::Specific`
        // only on the official endpoint.
        let gateway = OpenAiChatModel::from_context(
            &ModelCreationContext::new("openai", "gpt-4o")
                .api_key("sk-test")
                .base_url("https://example.test/v1"),
        )
        .expect("model");
        assert!(gateway.supports_native_structured_output());
        assert!(gateway.supports_native_structured_output_with_tools());
        assert!(
            !gateway.supports_tool_choice_specific(),
            "a third-party gateway accepts `tool_choice` but ignores the forced name"
        );

        let official = OpenAiChatModel::from_context(
            &ModelCreationContext::new("openai", "gpt-4o")
                .api_key("sk-test")
                .base_url("https://api.openai.com/v1"),
        )
        .expect("model");
        assert!(official.supports_tool_choice_specific());
    }

    #[test]
    fn capability_options_override_the_defaults() {
        let mut options = GenerateOptions::default();
        for (key, value) in [
            (NATIVE_STRUCTURED_OUTPUT_OPTION, serde_json::json!(false)),
            (
                NATIVE_STRUCTURED_OUTPUT_WITH_TOOLS_OPTION,
                serde_json::json!(true),
            ),
            (
                SUPPORTS_TOOL_CHOICE_SPECIFIC_OPTION,
                serde_json::json!(true),
            ),
            (CONTEXT_WINDOW_SIZE_OPTION, serde_json::json!(32000)),
        ] {
            options
                .additional_body_params
                .insert(key.to_string(), value);
        }
        let model = OpenAiChatModel::from_context(
            &ModelCreationContext::new("openai", "gateway-chat")
                .api_key("sk-test")
                .base_url("https://example.test/v1")
                .options(options),
        )
        .expect("model");
        assert!(!model.supports_native_structured_output());
        assert!(model.supports_native_structured_output_with_tools());
        assert!(model.supports_tool_choice_specific());
        assert_eq!(model.context_window_size(), 32000);

        // The knobs are provider-side only: they never reach the wire body.
        let (body, _) = model.prepare_body(vec![sample_msg()], vec![], GenerateOptions::default());
        assert!(body.get(NATIVE_STRUCTURED_OUTPUT_OPTION).is_none(), "{body:?}");
        assert!(body.get(CONTEXT_WINDOW_SIZE_OPTION).is_none(), "{body:?}");
    }

    #[test]
    fn unknown_reasoning_effort_is_passed_through_verbatim() {
        // MODEL-17: `async-openai`'s enum only covers the documented levels. A
        // vendor value it cannot model must still reach the wire (Java sets the
        // configured string as-is) instead of being dropped.
        let ctx = ModelCreationContext::new("openai", "gateway-chat").api_key("sk-test");
        let model = OpenAiChatModel::from_context(&ctx).expect("model");
        let per_call = GenerateOptions::builder()
            .reasoning_effort("ultra")
            .build();
        let (body, _) = model.prepare_body(vec![sample_msg()], vec![], per_call);
        assert_eq!(body["reasoning_effort"], serde_json::json!("ultra"));

        // Known levels keep going through the typed field.
        let per_call = GenerateOptions::builder().reasoning_effort("high").build();
        let (body, _) = model.prepare_body(vec![sample_msg()], vec![], per_call);
        assert_eq!(body["reasoning_effort"], serde_json::json!("high"));
    }

    #[test]
    fn per_call_options_win_over_creation_defaults() {
        let ctx = ModelCreationContext::new("openai", "gateway-chat")
            .api_key("sk-test")
            .options(GenerateOptions::builder().temperature(0.9).build());
        let model = OpenAiChatModel::from_context(&ctx).expect("model");
        let per_call = GenerateOptions::builder().temperature(0.1).build();
        let (body, _) = model.prepare_body(vec![sample_msg()], vec![], per_call);
        let temperature = body["temperature"].as_f64().expect("temperature");
        assert!((temperature - 0.1).abs() < 1e-6, "body: {body:?}");
    }

    #[test]
    fn usage_merges_across_chunks_without_zero_clobber() {
        let chunk =
            |json: serde_json::Value| serde_json::from_value::<OpenAiStreamChunk>(json).unwrap();
        let mut inc = StreamIncrementer::default();
        inc.accumulate(&chunk(serde_json::json!({
            "id": "x", "choices": [],
            "usage": {"prompt_tokens": 10, "completion_tokens": 0, "total_tokens": 10}
        })));
        inc.accumulate(&chunk(serde_json::json!({
            "id": "x", "choices": [],
            "usage": {"prompt_tokens": 0, "completion_tokens": 5, "total_tokens": 5}
        })));
        let final_resp = inc.into_final().expect("final response");
        assert_eq!(final_resp.usage.input_tokens, 10);
        assert_eq!(final_resp.usage.output_tokens, 5);
    }

    #[test]
    fn assistant_tool_calls_round_trip() {
        let input: JsonMap = serde_json::from_str(r#"{"city":"Beijing"}"#).unwrap();
        let msg = Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![ContentBlock::ToolUse(ToolUseBlock::new(
                "call_1",
                "get_weather",
                input,
            ))])
            .build();
        let msgs = to_request_messages(vec![msg], false).messages;
        match &msgs[0] {
            ChatCompletionRequestMessage::Assistant(a) => {
                let calls = a.tool_calls.as_ref().unwrap();
                assert_eq!(calls.len(), 1);
            }
            _ => panic!("expected assistant message"),
        }
    }

    #[test]
    fn assistant_without_text_or_tool_calls_still_has_content() {
        // Only a thinking block: extract_text_content yields "", so tool_calls is None.
        let msg = Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![ContentBlock::Thinking(ThinkingBlock::new("hmm"))])
            .build();
        let msgs = to_request_messages(vec![msg], false).messages;
        let json = serde_json::to_value(&msgs[0]).unwrap();
        assert_eq!(json["role"], "assistant");
        assert_eq!(json["content"], "");
        assert!(json.get("tool_calls").is_none());
    }

    #[test]
    fn assistant_with_only_tool_calls_still_has_content() {
        let input: JsonMap = serde_json::from_str(r#"{"city":"Beijing"}"#).unwrap();
        let msg = Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![ContentBlock::ToolUse(ToolUseBlock::new(
                "call_1",
                "get_weather",
                input,
            ))])
            .build();
        let msgs = to_request_messages(vec![msg], false).messages;
        let json = serde_json::to_value(&msgs[0]).unwrap();
        assert_eq!(json["content"], "");
        assert_eq!(json["tool_calls"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn reasoning_passback_replays_thinking_as_reasoning_content() {
        let msg = Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![
                ContentBlock::Thinking(ThinkingBlock::new("step 1")),
                ContentBlock::Text(TextBlock::new("answer")),
            ])
            .build();
        let RequestMessages {
            messages,
            reasoning,
            ..
        } = to_request_messages(vec![msg], true);
        let mut body = serde_json::json!({ "messages": messages });
        inject_reasoning_content(&mut body, &reasoning);
        assert_eq!(body["messages"][0]["reasoning_content"], "step 1");
        assert_eq!(body["messages"][0]["content"], "answer");
    }

    #[test]
    fn reasoning_passback_disabled_omits_field() {
        let msg = Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![
                ContentBlock::Thinking(ThinkingBlock::new("step 1")),
                ContentBlock::Text(TextBlock::new("answer")),
            ])
            .build();
        let RequestMessages {
            messages,
            reasoning,
            ..
        } = to_request_messages(vec![msg], false);
        let mut body = serde_json::json!({ "messages": messages });
        inject_reasoning_content(&mut body, &reasoning);
        assert!(body["messages"][0].get("reasoning_content").is_none());
    }

    #[test]
    fn reasoning_passback_backfills_empty_string_without_thinking() {
        // A plain-text assistant turn (no ThinkingBlock) must still carry the
        // `reasoning_content` key as an empty string, otherwise DeepSeek rejects
        // the request with HTTP 400 when tools are in play.
        let msg = Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![ContentBlock::Text(TextBlock::new("answer"))])
            .build();
        let RequestMessages {
            messages,
            reasoning,
            ..
        } = to_request_messages(vec![msg], true);
        let mut body = serde_json::json!({ "messages": messages });
        inject_reasoning_content(&mut body, &reasoning);
        assert_eq!(body["messages"][0]["reasoning_content"], "");
        assert_eq!(body["messages"][0]["content"], "answer");
    }

    #[test]
    fn reasoning_passback_keeps_non_empty_thinking_verbatim() {
        let msg = Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![
                ContentBlock::Thinking(ThinkingBlock::new("step 1")),
                ContentBlock::Text(TextBlock::new("answer")),
            ])
            .build();
        let RequestMessages {
            messages,
            reasoning,
            ..
        } = to_request_messages(vec![msg], true);
        let mut body = serde_json::json!({ "messages": messages });
        inject_reasoning_content(&mut body, &reasoning);
        assert_eq!(body["messages"][0]["reasoning_content"], "step 1");
    }

    #[test]
    fn reasoning_passback_stays_aligned_when_tool_message_fans_out() {
        let assistant = Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![ContentBlock::Thinking(ThinkingBlock::new("why"))])
            .build();
        let tool = Msg::builder()
            .role(MsgRole::Tool)
            .content(vec![
                ContentBlock::ToolResult(ToolResultBlock::new(
                    "call_1",
                    "get_weather",
                    vec![ContentBlock::Text(TextBlock::new("a"))],
                )),
                ContentBlock::ToolResult(ToolResultBlock::new(
                    "call_2",
                    "get_weather",
                    vec![ContentBlock::Text(TextBlock::new("b"))],
                )),
            ])
            .build();
        let RequestMessages {
            messages,
            reasoning,
            ..
        } = to_request_messages(vec![assistant, tool], true);
        assert_eq!(messages.len(), 3);
        assert_eq!(reasoning.len(), 3);
        assert_eq!(reasoning[0].as_deref(), Some("why"));
        assert!(reasoning[1].is_none());
        assert!(reasoning[2].is_none());
    }

    #[test]
    fn reasoning_passback_stays_aligned_across_user_turns() {
        // A user turn produces a wire message too, so the parallel `reasoning`
        // vector must be extended for it as well. Otherwise every later
        // assistant turn is shifted by one and its `reasoning_content` lands on
        // the preceding user message, leaving the assistant turn without the
        // field — DeepSeek then answers HTTP 400 ("The reasoning_content in the
        // thinking mode must be passed back to the API").
        let user = Msg::builder()
            .role(MsgRole::User)
            .content(vec![ContentBlock::Text(TextBlock::new("hi"))])
            .build();
        let assistant = Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![
                ContentBlock::Thinking(ThinkingBlock::new("why")),
                ContentBlock::Text(TextBlock::new("hello")),
            ])
            .build();
        let RequestMessages {
            messages,
            reasoning,
            ..
        } = to_request_messages(vec![user, assistant], true);
        assert_eq!(messages.len(), 2);
        assert_eq!(reasoning.len(), messages.len(), "reasoning must stay 1:1");
        let mut body = serde_json::json!({ "messages": messages });
        inject_reasoning_content(&mut body, &reasoning);
        assert!(body["messages"][0].get("reasoning_content").is_none());
        assert_eq!(body["messages"][1]["reasoning_content"], "why");
    }

    #[test]
    fn deepseek_models_require_reasoning_passback() {
        assert!(OpenAiChatModel::requires_reasoning_passback(
            "opencode-go/deepseek-v4.1-flash"
        ));
        assert!(OpenAiChatModel::requires_reasoning_passback(
            "DeepSeek-Chat"
        ));
        assert!(!OpenAiChatModel::requires_reasoning_passback("gpt-4o"));
    }

    #[test]
    fn tool_message_uses_result_id() {
        let tr = ToolResultBlock::new(
            "call_9",
            "get_weather",
            vec![ContentBlock::Text(TextBlock::new("sunny"))],
        );
        let msg = Msg::builder()
            .role(MsgRole::Tool)
            .content(vec![ContentBlock::ToolResult(tr)])
            .build();
        let msgs = to_request_messages(vec![msg], false).messages;
        match &msgs[0] {
            ChatCompletionRequestMessage::Tool(t) => assert_eq!(t.tool_call_id, "call_9"),
            _ => panic!("expected tool message"),
        }
    }

    #[test]
    fn empty_tools_yield_none() {
        assert!(to_request_tools(vec![]).is_none());
    }

    #[test]
    fn tool_choice_specific_maps_to_function() {
        let choice = ToolChoice::specific("get_weather");
        let mapped = to_tool_choice(&choice).unwrap();
        assert!(matches!(
            mapped,
            ChatCompletionToolChoiceOption::Function(_)
        ));
    }

    #[test]
    fn reasoning_effort_mapping() {
        assert_eq!(to_reasoning_effort("high"), Some(ReasoningEffort::High));
        assert_eq!(to_reasoning_effort("x-high"), Some(ReasoningEffort::Xhigh));
        assert_eq!(to_reasoning_effort("nope"), None);
    }

    #[test]
    fn parse_arguments_object_and_garbage() {
        assert!(parse_arguments(r#"{"a":1}"#).contains_key("a"));
        assert!(parse_arguments("not json").is_empty());
    }

    #[test]
    fn stream_incrementer_emits_text_and_thinking_deltas() {
        let mut inc = StreamIncrementer::default();
        // Chunk 1: only reasoning_content (thinking delta).
        let chunk1 = OpenAiStreamChunk {
            id: "r1".into(),
            choices: vec![OpenAiStreamChoice {
                delta: OpenAiStreamDelta {
                    content: None,
                    reasoning_content: Some("let me think".into()),
                    reasoning: None,
                    reasoning_details: None,
                    refusal: None,
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let r1 = inc.accumulate(&chunk1).expect("thinking delta emitted");
        assert!(matches!(
            &r1.content[0],
            ContentBlock::Thinking(t) if t.thinking == "let me think"
        ));

        // Chunk 2: only content (text delta).
        let chunk2 = OpenAiStreamChunk {
            id: "r1".into(),
            choices: vec![OpenAiStreamChoice {
                delta: OpenAiStreamDelta {
                    content: Some("hello".into()),
                    reasoning_content: None,
                    reasoning: None,
                    reasoning_details: None,
                    refusal: None,
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let r2 = inc.accumulate(&chunk2).expect("text delta emitted");
        assert!(matches!(
            &r2.content[0],
            ContentBlock::Text(t) if t.text == "hello"
        ));

        // No tool calls accumulated -> into_final yields nothing.
        assert!(inc.into_final().is_none());
    }

    #[test]
    fn stream_incrementer_flushes_tool_calls_and_usage_at_end() {
        let mut inc = StreamIncrementer::default();
        // Tool-call fragment arrives split across two chunks.
        let frag1 = OpenAiStreamChunk {
            id: "r2".into(),
            choices: vec![OpenAiStreamChoice {
                delta: OpenAiStreamDelta {
                    content: None,
                    reasoning_content: None,
                    reasoning: None,
                    reasoning_details: None,
                    refusal: None,
                    tool_calls: Some(vec![OpenAiToolCallChunk {
                        index: 0,
                        id: Some("call_1".into()),
                        function: Some(OpenAiFunctionChunk {
                            name: Some("get_weather".into()),
                            arguments: Some(r#"{"city":"Bei"#.into()),
                            thought_signature: None,
                        }),
                    }]),
                },
                finish_reason: None,
            }],
            usage: None,
        };
        assert!(inc.accumulate(&frag1).is_none());
        let frag2 = OpenAiStreamChunk {
            id: "r2".into(),
            choices: vec![OpenAiStreamChoice {
                delta: OpenAiStreamDelta {
                    content: None,
                    reasoning_content: None,
                    reasoning: None,
                    reasoning_details: None,
                    refusal: None,
                    tool_calls: Some(vec![OpenAiToolCallChunk {
                        index: 0,
                        id: None,
                        function: Some(OpenAiFunctionChunk {
                            name: None,
                            arguments: Some(r#"jing"}"#.into()),
                            thought_signature: None,
                        }),
                    }]),
                },
                finish_reason: Some(FinishReason::ToolCalls),
            }],
            usage: None,
        };
        assert!(inc.accumulate(&frag2).is_none());

        let final_resp = inc.into_final().expect("tool call flushed");
        let tool = match &final_resp.content[0] {
            ContentBlock::ToolUse(t) => t,
            other => panic!("expected ToolUse, got {other:?}"),
        };
        assert_eq!(tool.id, "call_1");
        assert_eq!(tool.name, "get_weather");
        assert_eq!(
            tool.input.get("city").and_then(|v| v.as_str()),
            Some("Beijing")
        );
        assert_eq!(final_resp.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn reasoning_details_and_thought_signature_round_trip() {
        // MODEL-05: OpenRouter / vLLM / Gemini-via-OpenAI send `reasoning_details`
        // and a per-call `thought_signature`; both must survive into the core
        // blocks and be replayed on the next request.
        let chunk: OpenAiStreamChunk = serde_json::from_value(serde_json::json!({
            "id": "x",
            "choices": [{
                "delta": {
                    "reasoning": "step",
                    "reasoning_details": [{"type": "reasoning.text", "text": "step"}],
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"city\":\"Beijing\"}",
                            "thought_signature": "sig-1"
                        }
                    }]
                }
            }]
        }))
        .unwrap();
        let mut inc = StreamIncrementer::default();
        let delta = inc.accumulate(&chunk).expect("reasoning delta");
        let thinking = match &delta.content[0] {
            ContentBlock::Thinking(thinking) => thinking.clone(),
            other => panic!("expected Thinking, got {other:?}"),
        };
        assert_eq!(thinking.thinking, "step");
        assert_eq!(
            thinking.metadata.as_ref().unwrap()[ThinkingBlock::METADATA_REASONING_DETAILS],
            serde_json::json!([{"type": "reasoning.text", "text": "step"}])
        );
        let final_resp = inc.into_final().expect("tool call flushed");
        let tool = match &final_resp.content[0] {
            ContentBlock::ToolUse(tool) => tool.clone(),
            other => panic!("expected ToolUse, got {other:?}"),
        };
        assert_eq!(
            tool.metadata[ToolUseBlock::METADATA_THOUGHT_SIGNATURE],
            serde_json::json!("sig-1")
        );

        let ctx = ModelCreationContext::new("openai", "gateway-chat").api_key("sk-test");
        let model = OpenAiChatModel::from_context(&ctx).expect("model");
        let assistant = Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![
                ContentBlock::Thinking(thinking),
                ContentBlock::ToolUse(tool),
            ])
            .build();
        let (body, _) = model.prepare_body(
            vec![Msg::user("hi"), assistant],
            vec![],
            GenerateOptions::default(),
        );
        assert_eq!(
            body["messages"][1]["reasoning_details"][0]["type"],
            serde_json::json!("reasoning.text")
        );
        assert_eq!(
            body["messages"][1]["tool_calls"][0]["function"]["thought_signature"],
            serde_json::json!("sig-1")
        );
    }

    fn json_schema_response_format(strict: Option<bool>) -> ResponseFormat {
        let mut schema = serde_json::Map::new();
        schema.insert("type".into(), serde_json::json!("object"));
        ResponseFormat {
            format_type: "json_schema".into(),
            json_schema: Some(agentscope_core::formatter::JsonSchema {
                name: Some("person".into()),
                schema,
                strict,
                description: None,
            }),
        }
    }

    #[test]
    fn strict_json_schema_default_applied_when_request_unset() {
        let rf = json_schema_response_format(None);
        let out = to_response_format(&rf, Some(true)).expect("json_schema format");
        match out {
            OaiResponseFormat::JsonSchema { json_schema } => {
                assert_eq!(json_schema.strict, Some(true));
            }
            other => panic!("expected JsonSchema, got {other:?}"),
        }
    }

    #[test]
    fn schema_level_strict_wins_over_model_default() {
        let rf = json_schema_response_format(Some(false));
        let out = to_response_format(&rf, Some(true)).expect("json_schema format");
        match out {
            OaiResponseFormat::JsonSchema { json_schema } => {
                assert_eq!(json_schema.strict, Some(false));
            }
            other => panic!("expected JsonSchema, got {other:?}"),
        }
    }

    #[test]
    fn strict_omitted_when_neither_request_nor_model_sets_it() {
        let rf = json_schema_response_format(None);
        let out = to_response_format(&rf, None).expect("json_schema format");
        match out {
            OaiResponseFormat::JsonSchema { json_schema } => {
                assert_eq!(json_schema.strict, None);
            }
            other => panic!("expected JsonSchema, got {other:?}"),
        }
    }

    #[test]
    fn json_schema_description_reaches_the_request_body() {
        // MODEL-17: Java keeps the schema description (Java `JsonSchema.description`);
        // dropping it silently loses caller intent.
        let mut rf = json_schema_response_format(None);
        rf.json_schema.as_mut().expect("schema").description = Some("a person".into());
        let model = cache_capable_model();
        let options = GenerateOptions::builder().response_format(rf).build();
        let (body, _) = model.prepare_body(vec![sample_msg()], vec![], options);
        assert_eq!(
            body["response_format"]["json_schema"]["description"],
            serde_json::json!("a person")
        );
    }

    #[test]
    fn image_detail_reaches_the_image_url_part() {
        // MODEL-17: `ImageBlock.detail` maps onto `image_url.detail`; an unset
        // detail leaves the field out so the provider default applies.
        for (detail, expected) in [
            (
                Some(agentscope_core::message::ImageDetail::Low),
                Some("low"),
            ),
            (
                Some(agentscope_core::message::ImageDetail::High),
                Some("high"),
            ),
            (
                Some(agentscope_core::message::ImageDetail::Auto),
                Some("auto"),
            ),
            (None, None),
        ] {
            let mut image = ImageBlock::new(Source::Url(
                agentscope_core::message::URLSource::new("https://example.test/cat.png"),
            ));
            image.detail = detail;
            let (req, _) = build_request(
                "gpt-4o".into(),
                vec![Msg::user_blocks(vec![ContentBlock::Image(image)])],
                vec![],
                GenerateOptions::default(),
                false,
                None,
                None,
            );
            let body = serde_json::to_value(&req).unwrap();
            let part = &body["messages"][0]["content"][0]["image_url"];
            assert_eq!(part["url"], "https://example.test/cat.png");
            match expected {
                Some(expected) => assert_eq!(part["detail"], serde_json::json!(expected)),
                None => assert!(
                    part.get("detail").is_none(),
                    "detail must be omitted when unset: {part:?}"
                ),
            }
        }
    }

    #[test]
    fn non_stream_request_sets_stream_false_and_omits_stream_options() {
        // TRAINING §4.3 #6: a backend that requires a plain JSON completion
        // (e.g. Trinity-RFT, Java `TrinityModelAdapter.stream(false)`) must not
        // receive `stream: true` nor the streaming-only `stream_options`.
        let options = GenerateOptions {
            stream: Some(false),
            ..Default::default()
        };
        let (req, _) = build_request(
            "gpt-4o".into(),
            vec![sample_msg()],
            vec![],
            options,
            false,
            None,
            None,
        );
        let body = serde_json::to_value(&req).unwrap();
        assert_eq!(body["stream"], serde_json::json!(false));
        assert!(
            body.get("stream_options").is_none(),
            "stream_options must be omitted for a non-streaming request, got {body:?}"
        );
    }

    #[test]
    fn non_stream_completion_maps_text_tool_calls_usage_and_reasoning() {
        // A pure JSON completion (the shape returned when `stream=false`) must
        // map onto ChatResponse without any SSE machinery.
        let raw = serde_json::json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 1_700_000_000u64,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "checking the weather",
                    "reasoning_content": "need to call the tool",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"city\":\"Beijing\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15,
                "prompt_tokens_details": { "cached_tokens": 4 }
            }
        });
        let response: OpenAiCompletionResponse =
            serde_json::from_value(raw).expect("completion decodes");
        let mapped = completion_to_response(response);

        assert_eq!(mapped.id, "chatcmpl-1");
        assert_eq!(mapped.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(mapped.usage.input_tokens, 10);
        assert_eq!(mapped.usage.output_tokens, 5);
        assert_eq!(mapped.usage.cached_tokens, 4);
        assert!(
            matches!(&mapped.content[0], ContentBlock::Thinking(t) if t.thinking == "need to call the tool"),
            "thinking block expected, got {:?}",
            mapped.content
        );
        assert!(
            matches!(&mapped.content[1], ContentBlock::Text(t) if t.text == "checking the weather"),
            "text block expected, got {:?}",
            mapped.content
        );
        let tool = match &mapped.content[2] {
            ContentBlock::ToolUse(t) => t,
            other => panic!("expected ToolUse, got {other:?}"),
        };
        assert_eq!(tool.id, "call_1");
        assert_eq!(tool.name, "get_weather");
        assert_eq!(
            tool.input.get("city").and_then(|v| v.as_str()),
            Some("Beijing")
        );
    }

    #[test]
    fn default_options_keep_streaming_request() {
        // Regression guard: `None` (the historical default) must still produce
        // an SSE request with usage reporting, so existing callers are
        // unaffected by the new non-streaming branch.
        let (req, _) = build_request(
            "gpt-4o".into(),
            vec![sample_msg()],
            vec![],
            GenerateOptions::default(),
            false,
            None,
            None,
        );
        assert_eq!(req.stream, Some(true));
        assert_eq!(
            req.stream_options.as_ref().and_then(|o| o.include_usage),
            Some(true)
        );
    }

    fn context(model: &str) -> ModelCreationContext {
        let mut ctx = ModelCreationContext::new("openai", model);
        ctx.api_key = Some("test-key".to_string());
        ctx.base_url = Some("https://example.invalid/v1".to_string());
        ctx
    }

    #[test]
    fn provider_applies_the_model_map_before_creating_the_model() {
        let mut map = ModelMap::default();
        map.insert("fast", "gpt-4.1");
        map.insert("gpt-4.1", "gpt-4.1-2026-01-01");
        let provider = OpenAiProvider::new().with_model_map(map);
        let model = provider.create(context("fast")).unwrap();
        // Chained resolution: fast -> gpt-4.1 -> gpt-4.1-2026-01-01.
        assert_eq!(model.model_name(), "gpt-4.1-2026-01-01");
    }

    #[test]
    fn provider_reports_model_map_cycles_instead_of_looping() {
        let mut map = ModelMap::default();
        map.insert("a", "b");
        map.insert("b", "a");
        let provider = OpenAiProvider::new().with_model_map(map);
        let err = match provider.create(context("a")) {
            Ok(_) => panic!("expected a cycle error"),
            Err(error) => error,
        };
        assert!(err.to_string().contains("cycle"), "got {err}");
    }

    #[test]
    fn provider_strips_the_reasoning_suffix_and_seeds_the_thinking_knobs() {
        let provider = OpenAiProvider::new().with_reasoning_suffix(true);
        let model = provider
            .create(context("gpt-4o-thinking-4096"))
            .unwrap();
        assert_eq!(model.model_name(), "gpt-4o");

        let model = provider.create(context("gpt-4o-high")).unwrap();
        assert_eq!(model.model_name(), "gpt-4o");

        // Without the opt-in the name goes through untouched.
        let model = OpenAiProvider::new()
            .create(context("gpt-4o-high"))
            .unwrap();
        assert_eq!(model.model_name(), "gpt-4o-high");
    }

    fn cache_capable_model() -> OpenAiChatModel {
        OpenAiChatModel::new("gpt-4o", Some("sk-test".to_string()), None).expect("model")
    }

    fn audio_message_block(media_type: &str, data: &str) -> Msg {
        Msg::user_blocks(vec![ContentBlock::Audio(agentscope_core::AudioBlock::new(
            Source::Base64(Base64Source::new(media_type, data)),
        ))])
    }

    #[test]
    fn user_audio_becomes_input_audio_part() {
        // MODEL-04: `AudioBlock` used to be dropped silently. Base64 wav/mp3
        // audio maps onto the `input_audio` content part (Java
        // `OpenAIMessageConverter#convertContentBlocks`).
        let (req, _) = build_request(
            "gpt-4o".into(),
            vec![audio_message_block("audio/wav", "UklGRg==")],
            vec![],
            GenerateOptions::default(),
            false,
            None,
            None,
        );
        let body = serde_json::to_value(&req).unwrap();
        assert_eq!(body["messages"][0]["content"][0]["type"], "input_audio");
        assert_eq!(
            body["messages"][0]["content"][0]["input_audio"]["format"],
            "wav"
        );
        assert_eq!(
            body["messages"][0]["content"][0]["input_audio"]["data"],
            "UklGRg=="
        );
    }

    #[test]
    fn unsupported_audio_format_degrades_to_a_text_reference() {
        // `opus` / `flac` are not accepted by `input_audio`, so the block must
        // be visible as text instead of being dropped or sent as a bad format.
        let (req, _) = build_request(
            "gpt-4o".into(),
            vec![audio_message_block("audio/opus", "T3B1cw==")],
            vec![],
            GenerateOptions::default(),
            false,
            None,
            None,
        );
        let body = serde_json::to_value(&req).unwrap();
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert_eq!(
            body["messages"][0]["content"][0]["text"],
            "[Audio - unsupported format: audio/opus]"
        );
    }

    #[test]
    fn user_video_block_reaches_the_wire() {
        // MODEL-04: `VideoBlock` used to hit the catch-all arm.
        // `async-openai` cannot express a `video_url` part, so the video is
        // degraded to a text reference instead of being dropped.
        let msg = Msg::user_blocks(vec![
            ContentBlock::Text(TextBlock::new("watch")),
            ContentBlock::Video(agentscope_core::VideoBlock::new(Source::Url(
                agentscope_core::URLSource::new("https://example.test/clip.mp4"),
            ))),
        ]);
        let (req, _) = build_request(
            "gpt-4o".into(),
            vec![msg],
            vec![],
            GenerateOptions::default(),
            false,
            None,
            None,
        );
        let body = serde_json::to_value(&req).unwrap();
        let parts = body["messages"][0]["content"]
            .as_array()
            .expect("media blocks switch the message to the content-part array");
        assert_eq!(
            parts[1]["text"],
            "[Video URL: https://example.test/clip.mp4]",
            "video must surface as a text reference: {body:?}"
        );
    }

    #[test]
    fn per_call_strict_json_schema_sets_the_default_without_leaking() {
        // MODEL-09: a per-call `strictJsonSchema` knob is a provider option, not
        // a wire field, exactly like the creation-time one.
        let mut options = GenerateOptions {
            response_format: Some(json_schema_response_format(None)),
            ..Default::default()
        };
        options.additional_body_params.insert(
            STRICT_JSON_SCHEMA_OPTION.to_string(),
            serde_json::json!(true),
        );
        let (body, _) = cache_capable_model().prepare_body(
            vec![sample_msg()],
            vec![],
            options,
        );
        assert_eq!(
            body["response_format"]["json_schema"]["strict"],
            serde_json::json!(true),
            "per-call knob must seed the schema default: {body:?}"
        );
        assert!(
            body.get(STRICT_JSON_SCHEMA_OPTION).is_none(),
            "provider knob leaked into the wire body: {body:?}"
        );
    }

    #[test]
    fn automatic_cache_control_marks_system_and_last_message() {
        // MODEL-03: `cache_control: true` mirrors Java
        // `OpenAIBaseFormatter#applyAutomaticCacheControl` — the final message
        // and every system message receive an ephemeral marker.
        let options = GenerateOptions {
            cache_control: Some(true),
            ..Default::default()
        };
        let messages = vec![
            Msg::system("be helpful"),
            Msg::user("hello"),
            Msg::assistant("hi"),
        ];
        let (body, _) = cache_capable_model().prepare_body(messages, vec![], options);
        let wire = body["messages"].as_array().expect("messages");
        assert_eq!(
            wire[0]["content"][0]["cache_control"]["type"], "ephemeral",
            "system message must be marked: {body:?}"
        );
        assert_eq!(
            wire[2]["content"][0]["cache_control"]["type"], "ephemeral",
            "last message must be marked: {body:?}"
        );
        assert_eq!(
            wire[1]["content"],
            serde_json::json!("hello"),
            "an unmarked middle message keeps its plain string content: {body:?}"
        );
    }

    #[test]
    fn explicit_metadata_cache_control_is_honored_without_the_option() {
        // Java `OpenAIMessageConverter#applyCacheControlFromMetadata`: a message
        // that asks for caching via `_cache_control` is marked even when the
        // request-level `cache_control` option is unset.
        let marked = Msg::builder()
            .role(MsgRole::User)
            .text_content("remember this")
            .metadata_entry(agentscope_core::metadata_keys::CACHE_CONTROL, serde_json::json!(true))
            .build();
        let (body, _) = cache_capable_model().prepare_body(
            vec![Msg::system("be helpful"), marked],
            vec![],
            GenerateOptions::default(),
        );
        let wire = body["messages"].as_array().expect("messages");
        assert_eq!(
            wire[1]["content"][0]["cache_control"]["type"], "ephemeral",
            "metadata directive must mark the message: {body:?}"
        );
        assert_eq!(
            wire[0]["content"],
            serde_json::json!("be helpful"),
            "without the option the system message stays unmarked: {body:?}"
        );
    }

    #[test]
    fn cache_markers_above_the_cap_skip_automatic_marking() {
        // Above `MAX_CACHE_MARKERS` explicit markers the provider only honours
        // the last N, so Java warns and adds no automatic marker at all.
        let marked = |text: &str| {
            Msg::builder()
                .role(MsgRole::Assistant)
                .text_content(text)
                .metadata_entry(
                    agentscope_core::metadata_keys::CACHE_CONTROL,
                    serde_json::json!(true),
                )
                .build()
        };
        let mut messages: Vec<Msg> = (0..5).map(|i| marked(&format!("turn {i}"))).collect();
        messages.push(Msg::user("fresh question"));
        let options = GenerateOptions {
            cache_control: Some(true),
            ..Default::default()
        };
        let (body, _) = cache_capable_model().prepare_body(messages, vec![], options);
        let wire = body["messages"].as_array().expect("messages");
        assert_eq!(
            wire.last().expect("last")["content"],
            serde_json::json!("fresh question"),
            "automatic marking must be skipped above the cap: {body:?}"
        );
    }
}
