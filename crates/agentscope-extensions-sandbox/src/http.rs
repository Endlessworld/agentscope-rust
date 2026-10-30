//! Shared HTTP plumbing for the remote sandbox backends.
//!
//! Wraps `reqwest::Client` with the option surface used by every Java backend
//! (connect/read timeouts, API-key headers, base-URL joining) and normalizes
//! non-success responses into [`SandboxExtensionError::Http`].

use std::time::Duration;

use reqwest::{Client, ClientBuilder, Method, RequestBuilder};
use serde_json::Value;

use crate::error::{SandboxExtensionError, SandboxResult};

/// Timeout / retry configuration shared by all remote backends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpOptions {
    /// TCP connect timeout in seconds (Java default: 30).
    pub connect_timeout_seconds: u64,
    /// Read timeout in seconds (Java default: 120).
    pub read_timeout_seconds: u64,
    /// Maximum retry attempts (Java default: 3).
    pub max_retries: u32,
}

impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            connect_timeout_seconds: 30,
            read_timeout_seconds: 120,
            max_retries: 3,
        }
    }
}

/// Returns a client builder carrying the given timeouts and the ambient proxy configuration.
pub fn client_builder(opts: &HttpOptions) -> ClientBuilder {
    Client::builder()
        .connect_timeout(Duration::from_secs(opts.connect_timeout_seconds))
        .timeout(Duration::from_secs(opts.read_timeout_seconds))
}

/// Builds a `reqwest::Client` honoring the given timeouts.
pub fn build_client(opts: &HttpOptions) -> SandboxResult<Client> {
    client_builder(opts)
        .build()
        .map_err(|e| SandboxExtensionError::Transport(format!("failed to build HTTP client: {e}")))
}

/// Returns whether `value` (a URL, or a bare `host[:port]`) addresses a loopback host.
///
/// Recognizes `localhost` (case-insensitively, with an optional trailing dot) and the whole
/// IPv4 `127.0.0.0/8` range as well as `::1`. A bare host is accepted so that values such as a
/// gateway address (`127.0.0.1:10000`) can be tested before they are turned into a URL.
pub fn is_loopback_target(value: &str) -> bool {
    fn host_is_loopback(url: &reqwest::Url) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim_end_matches('.');
        if host.eq_ignore_ascii_case("localhost") {
            return true;
        }
        host.parse::<std::net::IpAddr>()
            .map(|addr| addr.is_loopback())
            .unwrap_or(false)
    }

    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    if let Ok(url) = reqwest::Url::parse(value) {
        if url.host_str().is_some() {
            return host_is_loopback(&url);
        }
    }
    // Not a URL on its own (e.g. `127.0.0.1:10000`): retry with a synthetic scheme.
    match reqwest::Url::parse(&format!("http://{value}")) {
        Ok(url) => host_is_loopback(&url),
        Err(_) => false,
    }
}

/// Disables the ambient (environment / OS-level) proxy when any target in `targets` is a
/// loopback address.
///
/// Loopback traffic must never be handed to a proxy. `hyper-util` maps the Windows
/// `ProxyOverride` entry `127.*` onto a literal domain name, so the bypass list never matches
/// `127.0.0.1`; a machine with a system-wide proxy then routes every request to a locally
/// served sandbox endpoint (port-forward, local mock server) through that proxy, which
/// intermittently resets idle keep-alive connections. The Java backend is unaffected because
/// `OkHttp`'s default proxy selector always exempts local addresses.
///
/// Non-loopback traffic keeps the ambient proxy configuration.
pub fn bypass_proxy_for_loopback(builder: ClientBuilder, targets: &[&str]) -> ClientBuilder {
    if targets.iter().any(|target| is_loopback_target(target)) {
        builder.no_proxy()
    } else {
        builder
    }
}

/// Joins a base URL and a path, trimming exactly one slash at the boundary.
pub fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    if path.is_empty() {
        base.to_string()
    } else {
        format!("{}/{}", base, path.trim_start_matches('/'))
    }
}

/// A bound request builder factory that applies common options.
#[derive(Debug, Clone)]
pub struct HttpClient {
    client: Client,
    api_key: Option<String>,
    /// Header name used for the API key (`X-API-Key` by default).
    api_key_header: String,
}

