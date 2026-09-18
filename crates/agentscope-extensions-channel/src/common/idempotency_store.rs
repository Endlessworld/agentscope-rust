//! Bounded, TTL-expiring de-duplication store for inbound webhook events.
//!
//! Rust port of `io.agentscope.extensions.channel.common.IdempotencyStore`. Webhook providers
//! (notably WeCom) retry the same message id under failure; this store de-duplicates by key.
//! The backing map is bounded: when full, entries older than the TTL are swept and, if still
//! over budget, the oldest insertion-order entry is evicted.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Mutex;

use super::error::{ChannelError, Result};
use super::token::now_ms;

/// Default TTL: 5 minutes — sufficient for any single channel's retry burst.
pub const DEFAULT_TTL_MS: i64 = 5 * 60_000;
/// Default capacity: 10k entries.
pub const DEFAULT_MAX_ENTRIES: usize = 10_000;

#[derive(Debug, Clone, Copy)]
struct Entry {
    seen_at_ms: i64,
    /// Monotonic insertion sequence, used for oldest-first eviction.
    seq: u64,
}

/// In-memory, bounded, TTL-based idempotency store.
///
/// The design mirrors the Java class; an injectable clock
/// ([`IdempotencyStore::first_seen_at`]) keeps tests fully deterministic.
#[derive(Debug)]
pub struct IdempotencyStore {
    ttl_ms: i64,
    max_entries: usize,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    entries: HashMap<String, Entry>,
    /// Insertion order of keys still present, oldest first.
    order: VecDeque<String>,
    next_seq: u64,
}

impl Default for IdempotencyStore {
    fn default() -> Self {
        Self {
            ttl_ms: DEFAULT_TTL_MS,
            max_entries: DEFAULT_MAX_ENTRIES,
            inner: Mutex::new(Inner::default()),
        }
    }
}

impl IdempotencyStore {
    /// Store with the default 5-minute TTL and 10k capacity.
    pub fn with_defaults() -> Self {
        Self::default()
    }

    /// Store with an explicit TTL/capacity. Mirrors the Java constructor validation.
    pub fn new(ttl_ms: i64, max_entries: usize) -> Result<Self> {
        if ttl_ms <= 0 {
            return Err(ChannelError::Config(
                "idempotency ttl must be positive".to_string(),
            ));
        }
        if max_entries == 0 {
            return Err(ChannelError::Config(
                "idempotency max_entries must be positive".to_string(),
            ));
        }
        Ok(Self {
            ttl_ms,
            max_entries,
            inner: Mutex::new(Inner::default()),
        })
    }

    /// Record `key`. Returns `true` when this is the first sighting inside the TTL
    /// (the caller should proceed), `false` when it is a duplicate.
    pub fn first_seen(&self, key: &str) -> bool {
        self.first_seen_at(key, now_ms())
    }

    /// [`IdempotencyStore::first_seen`] with an injected clock (offline tests).
    pub fn first_seen_at(&self, key: &str, now_ms: i64) -> bool {
        // Java returns `true` for null keys; an empty key carries no identity, so treat it the
        // same way and never let it consume capacity.
        if key.is_empty() {
            return true;
        }
        let mut inner = self.inner.lock().expect("idempotency store mutex poisoned");
        inner.sweep(now_ms, self.ttl_ms, self.max_entries);

        if let Some(existing) = inner.entries.get(key) {
            return now_ms - existing.seen_at_ms > self.ttl_ms;
        }
        let seq = inner.next_seq;
        inner.next_seq += 1;
        inner.entries.insert(
            key.to_string(),
            Entry {
                seen_at_ms: now_ms,
                seq,
            },
        );
        inner.order.push_back(key.to_string());
        true
    }

    /// Current number of tracked keys (observability / tests).
    pub fn size(&self) -> usize {
        self.inner
            .lock()
            .expect("idempotency store mutex poisoned")
            .entries
            .len()
    }
}

impl Inner {
    fn sweep(&mut self, now_ms: i64, ttl_ms: i64, max_entries: usize) {
        if self.entries.len() < max_entries {
            return;
        }
        self.entries.retain(|_, e| now_ms - e.seen_at_ms <= ttl_ms);
        // Rebuild the insertion order against the surviving entries.
        let mut surviving: BTreeMap<u64, String> = BTreeMap::new();
        for (key, entry) in &self.entries {
            surviving.insert(entry.seq, key.clone());
        }
        self.order = surviving.into_values().collect();

        // Hard cap: evict oldest-first until there is room for one more key.
        while self.entries.len() >= max_entries {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.entries.remove(&oldest);
                }
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_bounds_are_rejected() {
        assert!(IdempotencyStore::new(0, 10).is_err());
        assert!(IdempotencyStore::new(1_000, 0).is_err());
        assert!(IdempotencyStore::new(1_000, 10).is_ok());
    }

    #[test]
    fn first_sighting_is_new_and_second_is_duplicate() {
        let store = IdempotencyStore::new(60_000, 100).unwrap();
        assert!(store.first_seen_at("msg-1", 1_000));
        assert!(!store.first_seen_at("msg-1", 1_001));
        assert!(store.first_seen_at("msg-2", 1_002));
        assert_eq!(store.size(), 2);
    }

    #[test]
    fn key_is_treated_as_new_again_after_ttl() {
        let store = IdempotencyStore::new(1_000, 100).unwrap();
        assert!(store.first_seen_at("msg-1", 0));
        assert!(!store.first_seen_at("msg-1", 1_000));
        // Strictly greater than the TTL -> the prior sighting is expired.
        assert!(store.first_seen_at("msg-1", 1_001));
    }

    #[test]
    fn empty_key_is_always_new_and_not_stored() {
        let store = IdempotencyStore::new(1_000, 100).unwrap();
        assert!(store.first_seen_at("", 0));
        assert!(store.first_seen_at("", 0));
        assert_eq!(store.size(), 0);
    }

    #[test]
    fn capacity_is_bounded_by_evicting_oldest() {
        let store = IdempotencyStore::new(60_000, 3).unwrap();
        assert!(store.first_seen_at("a", 0));
        assert!(store.first_seen_at("b", 1));
        assert!(store.first_seen_at("c", 2));
        // Full: the sweep cannot expire anything, so the oldest ("a") is evicted.
        assert!(store.first_seen_at("d", 3));
        assert_eq!(store.size(), 3);
        // "a" was evicted, so it is considered new again — but the lookup itself runs the sweep
        // first (Java `sweep` precedes `putIfAbsent`), dropping the then-oldest ("b").
        assert!(store.first_seen_at("a", 4));
        assert!(store.first_seen_at("b", 4));
        assert_eq!(store.size(), 3);
    }
}
