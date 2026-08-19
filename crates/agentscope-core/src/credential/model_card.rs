//! Description of one candidate chat model under a provider.
use serde::{Deserialize, Serialize};

/// Minimal description of a candidate chat model under a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCard {
    /// The model identifier.
    #[serde(rename = "model_name")]
    pub model_name: String,
    /// Human-readable display name.
    #[serde(rename = "display_name", skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Context window size in tokens.
    #[serde(rename = "context_size", skip_serializing_if = "Option::is_none")]
    pub context_size: Option<i32>,
}
