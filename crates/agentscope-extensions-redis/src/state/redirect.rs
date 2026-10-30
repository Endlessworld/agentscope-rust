//! Redis Cluster `MOVED` / `ASK` redirection.
//!
//! The crate talks to Redis through single-node connections (the driver's `cluster`
//! feature is deliberately off), so a command that targets a key owned by another primary
//! comes back as a redirect error instead of being routed by the client:
//!
//! ```text
//! MOVED 3999 127.0.0.1:6381   -- the slot moved for good: update the slot map, retry
//! ASK   3999 127.0.0.1:6381   -- a migration is in flight: ASKING + retry once, no caching
//! ```
//!
//! This module holds the pure pieces ([`parse_redirect`], [`slot_for_key`], [`SlotCache`])
//! and the retry loop ([`run_with_redirects`]), which is written against the
//! [`RedirectTransport`] seam so the "one redirection, then give up" rule is covered
//! offline by a scripted double — no cluster needed.

use std::collections::HashMap;

use agentscope_core::error::{Error, Result};
use async_trait::async_trait;

use super::connection::ClusterNodeAddr;

/// Number of hash slots in a Redis Cluster.
pub(crate) const CLUSTER_SLOTS: u16 = 16384;

/// A redirection reply from a cluster node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Redirect {
    /// The slot permanently lives elsewhere: cache it and retry on `node`.
    Moved { slot: u16, node: ClusterNodeAddr },
    /// The slot is being migrated: `ASKING` + retry on `node` **once**, never cached
    /// (the key may still live on the old node for other keys of the same slot).
    Ask { slot: u16, node: ClusterNodeAddr },
}

impl Redirect {
    pub(crate) fn slot(&self) -> u16 {
        match self {
            Redirect::Moved { slot, .. } | Redirect::Ask { slot, .. } => *slot,
        }
    }

    pub(crate) fn node(&self) -> &ClusterNodeAddr {
        match self {
            Redirect::Moved { node, .. } | Redirect::Ask { node, .. } => node,
        }
    }
}

/// Parses a `MOVED <slot> <host>:<port>` / `ASK <slot> <host>:<port>` error message.
///
/// The driver surfaces server errors as a display string (often prefixed with context), so
/// the parser scans for the redirect token instead of assuming it starts the message.
/// Tokens after it are validated: the slot must be `< 16384` and the address must be a
/// `host:port` pair (IPv6 literals are bracketed, `[::1]:7000`). Anything malformed yields
/// `None`, which makes the caller surface the original driver error rather than retrying
/// against a bogus node.
pub(crate) fn parse_redirect(message: &str) -> Option<Redirect> {
    let mut tokens = message.split_whitespace();
    while let Some(token) = tokens.next() {
        let moved = token == "MOVED";
        let ask = token == "ASK";
        if !moved && !ask {
            continue;
        }
        let slot: u16 = tokens.next()?.parse().ok()?;
        if slot >= CLUSTER_SLOTS {
            return None;
        }
        let node = parse_node_address(tokens.next()?)?;
        return Some(if moved {
            Redirect::Moved { slot, node }
        } else {
            Redirect::Ask { slot, node }
        });
    }
    None
}

/// Parses `host:port` (with optional IPv6 brackets) into a [`ClusterNodeAddr`].
pub(crate) fn parse_node_address(raw: &str) -> Option<ClusterNodeAddr> {
    let (host, port) = if let Some(rest) = raw.strip_prefix('[') {
        let (host, remainder) = rest.split_once(']')?;
        (host, remainder.strip_prefix(':')?)
    } else {
        raw.rsplit_once(':')?
    };
    if host.is_empty() {
        return None;
    }
    let port: u16 = port.parse().ok()?;
    if port == 0 {
        return None;
    }
    Some(ClusterNodeAddr::new(host, port))
}

