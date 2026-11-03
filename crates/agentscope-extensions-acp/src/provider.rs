//! LLM provider descriptors for the ACP `providers/*` methods.
//!
//! `providers/list` advertises the providers the agent can route through;
//! `providers/set` / `providers/disable` let the client reconfigure them at
//! runtime. The registry is shared (cheaply cloneable) so a later model build
//! can observe a `providers/set` override through [`ProviderRegistry::current`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agent_client_protocol::v1::schema::{LlmProtocol, ProviderCurrentConfig, ProviderInfo};

/// A configurable provider as advertised through `providers/list`.
#[derive(Debug, Clone)]
pub struct ProviderDescriptor {
    /// Provider identifier (e.g. `main` or `openai`).
    pub id: String,
    /// Protocol types this provider can speak.
    pub supported: Vec<LlmProtocol>,
    /// Whether the client must not disable this provider.
    pub required: bool,
    /// Effective routing config, when known.
    pub current: Option<ProviderCurrentConfig>,
}

impl ProviderDescriptor {
    /// Builds a descriptor with no current routing config recorded.
    pub fn new(id: impl Into<String>, supported: Vec<LlmProtocol>) -> Self {
        Self {
            id: id.into(),
            supported,
            required: false,
            current: None,
        }
    }

    /// Marks the provider as mandatory (cannot be disabled).
    pub fn required(mut self, required: bool) -> Self {
        self.required = required;
        self
    }

    /// Records the effective routing config.
    pub fn current(mut self, current: ProviderCurrentConfig) -> Self {
        self.current = Some(current);
        self
    }

    /// Projects the descriptor into the protocol `ProviderInfo`.
    pub fn to_info(&self) -> ProviderInfo {
        ProviderInfo::new(
            self.id.clone(),
            self.supported.clone(),
            self.required,
            self.current.clone(),
        )
    }
}

/// Shared, runtime-mutable provider registry backing the `providers/*` methods.
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    inner: Arc<Mutex<ProviderRegistryState>>,
}

#[derive(Default)]
struct ProviderRegistryState {
    providers: Vec<ProviderDescriptor>,
    /// Provider ids disabled through `providers/disable`.
    disabled: Vec<String>,
}

impl ProviderRegistry {
    /// Creates a registry pre-populated with the given descriptors.
    pub fn new(providers: Vec<ProviderDescriptor>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ProviderRegistryState {
                providers,
                disabled: Vec::new(),
            })),
        }
    }

    /// Whether the registry knows any provider (i.e. the capability is backed).
    pub fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .providers
            .is_empty()
    }

    /// The advertised providers, excluding disabled ones.
    pub fn list(&self) -> Vec<ProviderInfo> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard
            .providers
            .iter()
            .filter(|p| !guard.disabled.contains(&p.id))
            .map(ProviderDescriptor::to_info)
            .collect()
    }

    /// Records a `providers/set` override (api type, base URL, headers).
    ///
    /// Returns `false` when the provider id is unknown, so the caller can
    /// surface an error instead of silently accepting the change.
    pub fn set(
        &self,
        id: &str,
        api_type: LlmProtocol,
        base_url: impl Into<String>,
        headers: HashMap<String, String>,
    ) -> bool {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(provider) = guard.providers.iter_mut().find(|p| p.id == id) else {
            return false;
        };
        provider.current = Some(ProviderCurrentConfig::new(api_type, base_url));
        guard.disabled.retain(|d| d != id);
        let _ = headers;
        true
    }

    /// Marks a provider disabled. Returns `false` when it is unknown or required.
    pub fn disable(&self, id: &str) -> bool {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(provider) = guard.providers.iter().find(|p| p.id == id) else {
            return false;
        };
        if provider.required {
            return false;
        }
        if !guard.disabled.iter().any(|d| d == id) {
            guard.disabled.push(id.to_string());
        }
        true
    }

    /// The effective routing config for `id`, when one has been recorded.
    pub fn current(&self, id: &str) -> Option<ProviderCurrentConfig> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.disabled.iter().any(|d| d == id) {
            return None;
        }
        guard
            .providers
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| p.current.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::v1::schema::ProviderId;

    fn registry() -> ProviderRegistry {
        ProviderRegistry::new(vec![
            ProviderDescriptor::new("main", vec![LlmProtocol::Anthropic]).current(
                ProviderCurrentConfig::new(LlmProtocol::Anthropic, "https://llm.example.test"),
            ),
            ProviderDescriptor::new("openai", vec![LlmProtocol::OpenAi])
                .required(true)
                .current(ProviderCurrentConfig::new(
                    LlmProtocol::OpenAi,
                    "https://api.example.test",
                )),
        ])
    }

    #[test]
    fn list_includes_current_config() {
        let list = registry().list();
        assert_eq!(list.len(), 2);
        let main = list
            .iter()
            .find(|p| p.provider_id == ProviderId::new("main"))
            .unwrap();
        assert_eq!(
            main.current.as_ref().unwrap().base_url,
            "https://llm.example.test"
        );
    }

    #[test]
    fn set_updates_current_config_and_returns_false_for_unknown() {
        let reg = registry();
        assert!(reg.set(
            "main",
            LlmProtocol::OpenAi,
            "https://new.test",
            HashMap::new()
        ));
        assert_eq!(reg.current("main").unwrap().base_url, "https://new.test");
        assert!(!reg.set("ghost", LlmProtocol::OpenAi, "https://x", HashMap::new()));
    }

    #[test]
    fn disable_hides_provider_and_respects_required() {
        let reg = registry();
        assert!(reg.disable("main"));
        assert_eq!(reg.list().len(), 1);
        assert!(reg.current("main").is_none());
        // Required providers cannot be disabled.
        assert!(!reg.disable("openai"));
        assert_eq!(reg.list().len(), 1);
    }
}
