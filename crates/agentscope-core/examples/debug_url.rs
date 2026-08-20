use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use agentscope_core::message::{ContentBlock as CB, Msg, TextBlock};
use async_trait::async_trait;
use futures::stream::BoxStream;

/// A simple reqwest-based model for OpenAI-compatible APIs.
/// Works around async-openai URL issues with certain providers.
struct ReqwestModel {
    client: reqwest::Client,
    api_base: String,
    api_key: String,
    model_name: String,
}

impl ReqwestModel {
    fn new(api_base: String, api_key: String, model_name: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_base,
            api_key,
            model_name,
        }
    }
}

#[async_trait]
impl Model for ReqwestModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        _tools: Vec<ToolSchema>,
        _options: GenerateOptions,
    ) -> BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        // Build request body
        let msgs: Vec<serde_json::Value> = messages.iter().map(|m| {
            serde_json::json!({
                "role": match m.role {
                    agentscope_core::message::MsgRole::System => "system",
                    agentscope_core::message::MsgRole::User => "user",
                    agentscope_core::message::MsgRole::Assistant => "assistant",
                    _ => "user",
                },
                "content": m.text_content(),
            })
        }).collect();

        let body = serde_json::json!({
            "model": self.model_name,
            "messages": msgs,
            "stream": false,
        });

        let url = format!("{}/chat/completions", self.api_base);
        let api_key = self.api_key.clone();
        let client = self.client.clone();

        let fut = async move {
            let resp = client
                .post(&url)
                .header("Authorization", format!("Bearer {}", api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|e| agentscope_core::error::Error::Other(format!("HTTP error: {e}")))?;

            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(agentscope_core::error::Error::HttpStatus {
                    status: status.as_u16(),
                    message: text,
                });
            }

            let json: serde_json::Value = resp.json().await
                .map_err(|e| agentscope_core::error::Error::Other(format!("JSON parse error: {e}")))?;

            // Extract the reply text
            let text = json["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("(no content)")
                .to_string();

            let resp = ChatResponse::builder()
                .content(vec![CB::Text(TextBlock::new(text))])
                .build();

            Ok(resp)
        };
        Box::pin(futures::stream::once(fut))
    }

    fn model_name(&self) -> &str {
        &self.model_name
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base_url = std::env::var("OPENAI_BASE_URL").unwrap_or_default();
    let api_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
    let model = std::env::var("AGENTSCOPE_MODEL").unwrap_or_else(|_| "gpt-4o".to_string());

    println!("base_url = {base_url}");
    println!("model = {model}");

    let req_model = ReqwestModel::new(base_url, api_key, model);

    use agentscope_core::agent::CallableAgent;
    use agentscope_core::agent::ReActAgent;
    use agentscope_core::tool::Toolkit;

    let agent = ReActAgent::builder()
        .name("chat-agent")
        .model(std::sync::Arc::new(req_model) as std::sync::Arc<dyn agentscope_core::model::Model>)
        .toolkit(Toolkit::new())
        .max_iters(1)
        .build()?;

    let reply = agent.call(vec![Msg::user("Say hello in one sentence.")]).await?;
    println!("reply: {}", reply.text_content());
    Ok(())
}
