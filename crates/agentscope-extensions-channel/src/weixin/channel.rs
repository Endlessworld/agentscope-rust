//! `WeixinChannel` — the Personal Weixin (个人微信 / iLink) channel adapter.
//!
//! Rust port of `io.agentscope.extensions.channel.weixin.WeixinChannel`. Routing/gateway wiring lives
//! in the harness [`PlatformCtx`], so this type holds the Weixin-specific collaborators and
//! implements `deliver_reply`; the blanket impls in
//! `agentscope_harness::agent::gateway::platform_channel` then make it a routable `Channel` and a
//! `ChannelSink`.
//!
//! ## Consumer model
//!
//! iLink is a **long poll**, not a webhook, so the adapter owns a consumer loop instead of a
//! callback controller:
//!
//! 1. acquire the account-scoped lease (`one active consumer per account`);
//! 2. announce the consumer (`notifystart`);
//! 3. loop: `getupdates` → accept the batch with the next cursor → drain the inbox;
//! 4. on the way out, `notifystop` (only while the lease is still current) and release the lease.
//!
//! ## Design decisions (vs. Java)
//!
//! - Java's `Sinks.Empty` cancellation aborts the *in-flight* turn when the lease is lost. Rust's
//!   `PlatformChannel::dispatch` has no cancellation seam, so a lost lease suppresses the **send**
//!   (checked immediately before every outbound call, mirroring the Java `beforeSend` hook) but
//!   cannot abort a turn already running. The observable guarantee kept is the one the Java module
//!   states as its contract: a late reply is never delivered by a stale consumer.
//! - Java's `Gateway` abstraction does not exist here; `PlatformCtx::run_turn` plays its role, so
//!   the consumer routes through the shared harness router exactly like the other adapters.
//! - Java keeps `MAX_DISPATCH_ATTEMPTS` and the retry counter inside the lease session; the same
//!   counter is kept per session here so a new owner starts a fresh budget.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use agentscope_core::error::Result as CoreResult;
use agentscope_core::message::Msg;
use agentscope_harness::agent::{ChannelConfig, OutboundAddress, PlatformChannel, PlatformCtx};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::credentials::{FixedCredentialProvider, WeixinCredentialProvider};
use super::dto::{WeixinMessage, WeixinUpdatesResponse};
use super::error::{core_error_kind, to_core_error, WeixinError, WeixinResult};
use super::inbound::{context_token_of, WeixinInboundMapper};
use super::listener::{NoopWeixinRuntimeListener, WeixinRuntimeListener};
use super::outbound::{next_client_id, peer_of, WeixinOutboundClient};
use super::properties::WeixinChannelProperties;
use super::state_store::{WeixinInboxClaim, WeixinInboxMessage, WeixinLease, WeixinStateStore};
use crate::common::{to_core_error as channel_to_core_error, BotLoopGuard, ChannelCore, ChannelRegistry, HttpTransport, IdempotencyStore, Result};

/// Channel type name, mirroring `WeixinChannel.TYPE`.
pub const CHANNEL_TYPE: &str = "weixin";

/// Dispatch attempts per inbox message during one lease tenure. A new owner starts a fresh budget;
/// within a tenure this stops a message whose dispatch keeps failing from re-running the agent
/// forever.
pub const MAX_DISPATCH_ATTEMPTS: i32 = 3;

/// Typed registry of `WeixinChannel` handles, keyed by `channel_id`.
pub type WeixinChannelRegistry = ChannelRegistry<WeixinChannel>;

/// One lease tenure: the fencing token plus the per-attempt dispatch counters and liveness flag.
pub struct LeaseSession {
    lease: WeixinLease,
    valid: AtomicBool,
    /// Only the consumer task mutates this map.
    dispatch_failures: Mutex<HashMap<String, i32>>,
}

impl std::fmt::Debug for LeaseSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseSession")
            .field("lease", &self.lease)
            .field("valid", &self.valid.load(Ordering::Relaxed))
            .finish()
    }
}

impl LeaseSession {
    fn new(lease: WeixinLease) -> Self {
        Self {
            lease,
            valid: AtomicBool::new(true),
            dispatch_failures: Mutex::new(HashMap::new()),
        }
    }

    /// The fencing token handed to the state store.
    pub fn lease(&self) -> &WeixinLease {
        &self.lease
    }

    /// Whether this tenure is still allowed to talk to the provider.
    pub fn is_valid(&self) -> bool {
        self.valid.load(Ordering::SeqCst)
    }

    fn invalidate(&self) {
        self.valid.store(false, Ordering::SeqCst);
    }

    fn record_dispatch_failure(&self, message_id: &str) -> i32 {
        let mut failures = self
            .dispatch_failures
            .lock()
            .expect("lease session mutex poisoned");
        let entry = failures.entry(message_id.to_string()).or_insert(0);
        *entry += 1;
        *entry
    }

    fn clear_dispatch_failure(&self, message_id: &str) {
        self.dispatch_failures
            .lock()
            .expect("lease session mutex poisoned")
            .remove(message_id);
    }
}

