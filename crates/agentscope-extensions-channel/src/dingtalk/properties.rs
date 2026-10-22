//! `DingTalkChannelProperties` — configuration for one DingTalk channel instance.
//!
//! Rust port of `io.agentscope.extensions.channel.dingtalk.DingTalkChannelProperties`.
//!
//! Reception mode is selected by [`DingTalkChannelProperties::mode`]:
//! - [`MODE_STREAM`] (default) receives bot messages over a persistent WebSocket gateway.
//! - [`MODE_HTTP`] receives them through signed HTTP callbacks handled by
//!   [`super::controller::DingTalkCallbackController`].
//!
//! `aes_key` enables encrypted callback bodies (`{"encrypt": "..."}`). It is optional, exactly
//! like the Java record: a channel without an `aesKey` accepts plain JSON callback bodies, and a
//! channel with one rejects plain bodies (fail closed).
//!
//! `token` is the legacy DingTalk callback token. The Java 2026 implementation verifies callbacks
//! with `appSecret` (HMAC-SHA256) and therefore has no `token` property; the Rust port keeps it as
//! an optional alternative authenticator for deployments still on the classic
//! `msg_signature = SHA1(sort(token, timestamp, nonce, encrypt))` scheme.
//!
//! `require_encrypted_callback` is Rust-only hardening (default `false`, i.e. Java behaviour):
//! when enabled, `mode=http` additionally requires both `token` and `aes_key`, so a
//! misconfigured deployment fails at construction instead of accepting an unauthenticated plain
//! callback.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::common::props::get_str;
use crate::common::{ChannelError, Result};

/// Reception mode: bot messages arrive over a persistent Stream WebSocket.
pub const MODE_STREAM: &str = "stream";
/// Reception mode: bot messages arrive as signed HTTP callbacks.
pub const MODE_HTTP: &str = "http";

/// Configuration for a single DingTalk channel instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DingTalkChannelProperties {
    pub app_key: String,
    pub app_secret: String,
    pub robot_code: String,
    /// [`MODE_STREAM`] or [`MODE_HTTP`].
    pub mode: String,
    /// 43-character base64 (no padding) AES key; `None` disables encrypted callbacks.
    pub aes_key: Option<String>,
    /// Legacy callback token; `None` disables the classic `msg_signature` scheme.
    pub token: Option<String>,
    /// Rust-only: require `token` + `aes_key` when `mode=http`.
    pub require_encrypted_callback: bool,
    pub api_base: String,
    pub oapi_base: String,
    pub stream_register_url: String,
}

impl DingTalkChannelProperties {
    pub const DEFAULT_API_BASE: &'static str = "https://api.dingtalk.com";
    pub const DEFAULT_OAPI_BASE: &'static str = "https://oapi.dingtalk.com";
    pub const DEFAULT_STREAM_REGISTER_URL: &'static str =
        "https://api.dingtalk.com/v1.0/gateway/connections/open";

    /// 43 characters is the only accepted `aesKey` length (Java constructor guard).
    pub const AES_KEY_LEN: usize = 43;

