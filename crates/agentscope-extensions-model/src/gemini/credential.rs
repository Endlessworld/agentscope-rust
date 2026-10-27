//! Gemini credential and API constants.
//!
//! Mirrors `gemini.credential.GeminiCredential` plus the connection defaults
//! baked into the Java `GeminiChatModel` / Google GenAI SDK. As with the other
//! adapters, the credential is a plain data carrier: the core `Credential`
//! hierarchy in `agentscope_core::credential` covers the vendor-generic case.

use serde::{Deserialize, Serialize};

/// Credential type discriminator (mirrors `GeminiCredential.TYPE`).
pub const CREDENTIAL_TYPE: &str = "gemini_credential";

/// Default Google Generative Language API base URL.
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

/// REST API version prefix used by the Google SDK.
pub const API_VERSION: &str = "v1beta";

/// API-key header (Google's REST convention; the SDK sets the same header).
pub const API_KEY_HEADER: &str = "x-goog-api-key";

/// Environment variable read when the creation context carries no key.
pub const API_KEY_ENV: &str = "GEMINI_API_KEY";

/// Builds the `generateContent` endpoint path for a model.
pub fn generate_content_path(model: &str) -> String {
    format!("/{API_VERSION}/models/{model}:generateContent")
}

/// Builds the streaming (`streamGenerateContent`) endpoint path for a model.
///
/// `alt=sse` selects Server-Sent Events rather than the default JSON array of
/// chunk objects, which is what [`crate::common::decode_stream_line`] parses.
pub fn stream_generate_content_path(model: &str) -> String {
    format!("/{API_VERSION}/models/{model}:streamGenerateContent?alt=sse")
}

/// A Gemini API credential (API-key flavour).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeminiCredential {
    /// Opaque credential id.
    pub id: String,
    /// API key sent via [`API_KEY_HEADER`].
    #[serde(rename = "api_key")]
    pub api_key: String,
    /// Base URL override.
    #[serde(rename = "base_url", skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

impl GeminiCredential {
    /// Builds a credential from an API key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            id: String::new(),
            api_key: api_key.into(),
            base_url: None,
        }
    }

    /// Resolves the API key from an explicit override, then `GEMINI_API_KEY`
    /// (the precedence used by the Java `GeminiModelProvider`).
    pub fn resolve_api_key(explicit: Option<&str>) -> Option<String> {
        explicit
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                std::env::var(API_KEY_ENV)
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_match_the_rest_api_shape() {
        assert_eq!(
            generate_content_path("gemini-2.5-flash"),
            "/v1beta/models/gemini-2.5-flash:generateContent"
        );
        assert_eq!(
            stream_generate_content_path("gemini-2.5-flash"),
            "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn resolve_api_key_prefers_the_explicit_value() {
        assert_eq!(
            GeminiCredential::resolve_api_key(Some(" k ")),
            Some("k".to_string())
        );
        assert_eq!(GeminiCredential::resolve_api_key(Some("   ")), None);
    }

    #[test]
    fn credential_serializes_api_key() {
        let value = serde_json::to_value(GeminiCredential::new("k")).unwrap();
        assert_eq!(value["api_key"], serde_json::json!("k"));
        assert!(!value.as_object().unwrap().contains_key("base_url"));
        assert_eq!(CREDENTIAL_TYPE, "gemini_credential");
    }

    #[test]
    fn default_base_url_and_version() {
        assert_eq!(
            DEFAULT_BASE_URL,
            "https://generativelanguage.googleapis.com"
        );
        assert_eq!(API_VERSION, "v1beta");
    }
}