#[derive(Debug, Default)]
struct Runtime {
    active: Option<Arc<LeaseSession>>,
    running: bool,
    started: bool,
    backoff_ms: i64,
}

/// Weixin channel adapter: shared routing/gateway state plus Weixin-specific collaborators.
pub struct WeixinChannel {
    core: ChannelCore,
    properties: WeixinChannelProperties,
    mapper: WeixinInboundMapper,
    outbound: Arc<WeixinOutboundClient>,
    state_store: Arc<dyn WeixinStateStore>,
    listener: Arc<dyn WeixinRuntimeListener>,
    /// Identifies this process as the account-lease holder.
    lease_holder: String,
    runtime: RwLock<Runtime>,
}

impl std::fmt::Debug for WeixinChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeixinChannel")
            .field("channel_id", &self.core.channel_id())
            .field("account_id", &self.properties.account_id)
            .field("running", &self.is_running())
            .finish()
    }
}

impl WeixinChannel {
    /// Assemble a channel from host-supplied seams.
    pub fn create(
        channel_id: impl Into<String>,
        config: ChannelConfig,
        properties: WeixinChannelProperties,
        credentials: Arc<dyn WeixinCredentialProvider>,
        state_store: Arc<dyn WeixinStateStore>,
        listener: Arc<dyn WeixinRuntimeListener>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        let channel_id = channel_id.into();
        let mapper = WeixinInboundMapper::new(
            channel_id.clone(),
            properties.account_id.clone(),
            // `ilink_user_id` identifies the person who scanned the QR code, so peer filtering uses
            // the bot/account identity instead; see the module notes in `inbound.rs`.
            properties.account_id.clone(),
        );
        let outbound = Arc::new(WeixinOutboundClient::new(
            properties.clone(),
            credentials,
            transport,
        ));
        Self {
            core: ChannelCore::with_components(
                channel_id,
                config,
                IdempotencyStore::with_defaults(),
                BotLoopGuard::with_defaults(),
            ),
            properties,
            mapper,
            outbound,
            state_store,
            listener,
            lease_holder: next_client_id(),
            runtime: RwLock::new(Runtime {
                backoff_ms: 1_000,
                ..Runtime::default()
            }),
        }
    }

