//! Protocol-driven production assembly for model hosts.
//!
//! ## Why this module exists (`MODEL__GAP_TRACKER.md` MODEL-01)
//!
//! A host configured with an endpoint (`base_url` + [`ApiProtocol`]) dispatches
//! on the *wire protocol*, not on the vendor. Until this module existed the only
//! such dispatch point (ACP's `build_model_for_protocol`) hard-wired the two
//! SDK-backed adapters, even though this crate ships richer implementations for
//! the same protocols:
//!
//! | protocol | SDK adapter (host default) | capability-rich adapter |
//! |---|---|---|
//! | `Messages` | [`crate::anthropic_async`] (`async-anthropic` 0.6) | [`crate::anthropic`] — `cache_control`, extended thinking, server tools, deferred tool search |
//! | `Responses` | [`crate::responses`] (`async-openai` 0.41) | [`crate::openai_official`] — server tools, client `tool_search`, `prompt_cache`, `x-should-retry` |
//!
//! The rich adapters had **no construction point outside their unit tests**, so
//! those capabilities were implemented but unreachable in production. This
//! module is that construction point: [`ProtocolImplementations::full_capability`]
//! plus [`build_model_for_protocol`] (or [`build_model_for_protocol_with_transport`])
//! let a host opt in without touching the crate that owns the dispatch.
//!
//! ## Host wiring
//!
//! The ACP crate owns the dispatch site (`acp_harness_agent.rs::
//! build_model_for_protocol`). To activate the capability-rich adapters it only
//! has to delegate to [`build_model_for_protocol`] instead of matching on
//! [`ApiProtocol`] itself (and pass the selection from its own config / CLI
//! flag). [`ProtocolImplementations::new`] preserves today's host behaviour, so
//! the switch is opt-in.
//!
//! ## Registry alternative
//!
//! Hosts that resolve models through [`ModelRegistry`] ids instead of a
//! protocol can use [`full_registry`], which registers every provider in this
//! crate. The two Anthropic implementations share the Java provider id
//! `anthropic` (MODEL-X01), so the registry holds exactly one of them — the one
//! selected by [`ProtocolImplementations::messages`].

use std::sync::Arc;

use agentscope_core::model::provider::{ApiProtocol, ModelCreationContext, ModelRegistry};
use agentscope_core::model::Model;
use agentscope_core::Result;

use crate::anthropic::model::AnthropicChatModel;
use crate::anthropic::AnthropicModelProvider;
use crate::anthropic_async::{AnthropicMessagesModel, AnthropicMessagesProvider};
use crate::common::{ReqwestTransport, SharedTransport};
use crate::dashscope::DashScopeModelProvider;
use crate::gemini::GeminiModelProvider;
use crate::ollama::OllamaModelProvider;
use crate::openai::{OpenAiChatModel, OpenAiProvider};
use crate::openai_official::{OpenAIOfficialModelProvider, OpenAIResponsesChatModel};
use crate::responses::{OpenAiResponsesModel, OpenAiResponsesProvider};

/// Implementation that serves the Anthropic `Messages` wire protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessagesImplementation {
    /// `async-anthropic` SDK adapter ([`crate::anthropic_async`]): the legacy
    /// host default. No `cache_control`, extended thinking, server tools or
    /// deferred tool search.
    #[default]
    AsyncSdk,
    /// Transport-seam Messages wire adapter ([`crate::anthropic`]): full
    /// `cache_control` / thinking / server tool / tool-search support.
    Wire,
}

/// Implementation that serves the OpenAI `Responses` wire protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResponsesImplementation {
    /// `async-openai` SDK adapter ([`crate::responses`]): the legacy host
    /// default. No server tools, `tool_search`, `prompt_cache` or
    /// `x-should-retry` handling.
    #[default]
    AsyncSdk,
    /// OpenAI-official Responses wire adapter ([`crate::openai_official`]).
    OpenAiOfficial,
}

