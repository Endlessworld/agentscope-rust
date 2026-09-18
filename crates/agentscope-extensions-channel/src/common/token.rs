//! Access-token caching shared by the DingTalk / Feishu / WeCom adapters.
//!
//! Each Java provider caches one token and proactively refreshes at ~80 % of the TTL
//! (`refreshAt = now + expiresIn * 800ms`). [`TokenCache`] reproduces that policy; the
//! [`AccessTokenProvider`] trait is the async seam the outbound clients depend on, so tests can
//! substitute a static token without any network access.

use std::sync::Mutex;

use async_trait::async_trait;

use super::error::Result;

/// Wall-clock milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Compute the proactive-refresh deadline: 80 % of the token TTL.
///
/// Mirrors the Java `System.currentTimeMillis() + (long) (expiresIn * 800L)` expression where
/// `expiresIn` is expressed in seconds.
pub fn refresh_at_ms(now_ms: i64, expires_in_secs: i64) -> i64 {
    now_ms + expires_in_secs * 800
}

#[derive(Debug, Clone)]
struct CachedToken {
    value: String,
    refresh_at_ms: i64,
}

/// Single-entry, thread-safe token cache mirroring the Java `AtomicReference<TokenSlot>`.
#[derive(Debug, Default)]
pub struct TokenCache {
    slot: Mutex<Option<CachedToken>>,
}

impl TokenCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached token when it is still before its refresh deadline.
    pub fn get(&self, now_ms: i64) -> Option<String> {
        let slot = self.slot.lock().expect("token cache mutex poisoned");
        slot.as_ref()
            .filter(|t| t.refresh_at_ms > now_ms)
            .map(|t| t.value.clone())
    }

    /// Store a freshly-fetched token together with its refresh deadline.
    pub fn store(&self, value: impl Into<String>, refresh_at_ms: i64) {
        let mut slot = self.slot.lock().expect("token cache mutex poisoned");
        *slot = Some(CachedToken {
            value: value.into(),
            refresh_at_ms,
        });
    }

    /// Drop the cached token, forcing the next lookup to refresh.
    pub fn invalidate(&self) {
        let mut slot = self.slot.lock().expect("token cache mutex poisoned");
        *slot = None;
    }
}

/// Async access-token source used by the outbound clients.
#[async_trait]
pub trait AccessTokenProvider: Send + Sync {
    /// Return a currently-valid token, refreshing in-band when necessary.
    async fn token(&self) -> Result<String>;

    /// Invalidate the cached token (called after an auth-related provider error).
    fn invalidate(&self);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_provider_is_object_safe(p: &dyn AccessTokenProvider) {
        let _ = p;
    }

    #[test]
    fn refresh_deadline_is_80_percent_of_ttl() {
        assert_eq!(refresh_at_ms(0, 7200), 5_760_000);
        assert_eq!(refresh_at_ms(1_000, 1), 1_800);
    }

    #[test]
    fn cache_returns_token_until_refresh_deadline() {
        let cache = TokenCache::new();
        assert!(cache.get(0).is_none());
        cache.store("tok", refresh_at_ms(0, 100));
        assert_eq!(cache.get(0).as_deref(), Some("tok"));
        assert_eq!(cache.get(79_999).as_deref(), Some("tok"));
        // At the deadline the token must be considered stale.
        assert!(cache.get(80_000).is_none());
    }

    #[test]
    fn invalidate_forces_refresh() {
        let cache = TokenCache::new();
        cache.store("tok", 1_000_000);
        cache.invalidate();
        assert!(cache.get(0).is_none());
    }

    #[test]
    fn trait_is_object_safe() {
        struct Fixed(String);
        #[async_trait]
        impl AccessTokenProvider for Fixed {
            async fn token(&self) -> Result<String> {
                Ok(self.0.clone())
            }
            fn invalidate(&self) {}
        }
        let p: Box<dyn AccessTokenProvider> = Box::new(Fixed("t".to_string()));
        assert_provider_is_object_safe(p.as_ref());
    }
}
