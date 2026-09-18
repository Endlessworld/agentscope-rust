//! `GitHubChannelProperties` — configuration for one GitHub channel instance.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::common::props::{get_str, get_str_or};
use crate::common::{ChannelError, Result};

pub const DEFAULT_API_BASE: &str = "https://api.github.com";

/// Provider-specific configuration for a single GitHub channel instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubChannelProperties {
    pub token: String,
    pub webhook_secret: String,
    pub api_base: String,
    pub webhook_path: String,
    pub bot_user_login: Option<String>,
}

impl GitHubChannelProperties {
    pub fn new(
        channel_id: &str,
        token: Option<String>,
        webhook_secret: Option<String>,
        api_base: Option<String>,
        webhook_path: Option<String>,
        bot_user_login: Option<String>,
    ) -> Result<Self> {
        let token = token
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ChannelError::Config("github.token is required".to_string()))?;
        let webhook_secret = webhook_secret
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ChannelError::Config("github.webhookSecret is required".to_string()))?;
        let default_path = format!("/api/channels/github/{channel_id}/webhook");
        Ok(Self {
            token,
            webhook_secret,
            api_base: api_base
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
            webhook_path: webhook_path
                .filter(|s| !s.is_empty())
                .unwrap_or(default_path),
            bot_user_login: bot_user_login.filter(|s| !s.is_empty()),
        })
    }

    pub fn from_map(channel_id: &str, props: &BTreeMap<String, Value>) -> Result<Self> {
        Self::from_json(channel_id, &crate::common::props::object_from_btree(props))
    }

    pub fn from_json(channel_id: &str, props: &Value) -> Result<Self> {
        let default_path = format!("/api/channels/github/{channel_id}/webhook");
        Self::new(
            channel_id,
            get_str(props, "token"),
            get_str(props, "webhookSecret"),
            get_str(props, "apiBase"),
            Some(get_str_or(props, "webhookPath", &default_path)),
            get_str(props, "botUserLogin"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_token_and_webhook_secret() {
        assert!(GitHubChannelProperties::from_json("github", &serde_json::json!({})).is_err());
        assert!(GitHubChannelProperties::from_json(
            "github",
            &serde_json::json!({"token": "t"})
        )
        .is_err());
    }

    #[test]
    fn applies_default_path_and_api_base() {
        let props = serde_json::json!({"token": "t", "webhookSecret": "s"});
        let p = GitHubChannelProperties::from_json("gh-1", &props).unwrap();
        assert_eq!(p.api_base, DEFAULT_API_BASE);
        assert_eq!(p.webhook_path, "/api/channels/github/gh-1/webhook");
        assert!(p.bot_user_login.is_none());
    }

    #[test]
    fn honours_explicit_overrides() {
        let props = serde_json::json!({
            "token": "t",
            "webhookSecret": "s",
            "apiBase": "https://ghe.example/api/v3",
            "webhookPath": "/hooks/gh",
            "botUserLogin": "my-bot"
        });
        let p = GitHubChannelProperties::from_json("gh", &props).unwrap();
        assert_eq!(p.api_base, "https://ghe.example/api/v3");
        assert_eq!(p.webhook_path, "/hooks/gh");
        assert_eq!(p.bot_user_login.as_deref(), Some("my-bot"));
    }
}
