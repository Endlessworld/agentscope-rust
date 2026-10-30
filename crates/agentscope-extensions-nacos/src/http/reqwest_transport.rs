//! `reqwest`-backed [`NacosHttpTransport`] (feature `http-client`).
//!
//! Kept behind a feature so the default build pulls in no HTTP stack: the client logic in
//! [`super`] is fully exercised offline against a fake transport.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use super::{HttpRequest, HttpResponse, NacosHttpAiService, NacosHttpTransport};

/// A [`NacosHttpTransport`] over `reqwest`.
#[derive(Debug, Clone, Default)]
pub struct ReqwestNacosTransport {
    client: reqwest::Client,
}

impl ReqwestNacosTransport {
    /// Builds a transport with a default `reqwest::Client`.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    /// Reuses an application-wide `reqwest::Client` (connection pool / proxy settings).
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
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
/// This is the HTTP stand-in for the Nacos client's gRPC push stream: subscribed AgentCards /
/// prompts are re-fetched every `interval` and changed values are pushed to the listeners.
/// Fetch errors are logged and retried on the next tick.
pub fn spawn_polling(
    service: Arc<NacosHttpAiService>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            if let Err(e) = service.poll_once().await {
                tracing::warn!(error = %e, "nacos ai service poll failed");
            }
        }
    })
}