impl HttpClient {
    /// Creates an [`HttpClient`] with no default API-key header.
    pub fn new(client: Client) -> Self {
        Self {
            client,
            api_key: None,
            api_key_header: "X-API-Key".to_string(),
        }
    }

    /// Attaches an API key sent on every request.
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Overrides the header used to carry the API key.
    pub fn with_api_key_header(mut self, header: impl Into<String>) -> Self {
        self.api_key_header = header.into();
        self
    }

    /// Returns the configured API key, if any.
    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    /// Returns the underlying reqwest client.
    pub fn raw(&self) -> &Client {
        &self.client
    }

    /// Starts a request, applying the API-key header when configured.
    pub fn request(&self, method: Method, url: impl Into<String>) -> RequestBuilder {
        let mut builder = self.client.request(method, url.into());
        if let Some(key) = &self.api_key {
            builder = builder.header(self.api_key_header.as_str(), key);
        }
        builder
    }

    /// Sends a JSON request and decodes a JSON response, mapping non-2xx to an error.
    pub async fn send_json(
        &self,
        method: Method,
        url: impl Into<String>,
        body: Option<&Value>,
    ) -> SandboxResult<Value> {
        let url = url.into();
        let mut builder = self.request(method, url.clone());
        if let Some(body) = body {
            builder = builder.json(body);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| SandboxExtensionError::Transport(e.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| SandboxExtensionError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(SandboxExtensionError::http(status.as_u16(), text));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| {
            SandboxExtensionError::Serialization(format!(
                "failed to decode JSON response from {url}: {e}"
            ))
        })
    }

    /// Sends a request expecting an empty response body, mapping non-2xx to an error.
    pub async fn send_empty(
        &self,
        method: Method,
        url: impl Into<String>,
        body: Option<&Value>,
    ) -> SandboxResult<()> {
        let url = url.into();
        let mut builder = self.request(method, url.clone());
        if let Some(body) = body {
            builder = builder.json(body);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| SandboxExtensionError::Transport(e.to_string()))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(SandboxExtensionError::http(status, text));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_url_handles_slashes() {
        assert_eq!(
            join_url("https://api.test", "/sandboxes"),
            "https://api.test/sandboxes"
        );
        assert_eq!(
            join_url("https://api.test/", "sandboxes"),
            "https://api.test/sandboxes"
        );
        assert_eq!(
            join_url("https://api.test/", "/sandboxes/"),
            "https://api.test/sandboxes/"
        );
    }

    #[test]
    fn default_http_options_match_java() {
        let opts = HttpOptions::default();
        assert_eq!(opts.connect_timeout_seconds, 30);
        assert_eq!(opts.read_timeout_seconds, 120);
        assert_eq!(opts.max_retries, 3);
    }

    #[test]
    fn build_client_succeeds() {
        assert!(build_client(&HttpOptions::default()).is_ok());
    }

    #[test]
    fn loopback_targets_are_recognized() {
        for value in [
            "http://127.0.0.1:8080",
            "http://127.5.6.7",
            "https://127.0.0.1",
            "http://localhost:1234",
            "http://LOCALHOST/",
            "http://localhost./x",
            "http://[::1]:8080",
            "127.0.0.1:10000",
            "localhost:49983",
        ] {
            assert!(is_loopback_target(value), "{value} should be loopback");
        }
    }

    #[test]
    fn non_loopback_targets_are_rejected() {
        for value in [
            "",
            "   ",
            "https://api.e2b.app",
            "https://kubernetes.default.svc",
            "http://127.0.0.1.example.com",
            "http://localhost.evil.test",
            "http://10.0.0.5:8080",
            "http://[::2]:8080",
            "not a url",
        ] {
            assert!(!is_loopback_target(value), "{value} should not be loopback");
        }
    }

    #[test]
    fn proxy_bypass_only_applies_to_loopback_targets() {
        // A loopback target produces a client that never proxies, so requests reach the
        // local server even when the machine has a system-wide proxy configured.
        assert!(
            bypass_proxy_for_loopback(Client::builder(), &["http://127.0.0.1:9"])
                .build()
                .is_ok()
        );
        assert!(
            bypass_proxy_for_loopback(Client::builder(), &["https://api.e2b.app", ""])
                .build()
                .is_ok()
        );
        assert!(bypass_proxy_for_loopback(Client::builder(), &[])
            .build()
            .is_ok());
    }
}
