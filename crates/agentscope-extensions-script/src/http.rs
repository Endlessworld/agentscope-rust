use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use agentscope_core::error::{Error, Result};
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Url;
use serde_json::json;

/// Policy for the guarded HTTP tool.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// Reject literal/private/loopback/link-local addresses (SSRF defence).
    pub deny_private: bool,
    /// Optional host allowlist. Entries are exact hosts or `*.suffix` wildcards.
    pub allow_hosts: Option<Vec<String>>,
    pub timeout: Duration,
    pub max_bytes: usize,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            deny_private: true,
            allow_hosts: None,
            timeout: Duration::from_secs(15),
            max_bytes: 1 << 20,
        }
    }
}

/// A guarded `http_request` tool.
///
/// Redirects are **not** followed (`Policy::none`), so a 3xx cannot bounce the request to a host
/// that bypasses the SSRF check. Note the DNS pre-resolution here is advisory: `reqwest` resolves
/// again on connect, so a strict deployment should pin the resolved address (TOCTOU caveat).
pub struct HttpRequestTool {
    config: Arc<HttpConfig>,
    client: reqwest::Client,
}

impl HttpRequestTool {
    pub fn new(config: HttpConfig) -> Self {
        // Configuring a redirect-free client cannot fail for a valid timeout; do not fall back to
        // `Client::new()` because that *does* follow redirects and would bypass the SSRF guard.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(config.timeout)
            .build()
            .expect("guarded http client configuration is valid");
        Self {
            config: Arc::new(config),
            client,
        }
    }

    pub fn with_default() -> Self {
        Self::new(HttpConfig::default())
    }

    async fn guard(&self, url: &Url) -> Result<()> {
        match url.scheme() {
            "http" | "https" => {}
            other => return Err(Error::Tool(format!("scheme not allowed: {other}"))),
        }
        let host = url
            .host_str()
            .ok_or_else(|| Error::Tool("url has no host".into()))?;
        if let Some(list) = &self.config.allow_hosts {
            let allowed = list
                .iter()
                .any(|h| host == h || (h.starts_with("*.") && host.ends_with(&h[1..])));
            if !allowed {
                return Err(Error::Tool(format!("host not allowed: {host}")));
            }
        }
        if self.config.deny_private {
            if let Ok(ip) = host.parse::<IpAddr>() {
                if is_private_ip(&ip) {
                    return Err(Error::Tool(format!("private address not allowed: {host}")));
                }
            } else {
                let port = url.port_or_known_default().unwrap_or(80);
                let addrs = tokio::net::lookup_host((host, port))
                    .await
                    .map_err(|e| Error::Tool(format!("dns resolution failed: {e}")))?;
                for addr in addrs {
                    if is_private_ip(&addr.ip()) {
                        return Err(Error::Tool(format!(
                            "host resolves to a private address: {}",
                            addr.ip()
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Tool for HttpRequestTool {
    fn name(&self) -> &str {
        "http_request"
    }

    fn description(&self) -> &str {
        "Perform a guarded HTTP request (http/https, private addresses rejected, no redirects)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string" },
                "method": { "type": "string" },
                "headers": { "type": "object", "additionalProperties": { "type": "string" } },
                "body": { "type": "string" }
            },
            "required": ["url"]
        })
    }

    async fn call(&self, args: JsonMap, _ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let url_str = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Tool("missing string argument: url".into()))?;
        let url = Url::parse(url_str).map_err(|e| Error::Tool(format!("invalid url: {e}")))?;
        self.guard(&url).await?;

        let method = args
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("GET")
            .to_uppercase();
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| Error::Tool(format!("invalid method: {e}")))?;
        let mut req = self.client.request(method, url.clone());
        if let Some(headers) = args.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in headers {
                if let Some(s) = v.as_str() {
                    req = req.header(k.as_str(), s);
                }
            }
        }
        if let Some(body) = args.get("body").and_then(|v| v.as_str()) {
            req = req.body(body.to_string());
        }

        let resp = req.send().await.map_err(|e| Error::Http(e.to_string()))?;
        let status = resp.status().as_u16();
        if let Some(len) = resp.content_length() {
            if len as usize > self.config.max_bytes {
                return Err(Error::Tool(format!(
                    "response too large: {len} bytes (max {})",
                    self.config.max_bytes
                )));
            }
        }
        // Stream the body so an oversized (or chunked, no Content-Length) response is aborted as
        // soon as it crosses the cap instead of being fully buffered first.
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| Error::Http(e.to_string()))?;
            if buf.len() + chunk.len() > self.config.max_bytes {
                return Err(Error::Tool(format!(
                    "response too large (max {} bytes)",
                    self.config.max_bytes
                )));
            }
            buf.extend_from_slice(&chunk);
        }
        let body = String::from_utf8_lossy(&buf).to_string();
        let out = json!({ "status": status, "url": url.as_str(), "body": body });
        Ok(ToolResultBlock::text(out.to_string()))
    }
}

fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (seg[0] & 0xffc0) == 0xfe80 // link local fe80::/10
        }
    }
}
