//! Account-scoped state for at-least-once Weixin message processing.
//!
//! Rust port of `io.agentscope.extensions.channel.weixin.WeixinStateStore` plus its reference
//! `InMemoryWeixinStateStore`. The contract is deliberately one object rather than several maps:
//! cursor, peer context tokens, inbox claims and lease fencing are read and written under the same
//! lock, so a mutation either happens with a valid lease or not at all.
//!
//! ## Fencing rules (mirrored from Java, pinned by tests)
//!
//! - a released or expired lease never reuses its generation, even for the same holder;
//! - a claim is only finished by the exact, unexpired claim token that took it;
//! - accepted batches are deduplicated by message id, so a re-delivered batch cannot resurrect a
//!   completed (or abandoned) message;
//! - read-only lookups never materialise an account.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::common::{now_ms, ChannelError, Result};

/// Tombstone retention for completed / abandoned messages (7 days), as in Java.
pub const RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Opaque account lease token used to fence stale Weixin consumers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WeixinLease {
    pub holder_id: String,
    /// Monotonic per-account generation; always positive.
    pub generation: u64,
}

impl WeixinLease {
    /// Build a lease, mirroring the Java record's validation.
    pub fn new(holder_id: impl Into<String>, generation: u64) -> Result<Self> {
        let holder_id = holder_id.into();
        if holder_id.trim().is_empty() {
            return Err(ChannelError::Config(
                "holderId must not be blank".to_string(),
            ));
        }
        if generation == 0 {
            return Err(ChannelError::Config(
                "generation must be positive".to_string(),
            ));
        }
        Ok(Self {
            holder_id,
            generation,
        })
    }
}

/// Provider message to be accepted atomically with the next cursor.
#[derive(Clone, PartialEq, Eq)]
pub struct WeixinInboxMessage {
    pub message_id: String,
    pub payload: String,
}

impl WeixinInboxMessage {
    /// Build a batch entry, rejecting a blank message id.
    pub fn new(message_id: impl Into<String>, payload: impl Into<String>) -> Result<Self> {
        let message_id = message_id.into();
        if message_id.trim().is_empty() {
            return Err(ChannelError::Config(
                "messageId must not be blank".to_string(),
            ));
        }
        Ok(Self {
            message_id,
            payload: payload.into(),
        })
    }
}

/// Redacted so a payload (which carries the peer's message and its context token) never reaches a
/// log line through an accidental `{:?}`.
impl std::fmt::Debug for WeixinInboxMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WeixinInboxMessage[payload=<redacted>]")
    }
}

/// One delivery attempt; `claim_id` prevents a late completion from finishing a newer attempt.
#[derive(Clone, PartialEq, Eq)]
pub struct WeixinInboxClaim {
    pub message_id: String,
    pub payload: String,
    pub claim_id: String,
}

impl std::fmt::Debug for WeixinInboxClaim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WeixinInboxClaim[payload=<redacted>]")
    }
}

/// Account-scoped state for at-least-once message processing.
///
/// Implementations must atomically validate the unexpired lease on every mutation. External agent
/// and provider side effects are not part of this transaction and can be repeated after a crash.
pub trait WeixinStateStore: Send + Sync {
    /// The provider cursor (`get_updates_buf`) for `account_id`.
    fn load_cursor(&self, account_id: &str) -> String;

    /// The reply routing token stored for `peer_id`, if any.
    fn load_context_token(&self, account_id: &str, peer_id: &str) -> Option<String>;

    /// Store the context token for `peer_id` (no-op for a blank token).
    fn save_context_token(
        &self,
        account_id: &str,
        lease: &WeixinLease,
        peer_id: &str,
        token: &str,
    ) -> bool;

    /// Acquire or extend the account lease. Returns `None` while a different holder has a live
    /// lease; a released or expired lease always bumps the generation.
    fn acquire_lease(
        &self,
        account_id: &str,
        holder_id: &str,
        lease_ms: i64,
    ) -> Option<WeixinLease>;

