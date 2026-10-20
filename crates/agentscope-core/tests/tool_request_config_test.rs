//! End-to-end tests for per-call tool views (`ToolRequestConfig`).
//!
//! The point of the seam is that a call can present a different tool surface without
//! mutating the shared `Toolkit` — so concurrent calls cannot see each other's tools.
//! These tests drive a real agent run, because a config that is accepted but never read
//! would be the same dead-code failure as `wrap_acting`.
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use agentscope_core::message::{ContentBlock, JsonMap, TextBlock, ToolUseBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::tool::{
    SchemaOnlyTool, ToolMergeMode, ToolRequestConfig, TOOL_REQUEST_CONFIG_CONTEXT_KEY,
};
use agentscope_core::{
    Error, FunctionTool, Msg, ReActAgent, RuntimeContext, StreamOptions, Tool, Toolkit,
};

/// A model that answers with a fixed sequence of responses and records the tool names it
/// was shown on each call.
struct RecordingModel {
    responses: std::sync::Mutex<Vec<ChatResponse>>,
    seen_tools: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
}

impl RecordingModel {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            // Popped from the back, so reverse to keep the declared order.
            responses: std::sync::Mutex::new(responses.into_iter().rev().collect()),
            seen_tools: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
}

#[async_trait::async_trait]
impl Model for RecordingModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'_, agentscope_core::Result<ChatResponse>> {
        self.seen_tools
            .lock()
            .unwrap()
            .push(tools.iter().map(|t| t.name.clone()).collect());
        let next = self.responses.lock().unwrap().pop();
        Box::pin(futures::stream::once(async move {
            next.ok_or_else(|| Error::Model("no scripted response left".into()))
        }))
    }

    fn model_name(&self) -> &str {
        "recording"
    }
}

fn text_reply(text: &str) -> ChatResponse {
    ChatResponse::builder()
        .content(vec![ContentBlock::Text(TextBlock::new(text))])
        .build()
}

fn tool_call_reply(name: &str) -> ChatResponse {
    ChatResponse::builder()
        .content(vec![ContentBlock::ToolUse(ToolUseBlock::new(
            "call-1",
            name,
            JsonMap::new(),
        ))])
        .build()
}

fn local_tool(name: &str, counter: Arc<AtomicU32>) -> Arc<dyn Tool> {
    let tool_name = name.to_string();
    Arc::new(FunctionTool::new(
        name,
        format!("{name} description"),
        serde_json::json!({"type": "object"}),
        Box::new(move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(agentscope_core::ToolResultBlock::text(tool_name.clone()))
        }),
    ))
}

fn external_tool(name: &str) -> Arc<dyn Tool> {
    Arc::new(SchemaOnlyTool::new(ToolSchema::new(name, "frontend tool")))
}

fn extensions_with(config: ToolRequestConfig) -> agentscope_core::agent::Extensions {
    let mut extensions = agentscope_core::agent::Extensions::new();
    extensions.insert_arc(TOOL_REQUEST_CONFIG_CONTEXT_KEY, Arc::new(config));
    extensions
}

async fn drain(agent: &ReActAgent, extensions: agentscope_core::agent::Extensions) {
    use futures::StreamExt;
    let stream = agent.stream_events_with_extensions(
        vec![Msg::user("hi")],
        StreamOptions::defaults(),
        extensions,
    );
    let events: Vec<_> = stream.collect().await;
    for event in events {
        event.expect("the run must succeed");
    }
}

#[tokio::test]
async fn request_config_hides_backend_schemas_without_unregistering() {
    let counter = Arc::new(AtomicU32::new(0));
    let toolkit = Toolkit::new();
    toolkit.register_tool(local_tool("backend_tool", Arc::clone(&counter)));

    let model = Arc::new(RecordingModel::new(vec![text_reply("done")]));
    let seen = Arc::clone(&model.seen_tools);
    let agent = ReActAgent::builder()
        .name("cfg")
        .model(model)
        .toolkit(toolkit.clone())
        .build()
        .unwrap();

    let config =
        ToolRequestConfig::new(vec![external_tool("frontend_tool")], ToolMergeMode::ExternalOnly)
            .unwrap();
    drain(&agent, extensions_with(config)).await;

    let shown = seen.lock().unwrap().first().cloned().unwrap_or_default();
    assert_eq!(
        shown,
        vec!["frontend_tool".to_string()],
        "EXTERNAL_ONLY must present only the request-config tools, got {shown:?}"
    );
    // The shared registry is still intact for the next call.
    assert!(toolkit.get_tool("backend_tool").is_some());
}

#[tokio::test]
async fn request_config_external_tool_short_circuits_execution() {
    let counter = Arc::new(AtomicU32::new(0));
    let toolkit = Toolkit::new();
    toolkit.register_tool(local_tool("shared", Arc::clone(&counter)));

    // The model asks for `shared`; the request config shadows it with an external tool, so
    // the local implementation must never run.
    let model = Arc::new(RecordingModel::new(vec![
        tool_call_reply("shared"),
        text_reply("done"),
    ]));
    let agent = ReActAgent::builder()
        .name("cfg-shadow")
        .model(model)
        .toolkit(toolkit)
        .max_iters(3)
        .build()
        .unwrap();

    let config = ToolRequestConfig::new(
        vec![external_tool("shared")],
        ToolMergeMode::MergeExternalPriority,
    )
    .unwrap();
    drain(&agent, extensions_with(config)).await;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "an external override must not fall through to the backend implementation"
    );
}

#[tokio::test]
async fn without_a_config_the_shared_toolkit_is_used_as_is() {
    let counter = Arc::new(AtomicU32::new(0));
    let toolkit = Toolkit::new();
    toolkit.register_tool(local_tool("backend_tool", Arc::clone(&counter)));

    let model = Arc::new(RecordingModel::new(vec![text_reply("done")]));
    let seen = Arc::clone(&model.seen_tools);
    let agent = ReActAgent::builder()
        .name("cfg-none")
        .model(model)
        .toolkit(toolkit)
        .build()
        .unwrap();

    drain(&agent, Default::default()).await;

    let shown = seen.lock().unwrap().first().cloned().unwrap_or_default();
    assert!(
        shown.contains(&"backend_tool".to_string()),
        "the shared toolkit must be presented as-is, got {shown:?}"
    );
}

#[test]
fn request_config_is_absent_by_default() {
    let ctx = RuntimeContext::new();
    assert!(
        ctx.extension::<ToolRequestConfig>(TOOL_REQUEST_CONFIG_CONTEXT_KEY)
            .is_none(),
        "an ad-hoc context must not carry a tool view"
    );
}

#[test]
fn a_cloned_context_does_not_inherit_a_config_inserted_afterwards() {
    // Value-semantic extensions are what keep concurrent calls from seeing each other's
    // tool surface.
    let ctx = RuntimeContext::new();
    let mut forked = ctx.clone();
    let config =
        ToolRequestConfig::new(vec![external_tool("frontend_tool")], ToolMergeMode::ExternalOnly)
            .unwrap();
    forked.set_extension_arc(TOOL_REQUEST_CONFIG_CONTEXT_KEY, Arc::new(config));
    assert!(forked
        .extension::<ToolRequestConfig>(TOOL_REQUEST_CONFIG_CONTEXT_KEY)
        .is_some());
    assert!(ctx
        .extension::<ToolRequestConfig>(TOOL_REQUEST_CONFIG_CONTEXT_KEY)
        .is_none());
}
