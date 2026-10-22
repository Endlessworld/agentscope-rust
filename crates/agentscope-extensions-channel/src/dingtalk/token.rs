//! DingTalk OpenAPI `accessToken` provider (cached, 80%-of-TTL proactive refresh).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use crate::common::{
    now_ms, refresh_at_ms, AccessTokenProvider, ChannelError, HttpTransport, PreparedRequest,
    Result, TokenCache,
};

/// Fetches/caches a DingTalk access token via `POST /v1.0/oauth2/accessToken`.
pub struct DingTalkAccessTokenProvider {
    api_base: String,
    app_key: String,
    app_secret: String,
    cache: TokenCache,
    transport: Arc<dyn HttpTransport>,
}

impl DingTalkAccessTokenProvider {
    pub fn new(
        api_base: impl Into<String>,
        app_key: impl Into<String>,
        app_secret: impl Into<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            api_base: api_base.into(),
            app_key: app_key.into(),
            app_secret: app_secret.into(),
            cache: TokenCache::new(),
            transport,
        }
    }

    /// Build the token-refresh request (pure, testable).
    pub fn build_request(&self) -> PreparedRequest {
        PreparedRequest::post_json(
            format!(
                "{}/v1.0/oauth2/accessToken",
                self.api_base.trim_end_matches('/')
            ),
            json!({ "appKey": self.app_key, "appSecret": self.app_secret }),
        )
    }

    /// Parse the response body, cache the token and return it.
    pub fn parse_and_store(&self, body: &str) -> Result<String> {
        let node: serde_json::Value = serde_json::from_str(body)?;
        let token = node
            .get("accessToken")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                ChannelError::Parse(format!(
                    "DingTalk accessToken response missing accessToken: {body}"
                ))
            })?;
        let expires_in = node
            .get("expireIn")
            .and_then(|v| v.as_i64())
            .unwrap_or(7200);
        self.cache
            .store(token.clone(), refresh_at_ms(now_ms(), expires_in));
        Ok(token)
    }
}

#[async_trait]
impl AccessTokenProvider for DingTalkAccessTokenProvider {
    async fn token(&self) -> Result<String> {
        if let Some(cached) = self.cache.get(now_ms()) {
            return Ok(cached);
        }
        let response = self.transport.execute(self.build_request()).await?;
        self.parse_and_store(&response.body)
    }

    fn invalidate(&self) {
        self.cache.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testing::RecordingTransport;
    use crate::common::PreparedResponse;

    #[test]
    fn build_request_targets_new_openapi_endpoint() {
        let p = DingTalkAccessTokenProvider::new(
            "https://api.dingtalk.com",
            "key",
            "secret",
            Arc::new(RecordingTransport::new()),
        );
        let req = p.build_request();
        assert_eq!(req.url, "https://api.dingtalk.com/v1.0/oauth2/accessToken");
        let body = req.body.unwrap();
        assert_eq!(body["appKey"], json!("key"));
        assert_eq!(body["appSecret"], json!("secret"));
    }

    #[test]
    fn parse_and_store_reads_token_and_expiry() {
        let p = DingTalkAccessTokenProvider::new(
            "https://api.dingtalk.com",
            "k",
            "s",
            Arc::new(RecordingTransport::new()),
        );
        let token = p
            .parse_and_store(r#"{"accessToken":"abc","expireIn":7200}"#)
            .unwrap();
        assert_eq!(token, "abc");
        assert_eq!(p.cache.get(now_ms()).as_deref(), Some("abc"));
    }

    #[test]
    fn parse_and_store_rejects_missing_token() {
        let p = DingTalkAccessTokenProvider::new(
            "https://api.dingtalk.com",
            "k",
            "s",
            Arc::new(RecordingTransport::new()),
        );
        assert!(p.parse_and_store("{}").is_err());
    }

    #[tokio::test]
    async fn token_is_fetched_once_then_cached() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, r#"{"accessToken":"tok-9","expireIn":7200}"#),
        ]));
        let p = DingTalkAccessTokenProvider::new(
            "https://api.dingtalk.com",
            "k",
            "s",
            transport.clone(),
        );
        assert_eq!(p.token().await.unwrap(), "tok-9");
        assert_eq!(p.token().await.unwrap(), "tok-9");
        assert_eq!(transport.request_count(), 1);
    }

    #[tokio::test]
    async fn invalidate_forces_a_refresh() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, r#"{"accessToken":"tok-1","expireIn":7200}"#),
            PreparedResponse::new(200, r#"{"accessToken":"tok-2","expireIn":7200}"#),
        ]));
        let p = DingTalkAccessTokenProvider::new(
            "https://api.dingtalk.com",
            "k",
            "s",
            transport.clone(),
        );
        assert_eq!(p.token().await.unwrap(), "tok-1");
        p.invalidate();
        assert_eq!(p.token().await.unwrap(), "tok-2");
        assert_eq!(transport.request_count(), 2);
    }
}
