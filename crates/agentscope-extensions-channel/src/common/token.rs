//! Access-token caching shared by the DingTalk / Feishu / WeCom adapters.
//!
//! Each Java provider caches one token, proactively refreshes at ~80 % of the TTL
//! (`refreshAt = now + expiresIn * 800ms`) and writes through an injectable [`AccessTokenStore`].
//! The store is an opaque single-credential cache slot (one instance per credential), so a
//! multi-instance deployment can inject a shared-storage implementation and have one refresh / one
//! invalidation serve every instance. [`AccessTokenProvider`] is the async seam the outbound
//! clients depend on, so tests can substitute a static token without any network access.

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

/// A platform access token together with the refresh deadline chosen by the provider that fetched
/// it. Rust port of the Java `CachedAccessToken` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedAccessToken {
    /// The token string accepted by the platform API; never empty — absence of a token is
    /// represented by [`AccessTokenStore::get`] returning `None`.
    pub value: String,
    /// Wall-clock epoch milliseconds after which the owning provider fetches a new token instead of
    /// returning this one.
    pub refresh_at_ms: i64,
}

impl CachedAccessToken {
    pub fn new(value: impl Into<String>, refresh_at_ms: i64) -> Self {
        Self {
            value: value.into(),
            refresh_at_ms,
        }
    }
}

/// Caches the platform access token of **one** channel credential.
///
/// Rust port of the Java `AccessTokenStore` interface. The provider owns the policy (refresh
/// timing, response parsing, error-code invalidation) and treats the store as an opaque cache
/// slot: one store instance serves one credential, and the store is not keyed, so an instance
/// shared across providers built from different credentials would silently mix their tokens.
///
/// Implementations must be thread-safe. Concurrent refreshes are expected (there is no
/// cross-instance lock), so providers store fetched tokens with [`AccessTokenStore::put_if_newer`]
/// — racing refreshes cannot then regress the cache to an earlier refresh deadline.
pub trait AccessTokenStore: Send + Sync {
    /// The last stored token, or `None` when nothing is cached. Entries are returned regardless of
    /// age — the owning provider applies the refresh deadline carried in
    /// [`CachedAccessToken::refresh_at_ms`].
    fn get(&self) -> Option<CachedAccessToken>;

    /// Cache `token`, replacing any previous entry unconditionally.
    fn put(&self, token: CachedAccessToken);

    /// Cache `token` only when it is fresher than the entry currently held (strictly later
    /// [`CachedAccessToken::refresh_at_ms`]). The default implementation is a non-atomic
    /// read-compare-write; implementations backed by shared storage should override it with an
    /// atomic conditional write (for example a compare-and-set script).
    fn put_if_newer(&self, token: CachedAccessToken) -> bool {
        if self
            .get()
            .map(|current| token.refresh_at_ms <= current.refresh_at_ms)
            .unwrap_or(false)
        {
            return false;
        }
        self.put(token);
        true
    }

    /// Drop the cached token so the owning provider fetches a fresh one on its next call.
    fn clear(&self);
}

/// In-process [`AccessTokenStore`] holding a single token entry; Rust port of the Java
/// `InMemoryAccessTokenStore`. [`AccessTokenStore::put_if_newer`] is merged atomically (freshest
/// wins) under one lock, mirroring the Java `AtomicReference` compare-and-set loop.
#[derive(Debug, Default)]
pub struct InMemoryAccessTokenStore {
    slot: Mutex<Option<CachedAccessToken>>,
}

impl InMemoryAccessTokenStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl AccessTokenStore for InMemoryAccessTokenStore {
    fn get(&self) -> Option<CachedAccessToken> {
        self.slot
            .lock()
            .expect("token store mutex poisoned")
            .clone()
    }

    fn put(&self, token: CachedAccessToken) {
        *self.slot.lock().expect("token store mutex poisoned") = Some(token);
    }

    fn put_if_newer(&self, token: CachedAccessToken) -> bool {
        let mut slot = self.slot.lock().expect("token store mutex poisoned");
        if let Some(current) = slot.as_ref() {
            if token.refresh_at_ms <= current.refresh_at_ms {
                return false;
            }
        }
        *slot = Some(token);
        true
    }

    fn clear(&self) {
        *self.slot.lock().expect("token store mutex poisoned") = None;
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
    fn in_memory_store_round_trips_and_clears() {
        let store = InMemoryAccessTokenStore::new();
        assert_eq!(store.get(), None);
        store.put(CachedAccessToken::new("tok", 80_000));
        assert_eq!(store.get(), Some(CachedAccessToken::new("tok", 80_000)));
        store.clear();
        assert_eq!(store.get(), None);
    }

    #[test]
    fn put_if_newer_keeps_the_latest_deadline() {
        let store = InMemoryAccessTokenStore::new();
        assert!(store.put_if_newer(CachedAccessToken::new("a", 100)));
        // Strictly newer wins.
        assert!(store.put_if_newer(CachedAccessToken::new("b", 200)));
        assert_eq!(store.get().unwrap().value, "b");
        // An older (or equal) deadline is rejected: a racing refresh cannot regress the cache.
        assert!(!store.put_if_newer(CachedAccessToken::new("c", 200)));
        assert!(!store.put_if_newer(CachedAccessToken::new("d", 50)));
        assert_eq!(store.get().unwrap().value, "b");
    }

    #[test]
    fn default_put_if_newer_mirrors_the_in_memory_semantics() {
        #[derive(Default)]
        struct NaiveStore(Mutex<Option<CachedAccessToken>>);
        impl AccessTokenStore for NaiveStore {
            fn get(&self) -> Option<CachedAccessToken> {
                self.0.lock().unwrap().clone()
            }
            fn put(&self, token: CachedAccessToken) {
                *self.0.lock().unwrap() = Some(token);
            }
            fn clear(&self) {
                *self.0.lock().unwrap() = None;
            }
        }
        let store = NaiveStore::default();
        assert!(store.put_if_newer(CachedAccessToken::new("a", 100)));
        assert!(!store.put_if_newer(CachedAccessToken::new("b", 100)));
        assert!(store.put_if_newer(CachedAccessToken::new("c", 101)));
        assert_eq!(store.get().unwrap().value, "c");
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
