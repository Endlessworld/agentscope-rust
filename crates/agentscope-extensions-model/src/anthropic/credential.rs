//! Anthropic credential and API constants.
//!
//! Mirrors `anthropic.credential.AnthropicCredential` and the
//! `AnthropicChatModel` connection defaults. Java keeps the credential in the
//! core `CredentialBase` hierarchy; in Rust the axum credential types live in
//! `agentscope_core::credential`, so this module only carries the
//! Anthropic-specific data needed to build a request.

use serde::{Deserialize, Serialize};

/// Credential type discriminator (mirrors `AnthropicCredential.TYPE`).
pub const CREDENTIAL_TYPE: &str = "anthropic_credential";

/// Default Anthropic API base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Messages endpoint appended to the base URL.
pub const MESSAGES_ENDPOINT: &str = "/v1/messages";

/// Anthropic API version header value (the SDK's pinned default).
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Fallback `max_tokens` when the caller does not specify one. Anthropic
/// requires this field, so it can never be omitted.
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Authentication flavour: the Anthropic docs use `x-api-key`, while
/// Anthropic-compatible gateways accept a bearer token instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AnthropicAuthMode {
    /// `x-api-key: <key>` (Anthropic's native scheme).
    #[default]
    ApiKey,
    /// `Authorization: Bearer <token>` (compatible gateways).
    AuthToken,
}

/// An Anthropic credential: exactly one of `api_key` / `auth_token` is set.
///
/// `apiKey` and `authToken` are mutually exclusive, mirroring the Java
/// `AnthropicChatModel` constructor contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnthropicCredential {
    /// Opaque credential id.
    pub id: String,
    /// API key sent via `x-api-key`.
    #[serde(rename = "api_key", skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Bearer token sent via `Authorization`.
    #[serde(rename = "auth_token", skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    /// Base URL override.
    #[serde(rename = "base_url", skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

impl AnthropicCredential {
    /// Builds a credential from an API key.
    pub fn from_api_key(api_key: impl Into<String>) -> Self {
        Self {
            id: String::new(),
            api_key: Some(api_key.into()),
            auth_token: None,
            base_url: None,
        }
    }

    /// Builds a credential from a bearer token.
    pub fn from_auth_token(token: impl Into<String>) -> Self {
        Self {
            id: String::new(),
            api_key: None,
            auth_token: Some(token.into()),
            base_url: None,
        }
    }

    /// Resolves the credential from explicit key/token overrides, falling back
    /// to `ANTHROPIC_API_KEY` then `ANTHROPIC_AUTH_TOKEN` (the Java provider's
    /// precedence). Returns [`AnthropicAuthMode`] alongside the secret.
    pub fn resolve(
        api_key: Option<&str>,
        auth_token: Option<&str>,
    ) -> Option<(AnthropicAuthMode, String)> {
        if let Some(key) = trim_to_option(api_key) {
            return Some((AnthropicAuthMode::ApiKey, key));
        }
        if let Some(token) = trim_to_option(auth_token) {
            return Some((AnthropicAuthMode::AuthToken, token));
        }
        if let Some(key) = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .and_then(|v| trim_to_option(Some(&v)))
        {
            return Some((AnthropicAuthMode::ApiKey, key));
        }
        std::env::var("ANTHROPIC_AUTH_TOKEN")
            .ok()
            .and_then(|v| trim_to_option(Some(&v)))
            .map(|token| (AnthropicAuthMode::AuthToken, token))
    }
}

/// Trims a string and maps an empty result to `None`.
pub fn trim_to_option(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_prefers_explicit_api_key() {
        let (mode, secret) = AnthropicCredential::resolve(Some("key"), Some("token")).unwrap();
        assert_eq!(mode, AnthropicAuthMode::ApiKey);
        assert_eq!(secret, "key");
    }

    #[test]
    fn resolve_falls_back_to_auth_token() {
        let (mode, secret) = AnthropicCredential::resolve(None, Some("token")).unwrap();
        assert_eq!(mode, AnthropicAuthMode::AuthToken);
        assert_eq!(secret, "token");
    }

    #[test]
    fn resolve_ignores_blank_values() {
        // Only blank explicit values and (usually) no env vars set -> None.
        assert!(AnthropicCredential::resolve(Some("  "), Some("")).is_none());
    }

    #[test]
    fn credential_serializes_optional_fields() {
        let value = serde_json::to_value(AnthropicCredential::from_api_key("k")).unwrap();
        assert_eq!(value["api_key"], serde_json::json!("k"));
        assert!(!value.as_object().unwrap().contains_key("auth_token"));
        assert_eq!(CREDENTIAL_TYPE, "anthropic_credential");
    }

    #[test]
    fn default_base_url_and_endpoint() {
        assert_eq!(DEFAULT_BASE_URL, "https://api.anthropic.com");
        assert_eq!(MESSAGES_ENDPOINT, "/v1/messages");
    }
}
