//! Canonical [`RedisClientAdapter`] backed by the async `redis` crate.
//!
//! Mirrors the three Java adapters (Jedis / Lettuce / Redisson) collapsed into a single
//! connection-managed async client. Standalone / Sentinel / Cluster all work through
//! [`RedisConnection::connect`], which builds a [`redis::aio::ConnectionManager`] from a
//! URL (and tolerates a missing `redis://` scheme for parity with the Java examples that
//! pass `"localhost:6379"`).
//!
//! `find_keys_by_pattern` uses the **`SCAN` cursor protocol** (never `KEYS`), matching all
//! three Java adapters. The cursor loop and the per-node fan-out are expressed against two
//! crate-internal seams ([`ScanPager`] / [`ScanNodeSource`]) so the paging semantics can be
//! exercised offline; the production wiring ([`ConnectionScanPager`] /
//! [`SingleNodeScanSource`]) lives at the bottom of this module.
//!
//! Redis Cluster key enumeration does **not** need the driver's `cluster` feature: the
//! primaries are discovered with `CLUSTER SLOTS` on the seed connection
//! ([`ClusterTopologySource`]) and each primary gets its own standalone connection
//! ([`ClusterScanNodeSource`] + [`NodeConnectionFactory`]), so one `SCAN` iteration runs per
//! primary — the same coverage Java gets from `UnifiedJedis.scanIteration`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use redis::aio::ConnectionManager;

use agentscope_core::error::{Error, Result};

use super::adapter::RedisClientAdapter;
use super::redirect::{CommandFailure, RedirectTransport, SlotCache};
use crate::raw_client::{ManagerRedisCommands, RedisCommands};

fn map_err(context: &str, e: redis::RedisError) -> Error {
    Error::Other(format!("Redis {context}: {e}"))
}

/// `COUNT` hint passed to `SCAN` — bounded work per round-trip, matching the intent of the
/// Java adapters (which rely on the driver's default batching).
const SCAN_COUNT: usize = 100;

// ---------------------------------------------------------------------------
// SCAN seams (crate-internal)
// ---------------------------------------------------------------------------

/// A source of `SCAN` **pages** bound to a single Redis node.
///
/// `scan_page` maps to one `SCAN <cursor> MATCH <pattern> COUNT <count>` round-trip and
/// must return the **next** cursor together with that page's (possibly repeated, possibly
/// empty) keys. `0` means the iteration is finished.
#[async_trait]
pub(crate) trait ScanPager: Send + Sync + std::fmt::Debug {
    async fn scan_page(
        &self,
        cursor: u64,
        pattern: &str,
        count: usize,
    ) -> Result<(u64, Vec<String>)>;
}

/// Enumerates the nodes a `SCAN` fan-out has to visit.
///
/// Standalone / Sentinel deployments expose exactly one node (the connected one), so
/// [`SingleNodeScanSource`] yields a single pager. Redis Cluster needs **one pager per
/// primary** because `SCAN` is a per-node iteration: the `redis` driver routes `SCAN` to a
/// single node only, whereas Java's `UnifiedJedis.scanIteration` walks every master.
/// [`ClusterScanNodeSource`] covers that case by discovering the primaries with
/// `CLUSTER SLOTS`; the fan-out logic below is topology-agnostic and offline-tested.
#[async_trait]
pub(crate) trait ScanNodeSource: Send + Sync + std::fmt::Debug {
    async fn scan_nodes(&self) -> Result<Vec<Arc<dyn ScanPager>>>;

    /// `true` when the source fans out over several Cluster primaries.
    fn is_cluster(&self) -> bool {
        false
    }
}

/// Drives the `SCAN` cursor loop against one node until the server returns cursor `0`.
///
/// Keys repeated across pages are deduplicated; the result is sorted so callers get a
/// deterministic order regardless of hash-set iteration order.
pub(crate) async fn collect_keys_on_node(
    pager: &dyn ScanPager,
    pattern: &str,
) -> Result<Vec<String>> {
    let mut cursor: u64 = 0;
    let mut found: HashSet<String> = HashSet::new();
    loop {
        let (next, batch) = pager.scan_page(cursor, pattern, SCAN_COUNT).await?;
        found.extend(batch);
        if next == 0 {
            break;
        }
        cursor = next;
    }
    let mut out: Vec<String> = found.into_iter().collect();
    out.sort();
    Ok(out)
}

/// Runs one `SCAN` cursor loop per enumerated node and merges the results.
///
/// A failure on **any** node (enumeration or a page) aborts the whole scan: returning a
/// silently partial key set would make `clear_all_sessions` leak data and
/// `list_session_ids` under-report. This matches the Java adapters, whose `scanIteration`
/// propagates driver errors.
pub(crate) async fn collect_keys_across_nodes(
    source: &dyn ScanNodeSource,
    pattern: &str,
) -> Result<Vec<String>> {
    let mut found: HashSet<String> = HashSet::new();
    for node in source.scan_nodes().await? {
        found.extend(collect_keys_on_node(node.as_ref(), pattern).await?);
    }
    let mut out: Vec<String> = found.into_iter().collect();
    out.sort();
    Ok(out)
}

// ---------------------------------------------------------------------------
// Cluster topology (CLUSTER SLOTS)
// ---------------------------------------------------------------------------

/// A primary node address discovered through `CLUSTER SLOTS`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ClusterNodeAddr {
    pub(crate) host: String,
    pub(crate) port: u16,
}

impl ClusterNodeAddr {
    pub(crate) fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }

    /// `redis://` URL of this node, used for diagnostics (connection errors) only.
    ///
    /// IPv6 literals are bracketed so the URL stays unambiguous (`redis://[::1]:7000`).
    pub(crate) fn to_redis_url(&self) -> String {
        if self.host.contains(':') && !self.host.starts_with('[') {
            format!("redis://[{}]:{}", self.host, self.port)
        } else {
            format!("redis://{}:{}", self.host, self.port)
        }
    }
}

