//! Minimal end-to-end demo for the model layer.
//!
//! Resolves an OpenAI-compatible model through the [`default_registry`] and
//! streams a single chat completion, printing the accumulated reply.
//!
//! Credentials can come from environment variables (async-openai reads
//! `OPENAI_API_KEY` / `OPENAI_BASE_URL` automatically) or be set programmatically.
//!   - `OPENAI_API_KEY`  — required for hosted providers
//!   - `OPENAI_BASE_URL` — optional, point at DeepSeek/Kimi/GLM/Ollama etc.
//!   - `AGENTSCOPE_MODEL` — optional model id (default `gpt-4o`)
//!
//! Run:
//! ```text
//! cargo run -p agentscope-extensions-model --example chat_demo
//! ```

use agentscope_core::{ContentBlock, ModelCreationContext};
use agentscope_extensions_model::default_registry;
use futures::StreamExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let registry = default_registry();
    let model_id = std::env::var("AGENTSCOPE_MODEL").unwrap_or_else(|_| "mimo-v2.5".to_string());

    // Programmatic credentials: set API key and base URL directly in code.
    // For example, point to a local or third-party OpenAI-compatible endpoint.
    let api_key = std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|s| !s.is_empty());
    // Programmatic base URL - set directly in code
    let base_url = Some("https://opencode.ai/zen/go/v1".to_string());

    let mut ctx = ModelCreationContext::new("openai", &model_id);
    if let Some(k) = api_key {
        ctx = ctx.api_key(k);
    }
    if let Some(b) = base_url {
        ctx = ctx.base_url(b);
    }

    let model = registry.resolve_with(&model_id, ctx)?;
    println!("resolved model: {}", model.model_name());

    let messages = vec![agentscope_core::Msg::user(
        "Reply with exactly one short sentence.",
    )];
    let mut stream = model.stream(messages, Vec::new(), Default::default());

    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        for block in chunk.content {
            if let ContentBlock::Text(t) = block {
                text.push_str(&t.text);
            }
        }
    }
    println!("reply: {}", text.trim());
    Ok(())
}
