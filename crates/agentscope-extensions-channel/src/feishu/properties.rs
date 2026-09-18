//! `FeishuChannelProperties` — configuration for one Feishu channel instance.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::common::props::{get_str, get_str_or};
use crate::common::{ChannelError, Result};

pub const DEFAULT_API_BASE: &str = "https://open.feishu.cn";

/// Provider-specific configuration for a single Feishu channel instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeishuChannelProperties {
    pub app_id: String,
    pub app_secret: String,
    pub encrypt_key: Option<String>,
    pub verification_token: Option<String>,
    pub callback_path: String,
    pub api_base: String,
}

impl FeishuChannelProperties {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        channel_id: &str,
        app_id: Option<String>,
        app_secret: Option<String>,
        encrypt_key: Option<String>,
        verification_token: Option<String>,
        callback_path: Option<String>,
        api_base: Option<String>,
    ) -> Result<Self> {
        let app_id = app_id
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ChannelError::Config("feishu.appId is required".to_string()))?;
        let app_secret = app_secret
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ChannelError::Config("feishu.appSecret is required".to_string()))?;
        let default_callback = format!("/api/channels/feishu/{channel_id}/callback");
        Ok(Self {
            app_id,
            app_secret,
            encrypt_key: encrypt_key.filter(|s| !s.is_empty()),
            verification_token: verification_token.filter(|s| !s.is_empty()),
            callback_path: callback_path
                .filter(|s| !s.is_empty())
                .unwrap_or(default_callback),
            api_base: api_base
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
        })
    }

    /// Read the properties out of the harness channel-specific property map.
    pub fn from_map(channel_id: &str, props: &BTreeMap<String, Value>) -> Result<Self> {
        Self::from_json(channel_id, &crate::common::props::object_from_btree(props))
    }

    /// Read the properties out of a JSON object (Java `from(channelId, Map)` analogue).
    pub fn from_json(channel_id: &str, props: &Value) -> Result<Self> {
        let default_callback = format!("/api/channels/feishu/{channel_id}/callback");
        Self::new(
            channel_id,
            get_str(props, "appId"),
            get_str(props, "appSecret"),
            get_str(props, "encryptKey"),
            get_str(props, "verificationToken"),
            Some(get_str_or(props, "callbackPath", &default_callback)),
            get_str(props, "apiBase"),
        )
    }

    /// Whether AES payload decryption is enabled for this channel.
    pub fn is_encrypted(&self) -> bool {
        self.encrypt_key
            .as_deref()
            .map(|k| !k.trim().is_empty())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_app_id_and_secret() {
        assert!(FeishuChannelProperties::from_json("feishu", &serde_json::json!({})).is_err());
        assert!(FeishuChannelProperties::from_json(
            "feishu",
            &serde_json::json!({"appId": "cli_x"})
        )
        .is_err());
    }

    #[test]
    fn applies_defaults_and_callback_path() {
        let props = serde_json::json!({"appId": "cli_x", "appSecret": "s"});
        let p = FeishuChannelProperties::from_json("feishu-1", &props).unwrap();
        assert_eq!(p.callback_path, "/api/channels/feishu/feishu-1/callback");
        assert_eq!(p.api_base, DEFAULT_API_BASE);
        assert!(!p.is_encrypted());
        assert!(p.verification_token.is_none());
    }

    #[test]
    fn encrypt_key_enables_encryption() {
        let props = serde_json::json!({
            "appId": "cli_x",
            "appSecret": "s",
            "encryptKey": "k",
            "verificationToken": "vt"
        });
        let p = FeishuChannelProperties::from_json("feishu", &props).unwrap();
        assert!(p.is_encrypted());
        assert_eq!(p.verification_token.as_deref(), Some("vt"));
    }
}