/// Parses a `CLUSTER SLOTS` reply into the **deduplicated** list of primaries.
///
/// Reply shape (Redis 6+ / 7): an array of slot ranges, each
/// `[start_slot, end_slot, primary_node, replica_node...]`, where a node is
/// `[host, port, node_id, ...]`. Only the primary (`parts[2]`) is read; `host` may be
/// empty / NIL — Redis then means "the address you used to reach me", so `fallback_host`
/// (the host of the connection that ran `CLUSTER SLOTS`) is substituted.
///
/// A primary covering several slot ranges appears once; the returned order is the order of
/// first appearance, which keeps the fan-out (and therefore the logs) deterministic.
pub(crate) fn parse_cluster_slots(
    reply: &redis::Value,
    fallback_host: &str,
) -> Result<Vec<ClusterNodeAddr>> {
    use redis::Value;

    let ranges = match reply {
        Value::Array(ranges) => ranges,
        other => {
            return Err(Error::Other(format!(
                "Redis CLUSTER SLOTS: expected an array reply, got {}",
                value_kind(other)
            )))
        }
    };

    let mut primaries: Vec<ClusterNodeAddr> = Vec::new();
    for (index, range) in ranges.iter().enumerate() {
        let parts = match range {
            Value::Array(parts) => parts,
            other => {
                return Err(Error::Other(format!(
                    "Redis CLUSTER SLOTS: slot range #{index} is {}, expected an array",
                    value_kind(other)
                )))
            }
        };
        if parts.len() < 3 {
            return Err(Error::Other(format!(
                "Redis CLUSTER SLOTS: slot range #{index} has {} entries, \
                 expected [start, end, primary]",
                parts.len()
            )));
        }
        let start = value_to_i64(&parts[0]).ok_or_else(|| {
            Error::Other(format!(
                "Redis CLUSTER SLOTS: slot range #{index} has a non-integer start slot"
            ))
        })?;
        let end = value_to_i64(&parts[1]).ok_or_else(|| {
            Error::Other(format!(
                "Redis CLUSTER SLOTS: slot range #{index} has a non-integer end slot"
            ))
        })?;
        if start > end {
            return Err(Error::Other(format!(
                "Redis CLUSTER SLOTS: slot range #{index} is inverted ({start} > {end})"
            )));
        }
        let node = match &parts[2] {
            Value::Array(node) if node.len() >= 2 => node,
            other => {
                return Err(Error::Other(format!(
                    "Redis CLUSTER SLOTS: slot range #{index} primary is {}, \
                     expected a [host, port] array",
                    value_kind(other)
                )))
            }
        };
        let host = reply_host(&node[0])
            .filter(|host| !host.is_empty())
            .unwrap_or_else(|| fallback_host.to_string());
        let port = value_to_i64(&node[1])
            .filter(|port| (1..=u16::MAX as i64).contains(port))
            .ok_or_else(|| {
                Error::Other(format!(
                    "Redis CLUSTER SLOTS: slot range #{index} primary has an invalid port"
                ))
            })? as u16;
        let primary = ClusterNodeAddr::new(host, port);
        if !primaries.contains(&primary) {
            primaries.push(primary);
        }
    }
    Ok(primaries)
}

/// Topology discovery seam: the primaries that own the key space.
#[async_trait]
pub(crate) trait ClusterTopologySource: Send + Sync + std::fmt::Debug {
    async fn primaries(&self) -> Result<Vec<ClusterNodeAddr>>;
}

/// Opens one **standalone** connection per discovered primary.
///
/// A separate seam (instead of connecting inline) keeps the fan-out testable offline and
/// lets the production implementation carry the seed connection's credentials / DB / TLS
/// settings into every per-node connection.
#[async_trait]
pub(crate) trait NodeConnectionFactory: Send + Sync + std::fmt::Debug {
    async fn connect(&self, node: &ClusterNodeAddr) -> Result<Arc<dyn ScanPager>>;
}

/// [`ScanNodeSource`] for a Redis Cluster: `CLUSTER SLOTS` → one pager per primary.
///
/// The topology is read on **every** scan (no caching), so a failover / resharding is
/// picked up by the next call — the same freshness Java gets from the Jedis cluster
/// client's slot map refresh.
#[derive(Debug)]
pub(crate) struct ClusterScanNodeSource<P, F> {
    topology: P,
    factory: F,
}

impl<P, F> ClusterScanNodeSource<P, F> {
    pub(crate) fn new(topology: P, factory: F) -> Self {
        Self { topology, factory }
    }
}

#[async_trait]
impl<P, F> ScanNodeSource for ClusterScanNodeSource<P, F>
where
    P: ClusterTopologySource + 'static,
    F: NodeConnectionFactory + 'static,
{
    /// Resolves one pager per primary.
    ///
    /// A primary that cannot be reached aborts the whole scan (see
    /// [`collect_keys_across_nodes`]): silently skipping it would leak keys on
    /// `clear_all_sessions` and hide sessions on `list_session_ids`, whereas Java's
    /// `scanIteration` propagates the driver error.
    async fn scan_nodes(&self) -> Result<Vec<Arc<dyn ScanPager>>> {
        let primaries = self.topology.primaries().await?;
        let mut nodes = Vec::with_capacity(primaries.len());
        for primary in &primaries {
            nodes.push(self.factory.connect(primary).await?);
        }
        Ok(nodes)
    }

    fn is_cluster(&self) -> bool {
        true
    }
}

/// A connection-managed async Redis client.
///
/// Cloning is cheap: the underlying [`ConnectionManager`] multiplexes a single
/// reconnecting connection, making this safe to share via `Arc` across the state store,
/// store, snapshot client and execution guard.
#[derive(Clone)]
pub struct RedisConnection {
    manager: ConnectionManager,
    /// How `find_keys_by_pattern` reaches the key space: one pager for a standalone /
    /// Sentinel node, or one per primary for a Redis Cluster
    /// ([`RedisConnection::connect_cluster`]).
    scan_source: Arc<dyn ScanNodeSource>,
    /// `Some` for a cluster connection: routes key-scoped commands to the primary that owns
    /// the key and follows `MOVED` / `ASK` redirects once
    /// ([`super::redirect::run_with_redirects`]).
    router: Option<Arc<ClusterRouter>>,
}

impl std::fmt::Debug for RedisConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisConnection").finish_non_exhaustive()
    }
}

impl RedisConnection {
    /// Connects to Redis using a URL such as `redis://localhost:6379` or `localhost:6379`.
    ///
    /// A bare `host:port` (no scheme) is normalised to `redis://host:port`.
    pub async fn connect(url: &str) -> Result<Self> {
        let client = redis::Client::open(normalize_url(url))
            .map_err(|e| Error::Other(format!("Invalid Redis URL: {e}")))?;
        let manager = ConnectionManager::new(client)
            .await
            .map_err(|e| map_err("connect", e))?;
        Ok(Self {
            scan_source: Arc::new(SingleNodeScanSource {
                manager: manager.clone(),
            }),
            manager,
            router: None,
        })
    }

    /// Connects to a **Redis Cluster** seed node and enables cluster-wide key enumeration
    /// **and slot routing**.
    ///
    /// Key-scoped commands start at the primary that owns the key's slot (learned from
    /// previous `MOVED` replies, else the seed node) and follow one `MOVED` / `ASK`
    /// redirect, so a resharding or migrating cluster stops returning errors to callers.
    /// [`find_keys_by_pattern`] additionally discovers every primary with `CLUSTER SLOTS`
    /// and runs one `SCAN` iteration per primary, matching Java's
    /// `UnifiedJedis.scanIteration` ("traverses every master in cluster mode").
    ///
    /// Topology is resolved lazily (each key enumeration, and on every redirect) so failover
    /// / resharding is picked up, and every per-primary connection reuses the seed's
    /// credentials, DB and TLS settings.
    pub async fn connect_cluster(seed_url: &str) -> Result<Self> {
        let client = redis::Client::open(normalize_url(seed_url))
            .map_err(|e| Error::Other(format!("Invalid Redis URL: {e}")))?;
        let manager = ConnectionManager::new(client.clone())
            .await
            .map_err(|e| map_err("connect", e))?;
        let topology = ClusterSlotsTopologySource {
            fallback_host: seed_host(client.get_connection_info().addr()),
            manager: manager.clone(),
        };
        let factory = SeedClientNodeFactory {
            client: client.clone(),
        };
        Ok(Self {
            scan_source: Arc::new(ClusterScanNodeSource::new(topology, factory)),
            router: Some(Arc::new(ClusterRouter::new(client, manager.clone()))),
            manager,
        })
    }

