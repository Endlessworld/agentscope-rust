//! Credential layer: provider credentials and model cards.
//! Mirrors `io.agentscope.core.credential`.

pub mod credential;
pub mod model_card;

pub use credential::{
    Credential, DeepSeekCredential, DeepSeekCredentialBuilder, KimiCredential,
    KimiCredentialBuilder, XAICredential, XAICredentialBuilder,
};
pub use model_card::ModelCard;