    /// Validate + normalise. Required fields: `app_key`, `app_secret`, `robot_code`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        app_key: Option<String>,
        app_secret: Option<String>,
        robot_code: Option<String>,
        mode: Option<String>,
        aes_key: Option<String>,
        token: Option<String>,
        api_base: Option<String>,
        oapi_base: Option<String>,
        stream_register_url: Option<String>,
    ) -> Result<Self> {
        let require = |v: Option<String>, field: &str| -> Result<String> {
            match v {
                Some(s) if !s.trim().is_empty() => Ok(s),
                _ => Err(ChannelError::Config(format!(
                    "dingtalk.{field} is required"
                ))),
            }
        };
        let or_default = |v: Option<String>, default: &str| -> String {
            match v {
                Some(s) if !s.trim().is_empty() => s,
                _ => default.to_string(),
            }
        };
        let blank_to_none =
            |v: Option<String>| -> Option<String> { v.filter(|s| !s.trim().is_empty()) };

        // Java: blank / absent mode falls back to stream, then validates the two legal values.
        let mode = or_default(mode, MODE_STREAM);
        if mode != MODE_STREAM && mode != MODE_HTTP {
            return Err(ChannelError::Config(format!(
                "dingtalk.mode must be '{MODE_STREAM}' or '{MODE_HTTP}', got: {mode}"
            )));
        }

        let aes_key = blank_to_none(aes_key);
        if let Some(key) = &aes_key {
            // Java measures String.length() (UTF-16 units); the key must be ASCII base64 anyway,
            // so counting chars gives the same answer for every decodable value.
            if key.chars().count() != Self::AES_KEY_LEN {
                return Err(ChannelError::Config(format!(
                    "dingtalk.aesKey must be {} characters when set (got {})",
                    Self::AES_KEY_LEN,
                    key.chars().count()
                )));
            }
        }

        Ok(Self {
            app_key: require(app_key, "appKey")?,
            app_secret: require(app_secret, "appSecret")?,
            robot_code: require(robot_code, "robotCode")?,
            mode,
            aes_key,
            token: blank_to_none(token),
            require_encrypted_callback: false,
            api_base: or_default(api_base, Self::DEFAULT_API_BASE),
            oapi_base: or_default(oapi_base, Self::DEFAULT_OAPI_BASE),
            stream_register_url: or_default(stream_register_url, Self::DEFAULT_STREAM_REGISTER_URL),
        })
    }

    /// Read the properties out of the harness channel-specific property map.
    pub fn from_map(channel_id: &str, props: &BTreeMap<String, Value>) -> Result<Self> {
        let _ = channel_id;
        let value = crate::common::props::object_from_btree(props);
        Self::from_json(&value)
    }

    /// Read the properties out of a JSON object (the Java `from(channelId, Map)` analogue).
    pub fn from_json(props: &Value) -> Result<Self> {
        let mut parsed = Self::new(
            get_str(props, "appKey"),
            get_str(props, "appSecret"),
            get_str(props, "robotCode"),
            get_str(props, "mode"),
            get_str(props, "aesKey"),
            get_str(props, "token"),
            get_str(props, "apiBase"),
            get_str(props, "oapiBase"),
            get_str(props, "streamRegisterUrl"),
        )?;
        parsed.require_encrypted_callback = get_bool(props, "requireEncryptedCallback", false)?;
        parsed.validate_callback_security()?;
        Ok(parsed)
    }

    /// Whether callbacks arrive over HTTP (as opposed to the persistent Stream WebSocket).
    pub fn is_http(&self) -> bool {
        self.mode == MODE_HTTP
    }

    /// Whether encrypted callback bodies are enabled.
    pub fn has_aes_key(&self) -> bool {
        self.aes_key.is_some()
    }

    /// Fail fast when [`Self::require_encrypted_callback`] asks for the strict HTTP setup.
    ///
    /// Java permits `mode=http` without `aesKey` (plain callbacks) and has no `token` at all, so
    /// this check is opt-in: with the default `false` the adapter behaves exactly like Java.
    pub fn validate_callback_security(&self) -> Result<()> {
        if !self.require_encrypted_callback {
            return Ok(());
        }
        if !self.is_http() {
            return Err(ChannelError::Config(
                "dingtalk.requireEncryptedCallback is only valid with mode='http'".to_string(),
            ));
        }
        if self.aes_key.is_none() {
            return Err(ChannelError::Config(
                "dingtalk.aesKey is required when requireEncryptedCallback=true".to_string(),
            ));
        }
        if self.token.is_none() {
            return Err(ChannelError::Config(
                "dingtalk.token is required when requireEncryptedCallback=true".to_string(),
            ));
        }
        Ok(())
    }
}