    /// Extend the lease when it is still current.
    fn renew_lease(&self, account_id: &str, lease: &WeixinLease, lease_ms: i64) -> bool;

    /// Whether `lease` is the account's unexpired lease.
    fn is_lease_current(&self, account_id: &str, lease: &WeixinLease) -> bool;

    /// Release the lease (a later acquisition gets a new generation).
    fn release_lease(&self, account_id: &str, lease: &WeixinLease);

    /// Save the whole batch and its cursor in one transaction, deduplicating message ids.
    fn accept_batch(
        &self,
        account_id: &str,
        lease: &WeixinLease,
        next_cursor: &str,
        messages: &[WeixinInboxMessage],
    ) -> bool;

    /// Claim in reception order. A new lease may immediately recover the previous owner's work.
    fn claim_messages(
        &self,
        account_id: &str,
        lease: &WeixinLease,
        limit: usize,
        claim_ms: i64,
    ) -> Vec<WeixinInboxClaim>;

    /// Only the exact, unexpired claim may finish. Completed payloads must be removed.
    fn complete_message(
        &self,
        account_id: &str,
        lease: &WeixinLease,
        claim: &WeixinInboxClaim,
    ) -> bool;

    /// Return a claim to the inbox so it can be retried.
    fn fail_message(&self, account_id: &str, lease: &WeixinLease, claim: &WeixinInboxClaim)
        -> bool;

    /// Drop a claimed message the consumer will not retry again — the dispatch kept failing, or
    /// the provider kept refusing the reply. The message is tombstoned so a re-delivered batch
    /// cannot resurrect it, but it is not reported as completed.
    fn abandon_message(
        &self,
        account_id: &str,
        lease: &WeixinLease,
        claim: &WeixinInboxClaim,
    ) -> bool;

    /// Forget everything held for `account_id`: cursor, peer context tokens and inbox.
    ///
    /// Hosts call this when an account is retired so a long-lived process does not accumulate
    /// state for accounts it no longer serves.
    fn remove_account(&self, account_id: &str);
}

/// Convenience constructor mirroring the Java `WeixinStateStore.inMemory()`.
pub fn in_memory_store() -> InMemoryWeixinStateStore {
    InMemoryWeixinStateStore::new()
}

#[derive(Debug)]
struct InboxEntry {
    payload: Option<String>,
    lease: Option<WeixinLease>,
    claim_id: Option<String>,
    claim_until: i64,
    completed: bool,
    abandoned: bool,
    completed_at: i64,
}

#[derive(Debug, Default)]
struct Account {
    cursor: String,
    generation: u64,
    expires_at: i64,
    lease: Option<WeixinLease>,
    contexts: HashMap<String, String>,
    /// Reception order, mirroring the Java `LinkedHashMap` iteration order.
    inbox: Vec<(String, InboxEntry)>,
}

#[derive(Clone)]
enum Clock {
    System,
    Manual(Arc<dyn Fn() -> i64 + Send + Sync>),
}

impl Clock {
    fn now_ms(&self) -> i64 {
        match self {
            Self::System => now_ms(),
            Self::Manual(read) => read(),
        }
    }
}

/// Standalone / test adapter with the same fencing and claim rules as a durable host.
///
/// **Not for production.** Everything lives in this process: a restart loses the cursor, peer
/// context tokens and leases, and a second host instance sees a different store, so the
/// single-consumer guarantee only holds within one process.
#[derive(Clone)]
pub struct InMemoryWeixinStateStore {
    clock: Clock,
    accounts: Arc<Mutex<HashMap<String, Account>>>,
}

impl std::fmt::Debug for InMemoryWeixinStateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryWeixinStateStore")
            .field("retained_accounts", &self.retained_accounts())
            .finish()
    }
}

