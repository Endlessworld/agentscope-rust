//! Personal Weixin (个人微信) channel adapter backed by Tencent's official iLink long-poll API.
//!
//! Rust port of `io.agentscope.extensions.channel.weixin`:
//!
//! - [`WeixinChannel`] — the `Channel` lifecycle/routing adapter: long polling, `get_updates_buf`
//!   cursor persistence, `context_token` replies and direct-text inbound mapping.
//! - [`WeixinStateStore`] / [`InMemoryWeixinStateStore`] — the account-scoped state contract
//!   (cursor, peer context tokens, inbox claims, lease fencing) plus its reference in-process
//!   implementation for standalone use and tests.
//! - [`WeixinLoginClient`] — stateless QR login (`start` / `poll` / `verify`), including the
//!   `scaned_but_redirect` host switch.
//! - [`WeixinOutboundClient`] — `sendmessage` / `getupdates` / `notifystart` / `notifystop` on the
//!   shared [`HttpTransport`](crate::common::HttpTransport) seam.
//! - [`WeixinEndpointPolicy`](endpoint) — the allowlist applied before credentials are sent.
//! - [`WeixinCredentialProvider`] / [`WeixinRuntimeListener`] — the neutral seams a managed host
//!   implements.
//!
//! ## Compliance and scope
//!
//! This adapter drives a **personal** Weixin account rather than an enterprise bot. Personal-account
//! automation is governed by the provider's terms and by the operator's own policy, the account can
//! be restricted by the provider, and the host owns the decision to deploy it. As in Java, the module
//! carries no owner or vault references, no persistence policy, no public routes and no console
//! state: an iLink `-14` is surfaced as a credential-rejection observation, and the host decides
//! whether that means reauthorization, a notification, or something else.
//!
//! ## Not ported
//!
//! - the AgentScope *service / console* integration (control-plane connection and login flows,
//!   scheduler adapters, the QR connection page) — it lives in a separate Java module, not in this
//!   channel extension.
//! - a **durable** `WeixinStateStore` adapter. As in Java, the module ships no Redis or JDBC
//!   implementation: `cursor`, peer context tokens and leases live in one process, so a restart
//!   replays or drops messages and a second instance cannot see the first. That is what
//!   [`WeixinChannel::create`] is for — the host supplies the store.

pub mod channel;
pub mod credentials;
pub mod dto;
pub mod endpoint;
pub mod error;
pub mod inbound;
pub mod listener;
pub mod login;
pub mod outbound;
pub mod properties;
pub mod protocol;
pub mod state_store;

pub use channel::{
    digest, inbox_messages, weixin_to_core_error, LeaseSession, WeixinChannel, WeixinChannelRegistry,
    CHANNEL_TYPE, MAX_DISPATCH_ATTEMPTS,
};
pub use credentials::{
    FixedCredentialProvider, WeixinCredentialProvider, WeixinCredentials,
};
pub use dto::{
    WeixinItem, WeixinLoginChallenge, WeixinLoginSession, WeixinLoginStatusResponse, WeixinLoginStep,
    WeixinMessage, WeixinQrCodeResponse, WeixinTextItem, WeixinUpdatesResponse,
};
/// Alias for the provider inbound message DTO (see [`dto::WeixinMessage`]).
pub type WeixinInboundMessage = dto::WeixinMessage;
pub use endpoint::{
    is_loopback, is_official_host, normalize_base_url, validate_provider_endpoint, DEFAULT_BASE_URL,
};
pub use error::{sanitize, to_core_error, WeixinError, WeixinResult, MAX_SAFE_MESSAGE_LENGTH};
pub use inbound::{
    context_token_of, WeixinInboundMapper, META_CONTEXT_TOKEN, META_MESSAGE_ID,
};
pub use listener::{
    NoopWeixinRuntimeListener, RecordingWeixinRuntimeListener, WeixinRuntimeListener,
};
pub use login::{WeixinLoginChallengeResult, WeixinLoginClient, DEFAULT_LOGIN_TIMEOUT_MS};
pub use outbound::{next_client_id, peer_of, timeout_for, WeixinChallenge, WeixinOutboundClient};
pub use properties::{WeixinChannelProperties, DEFAULT_BOT_AGENT, DEFAULT_CHANNEL_VERSION};
pub use protocol::{
    form_urlencode, APP_CLIENT_VERSION, APP_ID, PATH_SEND_MESSAGE, PATH_UPDATES,
};
pub use state_store::{
    in_memory_store, InMemoryWeixinStateStore, WeixinInboxClaim, WeixinInboxMessage, WeixinLease,
    WeixinStateStore,
};
