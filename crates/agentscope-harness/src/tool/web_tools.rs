//! `WebTools` — builtin `web_fetch` / `web_search` tools, mirroring
//! `io.agentscope.harness.agent.tool.WebTools`.
//!
//! `web_search` uses the DeepSeek Web Search API when `DEEPSEEK_API_KEY` is
//! present; otherwise returns a clear configuration error. `web_fetch` uses the
//! DeepSeek Fetch API when `DEEPSEEK_API_KEY` is present, otherwise falls back
//! to a plain `GET` whose HTML body is reduced to readable text.
use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};
use async_trait::async_trait;
use std::time::Duration;

const USER_AGENT: &str = "AgentScope-Harness-WebFetch/1.0";
const DEEPSEEK_SEARCH_API: &str = "https://api.deepseek.com/v1/tools/search";
const DEEPSEEK_FETCH_API: &str = "https://api.deepseek.com/v1/tools/fetch";

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(30))
        .user_agent(USER_AGENT)
        .build()
        .unwrap_or_default()
}

/// Fetches content from an HTTP(S) URL and returns a truncated text preview.
///
/// When `DEEPSEEK_API_KEY` is available the DeepSeek Fetch API is used and the
/// returned Markdown body is passed through unchanged; otherwise a plain `GET`
/// is issued and an HTML response is reduced to readable text (no markup).
#[derive(Default)]
pub struct WebFetchTool {
    client: Option<reqwest::Client>,
    api_key: Option<String>,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self {
            client: None,
            api_key: None,
        }
    }

    /// Reads the API key from the `DEEPSEEK_API_KEY` environment variable.
    pub fn from_env() -> Self {
        Self {
            client: None,
            api_key: std::env::var("DEEPSEEK_API_KEY")
                .ok()
                .filter(|k| !k.trim().is_empty()),
        }
    }

    /// Supplies the DeepSeek API key explicitly (e.g. vault-injected).
    pub fn with_api_key(api_key: impl Into<String>) -> Self {
        let key = api_key.into();
        Self {
            client: None,
            api_key: (!key.trim().is_empty()).then_some(key),
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

    /// DeepSeek Fetch: returns `{title, content}` with a Markdown body.
    async fn fetch_via_deepseek(
        &self,
        url: &str,
        api_key: &str,
        max_chars: usize,
    ) -> Result<ToolResultBlock> {
        let payload = serde_json::json!({ "url": url });
        let resp = self
            .http()
            .post(DEEPSEEK_FETCH_API)
            .header("Authorization", format!("Bearer {api_key}"))
            .json(&payload)
            .send()
            .await;
        match resp {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status != 200 {
                    return Ok(ToolResultBlock::text(format!(
                        "Error: DeepSeek fetch API returned {status}"
                    )));
                }
                let body: serde_json::Value =
                    serde_json::from_str(&resp.text().await.unwrap_or_default())
                        .unwrap_or(serde_json::Value::Null);
                let title = body.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let content = body.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let content = truncate(content, max_chars);
                Ok(ToolResultBlock::text(format!("{title}\n\n{content}")))
            }
            Err(e) => Ok(ToolResultBlock::error(format!(
                "Error: web_fetch failed: {e}"
            ))),
        }
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch content from an HTTP(S) URL. Uses the DeepSeek Fetch API when \
         DEEPSEEK_API_KEY is set (returns Markdown), otherwise a plain GET whose \
         HTML body is reduced to readable text."
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
            return Ok(ToolResultBlock::error(
                "Error: only http/https URLs are allowed",
            ));
        }
        let max_chars = args
            .get("max_chars")
            .and_then(|v| v.as_u64())
            .filter(|&n| n > 0)
            .map_or(20_000usize, |n| (n as usize).min(100_000));

        if let Some(api_key) = self.api_key.clone() {
            return self.fetch_via_deepseek(url, &api_key, max_chars).await;
        }

        match self.http().get(url).send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let is_html = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(|v| v.to_ascii_lowercase().contains("html"))
                    .unwrap_or(false);
                let body = resp.text().await.unwrap_or_default();
                let text = if is_html || body.trim_start().starts_with('<') {
                    html_to_text(body_of(&body))
                } else {
                    body
                };
                let text = truncate(&text, max_chars);
                Ok(ToolResultBlock::text(format!("status={status}\n\n{text}")))
            }
            Err(e) => Ok(ToolResultBlock::error(format!(
                "Error: web_fetch failed: {e}"
            ))),
        }
    }
}

/// Truncates to `max_chars` characters, appending a marker when cut.
fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() > max_chars {
        text.chars().take(max_chars).collect::<String>() + "\n...[truncated]"
    } else {
        text.to_string()
    }
}

/// Narrows to the `<body>` element when present, so `<head>`/`<meta>` noise
/// never reaches the model. The lowercase copy is ASCII-only, so indices align.
fn body_of(html: &str) -> &str {
    let lower = html.to_ascii_lowercase();
    let Some(start) = lower.find("<body") else {
        return html;
    };
    let Some(gt) = lower[start..].find('>') else {
        return html;
    };
    let begin = start + gt + 1;
    let end = lower[begin..]
        .find("</body")
        .map_or(html.len(), |e| begin + e);
    &html[begin..end]
}

