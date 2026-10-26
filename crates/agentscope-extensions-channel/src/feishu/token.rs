//! Feishu `tenant_access_token` provider (cached, 80%-of-TTL proactive refresh).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use crate::common::{
    now_ms, refresh_at_ms, AccessTokenProvider, AccessTokenStore, CachedAccessToken, ChannelError,
    HttpTransport, InMemoryAccessTokenStore, PreparedRequest, Result,
};

/// Fetches/caches a Feishu tenant access token.
pub struct FeishuAccessTokenProvider {
    api_base: String,
    app_id: String,
    app_secret: String,
    store: Arc<dyn AccessTokenStore>,
    transport: Arc<dyn HttpTransport>,
}

impl FeishuAccessTokenProvider {
    pub fn new(
        api_base: impl Into<String>,
        app_id: impl Into<String>,
        app_secret: impl Into<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self::with_store(
            api_base,
            app_id,
            app_secret,
            transport,
            Arc::new(InMemoryAccessTokenStore::new()),
        )
    }

    /// Constructor variant that lets the application choose where the token is cached — for
    /// example a shared-storage [`AccessTokenStore`] so one refresh or invalidation serves all
    /// instances. Mirrors the Java `FeishuAccessTokenProvider(..., AccessTokenStore)` overload.
    pub fn with_store(
        api_base: impl Into<String>,
        app_id: impl Into<String>,
        app_secret: impl Into<String>,
        transport: Arc<dyn HttpTransport>,
        store: Arc<dyn AccessTokenStore>,
    ) -> Self {
        Self {
            api_base: api_base.into(),
            app_id: app_id.into(),
            app_secret: app_secret.into(),
            store,
            transport,
        }
    }

    /// Build the token-refresh request (pure, testable).
    pub fn build_request(&self) -> PreparedRequest {
        PreparedRequest::post_json(
            format!(
                "{}/open-apis/auth/v3/tenant_access_token/internal",
                self.api_base.trim_end_matches('/')
            ),
            json!({ "app_id": self.app_id, "app_secret": self.app_secret }),
        )
    }

    /// Parse the response body, cache the token and return it.
    pub fn parse_and_store(&self, body: &str) -> Result<String> {
        let node: serde_json::Value = serde_json::from_str(body)?;
        let code = node.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
        if code != 0 {
            return Err(ChannelError::Parse(format!(
                "Feishu tenant_access_token failed: code={code}, msg={}",
                node.get("msg").and_then(|v| v.as_str()).unwrap_or("")
            )));
        }
        let token = node
            .get("tenant_access_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let expires_in = node.get("expire").and_then(|v| v.as_i64()).unwrap_or(7200);
        self.store.put_if_newer(CachedAccessToken::new(
            token.clone(),
            refresh_at_ms(now_ms(), expires_in),
        ));
        Ok(token)
    }
}

#[async_trait]
impl AccessTokenProvider for FeishuAccessTokenProvider {
    async fn token(&self) -> Result<String> {
        if let Some(cached) = self.store.get() {
            if cached.refresh_at_ms > now_ms() {
                return Ok(cached.value);
            }
        }
        let response = self.transport.execute(self.build_request()).await?;
        self.parse_and_store(&response.body)
    }

    fn invalidate(&self) {
        self.store.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testing::RecordingTransport;
    use crate::common::PreparedResponse;

    #[test]
    fn build_request_matches_feishu_internal_endpoint() {
        let p = FeishuAccessTokenProvider::new(
            "https://open.feishu.cn",
            "cli_x",
            "s",
            Arc::new(RecordingTransport::new()),
        );
        let req = p.build_request();
        assert_eq!(
            req.url,
            "https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal"
        );
        let body = req.body.unwrap();
        assert_eq!(body["app_id"], json!("cli_x"));
        assert_eq!(body["app_secret"], json!("s"));
    }

    #[test]
    fn parse_reads_token_and_flags_nonzero_code() {
        let p = FeishuAccessTokenProvider::new(
            "https://open.feishu.cn",
            "cli_x",
            "s",
            Arc::new(RecordingTransport::new()),
        );
        assert_eq!(
            p.parse_and_store(r#"{"code":0,"tenant_access_token":"t-1","expire":7200}"#)
                .unwrap(),
            "t-1"
        );
        assert!(p
            .parse_and_store(r#"{"code":10003,"msg":"bad"}"#)
            .is_err());
    }

    #[tokio::test]
    async fn token_is_cached_until_invalidated() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, r#"{"code":0,"tenant_access_token":"t-1","expire":7200}"#),
            PreparedResponse::new(200, r#"{"code":0,"tenant_access_token":"t-2","expire":7200}"#),
        ]));
        let p = FeishuAccessTokenProvider::new("https://open.feishu.cn", "cli", "s", transport.clone());
        assert_eq!(p.token().await.unwrap(), "t-1");
        assert_eq!(p.token().await.unwrap(), "t-1");
        p.invalidate();
        assert_eq!(p.token().await.unwrap(), "t-2");
        assert_eq!(transport.request_count(), 2);
    }

    #[tokio::test]
    async fn an_injected_store_is_shared_across_providers() {
        let store: Arc<dyn AccessTokenStore> = Arc::new(InMemoryAccessTokenStore::new());
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, r#"{"code":0,"tenant_access_token":"t-1","expire":7200}"#),
        ]));
        let a = FeishuAccessTokenProvider::with_store(
            "https://open.feishu.cn",
            "cli",
            "s",
            transport.clone(),
            store.clone(),
        );
        let b = FeishuAccessTokenProvider::with_store(
            "https://open.feishu.cn",
            "cli",
            "s",
            transport.clone(),
            store.clone(),
        );
        assert_eq!(a.token().await.unwrap(), "t-1");
        // The second provider reads the shared slot instead of hitting the network again.
        assert_eq!(b.token().await.unwrap(), "t-1");
        assert_eq!(transport.request_count(), 1);
        // Invalidation through one provider clears the shared slot.
        b.invalidate();
        assert!(store.get().is_none());
    }
}