    /// Wraps an already-built [`ConnectionManager`].
    pub fn from_manager(manager: ConnectionManager) -> Self {
        Self {
            scan_source: Arc::new(SingleNodeScanSource {
                manager: manager.clone(),
            }),
            manager,
            router: None,
        }
    }

    /// `true` when this connection enumerates keys across every Cluster primary
    /// (built by [`connect_cluster`](Self::connect_cluster)).
    pub fn is_cluster_scan(&self) -> bool {
        self.scan_source.is_cluster()
    }

    /// Exposes the underlying connection manager for advanced/raw operations.
    pub fn manager(&self) -> &ConnectionManager {
        &self.manager
    }

    /// Creates a cheap clone sharing the same connection.
    pub fn clone_shared(&self) -> Self {
        self.clone()
    }

    /// Hands out this connection as a shared [`RedisClientAdapter`].
    ///
    /// Used by [`crate::RedisDistributedStore`] to wire the agent state store over the
    /// same connection.
    pub(crate) fn raw_adapter(&self) -> Arc<dyn RedisClientAdapter> {
        Arc::new(self.clone())
    }

    /// Hands out the crate-internal low-level command seam backed by the same
    /// connection, for the components that do not use [`RedisClientAdapter`]
    /// (`RedisRemoteSnapshotClient` / `RedisSandboxExecutionGuard`).
    ///
    /// A cluster connection also hands over its [`ClusterRouter`], so those two components
    /// follow slot ownership exactly like the state store does.
    pub(crate) fn raw_client(&self) -> Arc<dyn RedisCommands> {
        Arc::new(ManagerRedisCommands::with_router(
            self.manager.clone(),
            self.router.clone(),
        ))
    }

    /// Runs a key-scoped command, following `MOVED` / `ASK` redirects when this connection
    /// was built by [`connect_cluster`](Self::connect_cluster).
    ///
    /// A standalone / Sentinel connection (no router) sends straight to the single node, so
    /// the behaviour is unchanged there.
    pub(crate) async fn run_keyed(
        &self,
        key: &str,
        cmd: &redis::Cmd,
    ) -> Result<redis::Value> {
        match &self.router {
            Some(router) => router.run(key, cmd).await,
            None => self.run_on_connection(cmd).await,
        }
    }

    /// Runs a command that names no key (`PING`, `CLUSTER SLOTS`, an EVAL without keys).
    ///
    /// No key means no slot, so this always targets the seed connection: a cluster never
    /// redirects a keyless command.
    pub(crate) async fn run_unkeyed(&self, cmd: &redis::Cmd) -> Result<redis::Value> {
        self.run_on_connection(cmd).await
    }

    async fn run_on_connection(&self, cmd: &redis::Cmd) -> Result<redis::Value> {
        let mut conn = self.manager.clone();
        cmd.query_async(&mut conn).await.map_err(|e| map_err("command", e))
    }
}

#[async_trait]
impl RedisClientAdapter for RedisConnection {
    async fn set(&self, key: &str, value: &str) -> Result<()> {
        let mut cmd = redis::cmd("SET");
        cmd.arg(key).arg(value);
        let _: redis::Value = self.run_keyed(key, &cmd).await?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Option<String>> {
        let mut cmd = redis::cmd("GET");
        cmd.arg(key);
        let reply = self.run_keyed(key, &cmd).await?;
        decode_reply(&reply)
    }

    async fn right_push_list(&self, key: &str, value: &str) -> Result<()> {
        let mut cmd = redis::cmd("RPUSH");
        cmd.arg(key).arg(value);
        let _: redis::Value = self.run_keyed(key, &cmd).await?;
        Ok(())
    }

    async fn range_list(&self, key: &str, start: i64, end: i64) -> Result<Vec<String>> {
        let mut cmd = redis::cmd("LRANGE");
        cmd.arg(key).arg(start).arg(end);
        let reply = self.run_keyed(key, &cmd).await?;
        decode_reply(&reply)
    }

    async fn get_list_length(&self, key: &str) -> Result<i64> {
        let mut cmd = redis::cmd("LLEN");
        cmd.arg(key);
        let reply = self.run_keyed(key, &cmd).await?;
        decode_reply(&reply)
    }

    /// `DEL` for several keys.
    ///
    /// Redis Cluster rejects a multi-key command whose keys do not share a slot
    /// (`CROSSSLOT`). The callers here delete one session's keys, which the V1 key layout
    /// pins to a single slot (`{user/session}` hash tag); routing by the first key is
    /// therefore correct for both layouts — for V0, a cross-slot set would have been
    /// rejected by the cluster anyway, so no behaviour regresses.
    async fn delete_keys(&self, keys: &[String]) -> Result<()> {
        let Some(first) = keys.first() else {
            return Ok(());
        };
        let mut cmd = redis::cmd("DEL");
        for key in keys {
            cmd.arg(key);
        }
        let _: redis::Value = self.run_keyed(first, &cmd).await?;
        Ok(())
    }

    async fn add_to_set(&self, key: &str, member: &str) -> Result<()> {
        let mut cmd = redis::cmd("SADD");
        cmd.arg(key).arg(member);
        let _: redis::Value = self.run_keyed(key, &cmd).await?;
        Ok(())
    }

    async fn get_set_members(&self, key: &str) -> Result<Vec<String>> {
        let mut cmd = redis::cmd("SMEMBERS");
        cmd.arg(key);
        let reply = self.run_keyed(key, &cmd).await?;
        decode_reply(&reply)
    }

    async fn get_set_size(&self, key: &str) -> Result<i64> {
        let mut cmd = redis::cmd("SCARD");
        cmd.arg(key);
        let reply = self.run_keyed(key, &cmd).await?;
        decode_reply(&reply)
    }

    async fn key_exists(&self, key: &str) -> Result<bool> {
        let mut cmd = redis::cmd("EXISTS");
        cmd.arg(key);
        let reply = self.run_keyed(key, &cmd).await?;
        decode_reply(&reply)
    }

    /// Finds all keys matching a glob pattern using the `SCAN` cursor protocol.
    ///
    /// Implements the same contract as the Java adapters: loop `SCAN <cursor> MATCH
    /// <pattern> COUNT <n>` until the returned cursor is `0`, accumulating keys into a set
    /// (deduplicated) — never `KEYS`. The loop is shared with the cluster fan-out via
    /// [`collect_keys_across_nodes`]: a standalone connection contributes one node, a
    /// connection built by [`RedisConnection::connect_cluster`] contributes every primary.
    async fn find_keys_by_pattern(&self, pattern: &str) -> Result<Vec<String>> {
        collect_keys_across_nodes(self.scan_source.as_ref(), pattern).await
    }

    async fn eval_script(
        &self,
        script: &str,
        keys: &[String],
        args: &[String],
    ) -> Result<Option<i64>> {
        let mut cmd = redis::cmd("EVAL");
        cmd.arg(script).arg(keys.len());
        for k in keys {
            cmd.arg(k);
        }
        for a in args {
            cmd.arg(a);
        }
        // A script is routed by its first key (a cluster rejects a script whose keys span
        // slots). A keyless script can run anywhere, so it goes to the seed node.
        let reply = match keys.first() {
            Some(key) => self.run_keyed(key, &cmd).await?,
            None => self.run_unkeyed(&cmd).await?,
        };
        Ok(value_to_i64(&reply))
    }
}

// ---------------------------------------------------------------------------
// Production wiring
// ---------------------------------------------------------------------------

/// Decodes a raw reply into `T`, reporting a decode failure as a Redis error.
fn decode_reply<T: redis::FromRedisValue>(value: &redis::Value) -> Result<T> {
    T::from_redis_value(value.clone())
        .map_err(|e| Error::Other(format!("Redis reply decode: {e}")))
}

/// Routes key-scoped commands across a Redis Cluster.
///
/// The router starts at the primary that last claimed the key's slot (learned from `MOVED`
/// replies) and falls back to the seed connection, then follows **one** `MOVED` / `ASK`
/// redirect ([`super::redirect::run_with_redirects`]). Client-side slot routing is
/// deliberately hand-written instead of enabling the `redis` crate's `cluster` feature,
/// whose `cluster-async` client would replace the connection seam the whole crate is built
/// on.
pub(crate) struct ClusterRouter {
    /// Template for per-node connections (keeps AUTH / DB / TLS from the seed URL).
    client: redis::Client,
    /// The connection that ran `CLUSTER SLOTS` and serves keyless commands.
    seed: ConnectionManager,
    /// One lazily opened connection per primary, reused across commands.
    nodes: tokio::sync::Mutex<HashMap<ClusterNodeAddr, ConnectionManager>>,
    /// Slot -> primary mapping learned from `MOVED` replies.
    slots: Mutex<SlotCache>,
}

impl std::fmt::Debug for ClusterRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterRouter").finish_non_exhaustive()
    }
}