/// CRC16-CCITT (XMODEM) — the checksum Redis Cluster uses for `CLUSTER KEYSLOT`.
pub(crate) fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for byte in data {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Computes the cluster slot of a key, honouring Redis hash tags.
///
/// `{user}/session` hashes only `user`, which is exactly how the crate's V1 key layout
/// keeps a whole session in one slot (see [`super::key_layout`]).
pub(crate) fn slot_for_key(key: &str) -> u16 {
    crc16(hash_tag(key).as_bytes()) % CLUSTER_SLOTS
}

/// Returns the substring Redis hashes: the first non-empty `{...}` group, else the whole key.
pub(crate) fn hash_tag(key: &str) -> &str {
    if let Some(open) = key.find('{') {
        let after = &key[open + 1..];
        if let Some(close) = after.find('}') {
            if close > 0 {
                return &after[..close];
            }
        }
    }
    key
}

/// Slot -> primary mapping learned from `MOVED` replies.
///
/// Only `MOVED` is cached: an `ASK` is valid for exactly one command during a migration,
/// so caching it would send unrelated keys of the same slot to the wrong node.
#[derive(Debug, Default)]
pub(crate) struct SlotCache {
    slots: HashMap<u16, ClusterNodeAddr>,
}

impl SlotCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn get(&self, slot: u16) -> Option<&ClusterNodeAddr> {
        self.slots.get(&slot)
    }

    /// Records a `MOVED` owner for `slot`.
    pub(crate) fn record_moved(&mut self, slot: u16, node: ClusterNodeAddr) {
        self.slots.insert(slot, node);
    }

    /// Number of cached slots (test-only introspection).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }
}

/// Why a single command attempt failed.
#[derive(Debug)]
pub(crate) enum CommandFailure {
    /// The node asked us to retry elsewhere.
    Redirect(Redirect),
    /// A real failure (network, auth, wrong type, ...).
    Other(String),
}

impl CommandFailure {
    /// Builds a failure from a driver error message, recognising redirect replies.
    pub(crate) fn from_message(message: impl Into<String>) -> Self {
        let message = message.into();
        match parse_redirect(&message) {
            Some(redirect) => CommandFailure::Redirect(redirect),
            None => CommandFailure::Other(message),
        }
    }
}

/// One command attempt against one node.
///
/// The command itself lives in the implementation (a closure over the real connection
/// and the `redis::Cmd`), so the retry loop below stays protocol-only and testable.
#[async_trait]
pub(crate) trait RedirectTransport: Send + Sync {
    /// Sends the command to `target` (`None` = the connection this router started from).
    ///
    /// When `asking` is true the implementation must issue `ASKING` on the same connection
    /// first, as Redis requires for an `ASK` redirect.
    async fn send(
        &self,
        target: Option<&ClusterNodeAddr>,
        asking: bool,
    ) -> std::result::Result<redis::Value, CommandFailure>;
}

