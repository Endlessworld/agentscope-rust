//! Shared control-plane REST client, mirroring
//! `io.agentscope.extensions.controlplane.transport.ControlPlaneHttpClient`.
//!
//! Sends `X-Builder-Internal-Token` on every request and enforces a 10s timeout.

use std::time::Duration;

use reqwest::{Client, Method};
use serde_json::Value;

use crate::error::{ControlPlaneError, ControlPlaneResult};

/// Header carrying the trusted bootstrap token.
pub const INTERNAL_TOKEN_HEADER: &str = "X-Builder-Internal-Token";
/// Default per-request timeout.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Decoded HTTP response with a string body.
#[derive(Debug, Clone)]
pub struct Response {
    /// HTTP status code.
    pub status: u16,
    /// Response body as text.
    pub body: String,
}

/// Decoded HTTP response with a raw byte body (e.g. snapshot octet-streams).
#[derive(Debug, Clone)]
pub struct BytesResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response body bytes.
    pub body: Vec<u8>,
}

/// Shared HTTP client for control plane REST calls.
#[derive(Debug, Clone)]
pub struct ControlPlaneHttpClient {
    base_url: String,
    internal_token: String,
    http: Client,
}

impl ControlPlaneHttpClient {
    /// Creates a client against the given control-plane base URL.
    ///
    /// The trailing slash is trimmed; a missing token is allowed (empty header).
    pub fn new(
        base_url: impl Into<String>,
        internal_token: impl Into<String>,
    ) -> ControlPlaneResult<Self> {
        let http = Client::builder()
            .connect_timeout(HTTP_TIMEOUT)
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| ControlPlaneError::Http(e.to_string()))?;
        Ok(Self {
            base_url: trim_slash(&base_url.into()),
            internal_token: internal_token.into(),
            http,
        })
    }

    /// Returns the trimmed base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Sends a JSON request and returns the string body. `json_body == None` sends no body.
    pub async fn send(
        &self,
        method: Method,
        path: &str,
        json_body: Option<Value>,
    ) -> ControlPlaneResult<Response> {
        self.send_with_headers(method, path, json_body, &[]).await
    }

    /// Sends a JSON request with extra headers.
    pub async fn send_with_headers(
        &self,
        method: Method,
        path: &str,
        json_body: Option<Value>,
        extra_headers: &[(&str, &str)],
    ) -> ControlPlaneResult<Response> {
        let bytes = self
            .send_for_bytes(method, path, json_body, extra_headers)
            .await?;
        let body = String::from_utf8_lossy(&bytes.body).to_string();
        Ok(Response {
            status: bytes.status,
            body,
        })
    }

    /// Sends a request and returns the raw byte body.
    pub async fn send_for_bytes(
        &self,
        method: Method,
        path: &str,
        json_body: Option<Value>,
        extra_headers: &[(&str, &str)],
    ) -> ControlPlaneResult<BytesResponse> {
        let url = format!("{}{}", self.base_url, path);
        let mut req = self
            .http
            .request(method, &url)
            .header(INTERNAL_TOKEN_HEADER, &self.internal_token);
        for (k, v) in extra_headers {
            req = req.header(*k, *v);
        }
        if let Some(body) = json_body {
            req = req.json(&body);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ControlPlaneError::Http(format!("{url}: {e}")))?;
        let status = resp.status().as_u16();
        let body = resp
            .bytes()
            .await
            .map_err(|e| ControlPlaneError::Http(format!("{url}: {e}")))?
            .to_vec();
        Ok(BytesResponse { status, body })
    }

    /// Sends raw bytes (e.g. `PUT application/octet-stream`).
    pub async fn send_bytes(
        &self,
        method: Method,
        path: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> ControlPlaneResult<Response> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .http
            .request(method, &url)
            .header(INTERNAL_TOKEN_HEADER, &self.internal_token)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(body)
            .send()
            .await
            .map_err(|e| ControlPlaneError::Http(format!("{url}: {e}")))?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| ControlPlaneError::Http(format!("{url}: {e}")))?;
        Ok(Response { status, body: text })
    }
}

/// Trims trailing slashes from a base URL.
pub fn trim_slash(s: &str) -> String {
    s.trim_end_matches('/').to_string()
}

/// Percent-encodes a single path segment.
pub fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Percent-encodes a query parameter value.
pub fn encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Turns a non-2xx response into a [`ControlPlaneError::ControlPlane`].
pub fn require_ok(resp: &Response, operation: &str) -> ControlPlaneResult<()> {
    if (200..300).contains(&resp.status) {
        Ok(())
    } else {
        Err(ControlPlaneError::ControlPlane {
            status: resp.status,
            message: format!("{operation}: {}", resp.body),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_slashes() {
        assert_eq!(trim_slash("http://x/"), "http://x");
        assert_eq!(trim_slash("http://x///"), "http://x");
    }

    #[test]
    fn encodes_path_and_query() {
        assert_eq!(encode_path_segment("a/b c"), "a%2Fb%20c");
        assert_eq!(encode_query("a b"), "a+b");
    }

    #[test]
    fn require_ok_flags_non_2xx() {
        let ok = Response {
            status: 201,
            body: String::new(),
        };
        assert!(require_ok(&ok, "op").is_ok());
        let bad = Response {
            status: 500,
            body: "boom".into(),
        };
        assert!(require_ok(&bad, "op").is_err());
    }
}
