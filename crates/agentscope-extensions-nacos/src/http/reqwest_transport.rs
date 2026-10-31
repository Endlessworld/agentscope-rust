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

/// Upper bound of the polling backoff, mirroring the model-call retry cap.
pub const MAX_POLLING_BACKOFF: Duration = Duration::from_secs(60);

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

/// The delay before the next poll after `failures` consecutive failures (NACOS-06).
///
/// A healthy service polls every `interval`; a failing one backs off exponentially and is
/// capped at [`MAX_POLLING_BACKOFF`], so a broken connection does not hammer the server. Kept
/// as a pure function so the schedule is unit-testable without sleeping.
pub fn polling_backoff(interval: Duration, failures: u32) -> Duration {
    crate::service::exponential_backoff(interval, failures, MAX_POLLING_BACKOFF)
}

/// Drives [`NacosHttpAiService::poll_once`] on a timer until the handle is aborted.
///
/// This is the HTTP stand-in for the Nacos client's gRPC push stream: subscribed AgentCards /
/// prompts are re-fetched every `interval` and changed values are pushed to the listeners.
/// Fetch errors are logged and retried with exponential backoff (NACOS-06).
pub fn spawn_polling(
    service: Arc<NacosHttpAiService>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut failures = 0u32;
        loop {
            match service.poll_once().await {
                Ok(_) => failures = 0,
                Err(error) => {
                    failures = failures.saturating_add(1);
                    tracing::warn!(
                        error = %error,
                        failures,
                        "nacos ai service poll failed"
                    );
                }
            }
            let delay = polling_backoff(interval, failures);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
        }
    })
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

    #[test]
    fn polling_backs_off_exponentially_until_the_cap() {
        let interval = Duration::from_secs(1);
        assert_eq!(polling_backoff(interval, 0), interval);
        assert_eq!(polling_backoff(interval, 1), Duration::from_secs(2));
        assert_eq!(polling_backoff(interval, 3), Duration::from_secs(8));
        // Capped, so a long outage stops growing the delay.
        assert_eq!(polling_backoff(interval, 40), MAX_POLLING_BACKOFF);
        // A sub-second interval is still honoured while healthy.
        let fast = Duration::from_millis(5);
        assert_eq!(polling_backoff(fast, 0), fast);
        assert_eq!(polling_backoff(fast, 1), Duration::from_millis(10));
    }
}
