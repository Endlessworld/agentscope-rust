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
//! A failover produces no redirect at all: the cached primary is **demoted** to a replica and
//! keeps answering `READONLY` (or `CLUSTERDOWN` / `TRYAGAIN` while the cluster reconfigures).
//! Those codes are therefore treated as "the cached slot map is stale": the slot entry is
//! evicted and the command is retried once from the seed node, which relearns the owner.
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
        // The wire form is `MOVED <slot> <host>:<port>`, but `redis` renders the reply through
        // `Debug` for its own error kind, i.e. `Moved: <slot> <host>:<port>` / `Ask: ...`.
        // Matching the raw token case-sensitively therefore never recognised a live redirect
        // and every `MOVED` surfaced as `Other("Redis command failed: Moved: ...")`.
        let kind = token.trim_end_matches(':').to_ascii_uppercase();
        let moved = kind == "MOVED";
        let ask = kind == "ASK";
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

    /// Forgets the cached owner of `slot`, so the next command for that slot starts from the
    /// seed node and relearns the owner from the redirect it receives.
    ///
    /// Used when a node answers `READONLY` / `CLUSTERDOWN` / `TRYAGAIN`: the cached primary
    /// was demoted by a failover (or the cluster is reconfiguring), and a stale entry would
    /// keep sending the command to a replica that can only reject writes.
    ///
    /// Returns whether an entry was actually present.
    pub(crate) fn invalidate(&mut self, slot: u16) -> bool {
        self.slots.remove(&slot).is_some()
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
    /// The node does not own the slot (any more) or the cluster is reconfiguring: the cached
    /// slot map is stale.
    StaleTopology(String),
    /// A real failure (network, auth, wrong type, ...).
    Other(String),
}

/// Server error codes that mean "the client's slot map is stale".
///
/// * `READONLY` — a write reached a node that has been demoted to a replica by a failover;
///   the cached owner is no longer the primary.
/// * `CLUSTERDOWN` — the node reports no slot coverage (a failed primary not replaced yet).
/// * `TRYAGAIN` — the cluster is reconfiguring (a migration is in flight).
///
/// The crate keeps its own slot map (the driver's `cluster` feature is off), so nothing else
/// would ever refresh it: `MOVED` is the only other signal, and a demoted primary answers
/// `READONLY` instead of redirecting, which would leave every later write to that slot
/// failing forever.
const STALE_TOPOLOGY_CODES: [&str; 3] = ["READONLY", "CLUSTERDOWN", "TRYAGAIN"];

/// Scans a driver error message for a stale-topology code.
///
/// The `redis` crate renders a server error as `<Kind>: <CODE> <detail>` (or just the bare
/// reply when the kind is unknown), so the code is looked up token by token instead of
/// assuming it starts the message.
pub(crate) fn is_stale_topology(message: &str) -> bool {
    message.split_whitespace().any(|token| {
        let code = token.trim_end_matches(':').to_ascii_uppercase();
        STALE_TOPOLOGY_CODES.contains(&code.as_str())
    })
}

