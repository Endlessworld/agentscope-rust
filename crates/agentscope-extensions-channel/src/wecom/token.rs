//! WeCom `access_token` provider (cached, 80%-of-TTL proactive refresh).

use std::sync::Arc;

use async_trait::async_trait;

use crate::common::{
    now_ms, refresh_at_ms, AccessTokenProvider, ChannelError, HttpTransport, PreparedRequest,
    Result, TokenCache,
};

/// Fetches/caches a WeCom access token via `GET /cgi-bin/gettoken`.
pub struct WeComAccessTokenProvider {
    api_base: String,
    corp_id: String,
    secret: String,
    cache: TokenCache,
    transport: Arc<dyn HttpTransport>,
}

impl WeComAccessTokenProvider {
    pub fn new(
        api_base: impl Into<String>,
        corp_id: impl Into<String>,
        secret: impl Into<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            api_base: api_base.into(),
            corp_id: corp_id.into(),
            secret: secret.into(),
            cache: TokenCache::new(),
            transport,
        }
    }

    /// Build the token-refresh request (pure, testable).
    pub fn build_request(&self) -> PreparedRequest {
        PreparedRequest::get(format!(
            "{}/cgi-bin/gettoken?corpid={}&corpsecret={}",
            self.api_base.trim_end_matches('/'),
            self.corp_id,
            self.secret
        ))
    }

    /// Parse the response body, cache the token and return it.
    pub fn parse_and_store(&self, body: &str) -> Result<String> {
        let node: serde_json::Value = serde_json::from_str(body)?;
        let errcode = node.get("errcode").and_then(|v| v.as_i64()).unwrap_or(0);
        if errcode != 0 {
            return Err(ChannelError::Parse(format!(
                "WeCom gettoken failed: errcode={errcode}, errmsg={}",
                node.get("errmsg").and_then(|v| v.as_str()).unwrap_or("")
            )));
        }
        let token = node
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let expires_in = node.get("expires_in").and_then(|v| v.as_i64()).unwrap_or(7200);
        self.cache.store(token.clone(), refresh_at_ms(now_ms(), expires_in));
        Ok(token)
    }
}

#[async_trait]
impl AccessTokenProvider for WeComAccessTokenProvider {
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
    use crate::common::testing::{RecordingTransport, StaticTokenProvider};
    use crate::common::{HttpMethod, PreparedResponse};

    #[test]
    fn build_request_uses_gettoken_with_query_params() {
        let p = WeComAccessTokenProvider::new(
            "https://qyapi.weixin.qq.com",
            "corp-1",
            "sec",
            Arc::new(RecordingTransport::new()),
        );
        let req = p.build_request();
        assert_eq!(req.method, HttpMethod::Get);
        assert_eq!(
            req.url,
            "https://qyapi.weixin.qq.com/cgi-bin/gettoken?corpid=corp-1&corpsecret=sec"
        );
    }

    #[test]
    fn parse_reads_token_and_flags_errcodes() {
        let p = WeComAccessTokenProvider::new(
            "https://qyapi.weixin.qq.com",
            "corp",
            "sec",
            Arc::new(RecordingTransport::new()),
        );
        assert_eq!(
            p.parse_and_store(r#"{"errcode":0,"access_token":"t-1","expires_in":7200}"#)
                .unwrap(),
            "t-1"
        );
        assert!(p
            .parse_and_store(r#"{"errcode":40001,"errmsg":"bad secret"}"#)
            .is_err());
    }

    #[tokio::test]
    async fn token_is_cached_until_invalidated() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, r#"{"errcode":0,"access_token":"t-1","expires_in":7200}"#),
            PreparedResponse::new(200, r#"{"errcode":0,"access_token":"t-2","expires_in":7200}"#),
        ]));
        let p = WeComAccessTokenProvider::new("https://qyapi.weixin.qq.com", "corp", "sec", transport.clone());
        assert_eq!(p.token().await.unwrap(), "t-1");
        assert_eq!(p.token().await.unwrap(), "t-1");
        p.invalidate();
        assert_eq!(p.token().await.unwrap(), "t-2");
        assert_eq!(transport.request_count(), 2);
    }
}