/// Reduces an HTML fragment to readable text without a third-party parser:
/// comments, `<script>`/`<style>` bodies and all tags are dropped, block-level
/// tags and `<br>` become line breaks, common entities are decoded, and runs of
/// whitespace are collapsed.
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut i = 0usize;
    while i < html.len() {
        let rest = &html[i..];

        if let Some(after) = rest.strip_prefix("<!--") {
            match after.find("-->") {
                Some(off) => i += 4 + off + 3,
                None => break,
            }
            continue;
        }

        if let Some(after) = rest.strip_prefix('<') {
            let Some(end) = after.find('>') else { break };
            let tag = &after[..end];
            let closing = tag.starts_with('/');
            let name = tag
                .trim_start_matches('/')
                .split(|c: char| c.is_whitespace() || c == '/')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();

            if !closing && (name == "script" || name == "style") {
                let close = format!("</{name}");
                let hay = &after[end + 1..];
                match hay.to_ascii_lowercase().find(&close) {
                    Some(off) => match hay[off..].find('>') {
                        Some(gt) => i += 1 + end + 1 + off + gt + 1,
                        None => break,
                    },
                    None => break,
                }
                continue;
            }

            if matches!(
                name.as_str(),
                "br" | "p"
                    | "div"
                    | "li"
                    | "tr"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "ul"
                    | "ol"
                    | "table"
                    | "section"
                    | "article"
                    | "header"
                    | "footer"
                    | "pre"
                    | "blockquote"
            ) {
                out.push('\n');
            }
            i += 1 + end + 1;
            continue;
        }

        if let Some(after) = rest.strip_prefix('&') {
            if let Some(semi) = after.find(';').filter(|&s| s <= 12) {
                if let Some(ch) = decode_entity(&after[..semi]) {
                    out.push(ch);
                    i += 1 + semi + 1;
                    continue;
                }
            }
        }

        let ch = rest.chars().next().unwrap();
        out.push(if ch.is_whitespace() { ' ' } else { ch });
        i += ch.len_utf8();
    }

    out.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Decodes the named/numeric character entities that show up in real pages.
fn decode_entity(ent: &str) -> Option<char> {
    match ent {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" | "#39" => Some('\''),
        "nbsp" | "#160" => Some(' '),
        _ => {
            let num = ent.strip_prefix('#')?;
            let code = match num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse::<u32>().ok()?,
            };
            char::from_u32(code)
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
            api_key: std::env::var("DEEPSEEK_API_KEY")
                .ok()
                .filter(|k| !k.trim().is_empty()),
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
                let body: serde_json::Value =
                    serde_json::from_str(&resp.text().await.unwrap_or_default())
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
            Err(e) => Ok(ToolResultBlock::error(format!(
                "Error: web_search failed: {e}"
            ))),
        }
    }
}

/// Registers both web tools into a toolkit.
pub fn register_web_tools(toolkit: &agentscope_core::tool::Toolkit) {
    WebFetchTool::register(toolkit);
    WebSearchTool::register(toolkit);
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_drops_scripts_styles_and_tags() {
        let html = "<div><script>var x = '<b>not text</b>';</script>\
                    <style>.a{color:red}</style>\
                    <h1> Title </h1><p>First   paragraph.</p>\
                    <p>Second&nbsp;one &amp; more.</p><!-- ignore me --></div>";
        let text = html_to_text(html);
        assert!(!text.contains("var x"), "got: {text}");
        assert!(!text.contains("color:red"), "got: {text}");
        assert!(!text.contains("ignore me"), "got: {text}");
        assert!(!text.contains('<'), "got: {text}");
        assert_eq!(text, "Title\nFirst paragraph.\nSecond one & more.");
    }

    #[test]
    fn html_to_text_decodes_named_and_numeric_entities() {
        assert_eq!(
            html_to_text("a &lt;b&gt; c &#39;q&#39; &#x41;"),
            "a <b> c 'q' A"
        );
        // An unknown entity is left as-is rather than swallowed.
        assert_eq!(html_to_text("x &bogus; y"), "x &bogus; y");
    }

    #[test]
    fn html_to_text_breaks_lines_on_block_elements() {
        assert_eq!(html_to_text("one<br>two"), "one\ntwo");
        assert_eq!(html_to_text("<li>a</li><li>b</li>"), "a\nb");
        assert_eq!(html_to_text("<span>a</span><span>b</span>"), "ab");
    }

    #[test]
    fn body_of_narrows_to_the_body_element() {
        let html = "<html><head><meta name=\"x\"><title>T</title></head>\
                    <body><p>Hello</p></body></html>";
        let body = body_of(html);
        assert!(body.starts_with("<p>Hello</p>"), "got: {body}");
        assert!(!body.contains("title"), "got: {body}");
        assert_eq!(body_of("<p>no body tag</p>"), "<p>no body tag</p>");
        // Missing `</body>` still yields everything after the open tag.
        assert_eq!(body_of("<BODY>x"), "x");
    }

    #[test]
    fn truncate_counts_characters_not_bytes() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 3), "abc\n...[truncated]");
        // Multi-byte characters are not split mid-codepoint.
        assert_eq!(
            truncate("\u{4e2d}\u{6587}\u{6d4b}\u{8bd5}", 2),
            "\u{4e2d}\u{6587}\n...[truncated]"
        );
    }

    #[test]
    fn fetch_defaults_to_no_key_so_the_free_path_is_used() {
        assert!(WebFetchTool::new().api_key.is_none());
        assert!(WebFetchTool::with_api_key("  ").api_key.is_none());
        assert_eq!(
            WebFetchTool::with_api_key("k").api_key.as_deref(),
            Some("k")
        );
    }
}
