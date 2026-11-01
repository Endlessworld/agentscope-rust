//! `reqwest`-backed [`NacosHttpTransport`] (feature `http-client`).
//!
//! Kept behind a feature so the default build pulls in no HTTP stack: the client logic in
//! [`super`] is fully exercised offline against a fake transport.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use super::{HttpRequest, HttpResponse, NacosHttpAiService, NacosHttpTransport};

/// Request timeout applied unless the embedder overrides it (NACOS-06).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A [`NacosHttpTransport`] over `reqwest`.
#[derive(Debug, Clone)]
pub struct ReqwestNacosTransport {
    client: reqwest::Client,
    timeout: Option<Duration>,
}

impl Default for ReqwestNacosTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestNacosTransport {
    /// Builds a transport with a default `reqwest::Client` and [`DEFAULT_REQUEST_TIMEOUT`].
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            timeout: Some(DEFAULT_REQUEST_TIMEOUT),
        }
    }

    /// Reuses an application-wide `reqwest::Client` (connection pool / proxy settings).
    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client,
            timeout: Some(DEFAULT_REQUEST_TIMEOUT),
        }
    }

    /// Sets the per-request timeout (NACOS-06).
    ///
    /// Applied on every request rather than on the client, so it also works on top of a
    /// caller-supplied client.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Disables the per-request timeout (the client's own timeout still applies).
    pub fn without_timeout(mut self) -> Self {
        self.timeout = None;
        self
    }

    /// The configured per-request timeout.
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// The underlying client.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }
}

#[async_trait]
impl NacosHttpTransport for ReqwestNacosTransport {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, String> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|e| format!("unsupported http method `{}`: {e}", request.method))?;
        let mut builder = self.client.request(method, &request.url);
        if let Some(timeout) = self.timeout {
            builder = builder.timeout(timeout);
        }
        if let Some(body) = request.body {
            if let Some(content_type) = request.content_type {
                builder = builder.header(reqwest::header::CONTENT_TYPE, content_type);
            }
            builder = builder.body(body);
        }
        let response = builder.send().await.map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let body = response.bytes().await.map_err(|e| e.to_string())?.to_vec();
        Ok(HttpResponse::new(status, body))
    }
}

/// Drives [`NacosHttpAiService::poll_once`] on a timer until the handle is aborted.
///
/// Thin wrapper over [`NacosHttpAiService::polling`], kept for callers that only hold this free
/// function; the method itself needs no HTTP stack and is available in the default build too.
pub fn spawn_polling(
    service: Arc<NacosHttpAiService>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    service.polling(interval)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timeout_is_applied_by_default_and_can_be_overridden() {
        assert_eq!(
            ReqwestNacosTransport::new().timeout(),
            Some(DEFAULT_REQUEST_TIMEOUT)
        );
        let custom = ReqwestNacosTransport::new().with_timeout(Duration::from_millis(250));
        assert_eq!(custom.timeout(), Some(Duration::from_millis(250)));
        assert_eq!(ReqwestNacosTransport::new().without_timeout().timeout(), None);
    }
}