impl ClusterRouter {
    fn new(client: redis::Client, seed: ConnectionManager) -> Self {
        Self {
            client,
            seed,
            nodes: tokio::sync::Mutex::new(HashMap::new()),
            slots: Mutex::new(SlotCache::new()),
        }
    }

    /// Returns (and caches) a connection to `node`.
    async fn node_manager(&self, node: &ClusterNodeAddr) -> Result<ConnectionManager> {
        if let Some(existing) = self
            .nodes
            .lock()
            .await
            .get(node)
            .map(|manager| manager.clone())
        {
            return Ok(existing);
        }
        let info = self.client.get_connection_info();
        let addr = with_endpoint(info.addr(), node);
        let client = redis::Client::open(info.clone().set_addr(addr)).map_err(|e| {
            Error::Other(format!(
                "Invalid Redis cluster node {}: {e}",
                node.to_redis_url()
            ))
        })?;
        let manager = ConnectionManager::new(client).await.map_err(|e| {
            Error::Other(format!(
                "Redis cluster routing: cannot connect to {}: {e}",
                node.to_redis_url()
            ))
        })?;
        self.nodes
            .lock()
            .await
            .insert(node.clone(), manager.clone());
        Ok(manager)
    }

    /// Runs `cmd` for `key`, following at most one redirect and refreshing a stale slot map
    /// once.
    pub(crate) async fn run(&self, key: &str, cmd: &redis::Cmd) -> Result<redis::Value> {
        let slot = super::redirect::slot_for_key(key);
        let transport = RoutedCommand {
            router: self,
            cmd,
            slot,
        };
        super::redirect::run_with_redirects(&transport, &self.slots, slot).await
    }
}

/// One command attempt of a routed command: binds the router, the pending command and the
/// slot into the [`RedirectTransport`] seam the retry loop drives.
struct RoutedCommand<'a> {
    router: &'a ClusterRouter,
    cmd: &'a redis::Cmd,
    slot: u16,
}

#[async_trait]
impl RedirectTransport for RoutedCommand<'_> {
    async fn send(
        &self,
        target: Option<&ClusterNodeAddr>,
        asking: bool,
    ) -> std::result::Result<redis::Value, CommandFailure> {
        // Re-read the cached owner on *every* attempt: a stale-topology refresh evicts the
        // entry, and the retry must then start from the seed node instead of the demoted
        // primary that is still cached in a snapshot taken before the loop.
        let effective = target.cloned().or_else(|| {
            self.router
                .slots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(self.slot)
                .cloned()
        });
        let mut conn = match &effective {
            Some(node) => self
                .router
                .node_manager(node)
                .await
                .map_err(|e| CommandFailure::Other(e.to_string()))?,
            None => self.router.seed.clone(),
        };
        if asking {
            // Redis requires ASKING on the same connection, immediately before the command
            // that targets a key being migrated.
            let _: redis::Value = redis::cmd("ASKING")
                .query_async(&mut conn)
                .await
                .map_err(|e| CommandFailure::from_message(e.to_string()))?;
        }
        self.cmd
            .query_async(&mut conn)
            .await
            .map_err(|e| CommandFailure::from_message(e.to_string()))
    }
}

/// [`ScanPager`] backed by the shared [`ConnectionManager`] (one connected node).
#[derive(Debug)]
struct ConnectionScanPager {
    manager: ConnectionManager,
}

#[async_trait]
impl ScanPager for ConnectionScanPager {
    async fn scan_page(
        &self,
        cursor: u64,
        pattern: &str,
        count: usize,
    ) -> Result<(u64, Vec<String>)> {
        let mut conn = self.manager.clone();
        redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(pattern)
            .arg("COUNT")
            .arg(count)
            .query_async(&mut conn)
            .await
            .map_err(|e| map_err("SCAN", e))
    }
}

/// [`ScanNodeSource`] for a single-node topology (standalone / Sentinel primary).
#[derive(Debug)]
struct SingleNodeScanSource {
    manager: ConnectionManager,
}

#[async_trait]
impl ScanNodeSource for SingleNodeScanSource {
    async fn scan_nodes(&self) -> Result<Vec<Arc<dyn ScanPager>>> {
        Ok(vec![Arc::new(ConnectionScanPager {
            manager: self.manager.clone(),
        })])
    }
}

/// Production [`ClusterTopologySource`]: `CLUSTER SLOTS` on the seed connection.
///
/// `CLUSTER SLOTS` carries no key, so it never needs slot routing; running it on the seed
/// node is exactly what `redis-cli --cluster check` does.
#[derive(Debug)]
struct ClusterSlotsTopologySource {
    manager: ConnectionManager,
    /// Host of the seed connection, substituted for nodes that advertise an empty
    /// hostname ("the address you reached me on").
    fallback_host: String,
}

#[async_trait]
impl ClusterTopologySource for ClusterSlotsTopologySource {
    async fn primaries(&self) -> Result<Vec<ClusterNodeAddr>> {
        let mut conn = self.manager.clone();
        let reply: redis::Value = redis::cmd("CLUSTER")
            .arg("SLOTS")
            .query_async(&mut conn)
            .await
            .map_err(|e| map_err("CLUSTER SLOTS", e))?;
        parse_cluster_slots(&reply, &self.fallback_host)
    }
}