fn get_bool(props: &Value, key: &str, fallback: bool) -> Result<bool> {
    match props.get(key) {
        None | Some(Value::Null) => Ok(fallback),
        Some(Value::Bool(value)) => Ok(*value),
        Some(Value::String(value)) => match value.trim().to_ascii_lowercase().as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(ChannelError::Config(format!(
                "dingtalk.{key} must be a boolean, got: {value}"
            ))),
        },
        Some(other) => Err(ChannelError::Config(format!(
            "dingtalk.{key} must be a boolean, got: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn required() -> serde_json::Value {
        serde_json::json!({
            "appKey": "k",
            "appSecret": "s",
            "robotCode": "robot-1"
        })
    }

    fn with(extra: serde_json::Value) -> serde_json::Value {
        let mut base = required();
        let object = base.as_object_mut().unwrap();
        for (key, value) in extra.as_object().unwrap() {
            object.insert(key.clone(), value.clone());
        }
        base
    }

    #[test]
    fn requires_app_key_secret_and_robot_code() {
        assert!(DingTalkChannelProperties::from_json(
            &serde_json::json!({"appKey": "k", "appSecret": "s"})
        )
        .is_err());
        assert!(DingTalkChannelProperties::from_json(
            &serde_json::json!({"appKey": "k", "appSecret": "  ", "robotCode": "r"})
        )
        .is_err());
    }

    #[test]
    fn defaults_mode_to_stream_and_leaves_callback_secrets_unset() {
        let p = DingTalkChannelProperties::from_json(&required()).unwrap();
        assert_eq!(p.mode, MODE_STREAM);
        assert!(!p.is_http());
        assert_eq!(p.aes_key, None);
        assert_eq!(p.token, None);
        assert!(!p.require_encrypted_callback);
        assert_eq!(p.api_base, DingTalkChannelProperties::DEFAULT_API_BASE);
        assert_eq!(p.oapi_base, DingTalkChannelProperties::DEFAULT_OAPI_BASE);
        assert_eq!(
            p.stream_register_url,
            DingTalkChannelProperties::DEFAULT_STREAM_REGISTER_URL
        );
    }

    #[test]
    fn reads_http_mode_and_callback_secrets() {
        let p = DingTalkChannelProperties::from_json(&with(serde_json::json!({
            "mode": "http",
            "aesKey": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY",
            "token": "legacy-token"
        })))
        .unwrap();
        assert!(p.is_http());
        assert!(p.has_aes_key());
        assert_eq!(p.token.as_deref(), Some("legacy-token"));
    }

    #[test]
    fn rejects_an_unknown_mode_and_a_malformed_aes_key() {
        let err = DingTalkChannelProperties::from_json(&with(serde_json::json!({
            "mode": "websocket"
        })))
        .unwrap_err();
        assert!(err.to_string().contains("dingtalk.mode"), "{err}");

        let err = DingTalkChannelProperties::from_json(&with(serde_json::json!({
            "mode": "http",
            "aesKey": "too-short"
        })))
        .unwrap_err();
        assert!(err.to_string().contains("aesKey must be 43"), "{err}");
    }

    #[test]
    fn strict_http_mode_requires_both_callback_secrets() {
        // Rust-only opt-in: with the flag on, a missing aesKey / token is a construction error
        // instead of a plain-callback deployment.
        let err = DingTalkChannelProperties::from_json(&with(serde_json::json!({
            "mode": "http",
            "requireEncryptedCallback": true
        })))
        .unwrap_err();
        assert!(err.to_string().contains("aesKey is required"), "{err}");

        let err = DingTalkChannelProperties::from_json(&with(serde_json::json!({
            "mode": "http",
            "aesKey": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY",
            "requireEncryptedCallback": true
        })))
        .unwrap_err();
        assert!(err.to_string().contains("token is required"), "{err}");

        let err = DingTalkChannelProperties::from_json(&with(serde_json::json!({
            "mode": "stream",
            "requireEncryptedCallback": true
        })))
        .unwrap_err();
        assert!(
            err.to_string().contains("only valid with mode='http'"),
            "{err}"
        );

        // With both present the strict setup is accepted.
        assert!(
            DingTalkChannelProperties::from_json(&with(serde_json::json!({
                "mode": "http",
                "aesKey": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY",
                "token": "legacy-token",
                "requireEncryptedCallback": true
            })))
            .is_ok()
        );
    }

    #[test]
    fn numeric_properties_are_stringified() {
        let props = serde_json::json!({"appKey": 1, "appSecret": 2, "robotCode": 3});
        let p = DingTalkChannelProperties::from_json(&props).unwrap();
        assert_eq!(p.app_key, "1");
        assert_eq!(p.robot_code, "3");
    }
}
