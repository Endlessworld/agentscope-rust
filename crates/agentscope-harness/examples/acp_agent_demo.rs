//! ACP CodingAgent demo — runs the agent over stdio with a scripted model.
//!
//! This example starts an ACP agent (server role) that speaks the Agent Client
//! Protocol over stdio. It wraps a core `ReActAgent` backed by a scripted
//! model (no API key required) and a simple `add` tool, mirroring the Kotlin
//! `AgiAgent` / Java `HarnessCodingAgent`.
//!
//! To drive it, pipe ACP JSON-RPC messages over stdin. A minimal smoke check:
//!   echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}' \
//!     | cargo run -p agentscope-harness --example acp_agent_demo
//!
//! Note: a full interactive ACP client (IDE/CLI) is the intended peer.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;

use agentscope_core::error::Result;
use agentscope_core::message::{ContentBlock, JsonMap, Msg, TextBlock, ToolResultBlock};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::tool::{FunctionTool, Tool, ToolExecutionContext};
use agentscope_harness::acp::{AcpCodingAgent, config::ModeChoice};
use agentscope_harness::agent::HarnessAgent;

/// A scripted model that replies with a fixed text (no tool use).
struct ScriptedModel {
    name: String,
}

impl ScriptedModel {
    fn new() -> Self {
        Self { name: "scripted".to_string() }
    }
}

#[async_trait]
impl Model for ScriptedModel {
    fn stream(
        &self,
        _messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> BoxStream<'_, Result<ChatResponse>> {
        let resp = ChatResponse::builder()
            .content(vec![ContentBlock::Text(TextBlock::new(
                "Hello from the ACP CodingAgent! This is a scripted reply.",
            ))])
            .build();
        Box::pin(futures::stream::once(async move { Ok(resp) }))
    }

    fn model_name(&self) -> &str {
        &self.name
    }
}

/// An `add` tool used to demonstrate tool registration.
fn add_tool() -> Arc<dyn Tool> {
    Arc::new(FunctionTool::new(
        "add",
        "Adds two integers and returns their sum.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "a": { "type": "integer", "description": "first addend" },
                "b": { "type": "integer", "description": "second addend" }
            },
            "required": ["a", "b"]
        }),
        Box::new(|args: JsonMap, _ctx: ToolExecutionContext| {
            let a = args.get("a").and_then(|v| v.as_i64()).unwrap_or(0);
            let b = args.get("b").and_then(|v| v.as_i64()).unwrap_or(0);
            Ok(ToolResultBlock::text(format!(
                "{} + {} = {}",
                a,
                b,
                a + b
            )))
        }),
    ))
}

/// Builds a `HarnessAgent` for a given working directory.
fn build_agent(_cwd: PathBuf) -> HarnessAgent {
    let model: Arc<dyn Model> = Arc::new(ScriptedModel::new());
    // The harness agent embeds a ReActAgent wired to the same toolkit + model.
    // Register the add tool on the harness toolkit after build; the embedded
    // ReActAgent shares the toolkit by Arc, so the tool is picked up at run time.
    let harness = HarnessAgent::builder()
        .name("acp-coding-agent")
        .model(model)
        .cwd(_cwd)
        .max_iters(3)
        .build();
    harness.toolkit().register_tool(add_tool());
    harness
}

#[tokio::main]
async fn main() -> Result<()> {
    let modes = vec![
        ModeChoice {
            id: "yolo".to_string(),
            name: "YOLO".to_string(),
        },
        ModeChoice {
            id: "plan".to_string(),
            name: "Plan".to_string(),
        },
    ];

    let agent = AcpCodingAgent::builder()
        .name("agentscope-coding-agent")
        .model_factory(build_agent)
        .modes(modes)
        .build()
        .map_err(|e| agentscope_core::error::Error::Agent(e.to_string()))?;

    tracing::info!("ACP CodingAgent starting over stdio...");
    agent
        .run_stdio()
        .await
        .map_err(|e| agentscope_core::error::Error::Agent(e.to_string()))?;
    Ok(())
}