/// Runs a command, following **at most one** redirection.
///
/// * `MOVED` — records the new owner in `cache` and retries there once.
/// * `ASK` — retries on the target with `ASKING`, **without** touching the cache.
/// * a second redirect, or any other error, is returned as an error: a cluster that keeps
///   redirecting is unhealthy, and retrying forever would turn a routing bug into a hang.
///
/// The slot from a `MOVED` is cached before the retry so a later command for the same slot
/// (or a later call after a failure) skips the discovery round-trip.
pub(crate) async fn run_with_redirects(
    transport: &dyn RedirectTransport,
    cache: &std::sync::Mutex<SlotCache>,
) -> Result<redis::Value> {
    match transport.send(None, false).await {
        Ok(value) => return Ok(value),
        Err(CommandFailure::Other(message)) => {
            return Err(Error::Other(format!("Redis command failed: {message}")))
        }
        Err(CommandFailure::Redirect(redirect)) => {
            if let Redirect::Moved { slot, node } = &redirect {
                cache
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .record_moved(*slot, node.clone());
            }
            let target = redirect.node().clone();
            let asking = matches!(redirect, Redirect::Ask { .. });
            match transport.send(Some(&target), asking).await {
                Ok(value) => Ok(value),
                Err(CommandFailure::Redirect(again)) => Err(Error::Other(format!(
                    "Redis command was redirected twice ({} slot {} -> {}); giving up",
                    if matches!(again, Redirect::Ask { .. }) {
                        "ASK"
                    } else {
                        "MOVED"
                    },
                    again.slot(),
                    again.node().to_redis_url()
                ))),
                Err(CommandFailure::Other(message)) => Err(Error::Other(format!(
                    "Redis command failed after redirect: {message}"
                ))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use redis::Value;

    /// Scripted transport: pops one outcome per `send` and records the `(target, asking)`
    /// pairs it was called with.
    struct ScriptedTransport {
        outcomes: Mutex<VecDeque<std::result::Result<Value, CommandFailure>>>,
        calls: Mutex<Vec<(Option<ClusterNodeAddr>, bool)>>,
    }

    impl ScriptedTransport {
        fn new(outcomes: Vec<std::result::Result<Value, CommandFailure>>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes.into()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(Option<ClusterNodeAddr>, bool)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl RedirectTransport for ScriptedTransport {
        async fn send(
            &self,
            target: Option<&ClusterNodeAddr>,
            asking: bool,
        ) -> std::result::Result<Value, CommandFailure> {
            self.calls
                .lock()
                .unwrap()
                .push((target.cloned(), asking));
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted outcome")
        }
    }

    fn node(host: &str, port: u16) -> ClusterNodeAddr {
        ClusterNodeAddr::new(host, port)
    }

    #[test]
    fn parses_moved_and_ask() {
        assert_eq!(
            parse_redirect("MOVED 3999 127.0.0.1:6381"),
            Some(Redirect::Moved {
                slot: 3999,
                node: node("127.0.0.1", 6381)
            })
        );
        assert_eq!(
            parse_redirect("ASK 3999 10.0.0.7:7000"),
            Some(Redirect::Ask {
                slot: 3999,
                node: node("10.0.0.7", 7000)
            })
        );
    }

    #[test]
    fn parses_redirect_inside_a_prefixed_driver_message() {
        assert_eq!(
            parse_redirect("An error was returned: MOVED 1 127.0.0.1:6381"),
            Some(Redirect::Moved {
                slot: 1,
                node: node("127.0.0.1", 6381)
            })
        );
    }

    #[test]
    fn rejects_malformed_redirects() {
        // Slot beyond the cluster range, missing port, port 0, empty host, no tokens.
        assert_eq!(parse_redirect("MOVED 16384 127.0.0.1:6381"), None);
        assert_eq!(parse_redirect("MOVED 12 127.0.0.1"), None);
        assert_eq!(parse_redirect("ASK 12 127.0.0.1:0"), None);
        assert_eq!(parse_redirect("ASK 12 :6379"), None);
        assert_eq!(parse_redirect("WRONGTYPE Operation against a key"), None);
    }

    #[test]
    fn parses_ipv6_node_addresses() {
        assert_eq!(
            parse_redirect("MOVED 5 [::1]:7000"),
            Some(Redirect::Moved {
                slot: 5,
                node: node("::1", 7000)
            })
        );
        assert_eq!(node("::1", 7000).to_redis_url(), "redis://[::1]:7000");
    }

    /// Gold vectors from the Redis documentation (`CLUSTER KEYSLOT`).
    #[test]
    fn slot_for_key_matches_redis_documentation() {
        assert_eq!(slot_for_key("123456789"), 12739);
        assert_eq!(slot_for_key("foo"), 12182);
        assert_eq!(slot_for_key("bar"), 5061);
    }

    #[test]
    fn hash_tags_pin_a_session_to_one_slot() {
        let a = slot_for_key("agentscope:session:{alice/s1}:k1");
        let b = slot_for_key("agentscope:session:{alice/s1}:k2");
        assert_eq!(a, b);
        assert_eq!(a, slot_for_key("alice/s1"));
        // An empty or unterminated tag hashes the whole key (Redis semantics).
        assert_eq!(hash_tag("a{}b"), "a{}b");
        assert_eq!(hash_tag("a{b"), "a{b");
        assert_eq!(hash_tag("plain"), "plain");
    }

    #[test]
    fn slot_cache_records_and_clears() {
        let mut cache = SlotCache::new();
        assert!(cache.get(7).is_none());
        cache.record_moved(7, node("127.0.0.1", 7001));
        assert_eq!(cache.get(7), Some(&node("127.0.0.1", 7001)));
        assert_eq!(cache.len(), 1);
        cache.record_moved(7, node("127.0.0.1", 7002));
        assert_eq!(cache.len(), 1, "a re-move replaces the entry");
    }

    #[tokio::test]
    async fn moved_is_cached_and_retried_once() {
        let transport = ScriptedTransport::new(vec![
            Err(CommandFailure::Redirect(Redirect::Moved {
                slot: 3999,
                node: node("127.0.0.1", 6381),
            })),
            Ok(Value::Int(42)),
        ]);
        let cache = Mutex::new(SlotCache::new());

        let value = run_with_redirects(&transport, &cache).await.expect("retry");
        assert_eq!(value, Value::Int(42));
        assert_eq!(
            transport.calls(),
            vec![(None, false), (Some(node("127.0.0.1", 6381)), false)]
        );
        assert_eq!(cache.lock().unwrap().get(3999), Some(&node("127.0.0.1", 6381)));
    }

    #[tokio::test]
    async fn ask_sends_asking_and_is_not_cached() {
        let transport = ScriptedTransport::new(vec![
            Err(CommandFailure::Redirect(Redirect::Ask {
                slot: 12,
                node: node("127.0.0.1", 7000),
            })),
            Ok(Value::BulkString(b"v".to_vec())),
        ]);
        let cache = Mutex::new(SlotCache::new());

        run_with_redirects(&transport, &cache).await.expect("retry");
        assert_eq!(
            transport.calls(),
            vec![(None, false), (Some(node("127.0.0.1", 7000)), true)]
        );
        assert_eq!(cache.lock().unwrap().len(), 0, "ASK must never be cached");
    }

    #[tokio::test]
    async fn second_redirect_gives_up() {
        let transport = ScriptedTransport::new(vec![
            Err(CommandFailure::Redirect(Redirect::Moved {
                slot: 1,
                node: node("127.0.0.1", 7001),
            })),
            Err(CommandFailure::Redirect(Redirect::Ask {
                slot: 1,
                node: node("127.0.0.1", 7002),
            })),
        ]);
        let cache = Mutex::new(SlotCache::new());

        let err = run_with_redirects(&transport, &cache)
            .await
            .expect_err("a second redirect must fail");
        assert!(err.to_string().contains("redirected twice"), "{err}");
        assert_eq!(transport.calls().len(), 2, "exactly one retry");
    }

    #[tokio::test]
    async fn non_redirect_errors_propagate_without_retry() {
        let transport = ScriptedTransport::new(vec![Err(CommandFailure::Other(
            "WRONGTYPE Operation against a key".to_string(),
        ))]);
        let cache = Mutex::new(SlotCache::new());

        let err = run_with_redirects(&transport, &cache)
            .await
            .expect_err("must fail");
        assert!(err.to_string().contains("WRONGTYPE"), "{err}");
        assert_eq!(transport.calls().len(), 1, "no retry for real errors");
    }

    #[test]
    fn failures_are_classified_from_driver_messages() {
        assert!(matches!(
            CommandFailure::from_message("MOVED 1 127.0.0.1:6381"),
            CommandFailure::Redirect(Redirect::Moved { slot: 1, .. })
        ));
        assert!(matches!(
            CommandFailure::from_message("NOAUTH Authentication required"),
            CommandFailure::Other(_)
        ));
    }
}