    /// Build from the channel property map, mirroring the Java `fromProperties` factory.
    ///
    /// Credentials and runtime state stay in this process: the Java factory logs the same warning,
    /// and production hosts should call [`WeixinChannel::create`] with a durable state store.
    pub fn from_json(
        channel_id: &str,
        routing: ChannelConfig,
        props: &Value,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self> {
        let bot_token = crate::common::props::get_str(props, "botToken").unwrap_or_default();
        if bot_token.trim().is_empty() {
            return Err(crate::common::ChannelError::Config(
                "weixin.botToken is required by the standalone properties factory".to_string(),
            ));
        }
        let properties = WeixinChannelProperties::from_json(channel_id, props)?;
        let credentials = FixedCredentialProvider::from_token(bot_token)?.shared();
        Ok(Self::create(
            channel_id,
            routing,
            properties,
            credentials,
            Arc::new(super::state_store::in_memory_store()),
            Arc::new(NoopWeixinRuntimeListener),
            transport,
        ))
    }

    pub fn properties(&self) -> &WeixinChannelProperties {
        &self.properties
    }

    pub fn mapper(&self) -> &WeixinInboundMapper {
        &self.mapper
    }

    pub fn outbound(&self) -> &WeixinOutboundClient {
        &self.outbound
    }

    pub fn state_store(&self) -> &Arc<dyn WeixinStateStore> {
        &self.state_store
    }

    /// The routing/gateway state plus the idempotency / bot-loop guards.
    pub fn core(&self) -> &ChannelCore {
        &self.core
    }

    pub fn is_running(&self) -> bool {
        self.runtime
            .read()
            .expect("weixin runtime lock poisoned")
            .running
    }

    /// The active lease session, if this instance currently owns the account.
    pub fn active_session(&self) -> Option<Arc<LeaseSession>> {
        self.runtime
            .read()
            .expect("weixin runtime lock poisoned")
            .active
            .clone()
    }

    /// Acquire the account lease and publish it as this instance's active session.
    ///
    /// This is exactly what the consumer loop does first, and it is the only way a host (or a test)
    /// makes the channel able to deliver replies: `deliver_reply` refuses to talk to iLink without
    /// an active, current lease. Returns `None` while another holder owns the account.
    pub fn begin_consuming(&self) -> WeixinResult<Option<Arc<LeaseSession>>> {
        let Some(lease) = self.state_store.acquire_lease(
            &self.properties.account_id,
            &self.lease_holder,
            self.properties.lease_ms,
        ) else {
            return Ok(None);
        };
        let session = Arc::new(LeaseSession::new(lease));
        {
            let mut runtime = self.runtime.write().expect("weixin runtime lock poisoned");
            runtime.active = Some(session.clone());
            runtime.running = true;
            runtime.started = true;
            runtime.backoff_ms = 1_000;
        }
        self.notify("lease acquired", |listener| {
            listener.on_lease_acquired(&self.properties.account_id, session.lease())
        });
        Ok(Some(session))
    }

    /// Release the active session: fence it, ask the provider to stop, and drop the lease.
    pub async fn end_consuming(&self, session: &Arc<LeaseSession>) {
        session.invalidate();
        {
            let mut runtime = self.runtime.write().expect("weixin runtime lock poisoned");
            if runtime
                .active
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, session))
            {
                runtime.active = None;
                runtime.running = false;
            }
        }
        // Stop is an external side effect too: a stale consumer must not stop its successor.
        if self
            .state_store
            .is_lease_current(&self.properties.account_id, session.lease())
        {
            if let Err(error) = self.outbound.notify_stop().await {
                tracing::debug!(error = %error.safe_message(), "weixin notify_stop failed");
            }
        }
        self.state_store
            .release_lease(&self.properties.account_id, session.lease());
        self.notify("stopped", |listener| {
            listener.on_stopped(&self.properties.account_id)
        });
    }

    /// Long-lived consumer: acquire, announce, poll, drain, and back off on failure.
    ///
    /// Mirrors the Java `poll` / `consume` pair. Returns when the channel is stopped or credentials
    /// are rejected.
    pub async fn run_consumer(self: Arc<Self>) {
        loop {
            if !self.is_running() {
                return;
            }
            let session = match self.begin_consuming() {
                Ok(Some(session)) => session,
                Ok(None) => {
                    // Another holder owns the account: keep trying to take it over.
                    let wait = (self.properties.lease_ms / 3).clamp(10, 1_000);
                    tokio::time::sleep(std::time::Duration::from_millis(wait as u64)).await;
                    continue;
                }
                Err(error) => {
                    tracing::warn!(error = %error.safe_message(), "weixin lease acquisition failed");
                    return;
                }
            };

            let outcome = self.consume(&session).await;
            self.end_consuming(&session).await;
            if matches!(outcome, ConsumeOutcome::CredentialRejected) {
                let mut runtime = self.runtime.write().expect("weixin runtime lock poisoned");
                runtime.running = false;
                return;
            }
            if let Some(reason) = outcome.safe_reason() {
                tracing::warn!(
                    account = %self.properties.account_id,
                    reason = %reason,
                    "weixin consumer backoff"
                );
            }
            if !self.is_running() {
                return;
            }
            let backoff = {
                let mut runtime = self.runtime.write().expect("weixin runtime lock poisoned");
                runtime.backoff_ms = (runtime.backoff_ms * 2).min(self.properties.max_backoff_ms);
                runtime.backoff_ms
            };
            tokio::time::sleep(std::time::Duration::from_millis(backoff as u64)).await;
        }
    }

    /// One lease tenure: announce to the provider, then poll and drain until the lease is lost.
    async fn consume(&self, session: &Arc<LeaseSession>) -> ConsumeOutcome {
        if let Err(error) = self.require_lease(session) {
            return ConsumeOutcome::LeaseLost(error);
        }
        match self.outbound.notify_start().await {
            Ok(()) => {}
            Err(error) if error.is_credential_rejected() => {
                self.notify_credential_rejected(&error);
                return ConsumeOutcome::CredentialRejected;
            }
            Err(error) => {
                self.notify("transient failure", |listener| {
                    listener.on_transient_failure(&self.properties.account_id, &error.safe_message())
                });
                // The provider session never started: leave the lease so the outer loop backs off,
                // reacquires and retries startup.
                return ConsumeOutcome::Transient(
                    WeixinError::operation("Weixin provider session did not start"),
                );
            }
        }
        self.notify("running", |listener| {
            listener.on_running(&self.properties.account_id)
        });

        while self.is_running() && session.is_valid() {
            // Poll and drain fail independently: a provider that stops answering must not freeze
            // messages this consumer already accepted, and a message that cannot be dispatched must
            // not stop the cursor from advancing.
            if let Err(error) = self.require_lease(session) {
                return ConsumeOutcome::LeaseLost(error);
            }
            match self.poll_once(session).await {
                Ok(()) => {}
                Err(error) if error.is_credential_rejected() => {
                    self.notify_credential_rejected(&error);
                    return ConsumeOutcome::CredentialRejected;
                }
                Err(error) => return ConsumeOutcome::Transient(error),
            }
            if let Err(error) = self.require_lease(session) {
                return ConsumeOutcome::LeaseLost(error);
            }
            match self.drain_inbox(session).await {
                Ok(()) => {}
                Err(error) if error.is_credential_rejected() => {
                    self.notify_credential_rejected(&error);
                    return ConsumeOutcome::CredentialRejected;
                }
                Err(error) => return ConsumeOutcome::Transient(error),
            }
            self.notify("recovered", |listener| {
                listener.on_recovered(&self.properties.account_id)
            });
        }
        ConsumeOutcome::LeaseLost(WeixinError::operation("Weixin consumer stopped"))
    }

    /// One long poll and the batch it accepted.
    pub async fn poll_once(&self, session: &Arc<LeaseSession>) -> WeixinResult<()> {
        self.require_lease(session)?;
        let cursor = self.state_store.load_cursor(&self.properties.account_id);
        let response = self.outbound.updates(&cursor).await?;
        self.require_lease(session)?;
        if response.is_credential_rejected() {
            return Err(WeixinError::credential_rejected("getupdates", -14));
        }
        if response.is_failure() {
            return Err(WeixinError::operation("iLink polling failed"));
        }
        let messages = inbox_messages(&response.messages());
        let next_cursor = response.get_updates_buf.clone().unwrap_or_default();
        if !self.state_store.accept_batch(
            &self.properties.account_id,
            session.lease(),
            &next_cursor,
            &messages,
        ) {
            session.invalidate();
            return Err(WeixinError::operation("Weixin lease lost"));
        }
        Ok(())
    }

    /// Drain the inbox one claim at a time, applying the Java retry budget.
    pub async fn drain_inbox(&self, session: &Arc<LeaseSession>) -> WeixinResult<()> {
        loop {
            if !self.is_running() || !session.is_valid() {
                return Ok(());
            }
            self.require_lease(session)?;
            let claim_ms = self.properties.dispatch_timeout_ms + self.properties.lease_ms;
            let claims = self.state_store.claim_messages(
                &self.properties.account_id,
                session.lease(),
                1,
                claim_ms,
            );
            let Some(claim) = claims.into_iter().next() else {
                return Ok(());
            };
            match self.process_claim(&claim, session).await {
                Ok(()) => {
                    if !self.state_store.complete_message(
                        &self.properties.account_id,
                        session.lease(),
                        &claim,
                    ) {
                        session.invalidate();
                        return Err(WeixinError::operation("Weixin inbox completion fenced"));
                    }
                    session.clear_dispatch_failure(&claim.message_id);
                }
                Err(error) if error.is_credential_rejected() => {
                    // Reauthorization can recover this message: a rejected account credential is not
                    // a poison message and must not consume its dispatch budget.
                    self.state_store.fail_message(
                        &self.properties.account_id,
                        session.lease(),
                        &claim,
                    );
                    return Err(error);
                }
                Err(error) => {
                    if session.record_dispatch_failure(&claim.message_id) >= MAX_DISPATCH_ATTEMPTS {
                        session.clear_dispatch_failure(&claim.message_id);
                        if self.state_store.abandon_message(
                            &self.properties.account_id,
                            session.lease(),
                            &claim,
                        ) {
                            self.notify("dispatch failed", |listener| {
                                listener.on_dispatch_failed(
                                    &self.properties.account_id,
                                    &error.safe_message(),
                                )
                            });
                        }
                    } else {
                        self.state_store.fail_message(
                            &self.properties.account_id,
                            session.lease(),
                            &claim,
                        );
                    }
                    return Err(error);
                }
            }
        }
    }

    /// Route, run and deliver one claimed message.
    async fn process_claim(
        &self,
        claim: &WeixinInboxClaim,
        session: &Arc<LeaseSession>,
    ) -> WeixinResult<()> {
        let message: WeixinMessage = serde_json::from_str(&claim.payload)
            .map_err(|_| WeixinError::operation("Weixin inbox payload is unparseable"))?;
        let Some(inbound) = self.mapper.map(&message) else {
            // Non-text / loop-guarded payloads are acknowledged without running the agent.
            return Ok(());
        };
        self.require_lease(session)?;
        if !self.core.allow_peer(&inbound.peer.key()) {
            return Err(WeixinError::operation("Weixin peer rate limited"));
        }
        // The peer's context token is persisted before dispatch, so a reply queued after a restart
        // can still be routed back to the same conversation.
        if let Some(token) = inbound.messages.first().and_then(context_token_of) {
            if !self.state_store.save_context_token(
                &self.properties.account_id,
                session.lease(),
                &inbound.peer.id,
                &token,
            ) {
                session.invalidate();
                return Err(WeixinError::operation("Weixin context update fenced"));
            }
        }
        let dispatch_timeout = std::time::Duration::from_millis(self.properties.dispatch_timeout_ms as u64);
        let turn = tokio::time::timeout(dispatch_timeout, self.core.run_turn(&inbound)).await;
        let (reply, address) = match turn {
            Err(_elapsed) => {
                return Err(WeixinError::operation("Weixin dispatch timed out"));
            }
            Ok(Err(error)) => {
                // A harness/core failure may embed provider or agent text, so only its variant is
                // reported (mirrors the Java `safeMessage` rule).
                return Err(WeixinError::operation(format!(
                    "Weixin dispatch failed: {}",
                    core_error_kind(&error)
                )));
            }
            Ok(Ok(result)) => result,
        };
        // Fencing point: a lease lost while the turn was running must not send the late reply.
        self.require_lease(session)?;
        let token = inbound.messages.first().and_then(context_token_of);
        self.send_reply(&address, &[reply], token.as_deref()).await?;
        Ok(())
    }

    /// Send `messages` to `address`, checked against the active lease immediately before the
    /// provider call and using the peer's stored context token.
    pub async fn send_reply(
        &self,
        address: &OutboundAddress,
        messages: &[Msg],
        context_token: Option<&str>,
    ) -> WeixinResult<()> {
        let session = self
            .active_session()
            .ok_or_else(|| WeixinError::operation("Weixin consumer is not active"))?;
        self.require_lease(&session)?;
        let result = self.outbound.send(address, messages, context_token).await;
        match result {
            Ok(_receipts) => Ok(()),
            Err(error) => {
                if !error.is_credential_rejected() {
                    self.notify("delivery failed", |listener| {
                        listener.on_delivery_failed(
                            &self.properties.account_id,
                            &error.safe_message(),
                        )
                    });
                }
                Err(error)
            }
        }
    }

    /// Deliver using the context token stored for the addressed peer, mirroring the Java
    /// `deliver(address, messages)` used by a host delivery worker.
    pub async fn deliver_to_peer(
        &self,
        address: &OutboundAddress,
        messages: &[Msg],
    ) -> WeixinResult<()> {
        let token = self
            .state_store
            .load_context_token(&self.properties.account_id, peer_of(&address.to));
        self.send_reply(address, messages, token.as_deref()).await
    }

    /// Send one host-persisted reply and return the provider's receipt.
    ///
    /// A host that persists replies needs the receipt, not a local acknowledgement: only the
    /// provider's message id proves iLink accepted the reply.
    pub async fn deliver_with_receipt(
        &self,
        address: &OutboundAddress,
        message: &Msg,
        context_token: Option<&str>,
    ) -> WeixinResult<Option<String>> {
        let session = self
            .active_session()
            .ok_or_else(|| WeixinError::operation("Weixin consumer is not active"))?;
        self.require_lease(&session)?;
        self.outbound
            .send_with_receipt(address, message, context_token)
            .await
    }

    /// Stop the consumer: fence the active session and mark the channel stopped.
    pub fn stop(&self) {
        let session = {
            let mut runtime = self.runtime.write().expect("weixin runtime lock poisoned");
            runtime.running = false;
            runtime.active.take()
        };
        if let Some(session) = session {
            session.invalidate();
        }
    }

    /// A channel that was stopped cannot be restarted (mirrors the Java `WeixinOperationException`).
    pub fn start(&self) -> WeixinResult<()> {
        let mut runtime = self.runtime.write().expect("weixin runtime lock poisoned");
        if runtime.running {
            return Ok(());
        }
        if runtime.started {
            return Err(WeixinError::operation("Stopped channel cannot restart"));
        }
        runtime.running = true;
        runtime.started = true;
        Ok(())
    }

    /// Spawn the consumer loop on the current tokio runtime.
    pub fn spawn_consumer(self: &Arc<Self>) -> WeixinResult<()> {
        self.start()?;
        let channel = Arc::clone(self);
        tokio::spawn(async move { channel.run_consumer().await });
        Ok(())
    }

    /// Verify the session is still the active, unexpired lease owner; otherwise fence it.
    fn require_lease(&self, session: &Arc<LeaseSession>) -> WeixinResult<()> {
        if !session.is_valid() || !self.is_running() {
            session.invalidate();
            return Err(WeixinError::operation("Weixin consumer is not active"));
        }
        if !Arc::ptr_eq(
            &self.active_session().unwrap_or_else(|| Arc::new(LeaseSession::new(
                WeixinLease::new("none", 1).expect("literal lease is valid"),
            ))),
            session,
        ) {
            session.invalidate();
            return Err(WeixinError::operation("Weixin consumer is not active"));
        }
        if self
            .state_store
            .is_lease_current(&self.properties.account_id, session.lease())
        {
            return Ok(());
        }
        session.invalidate();
        Err(WeixinError::operation("Weixin lease lost"))
    }

    fn notify(&self, event: &str, notification: impl FnOnce(&dyn WeixinRuntimeListener)) {
        // A host listener is arbitrary code: a panic inside it must not take the consumer down.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            notification(self.listener.as_ref())
        }))
        .is_err()
        {
            tracing::warn!(event, "weixin runtime listener panicked");
        }
    }

    fn notify_credential_rejected(&self, error: &WeixinError) {
        self.notify("credential rejected", |listener| {
            listener.on_credential_rejected(&self.properties.account_id, &error.safe_message())
        });
    }
}