/// Production [`NodeConnectionFactory`]: one standalone [`ConnectionManager`] per primary.
///
/// The seed client is used as a **template** (`ConnectionInfo::set_addr`), so the node
/// connections inherit the seed's AUTH credentials, selected DB and TLS settings — a
/// naive `redis://host:port` URL would drop the password and fail with `NOAUTH` on an
/// authenticated cluster.
#[derive(Debug)]
struct SeedClientNodeFactory {
    client: redis::Client,
}

#[async_trait]
impl NodeConnectionFactory for SeedClientNodeFactory {
    async fn connect(&self, node: &ClusterNodeAddr) -> Result<Arc<dyn ScanPager>> {
        let info = self.client.get_connection_info();
        let addr = with_endpoint(info.addr(), node);
        let client = redis::Client::open(info.clone().set_addr(addr)).map_err(|e| {
            Error::Other(format!(
                "Invalid Redis cluster primary {}: {e}",
                node.to_redis_url()
            ))
        })?;
        let manager = ConnectionManager::new(client).await.map_err(|e| {
            Error::Other(format!(
                "Redis cluster scan: cannot connect to primary {}: {e}",
                node.to_redis_url()
            ))
        })?;
        Ok(Arc::new(ConnectionScanPager { manager }))
    }
}

/// Rebuilds a [`redis::ConnectionAddr`] for `node`, preserving the seed's flavour (plain
/// TCP vs TLS) so a TLS cluster keeps using TLS on every primary.
fn with_endpoint(base: &redis::ConnectionAddr, node: &ClusterNodeAddr) -> redis::ConnectionAddr {
    use redis::ConnectionAddr;
    match base {
        ConnectionAddr::TcpTls {
            insecure,
            tls_params,
            ..
        } => ConnectionAddr::TcpTls {
            host: node.host.clone(),
            port: node.port,
            insecure: *insecure,
            tls_params: tls_params.clone(),
        },
        // Unix sockets cannot be fanned out over a network, and any future variant has no
        // known host either: every primary gets a plain TCP endpoint.
        _ => ConnectionAddr::Tcp(node.host.clone(), node.port),
    }
}

/// Host of a seed connection, used when a node reports an empty hostname.
fn seed_host(addr: &redis::ConnectionAddr) -> String {
    match addr {
        redis::ConnectionAddr::Tcp(host, _) => host.clone(),
        redis::ConnectionAddr::TcpTls { host, .. } => host.clone(),
        _ => "127.0.0.1".to_string(),
    }
}

/// Normalises `host:port` (no scheme) to `redis://host:port`.
fn normalize_url(url: &str) -> String {
    if url.contains("://") {
        url.to_string()
    } else {
        format!("redis://{url}")
    }
}

/// Short human-readable description of a reply, for `CLUSTER SLOTS` shape errors.
fn value_kind(value: &redis::Value) -> &'static str {
    use redis::Value;
    match value {
        Value::Nil => "nil",
        Value::Int(_) => "an integer",
        Value::BulkString(_) => "a bulk string",
        Value::Array(_) => "an array",
        Value::SimpleString(_) => "a simple string",
        Value::Map(_) => "a map",
        Value::Set(_) => "a set",
        _ => "an unsupported value",
    }
}