impl CommandFailure {
    /// Builds a failure from a driver error message, recognising redirect replies and stale
    /// topology errors.
    pub(crate) fn from_message(message: impl Into<String>) -> Self {
        let message = message.into();
        if let Some(redirect) = parse_redirect(&message) {
            return CommandFailure::Redirect(redirect);
        }
        if is_stale_topology(&message) {
            return CommandFailure::StaleTopology(message);
        }
        CommandFailure::Other(message)
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

/// How many redirections a single command may follow.
const MAX_REDIRECTS: usize = 1;

/// How many times a stale slot map may be refreshed for one command.
const MAX_TOPOLOGY_REFRESHES: usize = 1;

/// Locks the slot cache, ignoring poisoning (the cache is a plain map; a panic elsewhere
/// must not turn routing into a hard failure).
fn lock_cache(cache: &std::sync::Mutex<SlotCache>) -> std::sync::MutexGuard<'_, SlotCache> {
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Runs a command, following **at most one** redirection and refreshing a stale slot map
/// **once**.
///
/// * `MOVED` — records the new owner in `cache` and retries there once.
/// * `ASK` — retries on the target with `ASKING`, **without** touching the cache.
/// * `READONLY` / `CLUSTERDOWN` / `TRYAGAIN` ([`CommandFailure::StaleTopology`]) — the cached
///   owner of `slot` is no longer the primary (a failover demoted it), so the entry is
///   evicted and the command is retried once from the seed node, which relearns the owner
///   through the redirect the new primary answers with.
/// * a second redirect, a second stale-topology signal, or any other error is returned as an
///   error: a cluster that keeps redirecting is unhealthy, and retrying forever would turn a
///   routing bug into a hang.
///
/// The slot from a `MOVED` is cached before the retry so a later command for the same slot
/// (or a later call after a failure) skips the discovery round-trip.
pub(crate) async fn run_with_redirects(
    transport: &dyn RedirectTransport,
    cache: &std::sync::Mutex<SlotCache>,
    slot: u16,
) -> Result<redis::Value> {
    let mut redirects = 0usize;
    let mut refreshes = 0usize;
    // Explicit target of the next attempt; `None` means "start from the cached owner, or the
    // seed node when the slot map has no entry".
    let mut target: Option<ClusterNodeAddr> = None;
    let mut asking = false;
    loop {
        match transport.send(target.as_ref(), asking).await {
            Ok(value) => return Ok(value),
            Err(CommandFailure::Redirect(redirect)) => {
                if redirects >= MAX_REDIRECTS {
                    return Err(Error::Other(format!(
                        "Redis command was redirected twice ({} slot {} -> {}); giving up",
                        if matches!(redirect, Redirect::Ask { .. }) {
                            "ASK"
                        } else {
                            "MOVED"
                        },
                        redirect.slot(),
                        redirect.node().to_redis_url()
                    )));
                }
                redirects += 1;
                if let Redirect::Moved { slot, node } = &redirect {
                    lock_cache(cache).record_moved(*slot, node.clone());
                }
                target = Some(redirect.node().clone());
                asking = matches!(redirect, Redirect::Ask { .. });
            }
            Err(CommandFailure::StaleTopology(message)) => {
                if refreshes >= MAX_TOPOLOGY_REFRESHES {
                    return Err(Error::Other(format!(
                        "Redis slot {slot} still reports a stale cluster topology after \
                         refreshing the slot map ({message}); giving up"
                    )));
                }
                refreshes += 1;
                let evicted = lock_cache(cache).invalidate(slot);
                tracing::debug!(
                    "Redis slot {} reported a stale topology ({message}); evicted={evicted}, \
                     retrying from the seed node",
                    slot
                );
                // Forget the redirect target as well: the retry must re-resolve the owner
                // from the (now emptied) slot map instead of reusing the demoted node.
                target = None;
                asking = false;
            }
            Err(CommandFailure::Other(message)) => {
                return Err(Error::Other(if redirects == 0 {
                    format!("Redis command failed: {message}")
                } else {
                    format!("Redis command failed after redirect: {message}")
                }));
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
            self.calls.lock().unwrap().push((target.cloned(), asking));
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

    /// The form a live server actually produces.
    ///
    /// `redis` renders a redirect reply through `Debug`: `Moved: <slot> <host>:<port>` and
    /// `Ask: ...` — capitalised, **with a colon after the keyword**, and possibly behind the
    /// driver's own prefix. This is the case the production path hits; the plain
    /// `MOVED <slot> <host>:<port>` wire text below is only kept for compatibility.
    #[test]
    fn parses_the_debug_rendered_redirect_from_a_live_cluster() {
        assert_eq!(
            parse_redirect("Redis command failed: Moved: 12536 127.0.0.1:7002"),
            Some(Redirect::Moved {
                slot: 12536,
                node: node("127.0.0.1", 7002)
            })
        );
        assert_eq!(
            parse_redirect("Ask: 3999 10.0.0.7:7000"),
            Some(Redirect::Ask {
                slot: 3999,
                node: node("10.0.0.7", 7000)
            })
        );
        // The raw driver error text, exactly as the live run surfaced it.
        match CommandFailure::from_message("Redis command failed: Moved: 12536 127.0.0.1:7002") {
            CommandFailure::Redirect(Redirect::Moved { slot, node }) => {
                assert_eq!(slot, 12536);
                assert_eq!(node.host, "127.0.0.1");
                assert_eq!(node.port, 7002);
            }
            other => panic!("expected a MOVED redirect, got {other:?}"),
        }
    }

    /// Compatibility: the bare wire spelling still parses.
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

        let value = run_with_redirects(&transport, &cache, 3999)
            .await
            .expect("retry");
        assert_eq!(value, Value::Int(42));
        assert_eq!(
            transport.calls(),
            vec![(None, false), (Some(node("127.0.0.1", 6381)), false)]
        );
        assert_eq!(
            cache.lock().unwrap().get(3999),
            Some(&node("127.0.0.1", 6381))
        );
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

        run_with_redirects(&transport, &cache, 12)
            .await
            .expect("retry");
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

        let err = run_with_redirects(&transport, &cache, 1)
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

        let err = run_with_redirects(&transport, &cache, 1)
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

    #[test]
    fn stale_topology_codes_are_classified() {
        for message in [
            // The raw server reply.
            "READONLY You can't write against a read only replica.",
            // How the driver renders it (kind + ": " + detail).
            "ReadOnly: READONLY You can't write against a read only replica.",
            "CLUSTERDOWN The cluster is down",
            "TryAgain: TRYAGAIN Multiple keys request during rehashing of slot",
        ] {
            assert!(
                matches!(
                    CommandFailure::from_message(message),
                    CommandFailure::StaleTopology(_)
                ),
                "not classified as stale topology: {message}"
            );
        }
        // A redirect always wins, and real errors stay real.
        assert!(matches!(
            CommandFailure::from_message("Moved: 1 127.0.0.1:7000"),
            CommandFailure::Redirect(Redirect::Moved { slot: 1, .. })
        ));
        assert!(matches!(
            CommandFailure::from_message("WRONGTYPE Operation against a key"),
            CommandFailure::Other(_)
        ));
    }

    /// A failover demotes the cached primary to a replica: the next write is answered with
    /// `READONLY`, and the cached slot entry must be evicted so the retry can start over
    /// from the seed node. Before the fix `READONLY` was `CommandFailure::Other`, i.e. the
    /// write failed and every later write to that slot kept failing on the same dead entry.
    #[tokio::test]
    async fn readonly_evicts_the_stale_slot_and_retries_once() {
        let transport = ScriptedTransport::new(vec![
            Err(CommandFailure::from_message(
                "ReadOnly: READONLY You can't write against a read only replica.",
            )),
            Ok(Value::Int(1)),
        ]);
        let mut slots = SlotCache::new();
        slots.record_moved(3999, node("10.0.0.1", 7000));
        let cache = Mutex::new(slots);

        let value = run_with_redirects(&transport, &cache, 3999)
            .await
            .expect("the refreshed slot map lets the write through");

        assert_eq!(value, Value::Int(1));
        assert_eq!(transport.calls().len(), 2, "exactly one retry");
        assert!(
            cache.lock().unwrap().get(3999).is_none(),
            "the demoted primary must be evicted from the slot map"
        );
    }

    /// The retry after a refresh may itself be redirected (the seed node knows the new
    /// owner): one redirect is still allowed, and it is cached.
    #[tokio::test]
    async fn a_refresh_can_be_followed_by_one_redirect() {
        let transport = ScriptedTransport::new(vec![
            Err(CommandFailure::from_message(
                "CLUSTERDOWN The cluster is down",
            )),
            Err(CommandFailure::Redirect(Redirect::Moved {
                slot: 7,
                node: node("10.0.0.2", 7001),
            })),
            Ok(Value::Nil),
        ]);
        let cache = Mutex::new(SlotCache::new());

        let value = run_with_redirects(&transport, &cache, 7)
            .await
            .expect("refresh + one redirect");

        assert_eq!(value, Value::Nil);
        assert_eq!(
            transport.calls(),
            vec![
                (None, false),
                (None, false),
                (Some(node("10.0.0.2", 7001)), false)
            ]
        );
        assert_eq!(cache.lock().unwrap().get(7), Some(&node("10.0.0.2", 7001)));
    }

    /// A cluster that keeps answering `READONLY` is broken: one refresh, then give up.
    #[tokio::test]
    async fn a_second_stale_topology_signal_gives_up() {
        let transport = ScriptedTransport::new(vec![
            Err(CommandFailure::from_message(
                "READONLY You can't write against a read only replica.",
            )),
            Err(CommandFailure::from_message(
                "TRYAGAIN rehashing in progress",
            )),
        ]);
        let cache = Mutex::new(SlotCache::new());

        let err = run_with_redirects(&transport, &cache, 42)
            .await
            .expect_err("a second stale signal must fail");

        assert!(err.to_string().contains("stale cluster topology"), "{err}");
        assert_eq!(transport.calls().len(), 2, "one refresh, then give up");
    }
}