enum ConsumeOutcome {
    CredentialRejected,
    LeaseLost(WeixinError),
    Transient(WeixinError),
}

impl ConsumeOutcome {
    /// The sanitised reason for a non-credential outcome, so a host log never carries a payload
    /// or a request URI.
    fn safe_reason(&self) -> Option<String> {
        match self {
            Self::Transient(error) | Self::LeaseLost(error) => Some(error.safe_message()),
            Self::CredentialRejected => None,
        }
    }
}

#[async_trait]
impl PlatformChannel for WeixinChannel {
    fn ctx(&self) -> &PlatformCtx {
        self.core.ctx()
    }

    async fn deliver_reply(&self, address: &OutboundAddress, messages: Vec<Msg>) -> CoreResult<()> {
        self.deliver_to_peer(address, &messages)
            .await
            .map_err(to_core_error)
    }
}

/// Derive one durable inbox id per message.
///
/// A message the provider sends without `message_id` is keyed by its position in the batch plus a
/// digest of the payload. The position keeps two byte-identical id-less messages — a user typing the
/// same thing twice — from collapsing into a single claim and losing one of them, while a
/// re-delivered batch keeps the same ids and is still deduplicated.
pub fn inbox_messages(messages: &[WeixinMessage]) -> Vec<WeixinInboxMessage> {
    messages
        .iter()
        .enumerate()
        .filter_map(|(position, message)| {
            let payload = serde_json::to_string(message).ok()?;
            let message_id = message
                .message_id()
                .unwrap_or_else(|| format!("payload-{position}-{}", digest(&payload)));
            WeixinInboxMessage::new(message_id, payload).ok()
        })
        .collect()
}

