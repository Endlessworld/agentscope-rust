//! Feishu `tenant_access_token` provider (cached, 80%-of-TTL proactive refresh).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;

use crate::common::http::{apply_response_timeout, ensure_positive_timeout};
use crate::common::{
    now_ms, refresh_at_ms, AccessTokenProvider, AccessTokenStore, CachedAccessToken, ChannelError,
    HttpTransport, InMemoryAccessTokenStore, PreparedRequest, Result,
};

/// Default token-refresh timeout, mirroring the Java
/// `FeishuAccessTokenProvider.DEFAULT_RESPONSE_TIMEOUT = Duration.ofSeconds(10)`.
pub const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Fetches/caches a Feishu tenant access token.
pub struct FeishuAccessTokenProvider {
    api_base: String,
    app_id: String,
    app_secret: String,
    store: Arc<dyn AccessTokenStore>,
    transport: Arc<dyn HttpTransport>,
    response_timeout: Duration,
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
    ///
    /// The response timeout defaults to [`DEFAULT_RESPONSE_TIMEOUT`]; chain
    /// [`Self::with_response_timeout`] to override it.
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
            response_timeout: DEFAULT_RESPONSE_TIMEOUT,
        }
    }

    /// Override the token-refresh timeout (Java's `responseTimeout` constructor parameter).
    ///
    /// Returns [`ChannelError::Config`] for a zero duration, mirroring the Java
    /// `"responseTimeout must be positive"` guard. When the injected transport is a
    /// [`crate::common::ReqwestTransport`] (or any transport that supports reconfiguration) the
    /// timeout is applied to it; otherwise it is recorded and left to the caller's transport.
    pub fn with_response_timeout(mut self, timeout: Duration) -> Result<Self> {
        ensure_positive_timeout(timeout)?;
        self.transport = apply_response_timeout(self.transport.clone(), timeout);
        self.response_timeout = timeout;
        Ok(self)
    }

    /// The configured token-refresh timeout.
    pub fn response_timeout(&self) -> Duration {
        self.response_timeout
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
    use crate::common::testing::{RecordingTransport, TimeoutAwareTransport};
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
        assert!(p.parse_and_store(r#"{"code":10003,"msg":"bad"}"#).is_err());
    }

    #[tokio::test]
    async fn token_is_cached_until_invalidated() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(
                200,
                r#"{"code":0,"tenant_access_token":"t-1","expire":7200}"#,
            ),
            PreparedResponse::new(
                200,
                r#"{"code":0,"tenant_access_token":"t-2","expire":7200}"#,
            ),
        ]));
        let p =
            FeishuAccessTokenProvider::new("https://open.feishu.cn", "cli", "s", transport.clone());
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
            PreparedResponse::new(
                200,
                r#"{"code":0,"tenant_access_token":"t-1","expire":7200}"#,
            ),
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

    #[tokio::test]
    async fn an_expired_cache_entry_is_refetched() {
        // A cached token whose proactive-refresh deadline has passed must not be reused: this is the
        // "token expired / re-fetch" path, exercised without any network access.
        let store: Arc<dyn AccessTokenStore> = Arc::new(InMemoryAccessTokenStore::new());
        store.put(CachedAccessToken::new("stale", now_ms() - 1));
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(
                200,
                r#"{"code":0,"tenant_access_token":"t-fresh","expire":7200}"#,
            ),
        ]));
        let p = FeishuAccessTokenProvider::with_store(
            "https://open.feishu.cn",
            "cli",
            "s",
            transport.clone(),
            store.clone(),
        );
        assert_eq!(p.token().await.unwrap(), "t-fresh");
        assert_eq!(transport.request_count(), 1);
        // The re-fetched token is usable immediately: its deadline is 80 % of the TTL out.
        let cached = store
            .get()
            .expect("the refresh must write through the store");
        assert_eq!(cached.value, "t-fresh");
        assert!(cached.refresh_at_ms > now_ms());
        assert_eq!(p.token().await.unwrap(), "t-fresh");
        assert_eq!(transport.request_count(), 1);
    }

    #[test]
    fn default_response_timeout_is_ten_seconds() {
        let p = FeishuAccessTokenProvider::new(
            "https://open.feishu.cn",
            "cli_x",
            "s",
            Arc::new(RecordingTransport::new()),
        );
        assert_eq!(p.response_timeout(), DEFAULT_RESPONSE_TIMEOUT);
        assert_eq!(DEFAULT_RESPONSE_TIMEOUT, Duration::from_secs(10));
    }

    #[test]
    fn explicit_response_timeout_is_applied_to_a_reconfigurable_transport() {
        let transport = Arc::new(TimeoutAwareTransport::new());
        let p = FeishuAccessTokenProvider::new(
            "https://open.feishu.cn",
            "cli_x",
            "s",
            transport.clone(),
        )
        .with_response_timeout(Duration::from_millis(1500))
        .unwrap();
        assert_eq!(p.response_timeout(), Duration::from_millis(1500));
        assert_eq!(transport.timeout(), Some(Duration::from_millis(1500)));
    }

    #[test]
    fn explicit_response_timeout_is_recorded_for_a_non_reconfigurable_transport() {
        let transport = Arc::new(RecordingTransport::new());
        let p = FeishuAccessTokenProvider::new("https://open.feishu.cn", "cli_x", "s", transport)
            .with_response_timeout(Duration::from_secs(4))
            .unwrap();
        assert_eq!(p.response_timeout(), Duration::from_secs(4));
    }

    #[test]
    fn zero_response_timeout_is_rejected() {
        let p = FeishuAccessTokenProvider::new(
            "https://open.feishu.cn",
            "cli_x",
            "s",
            Arc::new(RecordingTransport::new()),
        );
        let err = p
            .with_response_timeout(Duration::ZERO)
            .err()
            .expect("zero must be rejected");
        assert!(
            err.to_string().contains("responseTimeout must be positive"),
            "unexpected message: {err}"
        );
    }
}