/// Concrete implementation chosen for one wire protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelImplementation {
    /// `POST {base_url}/chat/completions` via `async-openai`.
    ChatCompletions,
    /// `POST {base_url}/responses` via `async-openai`.
    ResponsesSdk,
    /// `POST {base_url}/responses` via the OpenAI-official wire adapter.
    ResponsesOfficial,
    /// `POST {base_url}/messages` via `async-anthropic`.
    MessagesSdk,
    /// `POST {base_url}/messages` via the Anthropic wire adapter.
    MessagesWire,
}

/// Per-protocol implementation selection.
///
/// [`Default`] reproduces the historical host wiring (both SDK adapters), so
/// callers opt in explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProtocolImplementations {
    /// Implementation serving [`ApiProtocol::Messages`].
    pub messages: MessagesImplementation,
    /// Implementation serving [`ApiProtocol::Responses`].
    pub responses: ResponsesImplementation,
}

impl ProtocolImplementations {
    /// Host parity: `async-anthropic` for Messages, `async-openai` for Responses.
    pub const fn new() -> Self {
        Self {
            messages: MessagesImplementation::AsyncSdk,
            responses: ResponsesImplementation::AsyncSdk,
        }
    }

    /// Every capability this crate implements: the Anthropic wire adapter for
    /// Messages and the OpenAI-official adapter for Responses (Chat Completions
    /// is unchanged).
    pub const fn full_capability() -> Self {
        Self {
            messages: MessagesImplementation::Wire,
            responses: ResponsesImplementation::OpenAiOfficial,
        }
    }

    /// Overrides the `Messages` implementation.
    pub const fn with_messages(mut self, implementation: MessagesImplementation) -> Self {
        self.messages = implementation;
        self
    }

    /// Overrides the `Responses` implementation.
    pub const fn with_responses(mut self, implementation: ResponsesImplementation) -> Self {
        self.responses = implementation;
        self
    }
}

/// The concrete implementation that serves `protocol` under `impls`.
pub const fn implementation_for(
    protocol: ApiProtocol,
    impls: ProtocolImplementations,
) -> ModelImplementation {
    match protocol {
        ApiProtocol::ChatCompletions => ModelImplementation::ChatCompletions,
        ApiProtocol::Responses => match impls.responses {
            ResponsesImplementation::AsyncSdk => ModelImplementation::ResponsesSdk,
            ResponsesImplementation::OpenAiOfficial => ModelImplementation::ResponsesOfficial,
        },
        ApiProtocol::Messages => match impls.messages {
            MessagesImplementation::AsyncSdk => ModelImplementation::MessagesSdk,
            MessagesImplementation::Wire => ModelImplementation::MessagesWire,
        },
    }
}

/// Builds the concrete model for `ctx.protocol` with the default HTTP transport.
///
/// This is the production entry point for protocol-driven hosts (see the module
/// docs). [`ProtocolImplementations::new`] keeps the historical behaviour;
/// [`ProtocolImplementations::full_capability`] selects the capability-rich
/// adapters.
pub fn build_model_for_protocol(
    ctx: &ModelCreationContext,
    impls: ProtocolImplementations,
) -> Result<Arc<dyn Model>> {
    build_model_for_protocol_with_transport(ctx, impls, Arc::new(ReqwestTransport::new()))
}

/// Same as [`build_model_for_protocol`] but with an injected transport.
///
/// Hosts that already own a transport (custom proxies, tracing, tests) pass it
/// here. The `ChatCompletions` branch ignores it: `async-openai` builds its own
/// client internally.
pub fn build_model_for_protocol_with_transport(
    ctx: &ModelCreationContext,
    impls: ProtocolImplementations,
    transport: SharedTransport,
) -> Result<Arc<dyn Model>> {
    let model: Arc<dyn Model> = match implementation_for(ctx.protocol, impls) {
        ModelImplementation::ChatCompletions => Arc::new(OpenAiChatModel::from_context(ctx)?),
        ModelImplementation::ResponsesSdk => Arc::new(OpenAiResponsesModel::from_context(ctx)?),
        ModelImplementation::ResponsesOfficial => {
            Arc::new(build_openai_official_model(ctx, transport)?)
        }
        ModelImplementation::MessagesSdk => Arc::new(AnthropicMessagesModel::from_context(ctx)?),
        ModelImplementation::MessagesWire => Arc::new(build_messages_wire_model(ctx, transport)?),
    };
    Ok(model)
}

