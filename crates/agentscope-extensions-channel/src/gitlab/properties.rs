//! `GitLabChannelProperties` — configuration for one GitLab channel instance.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::common::props::{get_str, get_str_or};
use crate::common::{ChannelError, Result};

pub const DEFAULT_API_BASE: &str = "https://gitlab.com/api/v4";

/// Provider-specific configuration for a single GitLab channel instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitLabChannelProperties {
    pub token: String,
    pub webhook_token: String,
    pub api_base: String,
    pub webhook_path: String,
}

impl GitLabChannelProperties {
    pub fn new(
        channel_id: &str,
        token: Option<String>,
        webhook_token: Option<String>,
        api_base: Option<String>,
        webhook_path: Option<String>,
    ) -> Result<Self> {
        let token = token
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ChannelError::Config("gitlab.token is required".to_string()))?;
        let webhook_token = webhook_token
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ChannelError::Config("gitlab.webhookToken is required".to_string()))?;
        let api_base = api_base
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_API_BASE.to_string());
        let api_base = normalise_api_base(&api_base);
        let default_path = format!("/api/channels/gitlab/{channel_id}/webhook");
        Ok(Self {
            token,
            webhook_token,
            api_base,
            webhook_path: webhook_path
                .filter(|s| !s.is_empty())
                .unwrap_or(default_path),
        })
    }

    pub fn from_map(channel_id: &str, props: &BTreeMap<String, Value>) -> Result<Self> {
        Self::from_json(channel_id, &crate::common::props::object_from_btree(props))
    }

    pub fn from_json(channel_id: &str, props: &Value) -> Result<Self> {
        let default_path = format!("/api/channels/gitlab/{channel_id}/webhook");
        Self::new(
            channel_id,
            get_str(props, "token"),
            get_str(props, "webhookToken"),
            get_str(props, "apiBase"),
            Some(get_str_or(props, "webhookPath", &default_path)),
        )
    }
}

/// Mirrors the Java constructor: a bare host gets `/api/v4` appended.
fn normalise_api_base(api_base: &str) -> String {
    if api_base.ends_with("/api/v4") || api_base.contains("/api/v") {
        return api_base.to_string();
    }
    let trimmed = api_base.trim_end_matches('/');
    format!("{trimmed}/api/v4")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_token_and_webhook_token() {
        assert!(GitLabChannelProperties::from_json("gitlab", &serde_json::json!({})).is_err());
        assert!(GitLabChannelProperties::from_json(
            "gitlab",
            &serde_json::json!({"token": "t"})
        )
        .is_err());
    }

    #[test]
    fn appends_api_v4_to_bare_hosts() {
        let props = serde_json::json!({"token": "t", "webhookToken": "w", "apiBase": "https://gitlab.example/"});
        let p = GitLabChannelProperties::from_json("gl", &props).unwrap();
        assert_eq!(p.api_base, "https://gitlab.example/api/v4");
    }

    #[test]
    fn keeps_canonical_api_base_and_defaults() {
        let props = serde_json::json!({});
        let _ = props;
        let p = GitLabChannelProperties::from_json(
            "gl",
            &serde_json::json!({"token": "t", "webhookToken": "w"}),
        )
        .unwrap();
        assert_eq!(p.api_base, DEFAULT_API_BASE);
        assert_eq!(p.webhook_path, "/api/channels/gitlab/gl/webhook");

        let p = GitLabChannelProperties::from_json(
            "gl",
            &serde_json::json!({"token": "t", "webhookToken": "w", "apiBase": "https://gl.example/api/v4"}),
        )
        .unwrap();
        assert_eq!(p.api_base, "https://gl.example/api/v4");
    }
}
