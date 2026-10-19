//! `WeixinChannelProperties` — provider configuration for one Personal Weixin channel instance.
//!
//! Rust port of `io.agentscope.extensions.channel.weixin.WeixinChannelProperties`.
//!
//! `request_timeout_ms` bounds the control calls, while `long_poll_timeout_ms` bounds the
//! `getupdates` long poll: the provider holds that request open, so the client derives a longer
//! deadline for it (see [`super::outbound::timeout_for`]).

use std::collections::BTreeMap;

use serde_json::Value;

use super::endpoint::{normalize_base_url, DEFAULT_BASE_URL};
use crate::common::props::{get_int, get_str, get_str_or};
use crate::common::{ChannelError, Result};

/// Default `channel_version` reported in every `base_info`.
pub const DEFAULT_CHANNEL_VERSION: &str = "agentscope-weixin/1.0";
/// Default `bot_agent` reported in every `base_info`.
pub const DEFAULT_BOT_AGENT: &str = "AgentScope";

/// Provider-specific configuration for a single Weixin channel instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeixinChannelProperties {
    /// Account scoping the lease / cursor / context-token state.
    pub account_id: String,
    /// Normalized iLink base URL (already passed through the endpoint allowlist).
    pub base_url: String,
    /// The person who scanned the QR code, when the host knows it.
    pub ilink_user_id: Option<String>,
    /// `channel_version` sent in `base_info`.
    pub channel_version: String,
    /// `bot_agent` sent in `base_info`.
    pub bot_agent: String,
    /// Bounds the `getupdates` long poll.
    pub long_poll_timeout_ms: i64,
    /// Bounds every control call.
    pub request_timeout_ms: i64,
    /// Upper bound of the poll backoff.
    pub max_backoff_ms: i64,
    /// Account lease length; a standby instance retries acquisition while it is held.
    pub lease_ms: i64,
    /// Per-message dispatch budget.
    pub dispatch_timeout_ms: i64,
}

impl WeixinChannelProperties {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        channel_id: &str,
        account_id: Option<String>,
        base_url: Option<String>,
        ilink_user_id: Option<String>,
        channel_version: Option<String>,
        bot_agent: Option<String>,
        long_poll_timeout_ms: i64,
        request_timeout_ms: i64,
        max_backoff_ms: i64,
        lease_ms: i64,
        dispatch_timeout_ms: i64,
    ) -> Result<Self> {
        // Java falls back to the channel id so one channel is one account unless configured
        // otherwise; a blank value is not accepted as an account id.
        let account_id = account_id
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| channel_id.to_string());
        if account_id.trim().is_empty() {
            return Err(ChannelError::Config(
                "weixin.accountId is required".to_string(),
            ));
        }
        let base_url = normalize_base_url(base_url.as_deref().unwrap_or(DEFAULT_BASE_URL))?;
        Ok(Self {
            account_id,
            base_url,
            ilink_user_id: ilink_user_id.filter(|s| !s.is_empty()),
            channel_version: or_default(channel_version, DEFAULT_CHANNEL_VERSION),
            bot_agent: or_default(bot_agent, DEFAULT_BOT_AGENT),
            long_poll_timeout_ms: positive_or(long_poll_timeout_ms, 35_000),
            request_timeout_ms: positive_or(request_timeout_ms, 45_000),
            max_backoff_ms: positive_or(max_backoff_ms, 60_000),
            lease_ms: positive_or(lease_ms, 90_000),
            dispatch_timeout_ms: positive_or(dispatch_timeout_ms, 120_000),
        })
    }

    /// Build from the harness property map, mirroring the Java `from(channelId, raw)` factory.
    pub fn from_map(channel_id: &str, props: &BTreeMap<String, Value>) -> Result<Self> {
        Self::from_json(channel_id, &crate::common::props::object_from_btree(props))
    }

    /// Build from a JSON property object.
    pub fn from_json(channel_id: &str, props: &Value) -> Result<Self> {
        Self::new(
            channel_id,
            get_str(props, "accountId"),
            get_str(props, "baseUrl"),
            get_str(props, "ilinkUserId"),
            Some(get_str_or(props, "channelVersion", DEFAULT_CHANNEL_VERSION)),
            Some(get_str_or(props, "botAgent", DEFAULT_BOT_AGENT)),
            get_int(props, "longPollTimeoutMs").unwrap_or(35_000),
            get_int(props, "requestTimeoutMs").unwrap_or(45_000),
            get_int(props, "maxBackoffMs").unwrap_or(60_000),
            get_int(props, "leaseMs").unwrap_or(90_000),
            get_int(props, "dispatchTimeoutMs").unwrap_or(120_000),
        )
    }
}

fn or_default(value: Option<String>, fallback: &str) -> String {
    value
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn positive_or(value: i64, fallback: i64) -> i64 {
    if value > 0 {
        value
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_java_record() {
        let p = WeixinChannelProperties::from_json("binding-1", &Value::Null).unwrap();
        // Java: accountId defaults to the channel id, everything else to the documented defaults.
        assert_eq!(p.account_id, "binding-1");
        assert_eq!(p.base_url, DEFAULT_BASE_URL);
        assert_eq!(p.channel_version, DEFAULT_CHANNEL_VERSION);
        assert_eq!(p.bot_agent, DEFAULT_BOT_AGENT);
        assert_eq!(p.long_poll_timeout_ms, 35_000);
        assert_eq!(p.request_timeout_ms, 45_000);
        assert_eq!(p.max_backoff_ms, 60_000);
        assert_eq!(p.lease_ms, 90_000);
        assert_eq!(p.dispatch_timeout_ms, 120_000);
        assert_eq!(p.ilink_user_id, None);
    }

    #[test]
    fn reads_provider_configuration_and_ignores_unrelated_keys() {
        let p = WeixinChannelProperties::from_json(
            "binding-1",
            &serde_json::json!({
                "accountId": "acct-7",
                "baseUrl": "ilinkai.weixin.qq.com",
                "ilinkUserId": "user-1",
                "leaseMs": 1000,
                "botToken": "ignored-here",
            }),
        )
        .unwrap();
        assert_eq!(p.account_id, "acct-7");
        assert_eq!(p.base_url, "https://ilinkai.weixin.qq.com");
        assert_eq!(p.ilink_user_id.as_deref(), Some("user-1"));
        assert_eq!(p.lease_ms, 1000);
    }

    #[test]
    fn rejects_a_base_url_outside_the_provider_allowlist() {
        assert!(WeixinChannelProperties::from_json(
            "binding-1",
            &serde_json::json!({ "baseUrl": "https://evil.example.com" }),
        )
        .is_err());
    }

    #[test]
    fn non_positive_timeouts_fall_back_to_the_defaults() {
        let p = WeixinChannelProperties::from_json(
            "binding-1",
            &serde_json::json!({ "leaseMs": 0, "dispatchTimeoutMs": -5 }),
        )
        .unwrap();
        assert_eq!(p.lease_ms, 90_000);
        assert_eq!(p.dispatch_timeout_ms, 120_000);
    }
}
