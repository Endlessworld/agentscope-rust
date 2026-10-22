//! Credential for the `openai-official` provider.
//!
//! Mirrors `credential.OpenAIOfficialCredential`: the JSON shape is
//! `{id, type, api_key, organization, base_url}` where only `api_key` is
//! required. The environment fallbacks (`OPENAI_API_KEY` / `OPENAI_BASE_URL`)
//! happen at provider/model construction time, matching the Java
//! `OpenAIOfficialModelProvider` and `OpenAIResponsesChatModel.Builder`.

use serde::{Deserialize, Serialize};

use super::constants::{API_KEY_ENV, BASE_URL_ENV};

/// Credential type discriminator written to JSON.
pub const CREDENTIAL_TYPE: &str = "openai_official_credential";

/// OpenAI official credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAIOfficialCredential {
    /// Stable credential id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// API key (required).
    pub api_key: String,
    /// Optional OpenAI organization id (builder-level header).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// Optional base URL override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

impl OpenAIOfficialCredential {
    /// Credential type discriminator.
    pub fn credential_type(&self) -> &'static str {
        CREDENTIAL_TYPE
    }

    /// Creates a credential from an explicit API key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            id: None,
            api_key: api_key.into(),
            organization: None,
            base_url: None,
        }
    }

    /// Builder-style setter for the credential id.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Builder-style setter for the organization.
    pub fn with_organization(mut self, organization: impl Into<String>) -> Self {
        self.organization = Some(organization.into());
        self
    }

    /// Builder-style setter for the base URL.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// Resolves the API key: explicit value first, then `OPENAI_API_KEY`.
    ///
    /// Blank values are treated as absent (Java `firstNonBlank`).
    pub fn resolve_api_key(explicit: Option<&str>) -> Option<String> {
        first_non_blank(explicit).or_else(|| first_non_blank(std::env::var(API_KEY_ENV).ok().as_deref()))
    }

    /// Resolves the base URL: explicit value first, then `OPENAI_BASE_URL`.
    pub fn resolve_base_url(explicit: Option<&str>) -> Option<String> {
        first_non_blank(explicit)
            .or_else(|| first_non_blank(std::env::var(BASE_URL_ENV).ok().as_deref()))
    }
}

fn first_non_blank(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trip_uses_java_field_names() {
        let json = serde_json::json!({
            "id": "cred-1",
            "type": "openai_official_credential",
            "api_key": "sk-test",
            "organization": "org-1",
            "base_url": "https://proxy.test/v1"
        });
        let cred: OpenAIOfficialCredential = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(cred.api_key, "sk-test");
        assert_eq!(cred.organization.as_deref(), Some("org-1"));
        assert_eq!(cred.credential_type(), "openai_official_credential");

        let value = serde_json::to_value(&cred).unwrap();
        assert_eq!(value["api_key"], serde_json::json!("sk-test"));
        assert_eq!(value["base_url"], serde_json::json!("https://proxy.test/v1"));
    }

    #[test]
    fn explicit_values_win_over_environment() {
        assert_eq!(
            OpenAIOfficialCredential::resolve_api_key(Some("  explicit  ")).as_deref(),
            Some("explicit")
        );
        assert_eq!(
            OpenAIOfficialCredential::resolve_base_url(Some(" https://explicit.test ")).as_deref(),
            Some("https://explicit.test")
        );
    }

    #[test]
    fn blank_values_are_treated_as_absent() {
        // Only meaningful when the env var is unset/blank in the test process.
        if first_non_blank(std::env::var(API_KEY_ENV).ok().as_deref()).is_none() {
            assert_eq!(OpenAIOfficialCredential::resolve_api_key(Some("   ")), None);
        }
        if first_non_blank(std::env::var(BASE_URL_ENV).ok().as_deref()).is_none() {
            assert_eq!(OpenAIOfficialCredential::resolve_base_url(Some("")), None);
        }
    }
}