/// Builds the Anthropic transport-seam Messages model for `ctx`.
///
/// Unlike the `anthropic` provider's claim check this accepts any model id: the
/// wire protocol — not the vendor — selected the implementation, and
/// Anthropic-compatible gateways usually expose non-`claude*` ids.
pub fn build_messages_wire_model(
    ctx: &ModelCreationContext,
    transport: SharedTransport,
) -> Result<AnthropicChatModel> {
    AnthropicModelProvider::new().create_with_transport(ctx.clone(), transport)
}

/// Builds the OpenAI-official Responses model for `ctx`.
///
/// Provider knobs (`strictTools`, `strictJsonSchema`, `contextWindowSize`,
/// `multiAgentFormatter`, `serverTools`, `proxy`) are consumed here, exactly as
/// in the provider, and never reach the wire.
pub fn build_openai_official_model(
    ctx: &ModelCreationContext,
    transport: SharedTransport,
) -> Result<OpenAIResponsesChatModel> {
    OpenAIOfficialModelProvider::new().create_with_transport(ctx.clone(), transport)
}

/// A registry with **every** provider of this crate registered.
///
/// Registration order matters for bare (non `provider:model`) ids:
/// `OpenAiProvider` claims any id, so it is registered last. The Anthropic slot
/// is filled by `impls.messages` because the two implementations share the Java
/// provider id `anthropic` (MODEL-X01); the two Responses implementations keep
/// distinct ids (`openai-official` / `openai-responses`) and are both present.
pub fn full_registry(impls: ProtocolImplementations) -> ModelRegistry {
    let mut registry = ModelRegistry::new();
    match impls.messages {
        MessagesImplementation::Wire => registry.register(Arc::new(AnthropicModelProvider::new())),
        MessagesImplementation::AsyncSdk => registry.register(Arc::new(AnthropicMessagesProvider)),
    }
    registry.register(Arc::new(OpenAIOfficialModelProvider::new()));
    registry.register(Arc::new(OpenAiResponsesProvider));
    registry.register(Arc::new(GeminiModelProvider::new()));
    registry.register(Arc::new(DashScopeModelProvider::new()));
    registry.register(Arc::new(OllamaModelProvider::new()));
    // `OpenAiProvider::supports` claims every id: keep it last.
    registry.register(Arc::new(OpenAiProvider::new()));
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::model::GenerateOptions;
    use agentscope_core::{ModelProvider, Msg};
    use serde_json::json;

    use crate::common::MemoryTransport;

    fn transport() -> SharedTransport {
        Arc::new(MemoryTransport::default())
    }

    #[test]
    fn protocol_dispatch_defaults_to_the_host_parity_implementations() {
        let parity = ProtocolImplementations::new();
        assert_eq!(
            implementation_for(ApiProtocol::ChatCompletions, parity),
            ModelImplementation::ChatCompletions
        );
        assert_eq!(
            implementation_for(ApiProtocol::Responses, parity),
            ModelImplementation::ResponsesSdk
        );
        assert_eq!(
            implementation_for(ApiProtocol::Messages, parity),
            ModelImplementation::MessagesSdk
        );

        let full = ProtocolImplementations::full_capability();
        assert_eq!(
            implementation_for(ApiProtocol::Responses, full),
            ModelImplementation::ResponsesOfficial
        );
        assert_eq!(
            implementation_for(ApiProtocol::Messages, full),
            ModelImplementation::MessagesWire
        );
    }

    #[test]
    fn messages_wire_implementation_emits_cache_control_and_keeps_knobs_off_the_wire() {
        // The capabilities that only the transport-seam adapter has must be
        // observable on the request body of a protocol-dispatched model.
        let mut options = GenerateOptions::default();
        options.cache_control = Some(true);
        options
            .additional_body_params
            .insert("cacheTtl".to_string(), json!("1h"));
        options
            .additional_body_params
            .insert("contextWindowSize".to_string(), json!(180_000));
        options
            .additional_body_params
            .insert("authToken".to_string(), json!("gateway-token"));
        let ctx = ModelCreationContext::new("openai", "gateway-chat")
            .api_key("sk-anthropic")
            .base_url("https://gateway.test/v1")
            .protocol(ApiProtocol::Messages)
            .options(options);

        let model = build_messages_wire_model(&ctx, transport()).expect("wire model");
        assert_eq!(model.model_name(), "gateway-chat");
        assert_eq!(model.context_window_size(), 180_000);

        let spec = model
            .build_http_request(
                vec![Msg::system("be brief"), Msg::user("hi")],
                Vec::new(),
                &GenerateOptions::default(),
                false,
            )
            .expect("request");
        assert!(spec.url.ends_with("/messages"), "url: {}", spec.url);
        assert_eq!(spec.body["system"][0]["cache_control"]["ttl"], json!("1h"));
        assert_eq!(
            spec.body["messages"][0]["content"][0]["cache_control"]["type"],
            json!("ephemeral")
        );
        let raw = spec.body.to_string();
        for knob in ["cacheTtl", "contextWindowSize", "authToken"] {
            assert!(
                !raw.contains(knob),
                "provider knob '{knob}' leaked onto the wire: {raw}"
            );
        }
    }

    #[test]
    fn responses_official_implementation_declares_server_tools_on_the_wire() {
        let mut options = GenerateOptions::default();
        options
            .additional_body_params
            .insert("serverTools".to_string(), json!([{ "type": "web_search" }]));
        options
            .additional_body_params
            .insert("strictJsonSchema".to_string(), json!(true));
        let ctx = ModelCreationContext::new("openai", "gateway-chat")
            .api_key("sk-openai")
            .base_url("https://gateway.test/v1")
            .protocol(ApiProtocol::Responses)
            .options(options);

        let model = build_openai_official_model(&ctx, transport()).expect("official model");
        assert_eq!(model.model_name(), "gateway-chat");

        let spec = model
            .build_http_request(
                vec![Msg::user("hi")],
                Vec::new(),
                &GenerateOptions::default(),
                false,
            )
            .expect("request");
        assert!(spec.url.ends_with("/responses"), "url: {}", spec.url);
        assert_eq!(
            spec.body["tools"].as_array().map(Vec::len),
            Some(1),
            "only the declared server tool is sent: {:?}",
            spec.body
        );
        assert_eq!(spec.body["tools"][0]["type"], json!("web_search"));
        let raw = spec.body.to_string();
        for knob in ["serverTools", "strictJsonSchema"] {
            assert!(
                !raw.contains(knob),
                "provider knob '{knob}' leaked onto the wire: {raw}"
            );
        }
    }

    #[test]
    fn protocol_dispatch_accepts_gateway_model_ids_the_providers_reject() {
        // Anthropic-/OpenAI-compatible gateways expose arbitrary model ids; the
        // protocol already picked the implementation, so the vendor claim must
        // not reject them.
        let messages = ModelCreationContext::new("openai", "gateway-chat")
            .api_key("sk")
            .protocol(ApiProtocol::Messages);
        assert!(
            AnthropicModelProvider::new()
                .create(messages.clone())
                .is_err(),
            "bare gateway ids are not claimed by the anthropic provider"
        );
        assert!(
            build_model_for_protocol(&messages, ProtocolImplementations::full_capability()).is_ok()
        );

        let responses = ModelCreationContext::new("openai", "gateway-chat")
            .api_key("sk")
            .protocol(ApiProtocol::Responses);
        assert!(OpenAIOfficialModelProvider::new()
            .create(responses.clone())
            .is_err());
        assert!(
            build_model_for_protocol(&responses, ProtocolImplementations::full_capability())
                .is_ok()
        );
    }

    #[test]
    fn full_registry_registers_every_provider() {
        let registry = full_registry(ProtocolImplementations::full_capability());
        assert_eq!(registry.len(), 7);
        for id in [
            "anthropic:claude-sonnet-4-5",
            "openai-official:gpt-5",
            "openai-responses:gpt-5",
            "gemini:gemini-2.0-flash",
            "dashscope:qwen-max",
            "ollama:llama3",
            "openai:gpt-4o",
        ] {
            assert!(registry.can_resolve(id), "registry cannot resolve {id}");
        }
    }
}
