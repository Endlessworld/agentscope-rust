//! `WebTools` — builtin `web_fetch` / `web_search` tools, mirroring
//! `io.agentscope.harness.agent.tool.WebTools`.
//!
//! `web_search` uses the DeepSeek Web Search API when `DEEPSEEK_API_KEY` is
//! present; otherwise returns a clear configuration error.
use async_trait::async_trait;
use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};
use std::time::Duration;

const USER_AGENT: &str = "AgentScope-Harness-WebFetch/1.0";
const DEEPSEEK_SEARCH_API: &str = "https://api.deepseek.com/v1/tools/search";

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(30))
        .user_agent(USER_AGENT)
        .build()
        .unwrap_or_default()
}

/// Fetches content from an HTTP(S) URL and returns a truncated text preview.
#[derive(Default)]
pub struct WebFetchTool {
    client: Option<reqwest::Client>,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self { client: None }
    }

    /// Uses a caller-supplied HTTP client instead of the default one.
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client: Some(client) }
    }

    fn http(&self) -> reqwest::Client {
        self.client.clone().unwrap_or_else(client)
    }

    pub fn register(toolkit: &agentscope_core::tool::Toolkit) {
        toolkit.register_tool(std::sync::Arc::new(Self::new()));
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch content from an HTTP(S) URL and return a truncated text preview."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "HTTP or HTTPS URL to fetch"},
                "max_chars": {"type": "integer", "description": "Max characters to return (default 20000)"}
            },
            "required": ["url"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let Some(url) = args.get("url").and_then(|v| v.as_str()).map(str::trim) else {
            return Ok(ToolResultBlock::error("Error: url is required"));
        };
        if url.is_empty() {
            return Ok(ToolResultBlock::error("Error: url is required"));
        }
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Ok(ToolResultBlock::error("Error: only http/https URLs are allowed"));
        }
        let max_chars = args
            .get("max_chars")
            .and_then(|v| v.as_u64())
            .filter(|&n| n > 0)
            .map_or(20_000usize, |n| (n as usize).min(100_000));

        match self.http().get(url).send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let mut body = resp.text().await.unwrap_or_default();
                if body.chars().count() > max_chars {
                    body = body.chars().take(max_chars).collect::<String>() + "\n...[truncated]";
                }
                Ok(ToolResultBlock::text(format!("status={status}\n\n{body}")))
            }
            Err(e) => Ok(ToolResultBlock::error(format!("Error: web_fetch failed: {e}"))),
        }
    }
}

/// Searches the web via the DeepSeek Web Search API.
pub struct WebSearchTool {
    api_key: Option<String>,
    client: Option<reqwest::Client>,
}

impl WebSearchTool {
    /// Reads the API key from the `DEEPSEEK_API_KEY` environment variable.
    pub fn from_env() -> Self {
        Self {
            api_key: std::env::var("DEEPSEEK_API_KEY").ok().filter(|k| !k.trim().is_empty()),
            client: None,
        }
    }

    /// Supplies the DeepSeek API key explicitly (e.g. vault-injected).
    pub fn with_api_key(api_key: impl Into<String>) -> Self {
        let key = api_key.into();
        Self {
            api_key: (!key.trim().is_empty()).then_some(key),
            client: None,
        }
    }

    /// Uses a caller-supplied HTTP client instead of the default one.
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = Some(client);
        self
    }

    fn http(&self) -> reqwest::Client {
        self.client.clone().unwrap_or_else(client)
    }

    pub fn register(toolkit: &agentscope_core::tool::Toolkit) {
        toolkit.register_tool(std::sync::Arc::new(Self::from_env()));
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web for information. Requires DEEPSEEK_API_KEY. Returns titles, \
         URLs and summaries."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query"},
                "max_results": {"type": "integer", "description": "Maximum results (default 5)"}
            },
            "required": ["query"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let Some(query) = args.get("query").and_then(|v| v.as_str()).map(str::trim) else {
            return Ok(ToolResultBlock::error("Error: query is required"));
        };
        if query.is_empty() {
            return Ok(ToolResultBlock::error("Error: query is required"));
        }
        let Some(api_key) = self.api_key.clone() else {
            return Ok(ToolResultBlock::error(
                "Error: DEEPSEEK_API_KEY is not set. Configure the key via Environment vault \
                 credentials or process env to enable web_search.",
            ));
        };
        let limit = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .filter(|&n| n > 0)
            .map_or(5u64, |n| n.min(10));

        let payload = serde_json::json!({
            "query": query,
            "count": limit,
            "need_content": false
        });
        let resp = self
            .http()
            .post(DEEPSEEK_SEARCH_API)
            .header("Authorization", format!("Bearer {api_key}"))
            .json(&payload)
            .send()
            .await;
        match resp {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status != 200 {
                    return Ok(ToolResultBlock::text(format!(
                        "Error: DeepSeek search API returned {status}"
                    )));
                }
                let body: serde_json::Value = serde_json::from_str(&resp.text().await.unwrap_or_default())
                    .unwrap_or(serde_json::Value::Null);
                let Some(results) = body.get("results").and_then(|r| r.as_array()) else {
                    return Ok(ToolResultBlock::text("No results."));
                };
                if results.is_empty() {
                    return Ok(ToolResultBlock::text("No results."));
                }
                let get_str = |v: &serde_json::Value, k: &str| {
                    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
                };
                let mut sb = String::new();
                for (i, r) in results.iter().enumerate() {
                    let summary = get_str(r, "summary");
                    let text = if summary.is_empty() {
                        get_str(r, "snippet")
                    } else {
                        summary
                    };
                    sb.push_str(&format!(
                        "{}. {}\n   {}\n   {}\n\n",
                        i + 1,
                        get_str(r, "title"),
                        get_str(r, "url"),
                        text
                    ));
                }
                Ok(ToolResultBlock::text(sb.trim_end().to_string()))
            }
            Err(e) => Ok(ToolResultBlock::error(format!("Error: web_search failed: {e}"))),
        }
    }
}

/// Registers both web tools into a toolkit.
pub fn register_web_tools(toolkit: &agentscope_core::tool::Toolkit) {
    WebFetchTool::register(toolkit);
    WebSearchTool::register(toolkit);
}
