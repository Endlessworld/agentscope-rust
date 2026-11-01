//! Regression coverage for Java 51a74e2ef (`fix(core): prepare complete structured-output
//! requests`).
//!
//! The structured-output fallback builds its request by hand: the transient reminder plus the
//! synthetic `generate_response` tool schema. Those must reach the request-preparation seam
//! (harness `ContextMiddleware`, the Rust counterpart of Java `ModelRequestPreparer`) so the
//! prepared request — the one the provider actually receives — contains them.

use std::sync::{Arc, Mutex};

use agentscope_core::agent::CallableAgent;
use agentscope_core::formatter::JsonSchema;
use agentscope_core::message::{ContentBlock as CoreBlock, JsonMap, Msg, ToolUseBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_harness::agent::HarnessAgent;

/// Returns a `generate_response` tool call and records every request it receives.
struct StructuredModel {
    seen: Arc<Mutex<Vec<(Vec<String>, Vec<String>)>>>,
}

#[async_trait::async_trait]
impl Model for StructuredModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        self.seen.lock().unwrap().push((
            messages.iter().map(|m| m.text_content()).collect(),
            tools.iter().map(|t| t.name.clone()).collect(),
        ));
        let mut args = JsonMap::new();
        args.insert(
            "response".to_string(),
            serde_json::json!({"name": "bob", "age": 42}),
        );
        Box::pin(futures::stream::once(async move {
            Ok(ChatResponse::builder()
                .content(vec![CoreBlock::ToolUse(ToolUseBlock::new(
                    "g1",
                    "generate_response",
                    args,
                ))])
                .build())
        }))
    }

    fn model_name(&self) -> &str {
        "structured-recording"
    }

    fn context_window_size(&self) -> usize {
        128_000
    }
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("agentscope_so_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

fn person_schema() -> JsonSchema {
    let mut schema = serde_json::Map::new();
    schema.insert("type".into(), serde_json::json!("object"));
    schema.insert(
        "properties".into(),
        serde_json::json!({"name": {"type": "string"}, "age": {"type": "integer"}}),
    );
    schema.insert("required".into(), serde_json::json!(["name", "age"]));
    JsonSchema::new("person", schema)
}

#[tokio::test]
async fn structured_output_reminder_and_schema_reach_the_provider() {
    let dir = temp_dir("prepared");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let agent = HarnessAgent::builder()
        .name("so")
        .model(Arc::new(StructuredModel {
            seen: Arc::clone(&seen),
        }))
        .cwd(dir.clone())
        .workspace(dir.join("ws"))
        .instruction("change-policy", "Preserve public API behavior")
        .build();

    let reply = agent
        .call_with_schema(vec![Msg::user("give json")], person_schema())
        .await
        .expect("structured call");
    assert!(reply.has_structured_data(), "structured data missing");

    let batches = seen.lock().unwrap().clone();
    assert!(!batches.is_empty(), "the model was never called");
    let (messages, tools) = &batches[0];
    let rendered = messages.join("\n");
    // The reminder is request-local, but it must be part of the request the provider sees.
    assert!(
        rendered.contains("generate_response"),
        "reminder missing from the prepared request: {rendered}"
    );
    assert!(
        tools.iter().any(|name| name == "generate_response"),
        "generate_response schema missing from the prepared request: {tools:?}"
    );
    // Context preparation still runs: the registered material is compiled into the request.
    assert!(
        rendered.contains("Preserve public API behavior"),
        "context preparation did not run: {rendered}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
