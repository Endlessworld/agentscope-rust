//! `DingTalkChannelProperties` — configuration for one DingTalk Stream channel instance.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::common::props::{get_str, get_str_or};
use crate::common::{ChannelError, Result};

/// Configuration for a single DingTalk channel instance (Stream protocol).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DingTalkChannelProperties {
    pub app_key: String,
    pub app_secret: String,
    pub robot_code: String,
    pub api_base: String,
    pub oapi_base: String,
    pub stream_register_url: String,
}

impl DingTalkChannelProperties {
    pub const DEFAULT_API_BASE: &'static str = "https://api.dingtalk.com";
    pub const DEFAULT_OAPI_BASE: &'static str = "https://oapi.dingtalk.com";
    pub const DEFAULT_STREAM_REGISTER_URL: &'static str =
        "https://api.dingtalk.com/v1.0/gateway/connections/open";

    /// Validate + normalise. Required fields: `app_key`, `app_secret`, `robot_code`.
    pub fn new(
        app_key: Option<String>,
        app_secret: Option<String>,
        robot_code: Option<String>,
        api_base: Option<String>,
        oapi_base: Option<String>,
        stream_register_url: Option<String>,
    ) -> Result<Self> {
        let require = |v: Option<String>, field: &str| -> Result<String> {
            match v {
                Some(s) if !s.trim().is_empty() => Ok(s),
                _ => Err(ChannelError::Config(format!("dingtalk.{field} is required"))),
            }
        };
        let or_default = |v: Option<String>, default: &str| -> String {
            match v {
                Some(s) if !s.trim().is_empty() => s,
                _ => default.to_string(),
            }
        };
        Ok(Self {
            app_key: require(app_key, "appKey")?,
            app_secret: require(app_secret, "appSecret")?,
            robot_code: require(robot_code, "robotCode")?,
            api_base: or_default(api_base, Self::DEFAULT_API_BASE),
            oapi_base: or_default(oapi_base, Self::DEFAULT_OAPI_BASE),
            stream_register_url: or_default(stream_register_url, Self::DEFAULT_STREAM_REGISTER_URL),
        })
    }

    /// Read the properties out of the harness channel-specific property map.
    pub fn from_map(
        channel_id: &str,
        props: &BTreeMap<String, Value>,
    ) -> Result<Self> {
        let _ = channel_id;
        let value = crate::common::props::object_from_btree(props);
        Self::from_json(&value)
    }

    /// Read the properties out of a JSON object (the Java `from(channelId, Map)` analogue).
    pub fn from_json(props: &Value) -> Result<Self> {
        Self::new(
            get_str(props, "appKey"),
            get_str(props, "appSecret"),
            get_str(props, "robotCode"),
            get_str(props, "apiBase"),
            get_str(props, "oapiBase"),
            get_str(props, "streamRegisterUrl"),
        )
    }

    /// Convenience accessor used by tests: read a string property with fallback.
    pub fn str_or(props: &Value, key: &str, default: &str) -> String {
        get_str_or(props, key, default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_app_key_secret_and_robot_code() {
        assert!(DingTalkChannelProperties::new(
            None,
            Some("s".into()),
            Some("r".into()),
            None,
            None,
            None
        )
        .is_err());
        assert!(DingTalkChannelProperties::new(
            Some("k".into()),
            Some("  ".into()),
            Some("r".into()),
            None,
            None,
            None
        )
        .is_err());
        assert!(DingTalkChannelProperties::new(
            Some("k".into()),
            Some("s".into()),
            None,
            None,
            None,
            None
        )
        .is_err());
    }

    #[test]
    fn applies_defaults_for_optional_fields() {
        let props = serde_json::json!({
            "appKey": "k",
            "appSecret": "s",
            "robotCode": "robot-1"
        });
        let p = DingTalkChannelProperties::from_json(&props).unwrap();
        assert_eq!(p.api_base, DingTalkChannelProperties::DEFAULT_API_BASE);
        assert_eq!(p.oapi_base, DingTalkChannelProperties::DEFAULT_OAPI_BASE);
        assert_eq!(
            p.stream_register_url,
            DingTalkChannelProperties::DEFAULT_STREAM_REGISTER_URL
        );
    }

    #[test]
    fn honours_explicit_overrides() {
        let props = serde_json::json!({
            "appKey": "k",
            "appSecret": "s",
            "robotCode": "robot-1",
            "apiBase": "https://corp.example",
            "oapiBase": "https://oapi.example"
        });
        let p = DingTalkChannelProperties::from_json(&props).unwrap();
        assert_eq!(p.api_base, "https://corp.example");
        assert_eq!(p.oapi_base, "https://oapi.example");
    }

    #[test]
    fn numeric_properties_are_stringified() {
        let props = serde_json::json!({"appKey": 1, "appSecret": 2, "robotCode": 3});
        let p = DingTalkChannelProperties::from_json(&props).unwrap();
        assert_eq!(p.app_key, "1");
        assert_eq!(p.robot_code, "3");
    }
}
