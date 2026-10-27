//! Credential metadata for model providers, mirroring the Java `CredentialBase` hierarchy.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::model_card::ModelCard;

/// Base trait for provider credentials.
pub trait Credential: Send + Sync {
    /// The stable identifier of this credential instance.
    fn id(&self) -> &str;
    /// The credential type discriminator (e.g. `deepseek_credential`).
    fn credential_type(&self) -> &'static str;
    /// The API key for authentication.
    fn api_key(&self) -> &str;
    /// The base URL of the provider API.
    fn base_url(&self) -> &str;
    /// Enumerates candidate models, if supported by the provider.
    fn list_models(&self) -> Result<Vec<ModelCard>, crate::Error> {
        Err(crate::Error::Other(
            "listModels is not supported for this credential".into(),
        ))
    }
}

fn new_id() -> String {
    Uuid::new_v4().to_string().replace('-', "")
}

/// Credential for the DeepSeek API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeepSeekCredential {
    pub id: String,
    #[serde(rename = "api_key")]
    pub api_key: String,
    #[serde(rename = "base_url")]
    pub base_url: String,
}

impl DeepSeekCredential {
    pub const TYPE: &'static str = "deepseek_credential";
    pub const DEFAULT_BASE_URL: &'static str = "https://api.deepseek.com";

    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            id: new_id(),
            api_key: api_key.into(),
            base_url: Self::DEFAULT_BASE_URL.to_string(),
        }
    }

    pub fn builder() -> DeepSeekCredentialBuilder {
        DeepSeekCredentialBuilder::default()
    }
}

impl Credential for DeepSeekCredential {
    fn id(&self) -> &str {
        &self.id
    }
    fn credential_type(&self) -> &'static str {
        Self::TYPE
    }
    fn api_key(&self) -> &str {
        &self.api_key
    }
    fn base_url(&self) -> &str {
        &self.base_url
    }
}

#[derive(Debug, Default)]
pub struct DeepSeekCredentialBuilder {
    id: Option<String>,
    api_key: Option<String>,
    base_url: Option<String>,
}

impl DeepSeekCredentialBuilder {
    pub fn id(mut self, v: impl Into<String>) -> Self {
        self.id = Some(v.into());
        self
    }
    pub fn api_key(mut self, v: impl Into<String>) -> Self {
        self.api_key = Some(v.into());
        self
    }
    pub fn base_url(mut self, v: impl Into<String>) -> Self {
        self.base_url = Some(v.into());
        self
    }
    pub fn build(self) -> DeepSeekCredential {
        let api_key = self.api_key.expect("apiKey must not be null");
        DeepSeekCredential {
            id: self.id.unwrap_or_else(new_id),
            api_key,
            base_url: self
                .base_url
                .unwrap_or_else(|| DeepSeekCredential::DEFAULT_BASE_URL.into()),
        }
    }
}

/// Credential for the Kimi (Moonshot) API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KimiCredential {
    pub id: String,
    #[serde(rename = "api_key")]
    pub api_key: String,
    #[serde(rename = "base_url")]
    pub base_url: String,
}

impl KimiCredential {
    pub const TYPE: &'static str = "kimi_credential";
    pub const DEFAULT_BASE_URL: &'static str = "https://api.moonshot.cn/v1";

    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            id: new_id(),
            api_key: api_key.into(),
            base_url: Self::DEFAULT_BASE_URL.to_string(),
        }
    }

    pub fn builder() -> KimiCredentialBuilder {
        KimiCredentialBuilder::default()
    }
}

impl Credential for KimiCredential {
    fn id(&self) -> &str {
        &self.id
    }
    fn credential_type(&self) -> &'static str {
        Self::TYPE
    }
    fn api_key(&self) -> &str {
        &self.api_key
    }
    fn base_url(&self) -> &str {
        &self.base_url
    }
}

#[derive(Debug, Default)]
pub struct KimiCredentialBuilder {
    id: Option<String>,
    api_key: Option<String>,
    base_url: Option<String>,
}

impl KimiCredentialBuilder {
    pub fn id(mut self, v: impl Into<String>) -> Self {
        self.id = Some(v.into());
        self
    }
    pub fn api_key(mut self, v: impl Into<String>) -> Self {
        self.api_key = Some(v.into());
        self
    }
    pub fn base_url(mut self, v: impl Into<String>) -> Self {
        self.base_url = Some(v.into());
        self
    }
    pub fn build(self) -> KimiCredential {
        let api_key = self.api_key.expect("apiKey must not be null");
        KimiCredential {
            id: self.id.unwrap_or_else(new_id),
            api_key,
            base_url: self
                .base_url
                .unwrap_or_else(|| KimiCredential::DEFAULT_BASE_URL.into()),
        }
    }
}

/// Credential for the xAI API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XAICredential {
    pub id: String,
    #[serde(rename = "api_key")]
    pub api_key: String,
}

impl XAICredential {
    pub const TYPE: &'static str = "xai_credential";

    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            id: new_id(),
            api_key: api_key.into(),
        }
    }

    pub fn builder() -> XAICredentialBuilder {
        XAICredentialBuilder::default()
    }
}

impl Credential for XAICredential {
    fn id(&self) -> &str {
        &self.id
    }
    fn credential_type(&self) -> &'static str {
        Self::TYPE
    }
    fn api_key(&self) -> &str {
        &self.api_key
    }
    fn base_url(&self) -> &str {
        "https://api.x.ai"
    }
}

#[derive(Debug, Default)]
pub struct XAICredentialBuilder {
    id: Option<String>,
    api_key: Option<String>,
}

impl XAICredentialBuilder {
    pub fn id(mut self, v: impl Into<String>) -> Self {
        self.id = Some(v.into());
        self
    }
    pub fn api_key(mut self, v: impl Into<String>) -> Self {
        self.api_key = Some(v.into());
        self
    }
    pub fn build(self) -> XAICredential {
        XAICredential {
            id: self.id.unwrap_or_else(new_id),
            api_key: self.api_key.expect("apiKey must not be null"),
        }
    }
}
