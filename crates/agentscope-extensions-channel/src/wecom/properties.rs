//! `WeComChannelProperties` — configuration for one WeCom channel instance.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::common::props::{get_int, get_str, get_str_or};
use crate::common::{ChannelError, Result};

pub const DEFAULT_API_BASE: &str = "https://qyapi.weixin.qq.com";

/// Provider-specific configuration for a single WeCom channel instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeComChannelProperties {
    pub corp_id: String,
    pub agent_id: i32,
    pub secret: String,
    pub token: String,
    pub encoding_aes_key: String,
    pub callback_path: String,
    pub api_base: String,
}

impl WeComChannelProperties {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        channel_id: &str,
        corp_id: Option<String>,
        agent_id: i64,
        secret: Option<String>,
        token: Option<String>,
        encoding_aes_key: Option<String>,
        callback_path: Option<String>,
        api_base: Option<String>,
    ) -> Result<Self> {
        let corp_id = corp_id
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ChannelError::Config("wecom.corpId is required".to_string()))?;
        if agent_id <= 0 {
            return Err(ChannelError::Config(
                "wecom.agentId must be a positive integer".to_string(),
            ));
        }
        let secret = secret
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ChannelError::Config("wecom.secret is required".to_string()))?;
        let token = token
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ChannelError::Config("wecom.token is required".to_string()))?;
        let encoding_aes_key = encoding_aes_key
            .filter(|s| s.chars().count() == 43)
            .ok_or_else(|| {
                ChannelError::Config("wecom.encodingAesKey must be 43 characters".to_string())
            })?;
        let default_callback = format!("/api/channels/wecom/{channel_id}/callback");
        Ok(Self {
            corp_id,
            agent_id: agent_id as i32,
            secret,
            token,
            encoding_aes_key,
            callback_path: callback_path
                .filter(|s| !s.is_empty())
                .unwrap_or(default_callback),
            api_base: api_base
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
        })
    }

    pub fn from_map(channel_id: &str, props: &BTreeMap<String, Value>) -> Result<Self> {
        Self::from_json(channel_id, &crate::common::props::object_from_btree(props))
    }

    pub fn from_json(channel_id: &str, props: &Value) -> Result<Self> {
        let agent_id = get_int(props, "agentId").unwrap_or(0);
        let default_callback = format!("/api/channels/wecom/{channel_id}/callback");
        Self::new(
            channel_id,
            get_str(props, "corpId"),
            agent_id,
            get_str(props, "secret"),
            get_str(props, "token"),
            get_str(props, "encodingAesKey"),
            Some(get_str_or(props, "callbackPath", &default_callback)),
            get_str(props, "apiBase"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> Value {
        serde_json::json!({
            "corpId": "corp-1",
            "agentId": 100001,
            "secret": "s",
            "token": "t",
            "encodingAesKey": "0123456789012345678901234567890123456789012"
        })
    }

    #[test]
    fn requires_corp_id_secret_token_and_aes_key() {
        assert!(WeComChannelProperties::from_json("wecom", &serde_json::json!({})).is_err());
        let mut props = valid();
        props["secret"] = serde_json::json!("");
        assert!(WeComChannelProperties::from_json("wecom", &props).is_err());
        let mut props = valid();
        props["token"] = serde_json::json!("");
        assert!(WeComChannelProperties::from_json("wecom", &props).is_err());
        let mut props = valid();
        props["encodingAesKey"] = serde_json::json!("too-short");
        assert!(WeComChannelProperties::from_json("wecom", &props).is_err());
    }

    #[test]
    fn requires_positive_agent_id() {
        let mut props = valid();
        props["agentId"] = serde_json::json!(0);
        assert!(WeComChannelProperties::from_json("wecom", &props).is_err());
        assert!(WeComChannelProperties::from_json("wecom", &serde_json::json!({"corpId":"c","agentId":"not-a-number","secret":"s","token":"t","encodingAesKey":"0123456789012345678901234567890123456789012"})).is_err());
    }

    #[test]
    fn accepts_numeric_strings_and_sets_defaults() {
        let mut props = valid();
        props["agentId"] = serde_json::json!("100001");
        let p = WeComChannelProperties::from_json("wecom-1", &props).unwrap();
        assert_eq!(p.agent_id, 100001);
        assert_eq!(p.api_base, DEFAULT_API_BASE);
        assert_eq!(p.callback_path, "/api/channels/wecom/wecom-1/callback");
    }
}