/// Lower-case hex SHA-256 of `value`, mirroring the Java `digest` helper.
pub fn digest(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Map an unparseable-batch response into an error without quoting the body.
impl WeixinUpdatesResponse {
    /// The batch payloads, in provider order.
    pub fn payloads(&self) -> Vec<String> {
        self.messages()
            .iter()
            .filter_map(|message| serde_json::to_string(message).ok())
            .collect()
    }
}

/// Re-export for hosts that map the channel error themselves.
pub fn weixin_to_core_error(error: WeixinError) -> agentscope_core::error::Error {
    to_core_error(error)
}

/// Bridge the shared channel error into the core error used by the harness traits.
pub fn channel_error_to_core(error: crate::common::ChannelError) -> agentscope_core::error::Error {
    channel_to_core_error(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testing::RecordingTransport;
    use crate::common::PreparedResponse;
    use crate::weixin::listener::RecordingWeixinRuntimeListener;
    use crate::weixin::state_store::in_memory_store;
    use agentscope_core::message::MsgRole;
    // `channel_id` / `config` / `init` come from the blanket `Channel` impl, so the trait has to be
    // in scope (the same pattern the other platform adapters' test modules use).
    use agentscope_harness::agent::gateway::channel::Channel as _;

    const ACCOUNT: &str = "acct-1";

    #[allow(clippy::type_complexity)]
    fn channel(
        transport: Arc<RecordingTransport>,
    ) -> (Arc<WeixinChannel>, Arc<dyn WeixinStateStore>) {
        let store: Arc<dyn WeixinStateStore> = Arc::new(in_memory_store());
        let properties = WeixinChannelProperties::from_json(
            "weixin",
            &serde_json::json!({
                "accountId": ACCOUNT,
                "leaseMs": 60_000,
                "dispatchTimeoutMs": 5_000,
            }),
        )
        .unwrap();
        let channel = Arc::new(WeixinChannel::create(
            "weixin",
            ChannelConfig::of_with_default("weixin", "main"),
            properties,
            FixedCredentialProvider::from_token("tok").unwrap().shared(),
            store.clone(),
            Arc::new(RecordingWeixinRuntimeListener::new()),
            transport,
        ));
        (channel, store)
    }

    fn assistant(text: &str) -> Msg {
        Msg::builder().role(MsgRole::Assistant).text_content(text).build()
    }

    fn address() -> OutboundAddress {
        OutboundAddress::direct("weixin", "weixin:DIRECT:peer-a")
    }

    #[test]
    fn from_json_requires_a_bot_token_and_wires_the_properties() {
        let transport: Arc<dyn HttpTransport> = Arc::new(RecordingTransport::new());
        let error = WeixinChannel::from_json(
            "weixin",
            ChannelConfig::of("weixin"),
            &serde_json::json!({ "accountId": "acct" }),
            transport.clone(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("botToken"));

        let channel = WeixinChannel::from_json(
            "weixin",
            ChannelConfig::of("weixin"),
            &serde_json::json!({ "botToken": "tok", "accountId": "acct" }),
            transport,
        )
        .unwrap();
        assert_eq!(channel.channel_id(), "weixin");
        assert_eq!(channel.properties().account_id, "acct");
        // The mapper filters on the account identity, never on `ilink_user_id`.
        assert_eq!(channel.mapper().account_id(), "acct");
    }

    #[tokio::test]
    async fn delivery_without_an_active_consumer_is_refused() {
        let transport = Arc::new(RecordingTransport::new());
        let (channel, _store) = channel(transport.clone());
        let error = channel
            .deliver_to_peer(&address(), &[assistant("hello")])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not active"));
        assert_eq!(transport.request_count(), 0);
        // `PlatformChannel::deliver_reply` reports the same refusal through the core error type.
        assert!(PlatformChannel::deliver_reply(&*channel, &address(), vec![assistant("hello")])
            .await
            .is_err());
    }

    #[tokio::test]
    async fn delivery_uses_the_context_token_stored_for_the_peer() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, "{\"message_id\":\"receipt-1\"}"),
        ]));
        let (channel, store) = channel(transport.clone());
        let session = channel.begin_consuming().unwrap().unwrap();
        // A host would have stored this while processing the inbound message.
        assert!(store.save_context_token(ACCOUNT, session.lease(), "peer-a", "ctx-9"));

        channel
            .deliver_to_peer(&address(), &[assistant("hello")])
            .await
            .unwrap();
        let body = transport.last_request().unwrap().body.unwrap();
        assert_eq!(body["msg"]["to_user_id"], serde_json::json!("peer-a"));
        assert_eq!(body["msg"]["context_token"], serde_json::json!("ctx-9"));
        assert_eq!(
            body["msg"]["item_list"][0]["text_item"]["text"],
            serde_json::json!("hello")
        );
    }

    #[tokio::test]
    async fn a_lost_lease_suppresses_the_late_reply() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, "{\"message_id\":\"receipt-1\"}"),
        ]));
        let (channel, store) = channel(transport.clone());
        let session = channel.begin_consuming().unwrap().unwrap();

        // Another instance takes the account over before this one sends its reply.
        store.release_lease(ACCOUNT, session.lease());
        let error = channel
            .deliver_to_peer(&address(), &[assistant("late")])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("lease lost"));
        assert_eq!(transport.request_count(), 0, "a stale consumer must not send");
        assert!(!session.is_valid());
    }

    #[tokio::test]
    async fn a_second_instance_is_refused_while_the_lease_is_live() {
        let transport = Arc::new(RecordingTransport::new());
        let (channel, _store) = channel(transport);
        let session = channel.begin_consuming().unwrap().unwrap();
        // The same instance reuses its own lease, so it stays the active consumer.
        let again = channel.begin_consuming().unwrap().unwrap();
        assert_eq!(again.lease(), session.lease());

        let other: Arc<dyn WeixinStateStore> = Arc::new(in_memory_store());
        assert!(other
            .acquire_lease(ACCOUNT, "someone-else", 1_000)
            .is_some());
    }

    #[tokio::test]
    async fn stop_fences_the_session_and_a_stopped_channel_cannot_restart() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![PreparedResponse::new(
            200,
            "{\"ret\":0}",
        )]));
        let (channel, _store) = channel(transport);
        let session = channel.begin_consuming().unwrap().unwrap();
        assert!(channel.is_running());
        channel.stop();
        assert!(!channel.is_running());
        assert!(!session.is_valid());
        assert!(channel.active_session().is_none());
        assert!(channel.start().is_err());
    }

    #[test]
    fn id_less_messages_are_keyed_by_position_and_digest() {
        let batch: Vec<WeixinMessage> = serde_json::from_value(serde_json::json!([
            {"message_type": 1, "from_user_id": "u1", "item_list": [{"type": 1, "text_item": {"text": "same"}}]},
            {"message_type": 1, "from_user_id": "u1", "item_list": [{"type": 1, "text_item": {"text": "same"}}]},
            {"message_type": 1, "message_id": "m3", "from_user_id": "u1", "item_list": [{"type": 1, "text_item": {"text": "same"}}]}
        ]))
        .unwrap();
        let inbox = inbox_messages(&batch);
        assert_eq!(inbox.len(), 3);
        // Two byte-identical id-less messages must not collapse into one claim.
        assert_ne!(inbox[0].message_id, inbox[1].message_id);
        assert!(inbox[0].message_id.starts_with("payload-0-"));
        assert!(inbox[1].message_id.starts_with("payload-1-"));
        assert_eq!(inbox[2].message_id, "m3");
        // The same batch re-delivered produces the same ids, so it is still deduplicated.
        let replay = inbox_messages(&batch);
        assert_eq!(replay[0].message_id, inbox[0].message_id);
        assert_eq!(replay[1].message_id, inbox[1].message_id);
        // The id embeds the payload digest, so a changed payload gets a different id.
        assert_eq!(inbox[0].message_id, format!("payload-0-{}", digest(&inbox[0].payload)));
    }

    #[test]
    fn digest_is_the_lowercase_hex_sha256_of_the_payload() {
        assert_eq!(
            digest(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn poll_once_accepts_the_batch_and_advances_the_cursor() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let transport = Arc::new(RecordingTransport::with_responses(vec![
                PreparedResponse::new(
                    200,
                    "{\"ret\":0,\"get_updates_buf\":\"next\",\"msgs\":[{\"message_type\":1,\"message_id\":\"m1\",\"from_user_id\":\"u1\",\"item_list\":[{\"type\":1,\"text_item\":{\"text\":\"hi\"}}]}]}",
                ),
            ]));
            let (channel, store) = channel(transport.clone());
            let session = channel.begin_consuming().unwrap().unwrap();
            channel.poll_once(&session).await.unwrap();
            assert_eq!(store.load_cursor(ACCOUNT), "next");
            // The batch is now claimable work.
            let claims = store.claim_messages(ACCOUNT, session.lease(), 1, 1_000);
            assert_eq!(claims.len(), 1);
            assert_eq!(claims[0].message_id, "m1");
        });
    }

    #[tokio::test]
    async fn poll_once_surfaces_a_credential_rejection() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            PreparedResponse::new(200, "{\"ret\":-14}"),
        ]));
        let (channel, store) = channel(transport);
        let session = channel.begin_consuming().unwrap().unwrap();
        let error = channel.poll_once(&session).await.unwrap_err();
        assert!(error.is_credential_rejected());
        assert_eq!(store.load_cursor(ACCOUNT), String::new());
    }

    #[tokio::test]
    async fn drain_inbox_abandons_a_message_after_the_retry_budget() {
        let transport = Arc::new(RecordingTransport::new());
        let (channel, store) = channel(transport);
        let session = channel.begin_consuming().unwrap().unwrap();
        // An unparseable payload can never succeed, so it burns the dispatch budget.
        assert!(store.accept_batch(
            ACCOUNT,
            session.lease(),
            "c1",
            &[WeixinInboxMessage::new("bad", "not json at all").unwrap()],
        ));

        for _ in 0..MAX_DISPATCH_ATTEMPTS {
            assert!(channel.drain_inbox(&session).await.is_err());
        }
        // The budget is exhausted: the message is tombstoned and never claimed again.
        assert!(store
            .claim_messages(ACCOUNT, session.lease(), 1, 1_000)
            .is_empty());
        assert!(channel.drain_inbox(&session).await.is_ok());
        // A re-delivered batch cannot resurrect it.
        assert!(store.accept_batch(
            ACCOUNT,
            session.lease(),
            "c2",
            &[WeixinInboxMessage::new("bad", "not json at all").unwrap()],
        ));
        assert!(store
            .claim_messages(ACCOUNT, session.lease(), 1, 1_000)
            .is_empty());
    }

    #[tokio::test]
    async fn drain_inbox_completes_non_text_payloads_without_running_the_agent() {
        let transport = Arc::new(RecordingTransport::new());
        let (channel, store) = channel(transport);
        let session = channel.begin_consuming().unwrap().unwrap();
        // `message_type = 2` is not a user text message, so the mapper drops it.
        let payload = serde_json::json!({
            "message_type": 2,
            "from_user_id": "u1",
            "item_list": [{"type": 1, "text_item": {"text": "ignored"}}],
        })
        .to_string();
        assert!(store.accept_batch(
            ACCOUNT,
            session.lease(),
            "c1",
            &[WeixinInboxMessage::new("m1", payload).unwrap()],
        ));
        // It completes (not retried) without any gateway configured.
        channel.drain_inbox(&session).await.unwrap();
        assert!(store
            .claim_messages(ACCOUNT, session.lease(), 1, 1_000)
            .is_empty());
    }

    #[tokio::test]
    async fn a_dispatch_failure_is_reported_as_a_host_safe_reason() {
        let transport = Arc::new(RecordingTransport::new());
        let store: Arc<dyn WeixinStateStore> = Arc::new(in_memory_store());
        let listener = Arc::new(RecordingWeixinRuntimeListener::new());
        let properties = WeixinChannelProperties::from_json(
            "weixin",
            &serde_json::json!({ "accountId": ACCOUNT, "dispatchTimeoutMs": 30 }),
        )
        .unwrap();
        let channel = Arc::new(WeixinChannel::create(
            "weixin",
            ChannelConfig::of_with_default("weixin", "main"),
            properties,
            FixedCredentialProvider::from_token("tok").unwrap().shared(),
            store.clone(),
            listener.clone(),
            transport,
        ));
        // A gateway whose agent never answers, so the dispatch budget times out.
        channel.init(Arc::new(agentscope_harness::agent::HarnessGateway::create_standalone()));
        let session = channel.begin_consuming().unwrap().unwrap();
        let payload = serde_json::json!({
            "message_type": 1,
            "message_id": "m1",
            "from_user_id": "u1",
            "item_list": [{"type": 1, "text_item": {"text": "hi"}}],
        })
        .to_string();
        assert!(store.accept_batch(
            ACCOUNT,
            session.lease(),
            "c1",
            &[WeixinInboxMessage::new("m1", payload).unwrap()],
        ));
        let error = channel.drain_inbox(&session).await.unwrap_err();
        // The reason is a module constant or a variant name — never a payload or a URI.
        let reason = error.safe_message();
        assert!(!reason.contains("hi"), "reason leaked the payload: {reason}");
        assert!(reason.starts_with("WeixinOperationException"), "{reason}");
    }

    #[test]
    fn begin_consuming_returns_none_while_another_holder_owns_the_account() {
        let transport = Arc::new(RecordingTransport::new());
        let (channel, store) = channel(transport);
        let other = store.acquire_lease(ACCOUNT, "other-holder", 60_000).unwrap();
        assert!(channel.begin_consuming().unwrap().is_none());
        // Once the other holder releases, this instance takes over with a new generation.
        store.release_lease(ACCOUNT, &other);
        let session = channel.begin_consuming().unwrap().unwrap();
        assert!(session.lease().generation > other.generation);
        assert_eq!(session.lease().holder_id, channel.lease_holder);
    }
}
