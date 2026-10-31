//! Wire-contract guard between the RAG embedding providers and `agentscope-extensions-model`
//! (tracker RAG-10).
//!
//! The 2026-10-31 裁定 keeps `EmbeddingModel` **in this crate** (A: 维持 RAG 自持) instead of
//! moving it to the MODEL extension. Both crates therefore build the same provider requests
//! independently, and the only thing that can drift silently is the **wire**: the JSON sent to
//! `POST /api/embed` / `POST /embeddings` and the JSON read back.
//!
//! Each test pins one exact request body / response envelope and cross-checks it against the MODEL
//! extension's own DTOs (`agentscope_extensions_model::ollama::dto`), so a change on either side
//! fails here instead of at runtime. Everything is offline: the HTTP transport is an in-memory
//! double.

use std::sync::{Arc, Mutex};

use agentscope_core::message::{ContentBlock, TextBlock};
use agentscope_extensions_model::ollama::dto::{OllamaEmbeddingRequest, OllamaEmbeddingResponse};
use agentscope_extensions_rag::embedding::{
    EmbeddingExecutionConfig, EmbeddingModel, OllamaTextEmbedding, OpenAiTextEmbedding,
};
use agentscope_extensions_rag::transport::{HttpRequest, HttpResponse, HttpTransport};
use agentscope_extensions_rag::{RagError, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

/// The exact Ollama embedding request both crates must send.
///
/// Field order is part of the pinned contract: `build_body` emits `model` before `input`, and
/// `serde_json::to_string` on the parsed body must reproduce it byte for byte.
const OLLAMA_REQUEST: &str = r#"{"model":"nomic-embed-text","input":["hello"]}"#;

/// A realistic Ollama `/api/embed` reply (f32 vectors + `prompt_eval_count`).
const OLLAMA_RESPONSE: &str =
    r#"{"model":"nomic-embed-text","embeddings":[[0.5,0.25,-0.5]],"prompt_eval_count":7}"#;

/// The exact OpenAI embeddings request the RAG provider must send.
const OPENAI_REQUEST: &str = r#"{"dimensions":3,"encoding_format":"float","input":["hello"],"model":"text-embedding-3-small"}"#;

/// A realistic OpenAI embeddings reply.
const OPENAI_RESPONSE: &str = r#"{"object":"list","data":[{"object":"embedding","index":0,"embedding":[0.5,0.25,-0.5]}],"model":"text-embedding-3-small","usage":{"prompt_tokens":3,"total_tokens":3}}"#;

/// Offline transport double: records requests and replays scripted responses (LIFO).
#[derive(Default)]
struct ScriptedTransport {
    responses: Mutex<Vec<HttpResponse>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl ScriptedTransport {
    fn with_responses(responses: Vec<HttpResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl HttpTransport for ScriptedTransport {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
        self.requests.lock().unwrap().push(request);
        self.responses
            .lock()
            .unwrap()
            .pop()
            .ok_or_else(|| RagError::Http("no scripted response left".to_string()))
    }
}

fn ollama_model(transport: Arc<ScriptedTransport>) -> OllamaTextEmbedding {
    OllamaTextEmbedding::builder()
        .base_url("http://localhost:11434")
        .model_name("nomic-embed-text")
        .dimensions(3)
        .execution_config(EmbeddingExecutionConfig::disabled())
        .build()
        .unwrap()
        .with_transport(transport)
}

fn openai_model(transport: Arc<ScriptedTransport>) -> OpenAiTextEmbedding {
    OpenAiTextEmbedding::builder()
        .api_key("test-key")
        .model_name("text-embedding-3-small")
        .dimensions(3)
        .base_url("https://api.openai.com/v1")
        .execution_config(EmbeddingExecutionConfig::disabled())
        .build()
        .unwrap()
        .with_transport(transport)
}

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text(TextBlock::new(text))
}

#[test]
fn the_ollama_embedding_request_is_the_contract_the_model_extension_reads() {
    let model = ollama_model(ScriptedTransport::with_responses(Vec::new()));
    let body = model.build_body("hello");
    assert_eq!(
        body,
        serde_json::from_str::<Value>(OLLAMA_REQUEST).unwrap(),
        "the RAG request body drifted from the pinned contract"
    );
    assert_eq!(
        serde_json::to_string(&body).unwrap(),
        OLLAMA_REQUEST,
        "the serialized body must stay byte-identical"
    );

    // The MODEL extension must be able to consume exactly this body...
    let request: OllamaEmbeddingRequest = serde_json::from_str(OLLAMA_REQUEST).unwrap();
    assert_eq!(request.model, "nomic-embed-text");
    assert_eq!(request.input, vec!["hello".to_string()]);
    assert_eq!(request.keep_alive, None);
    assert_eq!(request.truncate, None);
    assert_eq!(request.options, None);
    // ...and reproduce it unchanged (no extra keys, no dropped fields).
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        serde_json::from_str::<Value>(OLLAMA_REQUEST).unwrap()
    );
}

#[test]
fn the_ollama_embedding_request_targets_api_embed_on_the_shared_endpoint() {
    let model = ollama_model(ScriptedTransport::with_responses(Vec::new()));
    let request = model.build_embedding_request(&text("hello")).unwrap();
    assert_eq!(request.url, "http://localhost:11434/api/embed");
    assert!(request.header("Authorization").is_none(), "Ollama is unauthenticated");
    assert_eq!(
        serde_json::from_str::<Value>(request.body.as_deref().unwrap()).unwrap(),
        serde_json::from_str::<Value>(OLLAMA_REQUEST).unwrap()
    );
}

#[tokio::test]
async fn both_crates_read_the_same_ollama_embedding_response() {
    let transport = ScriptedTransport::with_responses(vec![HttpResponse::new(200, OLLAMA_RESPONSE)]);
    let model = ollama_model(transport);
    let response = model.embed_with_usage(&text("hello")).await.unwrap();
    assert_eq!(response.embedding, vec![0.5, 0.25, -0.5]);
    assert_eq!(response.usage.input_tokens, 7);
    assert_eq!(response.usage.total_tokens, 7);
    assert_eq!(response.model, "nomic-embed-text");

    // The MODEL extension's DTO reads the same envelope (f32 vectors, same counter).
    let parsed: OllamaEmbeddingResponse = serde_json::from_str(OLLAMA_RESPONSE).unwrap();
    assert_eq!(parsed.embeddings, vec![vec![0.5f32, 0.25, -0.5]]);
    assert_eq!(parsed.prompt_eval_count, Some(7));
    assert_eq!(parsed.model.as_deref(), Some("nomic-embed-text"));
}

#[test]
fn the_openai_embedding_request_is_the_pinned_contract() {
    let model = openai_model(ScriptedTransport::with_responses(Vec::new()));
    let request = model.build_embedding_request(&text("hello")).unwrap();
    assert_eq!(request.url, "https://api.openai.com/v1/embeddings");
    assert_eq!(request.header("Authorization"), Some("Bearer test-key"));
    let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
    assert_eq!(body, serde_json::from_str::<Value>(OPENAI_REQUEST).unwrap());
    assert_eq!(
        body.as_object().unwrap().len(),
        4,
        "model/input/dimensions/encoding_format only"
    );
    assert_eq!(body["encoding_format"], json!("float"));
}

#[tokio::test]
async fn the_openai_embedding_response_envelope_is_the_pinned_contract() {
    let transport = ScriptedTransport::with_responses(vec![HttpResponse::new(200, OPENAI_RESPONSE)]);
    let model = openai_model(transport);
    let response = model.embed_with_usage(&text("hello")).await.unwrap();

    assert_eq!(response.embedding, vec![0.5, 0.25, -0.5]);
    assert_eq!(response.usage.input_tokens, 3);
    assert_eq!(response.usage.total_tokens, 3);
    assert_eq!(response.model, "text-embedding-3-small");
}

#[tokio::test]
async fn a_provider_reply_of_the_other_shape_is_rejected_rather_than_misread() {
    // An OpenAI envelope must not be silently accepted by the Ollama client (the drift that the
    // shared contract exists to prevent).
    let transport = ScriptedTransport::with_responses(vec![HttpResponse::new(200, OPENAI_RESPONSE)]);
    let model = ollama_model(transport);
    let error = model
        .embed_with_usage(&text("hello"))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("embedding"),
        "unexpected error: {error}"
    );
}
