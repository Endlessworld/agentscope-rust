//! DashScope credentials and endpoint selection.
//!
//! Mirrors `DashScopeCredential` and `EndpointType`. The credential is a plain
//! value type (the Rust equivalent of the Java `CredentialBase` subclass): the
//! provider passes `api_key`/`base_url` directly to the model.

use serde::{Deserialize, Serialize};

/// Default DashScope base URL (native protocol).
pub const DEFAULT_BASE_URL: &str = "https://dashscope.aliyuncs.com";

/// Explicit endpoint selection, overriding model-name auto-detection.
///
/// Mirrors `io.agentscope.extensions.model.dashscope.EndpointType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "UPPERCASE")]
pub enum EndpointType {
    /// Auto-detect from the model name (default).
    #[default]
    Auto,
    /// Force the text-generation API.
    Text,
    /// Force the multimodal-generation API.
    Multimodal,
}

impl EndpointType {
    /// Parses a case-insensitive option string (e.g. from `GenerateOptions`).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_uppercase().as_str() {
            "AUTO" => Some(EndpointType::Auto),
            "TEXT" => Some(EndpointType::Text),
            "MULTIMODAL" => Some(EndpointType::Multimodal),
            _ => None,
        }
    }
}

/// DashScope credential: API key plus an optional base-URL override.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashScopeCredential {
    /// Credential id (informational).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// DashScope API key.
    #[serde(rename = "api_key")]
    pub api_key: String,
    /// Base URL; defaults to [`DEFAULT_BASE_URL`].
    #[serde(rename = "base_url", default = "default_base_url")]
    pub base_url: String,
}

/// Credential type discriminator (mirrors Java `TYPE`).
pub const CREDENTIAL_TYPE: &str = "dashscope_credential";

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_string()
}

impl DashScopeCredential {
    /// Creates a credential, falling back to the default base URL when blank.
    pub fn new(api_key: impl Into<String>, base_url: Option<String>) -> Self {
        let api_key = api_key.into();
        let base_url = base_url
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(default_base_url);
        Self {
            id: None,
            api_key,
            base_url,
        }
    }

    /// Sets the credential id.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// The credential type discriminator.
    pub fn credential_type(&self) -> &'static str {
        CREDENTIAL_TYPE
    }

    /// Reads the API key from `DASHSCOPE_API_KEY` when `api_key` is blank.
    pub fn resolve(api_key: Option<String>, base_url: Option<String>) -> Option<Self> {
        let key = api_key
            .filter(|k| !k.trim().is_empty())
            .or_else(|| std::env::var("DASHSCOPE_API_KEY").ok());
        key.map(|key| Self::new(key, base_url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_defaults_base_url_and_round_trips() {
        let credential = DashScopeCredential::new("sk-test", None);
        assert_eq!(credential.base_url, DEFAULT_BASE_URL);
        assert_eq!(credential.credential_type(), CREDENTIAL_TYPE);

        let json = serde_json::to_string(&credential).unwrap();
        assert!(json.contains("\"api_key\":\"sk-test\""));
        assert!(json.contains("\"base_url\""));
        let back: DashScopeCredential = serde_json::from_str(&json).unwrap();
        assert_eq!(back, credential);
    }

    #[test]
    fn credential_accepts_json_without_base_url() {
        let credential: DashScopeCredential =
            serde_json::from_str(r#"{"api_key":"sk-x"}"#).unwrap();
        assert_eq!(credential.base_url, DEFAULT_BASE_URL);
        assert_eq!(credential.id, None);
    }

    #[test]
    fn endpoint_type_parsing() {
        assert_eq!(
            EndpointType::parse("multimodal"),
            Some(EndpointType::Multimodal)
        );
        assert_eq!(EndpointType::parse(" TEXT "), Some(EndpointType::Text));
        assert_eq!(EndpointType::parse("auto"), Some(EndpointType::Auto));
        assert_eq!(EndpointType::parse("nope"), None);
        assert_eq!(EndpointType::default(), EndpointType::Auto);
        assert_eq!(
            serde_json::to_value(EndpointType::Multimodal).unwrap(),
            serde_json::json!("MULTIMODAL")
        );
    }
}