/// Reads a node host from a `CLUSTER SLOTS` node array entry.
///
/// Accepts bulk/simple/verbatim strings; NIL and any other type surface as `None` so the
/// caller falls back to the seed host.
fn reply_host(value: &redis::Value) -> Option<String> {
    use redis::Value;
    match value {
        Value::BulkString(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        Value::SimpleString(text) => Some(text.clone()),
        Value::VerbatimString { text, .. } => Some(text.clone()),
        _ => None,
    }
}

/// Coerces a Redis integer reply into `i64`.
///
/// Redis returns bulk strings for numbers produced via `tostring(...)` (used by the
/// `PUT_SCRIPT`), so both `Int` and string `BulkString` replies are handled.
fn value_to_i64(value: &redis::Value) -> Option<i64> {
    use redis::Value;
    match value {
        Value::Int(i) => Some(*i),
        Value::BulkString(bytes) => std::str::from_utf8(bytes).ok()?.trim().parse().ok(),
        Value::SimpleString(s) => s.trim().parse().ok(),
        Value::Nil => None,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use redis::Value;

    fn page(cursor: u64, keys: &[&str]) -> Result<(u64, Vec<String>)> {
        Ok((cursor, keys.iter().map(|k| k.to_string()).collect()))
    }

    /// [`ScanPager`] replaying a fixed script of pages.
    ///
    /// Records every cursor / pattern / count it is asked for, so a test can prove the
    /// loop followed the cursor chain the server handed back instead of stopping early.
    #[derive(Debug)]
    struct ScriptedPager {
        pages: Mutex<VecDeque<Result<(u64, Vec<String>)>>>,
        cursors: Mutex<Vec<u64>>,
        patterns: Mutex<Vec<String>>,
        counts: Mutex<Vec<usize>>,
    }

    impl ScriptedPager {
        fn new(pages: Vec<Result<(u64, Vec<String>)>>) -> Arc<Self> {
            Arc::new(Self {
                pages: Mutex::new(pages.into()),
                cursors: Mutex::new(Vec::new()),
                patterns: Mutex::new(Vec::new()),
                counts: Mutex::new(Vec::new()),
            })
        }

        fn cursors(&self) -> Vec<u64> {
            self.cursors.lock().unwrap().clone()
        }

        fn patterns(&self) -> Vec<String> {
            self.patterns.lock().unwrap().clone()
        }

        fn counts(&self) -> Vec<usize> {
            self.counts.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ScanPager for ScriptedPager {
        async fn scan_page(
            &self,
            cursor: u64,
            pattern: &str,
            count: usize,
        ) -> Result<(u64, Vec<String>)> {
            self.cursors.lock().unwrap().push(cursor);
            self.patterns.lock().unwrap().push(pattern.to_string());
            self.counts.lock().unwrap().push(count);
            let next = self.pages.lock().unwrap().pop_front().unwrap_or_else(|| {
                panic!("pager called more often than scripted (cursor {cursor})")
            });
            next
        }
    }

    /// Multi-node [`ScanNodeSource`] (stands in for a Cluster primary list).
    #[derive(Debug)]
    struct FakeNodeSource {
        nodes: Vec<Arc<dyn ScanPager>>,
    }

    #[async_trait]
    impl ScanNodeSource for FakeNodeSource {
        async fn scan_nodes(&self) -> Result<Vec<Arc<dyn ScanPager>>> {
            Ok(self.nodes.clone())
        }
    }

    /// Node enumeration that fails (e.g. `CLUSTER SLOTS` unavailable).
    #[derive(Debug)]
    struct BrokenNodeSource;

    #[async_trait]
    impl ScanNodeSource for BrokenNodeSource {
        async fn scan_nodes(&self) -> Result<Vec<Arc<dyn ScanPager>>> {
            Err(Error::Other("topology unavailable".to_string()))
        }
    }

    fn assert_other_contains(err: Error, needle: &str) {
        match err {
            Error::Other(message) => assert!(
                message.contains(needle),
                "expected message to contain {needle:?}, got {message:?}"
            ),
            other => panic!("expected Error::Other, got {other:?}"),
        }
    }

    #[test]
    fn value_to_i64_handles_int_bulk_and_simple() {
        assert_eq!(value_to_i64(&Value::Int(7)), Some(7));
        assert_eq!(value_to_i64(&Value::BulkString(b"12".to_vec())), Some(12));
        assert_eq!(value_to_i64(&Value::SimpleString("3".to_string())), Some(3));
        assert_eq!(value_to_i64(&Value::Nil), None);
    }

    #[tokio::test]
    async fn scan_loop_aggregates_every_page() {
        let pager = ScriptedPager::new(vec![page(7, &["a", "b"]), page(0, &["c"])]);

        let keys = collect_keys_on_node(pager.as_ref(), "t:*").await.unwrap();

        assert_eq!(keys, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn scan_loop_follows_server_cursor_until_zero() {
        // Three pages: the loop must reuse the cursor the server returned (7 -> 42 -> 0)
        // instead of stopping while the cursor is still non-zero.
        let pager = ScriptedPager::new(vec![page(7, &["k1"]), page(42, &["k2"]), page(0, &["k3"])]);

        let keys = collect_keys_on_node(pager.as_ref(), "t:*").await.unwrap();

        assert_eq!(keys, vec!["k1", "k2", "k3"], "every page was aggregated");
        assert_eq!(pager.cursors(), vec![0, 7, 42], "cursor chain followed");
    }

    #[tokio::test]
    async fn scan_loop_uses_match_pattern_and_count_hint() {
        let pager = ScriptedPager::new(vec![page(0, &[])]);

        let keys = collect_keys_on_node(pager.as_ref(), "t:session:u1/*:_keys")
            .await
            .unwrap();

        assert!(keys.is_empty());
        assert_eq!(pager.patterns(), vec!["t:session:u1/*:_keys".to_string()]);
        assert_eq!(pager.counts(), vec![SCAN_COUNT]);
        assert_eq!(pager.cursors(), vec![0], "first page starts at cursor 0");
    }

    #[tokio::test]
    async fn scan_loop_deduplicates_keys_repeated_across_pages() {
        // `SCAN` may return the same key more than once (rehashing / resizing).
        let pager = ScriptedPager::new(vec![page(3, &["k1", "k2"]), page(0, &["k2", "k1", "k3"])]);

        let keys = collect_keys_on_node(pager.as_ref(), "*").await.unwrap();

        assert_eq!(keys, vec!["k1", "k2", "k3"], "no duplicates, sorted");
    }

    #[tokio::test]
    async fn scan_loop_propagates_page_failures() {
        let pager = ScriptedPager::new(vec![
            page(5, &["a"]),
            Err(Error::Other("SCAN boom".to_string())),
        ]);

        let err = collect_keys_on_node(pager.as_ref(), "*").await.unwrap_err();

        assert_other_contains(err, "SCAN boom");
        assert_eq!(
            pager.cursors(),
            vec![0, 5],
            "the failing page was the one after cursor 5"
        );
    }

    #[tokio::test]
    async fn scan_loop_returns_empty_for_an_empty_keyspace() {
        let pager = ScriptedPager::new(vec![page(0, &[])]);
        assert!(collect_keys_on_node(pager.as_ref(), "*")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn scan_fanout_visits_every_node_and_merges_results() {
        let node_a = ScriptedPager::new(vec![page(9, &["a"]), page(0, &["b"])]);
        let node_b = ScriptedPager::new(vec![page(0, &["c", "a"])]);
        let node_c = ScriptedPager::new(vec![page(0, &[])]);
        let source = FakeNodeSource {
            nodes: vec![node_a.clone(), node_b.clone(), node_c.clone()],
        };

        let keys = collect_keys_across_nodes(&source, "*").await.unwrap();

        assert_eq!(
            keys,
            vec!["a", "b", "c"],
            "per-node pages merged and deduplicated"
        );
        assert_eq!(node_a.cursors(), vec![0, 9], "node A paged to completion");
        assert_eq!(node_b.cursors(), vec![0], "node B was scanned too");
        assert_eq!(
            node_c.cursors(),
            vec![0],
            "an empty primary is still visited"
        );
    }

    #[tokio::test]
    async fn scan_fanout_propagates_a_failing_node() {
        // A partial result here would silently skip keys living on the failed primary
        // (leaked sessions on clear, missing sessions on list).
        let node_a = ScriptedPager::new(vec![page(0, &["a"])]);
        let node_b = ScriptedPager::new(vec![Err(Error::Other("node B down".to_string()))]);
        let source = FakeNodeSource {
            nodes: vec![node_a, node_b],
        };

        let err = collect_keys_across_nodes(&source, "*").await.unwrap_err();

        assert_other_contains(err, "node B down");
    }

    #[tokio::test]
    async fn scan_fanout_propagates_topology_failures() {
        let err = collect_keys_across_nodes(&BrokenNodeSource, "*")
            .await
            .unwrap_err();
        assert_other_contains(err, "topology unavailable");
    }

    #[tokio::test]
    async fn scan_fanout_over_an_empty_topology_is_empty() {
        let source = FakeNodeSource { nodes: Vec::new() };
        assert!(collect_keys_across_nodes(&source, "*")
            .await
            .unwrap()
            .is_empty());
    }

    // -----------------------------------------------------------------------
    // CLUSTER SLOTS topology parsing
    // -----------------------------------------------------------------------

    fn node(host: redis::Value, port: redis::Value) -> Value {
        Value::Array(vec![host, port, Value::BulkString(b"node-id".to_vec())])
    }

    fn bulk(text: &str) -> Value {
        Value::BulkString(text.as_bytes().to_vec())
    }

    fn slot_range(start: i64, end: i64, primary: Value, replicas: Vec<Value>) -> Value {
        let mut parts = vec![Value::Int(start), Value::Int(end), primary];
        parts.extend(replicas);
        Value::Array(parts)
    }

    fn addr(host: &str, port: u16) -> ClusterNodeAddr {
        ClusterNodeAddr::new(host, port)
    }

    #[test]
    fn cluster_slots_parses_every_primary_in_order() {
        let reply = Value::Array(vec![
            slot_range(
                0,
                5460,
                node(bulk("10.0.0.1"), Value::Int(7000)),
                vec![node(bulk("10.0.0.9"), Value::Int(7000))],
            ),
            slot_range(
                5461,
                10922,
                node(bulk("10.0.0.2"), Value::Int(7001)),
                vec![],
            ),
            slot_range(
                10923,
                16383,
                node(bulk("10.0.0.3"), Value::Int(7002)),
                vec![],
            ),
        ]);

        let primaries = parse_cluster_slots(&reply, "127.0.0.1").unwrap();

        assert_eq!(
            primaries,
            vec![
                addr("10.0.0.1", 7000),
                addr("10.0.0.2", 7001),
                addr("10.0.0.3", 7002)
            ],
            "replicas are ignored, primaries keep the reply order"
        );
    }

    #[test]
    fn cluster_slots_merges_a_primary_covering_several_ranges() {
        let reply = Value::Array(vec![
            slot_range(0, 100, node(bulk("10.0.0.1"), Value::Int(7000)), vec![]),
            slot_range(101, 200, node(bulk("10.0.0.2"), Value::Int(7001)), vec![]),
            slot_range(201, 300, node(bulk("10.0.0.1"), Value::Int(7000)), vec![]),
        ]);

        let primaries = parse_cluster_slots(&reply, "127.0.0.1").unwrap();

        assert_eq!(
            primaries,
            vec![addr("10.0.0.1", 7000), addr("10.0.0.2", 7001)],
            "deduplicated by (host, port), first appearance wins"
        );
    }

    #[test]
    fn cluster_slots_falls_back_to_the_seed_host_for_empty_hostnames() {
        // Redis reports an empty / NIL hostname when a node does not announce an address;
        // the documented meaning is "the address you reached me on".
        let reply = Value::Array(vec![
            slot_range(0, 5460, node(bulk(""), Value::Int(7000)), vec![]),
            slot_range(5461, 10922, node(Value::Nil, Value::Int(7001)), vec![]),
            slot_range(
                10923,
                16383,
                node(bulk("10.0.0.3"), Value::Int(7002)),
                vec![],
            ),
        ]);

        let primaries = parse_cluster_slots(&reply, "127.0.0.1").unwrap();

        assert_eq!(
            primaries,
            vec![
                addr("127.0.0.1", 7000),
                addr("127.0.0.1", 7001),
                addr("10.0.0.3", 7002)
            ],
            "empty and NIL hostnames both fall back to the seed host"
        );
    }

    #[test]
    fn cluster_slots_accepts_ipv6_hosts_and_string_ports() {
        // Proxies / older servers may report the port as a bulk string.
        let reply = Value::Array(vec![
            slot_range(0, 8191, node(bulk("::1"), bulk("7000")), vec![]),
            slot_range(
                8192,
                16383,
                node(
                    Value::SimpleString("redis-1.local".to_string()),
                    Value::Int(7001),
                ),
                vec![],
            ),
        ]);

        let primaries = parse_cluster_slots(&reply, "127.0.0.1").unwrap();

        assert_eq!(
            primaries,
            vec![addr("::1", 7000), addr("redis-1.local", 7001)]
        );
        assert_eq!(
            primaries[0].to_redis_url(),
            "redis://[::1]:7000",
            "IPv6 literals are bracketed, so the URL stays parseable"
        );
        assert_eq!(primaries[1].to_redis_url(), "redis://redis-1.local:7001");
    }

    #[test]
    fn cluster_slots_over_an_empty_topology_is_empty() {
        assert!(parse_cluster_slots(&Value::Array(Vec::new()), "127.0.0.1")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn cluster_slots_rejects_a_non_array_reply() {
        let err = parse_cluster_slots(&Value::Int(3), "127.0.0.1").unwrap_err();
        assert_other_contains(err, "expected an array reply");
    }

    #[test]
    fn cluster_slots_rejects_a_truncated_range() {
        let reply = Value::Array(vec![Value::Array(vec![Value::Int(0), Value::Int(100)])]);

        let err = parse_cluster_slots(&reply, "127.0.0.1").unwrap_err();

        assert_other_contains(err, "expected [start, end, primary]");
    }

    #[test]
    fn cluster_slots_rejects_non_integer_slot_bounds() {
        let reply = Value::Array(vec![Value::Array(vec![
            bulk("zero"),
            Value::Int(100),
            node(bulk("10.0.0.1"), Value::Int(7000)),
        ])]);

        let err = parse_cluster_slots(&reply, "127.0.0.1").unwrap_err();

        assert_other_contains(err, "non-integer start slot");
    }

    #[test]
    fn cluster_slots_rejects_an_inverted_slot_range() {
        let reply = Value::Array(vec![slot_range(
            5461,
            0,
            node(bulk("10.0.0.1"), Value::Int(7000)),
            vec![],
        )]);

        let err = parse_cluster_slots(&reply, "127.0.0.1").unwrap_err();

        assert_other_contains(err, "inverted");
    }

    #[test]
    fn cluster_slots_rejects_a_truncated_primary_entry() {
        let reply = Value::Array(vec![slot_range(
            0,
            100,
            Value::Array(vec![bulk("10.0.0.1")]),
            vec![],
        )]);

        let err = parse_cluster_slots(&reply, "127.0.0.1").unwrap_err();

        assert_other_contains(err, "expected a [host, port] array");
    }

    #[test]
    fn cluster_slots_rejects_an_invalid_primary_port() {
        for port in [Value::Int(0), bulk("not-a-port")] {
            let reply = Value::Array(vec![slot_range(
                0,
                100,
                node(bulk("10.0.0.1"), port),
                vec![],
            )]);

            let err = parse_cluster_slots(&reply, "127.0.0.1").unwrap_err();

            assert_other_contains(err, "invalid port");
        }
    }

    // -----------------------------------------------------------------------
    // Cluster fan-out
    // -----------------------------------------------------------------------

    /// [`ClusterTopologySource`] replaying a fixed primary list.
    #[derive(Debug, Clone)]
    struct FakeTopology {
        primaries: Vec<ClusterNodeAddr>,
        error: Option<&'static str>,
        calls: Arc<AtomicUsize>,
    }

    impl FakeTopology {
        fn new(primaries: Vec<ClusterNodeAddr>) -> Self {
            Self {
                primaries,
                error: None,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn failing(message: &'static str) -> Self {
            Self {
                primaries: Vec::new(),
                error: Some(message),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ClusterTopologySource for FakeTopology {
        async fn primaries(&self) -> Result<Vec<ClusterNodeAddr>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(message) = self.error {
                return Err(Error::Other(message.to_string()));
            }
            Ok(self.primaries.clone())
        }
    }

    /// [`NodeConnectionFactory`] handing out scripted pagers per primary address.
    #[derive(Debug, Clone, Default)]
    struct FakeNodeFactory {
        pagers: Arc<Mutex<HashMap<(String, u16), Arc<dyn ScanPager>>>>,
        fail_on: Option<ClusterNodeAddr>,
        connected: Arc<Mutex<Vec<ClusterNodeAddr>>>,
    }

    impl FakeNodeFactory {
        fn new(pagers: Vec<(ClusterNodeAddr, Arc<dyn ScanPager>)>) -> Self {
            let mut map: HashMap<(String, u16), Arc<dyn ScanPager>> = HashMap::new();
            for (node, pager) in pagers {
                map.insert((node.host.clone(), node.port), pager);
            }
            Self {
                pagers: Arc::new(Mutex::new(map)),
                fail_on: None,
                connected: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn failing_on(mut self, node: ClusterNodeAddr) -> Self {
            self.fail_on = Some(node);
            self
        }

        fn connected(&self) -> Vec<ClusterNodeAddr> {
            self.connected.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl NodeConnectionFactory for FakeNodeFactory {
        async fn connect(&self, node: &ClusterNodeAddr) -> Result<Arc<dyn ScanPager>> {
            self.connected.lock().unwrap().push(node.clone());
            if self.fail_on.as_ref() == Some(node) {
                return Err(Error::Other(format!(
                    "primary {} is down",
                    node.to_redis_url()
                )));
            }
            self.pagers
                .lock()
                .unwrap()
                .get(&(node.host.clone(), node.port))
                .cloned()
                .ok_or_else(|| {
                    Error::Other(format!("no scripted pager for {}", node.to_redis_url()))
                })
        }
    }

    #[tokio::test]
    async fn cluster_fanout_scans_every_primary_and_merges() {
        let node_a = addr("10.0.0.1", 7000);
        let node_b = addr("10.0.0.2", 7001);
        let node_c = addr("10.0.0.3", 7002);
        let pager_a = ScriptedPager::new(vec![page(4, &["a"]), page(0, &["b"])]);
        let pager_b = ScriptedPager::new(vec![page(0, &["c", "a"])]);
        let pager_c = ScriptedPager::new(vec![page(0, &[])]);
        let topology = FakeTopology::new(vec![node_a.clone(), node_b.clone(), node_c.clone()]);
        let factory = FakeNodeFactory::new(vec![
            (node_a, pager_a.clone()),
            (node_b, pager_b.clone()),
            (node_c, pager_c.clone()),
        ]);
        let source = ClusterScanNodeSource::new(topology.clone(), factory.clone());

        let keys = collect_keys_across_nodes(&source, "*").await.unwrap();

        assert_eq!(
            keys,
            vec!["a", "b", "c"],
            "one full SCAN iteration per primary, merged and deduplicated"
        );
        assert_eq!(
            factory.connected(),
            vec![
                addr("10.0.0.1", 7000),
                addr("10.0.0.2", 7001),
                addr("10.0.0.3", 7002)
            ],
            "one standalone connection per discovered primary"
        );
        assert_eq!(
            pager_a.cursors(),
            vec![0, 4],
            "primary A paged to completion"
        );
        assert_eq!(pager_b.cursors(), vec![0], "primary B was scanned");
        assert_eq!(pager_c.cursors(), vec![0], "an empty primary is visited");
        assert_eq!(topology.calls(), 1);
        assert!(source.is_cluster());
    }

    #[tokio::test]
    async fn cluster_fanout_propagates_a_primary_connection_failure() {
        // Skipping the unreachable primary would silently hide its keys.
        let node_a = addr("10.0.0.1", 7000);
        let node_b = addr("10.0.0.2", 7001);
        let node_c = addr("10.0.0.3", 7002);
        let pager_a = ScriptedPager::new(vec![page(0, &["a"])]);
        let pager_c = ScriptedPager::new(vec![page(0, &["c"])]);
        let topology = FakeTopology::new(vec![node_a.clone(), node_b.clone(), node_c.clone()]);
        let factory = FakeNodeFactory::new(vec![(node_a, pager_a), (node_c, pager_c.clone())])
            .failing_on(node_b);
        let source = ClusterScanNodeSource::new(topology, factory.clone());

        let err = collect_keys_across_nodes(&source, "*").await.unwrap_err();

        assert_other_contains(err, "redis://10.0.0.2:7001");
        assert_eq!(
            factory.connected(),
            vec![addr("10.0.0.1", 7000), addr("10.0.0.2", 7001)],
            "the scan aborts at the first unreachable primary"
        );
        assert!(pager_c.cursors().is_empty(), "primary C was never scanned");
    }

    #[tokio::test]
    async fn cluster_fanout_propagates_topology_failure() {
        let factory = FakeNodeFactory::default();
        let source = ClusterScanNodeSource::new(
            FakeTopology::failing("CLUSTER SLOTS disabled"),
            factory.clone(),
        );

        let err = collect_keys_across_nodes(&source, "*").await.unwrap_err();

        assert_other_contains(err, "CLUSTER SLOTS disabled");
        assert!(
            factory.connected().is_empty(),
            "no node is contacted when the topology is unavailable"
        );
    }

    #[tokio::test]
    async fn cluster_fanout_over_an_empty_topology_connects_no_node() {
        // A cluster with no slots assigned owns no keys: report an empty key set instead of
        // silently scanning the seed node (that would be a partial view of a real cluster).
        let factory = FakeNodeFactory::default();
        let source = ClusterScanNodeSource::new(FakeTopology::new(Vec::new()), factory.clone());

        let keys = collect_keys_across_nodes(&source, "*").await.unwrap();

        assert!(keys.is_empty());
        assert!(factory.connected().is_empty());
    }

    #[tokio::test]
    async fn cluster_fanout_rereads_the_topology_on_every_scan() {
        // Failover / resharding must be picked up without rebuilding the connection.
        let node_a = addr("10.0.0.1", 7000);
        let pager_a = ScriptedPager::new(vec![page(0, &["a"]), page(0, &["a"])]);
        let topology = FakeTopology::new(vec![node_a.clone()]);
        let factory = FakeNodeFactory::new(vec![(node_a, pager_a)]);
        let source = ClusterScanNodeSource::new(topology.clone(), factory);

        collect_keys_across_nodes(&source, "*").await.unwrap();
        collect_keys_across_nodes(&source, "*").await.unwrap();

        assert_eq!(topology.calls(), 2, "topology is not cached");
    }

    #[test]
    fn cluster_node_endpoint_keeps_the_seed_connection_flavour() {
        let plain = with_endpoint(
            &redis::ConnectionAddr::Tcp("127.0.0.1".to_string(), 6379),
            &addr("10.0.0.2", 7001),
        );
        assert_eq!(
            plain,
            redis::ConnectionAddr::Tcp("10.0.0.2".to_string(), 7001)
        );

        let tls = with_endpoint(
            &redis::ConnectionAddr::TcpTls {
                host: "seed".to_string(),
                port: 6379,
                insecure: true,
                tls_params: None,
            },
            &addr("10.0.0.3", 7002),
        );
        match tls {
            redis::ConnectionAddr::TcpTls {
                host,
                port,
                insecure,
                ..
            } => {
                assert_eq!(host, "10.0.0.3");
                assert_eq!(port, 7002);
                assert!(insecure, "TLS mode is inherited from the seed");
            }
            other => panic!("expected a TLS endpoint, got {other:?}"),
        }

        let from_unix = with_endpoint(
            &redis::ConnectionAddr::Unix(std::path::PathBuf::from("/tmp/redis.sock")),
            &addr("10.0.0.4", 7003),
        );
        assert_eq!(
            from_unix,
            redis::ConnectionAddr::Tcp("10.0.0.4".to_string(), 7003),
            "a unix-socket seed still yields TCP connections for network primaries"
        );
    }

    #[test]
    fn cluster_seed_host_prefers_the_connected_host() {
        assert_eq!(
            seed_host(&redis::ConnectionAddr::Tcp("10.9.9.9".to_string(), 6379)),
            "10.9.9.9"
        );
        assert_eq!(
            seed_host(&redis::ConnectionAddr::Unix(std::path::PathBuf::from(
                "/tmp/redis.sock"
            ))),
            "127.0.0.1"
        );
    }

    #[test]
    fn normalize_url_adds_a_missing_scheme() {
        assert_eq!(normalize_url("localhost:6379"), "redis://localhost:6379");
        assert_eq!(normalize_url("rediss://cache:6380"), "rediss://cache:6380");
    }
}