impl Default for InMemoryWeixinStateStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryWeixinStateStore {
    /// A store reading the system clock.
    pub fn new() -> Self {
        Self {
            clock: Clock::System,
            accounts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// A store reading an injected clock (offline / deterministic tests).
    pub fn with_clock(read_ms: Arc<dyn Fn() -> i64 + Send + Sync>) -> Self {
        Self {
            clock: Clock::Manual(read_ms),
            accounts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Number of accounts currently retained; used by tests and diagnostics.
    pub fn retained_accounts(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Account>> {
        self.accounts
            .lock()
            .expect("weixin state store mutex poisoned")
    }

    /// Whether the account id is usable, mirroring the Java blank check.
    fn require_account(account_id: &str) -> Result<()> {
        if account_id.trim().is_empty() {
            return Err(ChannelError::Config("accountId is required".to_string()));
        }
        Ok(())
    }

    fn existing(&self, account_id: &str) -> Result<Option<Account>> {
        Self::require_account(account_id)?;
        Ok(self.lock().get(account_id).cloned())
    }

    fn now(&self) -> i64 {
        self.clock.now_ms()
    }
}

// `Account` is cloned out of the locked map only where Java would read it under the same lock; the
// methods below therefore take the lock once and mutate in place.
impl Clone for Account {
    fn clone(&self) -> Self {
        Self {
            cursor: self.cursor.clone(),
            generation: self.generation,
            expires_at: self.expires_at,
            lease: self.lease.clone(),
            contexts: self.contexts.clone(),
            inbox: self
                .inbox
                .iter()
                .map(|(id, entry)| (id.clone(), entry.clone()))
                .collect(),
        }
    }
}

impl Clone for InboxEntry {
    fn clone(&self) -> Self {
        Self {
            payload: self.payload.clone(),
            lease: self.lease.clone(),
            claim_id: self.claim_id.clone(),
            claim_until: self.claim_until,
            completed: self.completed,
            abandoned: self.abandoned,
            completed_at: self.completed_at,
        }
    }
}

fn is_current(account: &Account, lease: &WeixinLease, now: i64) -> bool {
    account.lease.as_ref() == Some(lease) && account.expires_at > now
}

impl WeixinStateStore for InMemoryWeixinStateStore {
    fn load_cursor(&self, account_id: &str) -> String {
        self.existing(account_id)
            .ok()
            .flatten()
            .map(|account| account.cursor)
            .unwrap_or_default()
    }

    fn load_context_token(&self, account_id: &str, peer_id: &str) -> Option<String> {
        self.existing(account_id)
            .ok()
            .flatten()
            .and_then(|account| account.contexts.get(peer_id).cloned())
    }

    fn save_context_token(
        &self,
        account_id: &str,
        lease: &WeixinLease,
        peer_id: &str,
        token: &str,
    ) -> bool {
        if Self::require_account(account_id).is_err() {
            return false;
        }
        let now = self.now();
        let mut accounts = self.lock();
        let Some(account) = accounts.get_mut(account_id) else {
            return false;
        };
        if !is_current(account, lease, now) {
            return false;
        }
        if !peer_id.trim().is_empty() && !token.trim().is_empty() {
            account
                .contexts
                .insert(peer_id.to_string(), token.to_string());
        }
        true
    }

    fn acquire_lease(
        &self,
        account_id: &str,
        holder_id: &str,
        lease_ms: i64,
    ) -> Option<WeixinLease> {
        if Self::require_account(account_id).is_err()
            || holder_id.trim().is_empty()
            || lease_ms <= 0
        {
            return None;
        }
        let now = self.now();
        let mut accounts = self.lock();
        let account = accounts.entry(account_id.to_string()).or_default();
        match &account.lease {
            Some(current) if account.expires_at > now => {
                // A live lease held by someone else keeps the consumer out; the holder itself may
                // simply extend it (same generation, mirroring the Java re-entrant branch).
                if current.holder_id != holder_id {
                    return None;
                }
            }
            _ => {
                account.generation += 1;
                account.lease = WeixinLease::new(holder_id, account.generation).ok();
            }
        }
        account.expires_at = now + lease_ms;
        account.lease.clone()
    }

    fn renew_lease(&self, account_id: &str, lease: &WeixinLease, lease_ms: i64) -> bool {
        if lease_ms <= 0 {
            return false;
        }
        let now = self.now();
        let mut accounts = self.lock();
        match accounts.get_mut(account_id) {
            Some(account) if is_current(account, lease, now) => {
                account.expires_at = now + lease_ms;
                true
            }
            _ => false,
        }
    }

    fn is_lease_current(&self, account_id: &str, lease: &WeixinLease) -> bool {
        let now = self.now();
        let accounts = self.lock();
        accounts
            .get(account_id)
            .is_some_and(|account| is_current(account, lease, now))
    }

    fn release_lease(&self, account_id: &str, lease: &WeixinLease) {
        let mut accounts = self.lock();
        if let Some(account) = accounts.get_mut(account_id) {
            if account.lease.as_ref() == Some(lease) {
                account.expires_at = 0;
            }
        }
    }

    fn accept_batch(
        &self,
        account_id: &str,
        lease: &WeixinLease,
        next_cursor: &str,
        messages: &[WeixinInboxMessage],
    ) -> bool {
        if Self::require_account(account_id).is_err() {
            return false;
        }
        let now = self.now();
        let mut accounts = self.lock();
        let Some(account) = accounts.get_mut(account_id) else {
            return false;
        };
        if !is_current(account, lease, now) {
            return false;
        }
        for message in messages {
            let known = account
                .inbox
                .iter()
                .any(|(id, _)| id == &message.message_id);
            if !known {
                account.inbox.push((
                    message.message_id.clone(),
                    InboxEntry {
                        payload: Some(message.payload.clone()),
                        lease: None,
                        claim_id: None,
                        claim_until: 0,
                        completed: false,
                        abandoned: false,
                        completed_at: 0,
                    },
                ));
            }
        }
        if !next_cursor.trim().is_empty() {
            account.cursor = next_cursor.to_string();
        }
        prune(&mut accounts, account_id, now);
        true
    }

    fn claim_messages(
        &self,
        account_id: &str,
        lease: &WeixinLease,
        limit: usize,
        claim_ms: i64,
    ) -> Vec<WeixinInboxClaim> {
        if limit == 0 || claim_ms <= 0 {
            return Vec::new();
        }
        let now = self.now();
        let mut accounts = self.lock();
        let Some(account) = accounts.get_mut(account_id) else {
            return Vec::new();
        };
        if !is_current(account, lease, now) {
            return Vec::new();
        }
        let mut claims = Vec::new();
        for index in 0..account.inbox.len() {
            if claims.len() >= limit {
                break;
            }
            let (message_id, entry) = &mut account.inbox[index];
            if entry.completed || entry.abandoned {
                continue;
            }
            // Java stops the scan at the first live claim held by this lease, so one consumer
            // cannot re-claim work it is already dispatching.
            if entry.lease.as_ref() == Some(lease) && entry.claim_until > now {
                break;
            }
            entry.lease = Some(lease.clone());
            entry.claim_id = Some(next_claim_id());
            entry.claim_until = now + claim_ms;
            claims.push(WeixinInboxClaim {
                message_id: message_id.clone(),
                payload: entry.payload.clone().unwrap_or_default(),
                claim_id: entry.claim_id.clone().unwrap_or_default(),
            });
        }
        claims
    }

    fn complete_message(
        &self,
        account_id: &str,
        lease: &WeixinLease,
        claim: &WeixinInboxClaim,
    ) -> bool {
        self.finish_claim(account_id, lease, claim, |entry, now| {
            entry.completed = true;
            entry.completed_at = now;
            entry.payload = None;
            entry.claim_id = None;
        })
    }

    fn fail_message(
        &self,
        account_id: &str,
        lease: &WeixinLease,
        claim: &WeixinInboxClaim,
    ) -> bool {
        self.finish_claim(account_id, lease, claim, |entry, _| {
            entry.claim_until = 0;
            entry.claim_id = None;
        })
    }

    fn abandon_message(
        &self,
        account_id: &str,
        lease: &WeixinLease,
        claim: &WeixinInboxClaim,
    ) -> bool {
        self.finish_claim(account_id, lease, claim, |entry, now| {
            entry.abandoned = true;
            entry.completed_at = now;
            entry.payload = None;
            entry.claim_id = None;
        })
    }

    fn remove_account(&self, account_id: &str) {
        if Self::require_account(account_id).is_err() {
            return;
        }
        self.lock().remove(account_id);
    }
}

impl InMemoryWeixinStateStore {
    /// Validate a claim token and apply `mutate` to the exact, unexpired claim.
    fn finish_claim(
        &self,
        account_id: &str,
        lease: &WeixinLease,
        claim: &WeixinInboxClaim,
        mutate: impl FnOnce(&mut InboxEntry, i64),
    ) -> bool {
        let now = self.now();
        let mut accounts = self.lock();
        let Some(account) = accounts.get_mut(account_id) else {
            return false;
        };
        if !is_current(account, lease, now) {
            return false;
        }
        let Some((_, entry)) = account
            .inbox
            .iter_mut()
            .find(|(id, _)| id == &claim.message_id)
        else {
            return false;
        };
        let valid = !entry.completed
            && entry.lease.as_ref() == Some(lease)
            && entry.claim_id.as_deref() == Some(claim.claim_id.as_str())
            && entry.claim_until > now;
        if !valid {
            return false;
        }
        mutate(entry, now);
        true
    }
}

/// Expire tombstones in every account and forget accounts that hold no state at all.
///
/// An account with a cursor, peer context or pending inbox entry is never dropped: dropping the
/// cursor would make the consumer replay the provider backlog from the beginning.
fn prune(accounts: &mut HashMap<String, Account>, active_account_id: &str, now: i64) {
    for account in accounts.values_mut() {
        account.inbox.retain(|(_, entry)| {
            !(entry.completed || entry.abandoned) || entry.completed_at >= now - RETENTION_MS
        });
    }
    accounts.retain(|id, account| {
        id == active_account_id
            || account.expires_at > now
            || !account.cursor.is_empty()
            || !account.contexts.is_empty()
            || !account.inbox.is_empty()
    });
}

/// Process-unique claim id. Only uniqueness matters (it fences late completions), so a monotonic
/// counter seeded from the clock is enough and avoids a `uuid` dependency for this module.
fn next_claim_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("claim-{:x}-{:x}", now_ms() as u64, sequence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    fn manual_store(time: &Arc<AtomicI64>) -> InMemoryWeixinStateStore {
        let clock = time.clone();
        InMemoryWeixinStateStore::with_clock(Arc::new(move || clock.load(Ordering::SeqCst)))
    }

    fn batch(entries: &[(&str, &str)]) -> Vec<WeixinInboxMessage> {
        entries
            .iter()
            .map(|(id, payload)| WeixinInboxMessage::new(*id, *payload).unwrap())
            .collect()
    }

    #[test]
    fn expired_claims_and_leases_cannot_complete_newer_work() {
        let time = Arc::new(AtomicI64::new(1000));
        let store = manual_store(&time);
        let lease = store.acquire_lease("account", "holder", 1000).unwrap();
        assert!(store.accept_batch(
            "account",
            &lease,
            "cursor",
            &batch(&[("b", "payload"), ("a", "payload")])
        ));

        let old = store.claim_messages("account", &lease, 1, 50).remove(0);
        assert_eq!(old.message_id, "b");
        // The live claim stops the scan, so the same lease cannot claim more work yet.
        assert!(store.claim_messages("account", &lease, 1, 50).is_empty());

        time.fetch_add(50, Ordering::SeqCst);
        let next = store.claim_messages("account", &lease, 1, 50).remove(0);
        assert_ne!(old.claim_id, next.claim_id);
        // The stale claim token cannot finish the newer attempt.
        assert!(!store.complete_message("account", &lease, &old));
        assert!(store.complete_message("account", &lease, &next));
        assert_eq!(
            store.claim_messages("account", &lease, 1, 50)[0].message_id,
            "a"
        );

        time.fetch_add(1000, Ordering::SeqCst);
        assert!(!store.renew_lease("account", &lease, 1000));
        let newer = store.acquire_lease("account", "holder", 1000).unwrap();
        assert!(newer.generation > lease.generation);
        assert!(!store.accept_batch("account", &lease, "stale", &[]));
        assert_eq!(store.load_cursor("account"), "cursor");
    }

    #[test]
    fn read_only_lookups_do_not_retain_accounts() {
        let store = InMemoryWeixinStateStore::new();
        assert_eq!(store.load_cursor("ghost"), String::new());
        assert_eq!(store.load_context_token("ghost", "peer"), None);
        assert!(!store.is_lease_current("ghost", &WeixinLease::new("h", 1).unwrap()));
        store.remove_account("ghost");
        assert_eq!(
            store.retained_accounts(),
            0,
            "lookups must not materialise accounts"
        );
    }

    #[test]
    fn completed_and_abandoned_messages_are_never_claimed_again() {
        let store = InMemoryWeixinStateStore::new();
        let lease = store.acquire_lease("account", "holder", 60_000).unwrap();
        assert!(store.accept_batch(
            "account",
            &lease,
            "cursor",
            &batch(&[("gone", "payload"), ("done", "payload")])
        ));

        let claim = store.claim_messages("account", &lease, 1, 60_000).remove(0);
        assert!(store.abandon_message("account", &lease, &claim));
        let next = store.claim_messages("account", &lease, 1, 60_000).remove(0);
        assert_eq!(next.message_id, "done");
        assert!(store.complete_message("account", &lease, &next));
        assert!(store
            .claim_messages("account", &lease, 1, 60_000)
            .is_empty());

        // A re-delivered batch cannot resurrect either message.
        assert!(store.accept_batch(
            "account",
            &lease,
            "cursor-2",
            &batch(&[("gone", "payload"), ("done", "payload")])
        ));
        assert!(store
            .claim_messages("account", &lease, 1, 60_000)
            .is_empty());
    }

    #[test]
    fn a_released_lease_cannot_become_current_again_for_the_same_holder() {
        let store = InMemoryWeixinStateStore::new();
        let old = store.acquire_lease("account", "holder", 60_000).unwrap();
        store.release_lease("account", &old);
        let current = store.acquire_lease("account", "holder", 60_000).unwrap();

        assert!(current.generation > old.generation);
        assert!(!store.is_lease_current("account", &old));
        assert!(!store.renew_lease("account", &old, 60_000));
        assert!(!store.accept_batch("account", &old, "lost", &[]));
        assert_eq!(store.load_cursor("account"), String::new());
    }

    #[test]
    fn a_second_holder_is_refused_while_the_lease_is_live_and_takes_over_once_released() {
        let time = Arc::new(AtomicI64::new(1000));
        let store = manual_store(&time);
        let first = store.acquire_lease("account", "first", 1000).unwrap();
        assert!(store.acquire_lease("account", "second", 1000).is_none());
        // The holder itself may extend without losing its generation.
        let extended = store.acquire_lease("account", "first", 5000).unwrap();
        assert_eq!(extended, first);

        time.fetch_add(6000, Ordering::SeqCst);
        let second = store.acquire_lease("account", "second", 1000).unwrap();
        assert_eq!(second.holder_id, "second");
        assert!(second.generation > first.generation);
        assert!(!store.is_lease_current("account", &first));
    }

    #[test]
    fn a_new_lease_recovers_the_previous_owners_pending_work() {
        let store = InMemoryWeixinStateStore::new();
        let first = store.acquire_lease("account", "first", 60_000).unwrap();
        assert!(store.accept_batch("account", &first, "cursor", &batch(&[("m1", "p")])));
        let stale = store.claim_messages("account", &first, 1, 60_000).remove(0);
        store.release_lease("account", &first);

        let second = store.acquire_lease("account", "second", 60_000).unwrap();
        let recovered = store.claim_messages("account", &second, 1, 60_000)[0].clone();
        assert_eq!(recovered.message_id, "m1");
        // The previous owner can no longer finish its claim. `first` still holds the live claim
        // token but its lease is gone, so the completion is fenced twice over.
        store.release_lease("account", &first);
        assert!(!store.complete_message("account", &second, &stale));
        assert!(store.complete_message("account", &second, &recovered));
    }

    #[test]
    fn context_tokens_are_lease_fenced_and_scoped_per_peer() {
        let store = InMemoryWeixinStateStore::new();
        let lease = store.acquire_lease("account", "holder", 60_000).unwrap();
        assert!(store.save_context_token("account", &lease, "peer-a", "ctx-a"));
        assert!(store.save_context_token("account", &lease, "peer-b", "ctx-b"));
        assert_eq!(
            store.load_context_token("account", "peer-a").as_deref(),
            Some("ctx-a")
        );
        assert_eq!(
            store.load_context_token("account", "peer-b").as_deref(),
            Some("ctx-b")
        );
        // A blank token is not stored, mirroring the Java guard.
        assert!(store.save_context_token("account", &lease, "peer-a", "  "));
        assert_eq!(
            store.load_context_token("account", "peer-a").as_deref(),
            Some("ctx-a")
        );
        // A stale lease cannot write.
        let stale = WeixinLease::new("holder", lease.generation + 1).unwrap();
        assert!(!store.save_context_token("account", &stale, "peer-c", "ctx-c"));
        assert_eq!(store.load_context_token("account", "peer-c"), None);
    }

    #[test]
    fn retires_accounts_explicitly_and_on_prune_it_keeps_the_position() {
        let time = Arc::new(AtomicI64::new(1000));
        let store = manual_store(&time);
        let active = store.acquire_lease("active", "holder", 60_000).unwrap();
        let idle = store.acquire_lease("idle", "holder", 1000).unwrap();
        assert!(store.accept_batch("idle", &idle, "cursor-idle", &[]));

        time.fetch_add(2000, Ordering::SeqCst);
        assert!(store.accept_batch("active", &active, "cursor-active", &[]));
        // A stateless account is forgotten; one holding a cursor keeps its position.
        assert_eq!(store.load_cursor("idle"), "cursor-idle");
        assert_eq!(store.load_cursor("active"), "cursor-active");

        let seen = store.acquire_lease("seen-once", "holder", 1000).unwrap();
        store.release_lease("seen-once", &seen);
        let before = store.retained_accounts();
        time.fetch_add(2000, Ordering::SeqCst);
        assert!(store.accept_batch("active", &active, "cursor-2", &[]));
        assert_eq!(store.retained_accounts(), before - 1);
        assert_eq!(store.load_cursor("seen-once"), String::new());

        store.remove_account("active");
        assert_eq!(store.load_cursor("active"), String::new());
    }

    #[test]
    fn diagnostic_strings_do_not_expose_payloads() {
        let message = WeixinInboxMessage::new("id", "private-context").unwrap();
        assert!(!format!("{message:?}").contains("private-context"));
        let claim = WeixinInboxClaim {
            message_id: "id".to_string(),
            payload: "private-context".to_string(),
            claim_id: "claim".to_string(),
        };
        assert!(!format!("{claim:?}").contains("private-context"));
    }

    #[test]
    fn batch_entries_and_leases_validate_their_inputs() {
        assert!(WeixinInboxMessage::new("  ", "p").is_err());
        assert!(WeixinLease::new("", 1).is_err());
        assert!(WeixinLease::new("holder", 0).is_err());
        let store = InMemoryWeixinStateStore::new();
        assert!(store.acquire_lease("account", "holder", 0).is_none());
        assert!(store.acquire_lease("account", "  ", 1000).is_none());
        let lease = store.acquire_lease("account", "holder", 1000).unwrap();
        assert!(store.claim_messages("account", &lease, 0, 1000).is_empty());
        assert!(store.claim_messages("account", &lease, 1, 0).is_empty());
    }
}
