//! Tool-agent end-to-end demo (no API key required).
//!
//! Wires a scripted [`Model`](agentscope_core::model::Model) together with a
//! [`Toolkit`] exposing an `add` tool inside a [`ReActAgent`]. The scripted model
//! behaves like a tool-calling LLM: on the first turn it requests a call to `add`,
//! and once the tool result is fed back it returns the final text reply.
//!
//! This demonstrates the full reasoning -> acting loop (model -> toolkit -> result ->
//! final reply) without needing an OpenAI-compatible API key.
//!
//! Run: `cargo run -p agentscope-core --example tool_agent_demo`

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;

use agentscope_core::agent::ReActAgent;
use agentscope_core::error::Result;
use agentscope_core::message::{
    ContentBlock, JsonMap, Msg, MsgRole, TextBlock, ToolResultBlock, ToolUseBlock,
};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::tool::{FunctionTool, Tool, ToolExecutionContext, Toolkit};

/// A scripted model that first requests the `add` tool and then replies with text.
struct ScriptedModel {
    name: String,
    tool_name: String,
    tool_input: JsonMap,
    final_text: String,
}

impl ScriptedModel {
    fn new() -> Self {
        let mut input = JsonMap::new();
        input.insert("a".to_string(), serde_json::json!(3));
        input.insert("b".to_string(), serde_json::json!(4));
        Self {
            name: "scripted".to_string(),
            tool_name: "add".to_string(),
            tool_input: input,
            final_text: "The sum of 3 and 4 is 7.".to_string(),
        }
    }
}

#[async_trait]
impl Model for ScriptedModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> BoxStream<'_, Result<ChatResponse>> {
        let has_tool_result = messages.iter().any(|m| m.role == MsgRole::Tool);
        let resp = if has_tool_result {
            ChatResponse::builder()
                .content(vec![ContentBlock::Text(TextBlock::new(self.final_text.clone()))])
                .build()
        } else {
            ChatResponse::builder()
                .content(vec![ContentBlock::ToolUse(ToolUseBlock::new(
                    "call_add_1",
                    &self.tool_name,
                    self.tool_input.clone(),
                ))])
                .build()
        };
        Box::pin(futures::stream::once(async move { Ok(resp) }))
    }

    fn model_name(&self) -> &str {
        &self.name
    }
}

/// An `add` tool that returns the sum of its two integer arguments.
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
            Ok(ToolResultBlock::text(format!("{} + {} = {}", a, b, a + b)))
        }),
    ))
}

#[tokio::main]
async fn main() -> Result<()> {
    let toolkit = Toolkit::new();
    toolkit.register_tool(add_tool());

    let model: Arc<dyn Model> = Arc::new(ScriptedModel::new());

    let agent = ReActAgent::builder()
        .name("tool-agent")
        .model(model)
        .toolkit(toolkit)
        .max_iters(3)
        .build()?;

    let reply = agent
        .call(vec![Msg::user("What is 3 + 4? Use the add tool.")])
        .await?;

    println!("final reply: {}", reply.text_content());
    assert_eq!(reply.text_content(), "The sum of 3 and 4 is 7.");
    println!("tool-agent demo passed \u{2713}");
    Ok(())
}
