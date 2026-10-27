//! The Nacos AI service abstraction.
//!
//! Mirrors the subset of `com.alibaba.nacos.api.ai.AiService` this crate needs.
//! The Java module receives an `AiService` from `AiFactory.createAiService`;
//! the Rust port inverts the dependency: callers implement [`NacosAiService`]
//! and inject it. [`InMemoryNacosAiService`] is a test/embeddable implementation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::a2a::model::{NacosAgentCard, NacosAgentEndpoint};

/// Error type for Nacos AI operations, mirroring `NacosException`.
#[derive(Debug, Clone, thiserror::Error)]
#[error("nacos error {code}: {message}")]
pub struct AiServiceError {
    pub code: i64,
    pub message: String,
}

impl AiServiceError {
    /// Nacos `NOT_FOUND` error code, mirrored from `NacosException.NOT_FOUND`.
    pub const NOT_FOUND: i64 = 404;
    /// Nacos `INVALID_PARAM` error code, mirrored from `NacosException.INVALID_PARAM`.
    pub const INVALID_PARAM: i64 = 400;

    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(Self::NOT_FOUND, message)
    }

    pub fn is_not_found(&self) -> bool {
        self.code == Self::NOT_FOUND
    }
}

/// Convenience result alias.
pub type AiServiceResult<T> = Result<T, AiServiceError>;

/// A prompt payload returned by [`NacosAiService::subscribe_prompt`], mirroring
/// `com.alibaba.nacos.api.ai.model.prompt.Prompt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NacosPrompt {
    pub template: String,
    pub version: Option<String>,
    pub label: Option<String>,
}

impl NacosPrompt {
    pub fn new(template: impl Into<String>) -> Self {
        Self {
            template: template.into(),
            version: None,
            label: None,
        }
    }

    /// Renders the template, replacing `{{key}}` and `${key}` placeholders with
    /// the supplied values. Mirrors `Prompt#render(Map)`.
    pub fn render(&self, args: Option<&HashMap<String, String>>) -> String {
        render_template(&self.template, args)
    }
}

/// Renders `{{key}}` / `${key}` placeholders in `template` with `args`.
///
/// Unknown placeholders are left untouched, matching Java `Prompt#render`
/// behaviour for unresolved variables.
pub fn render_template(template: &str, args: Option<&HashMap<String, String>>) -> String {
    let Some(args) = args else {
        return template.to_string();
    };
    if args.is_empty() {
        return template.to_string();
    }
    let mut out = template.to_string();
    for (key, value) in args {
        out = out.replace(&format!("{{{{{key}}}}}"), value);
        out = out.replace(&format!("${{{key}}}"), value);
    }
    out
}

/// A skill package downloaded from Nacos, mirroring the byte[] returned by
/// `AiService#downloadSkillZip*`.
pub type SkillZip = Vec<u8>;

/// Callback invoked when a subscribed prompt changes, mirroring
/// `AbstractNacosPromptListener#onEvent`.
pub type PromptListener = Arc<dyn Fn(&str, &NacosPrompt) + Send + Sync>;

/// Callback invoked when a subscribed AgentCard changes, mirroring
/// `AbstractNacosAgentCardListener#onEvent`.
pub type AgentCardListener = Arc<dyn Fn(&str, &NacosAgentCard) + Send + Sync>;

/// The Nacos AI service surface used by this crate.
///
/// Mirrors the relevant methods of `com.alibaba.nacos.api.ai.AiService`.
#[async_trait]
pub trait NacosAiService: Send + Sync {
    // ---- A2A registry ----

    /// Publishes an AgentCard, mirroring `releaseAgentCard(card, endpointType, setAsLatest)`.
    async fn release_agent_card(
        &self,
        card: &NacosAgentCard,
        set_as_latest: bool,
    ) -> AiServiceResult<()>;

    /// Registers a single Agent endpoint, mirroring `registerAgentEndpoint(name, endpoint)`.
    async fn register_agent_endpoint(
        &self,
        agent_name: &str,
        endpoint: &NacosAgentEndpoint,
    ) -> AiServiceResult<()>;

    /// Fetches an AgentCard by name + version, mirroring `getAgentCard(name, version)`.
    async fn get_agent_card(
        &self,
        agent_name: &str,
        version: Option<&str>,
    ) -> AiServiceResult<Option<NacosAgentCard>>;

    /// Subscribes to an AgentCard and returns the current value, mirroring
    /// `subscribeAgentCard(name, listener)`.
    async fn subscribe_agent_card(
        &self,
        agent_name: &str,
        listener: AgentCardListener,
    ) -> AiServiceResult<NacosAgentCard>;

    // ---- Prompt ----

    /// Subscribes to a prompt and returns the current value, mirroring
    /// `subscribePrompt(key, version, label, listener)`.
    async fn subscribe_prompt(
        &self,
        prompt_key: &str,
        version: Option<&str>,
        label: Option<&str>,
        listener: PromptListener,
    ) -> AiServiceResult<Option<NacosPrompt>>;

    // ---- Skill ----

    /// Downloads a skill ZIP, mirroring `downloadSkillZip(skillName)`.
    async fn download_skill_zip(&self, skill_name: &str) -> AiServiceResult<SkillZip>;

    /// Downloads a skill ZIP by version, mirroring `downloadSkillZipByVersion`.
    async fn download_skill_zip_by_version(
        &self,
        skill_name: &str,
        version: &str,
    ) -> AiServiceResult<SkillZip>;

    /// Downloads a skill ZIP by label, mirroring `downloadSkillZipByLabel`.
    async fn download_skill_zip_by_label(
        &self,
        skill_name: &str,
        label: &str,
    ) -> AiServiceResult<SkillZip>;
}

/// In-memory [`NacosAiService`] for tests and embedding.
///
/// Mirrors the in-memory maps an embedder would populate. Not a real Nacos
/// client — it performs no network I/O.
#[derive(Default)]
pub struct InMemoryNacosAiService {
    cards: Mutex<HashMap<String, NacosAgentCard>>,
    released: Mutex<Vec<(String, bool)>>,
    endpoints: Mutex<Vec<(String, NacosAgentEndpoint)>>,
    prompts: Mutex<HashMap<String, NacosPrompt>>,
    prompt_listeners: Mutex<HashMap<String, PromptListener>>,
    prompt_subscriptions: Mutex<Vec<(String, Option<String>, Option<String>)>>,
    card_listeners: Mutex<HashMap<String, AgentCardListener>>,
    skills: Mutex<HashMap<String, SkillZip>>,
}

impl InMemoryNacosAiService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds an AgentCard served by [`NacosAiService::subscribe_agent_card`] /
    /// [`NacosAiService::get_agent_card`].
    pub fn set_agent_card(&self, name: impl Into<String>, card: NacosAgentCard) {
        self.cards.lock().unwrap().insert(name.into(), card);
    }

    /// Seeds a prompt served by [`NacosAiService::subscribe_prompt`].
    pub fn set_prompt(&self, key: impl Into<String>, prompt: NacosPrompt) {
        self.prompts.lock().unwrap().insert(key.into(), prompt);
    }

    /// Seeds a skill ZIP served by the `download_skill_zip*` methods.
    pub fn set_skill_zip(&self, name: impl Into<String>, zip: SkillZip) {
        self.skills.lock().unwrap().insert(name.into(), zip);
    }

    /// Records released AgentCards `(name, set_as_latest)`.
    pub fn released_cards(&self) -> Vec<(String, bool)> {
        self.released.lock().unwrap().clone()
    }

    /// Records registered endpoints.
    pub fn registered_endpoints(&self) -> Vec<(String, NacosAgentEndpoint)> {
        self.endpoints.lock().unwrap().clone()
    }

    /// Records prompt subscriptions `(key, version, label)` for assertions.
    pub fn prompt_subscriptions(&self) -> Vec<(String, Option<String>, Option<String>)> {
        self.prompt_subscriptions.lock().unwrap().clone()
    }

    /// Pushes an updated AgentCard to the registered subscriber, mirroring a Nacos
    /// config/registry push. Returns whether a subscription existed.
    pub fn fire_agent_card_update(&self, name: &str, card: NacosAgentCard) -> bool {
        self.cards
            .lock()
            .unwrap()
            .insert(name.to_string(), card.clone());
        let listener = self.card_listeners.lock().unwrap().get(name).cloned();
        match listener {
            Some(listener) => {
                listener(name, &card);
                true
            }
            None => false,
        }
    }

    /// Pushes an updated prompt to the registered subscriber, mirroring a Nacos
    /// config change push. Returns whether a subscription existed.
    pub fn fire_prompt_update(&self, key: &str, prompt: NacosPrompt) -> bool {
        self.prompts
            .lock()
            .unwrap()
            .insert(key.to_string(), prompt.clone());
        let listener = self.prompt_listeners.lock().unwrap().get(key).cloned();
        match listener {
            Some(listener) => {
                listener(key, &prompt);
                true
            }
            None => false,
        }
    }
}

#[async_trait]
impl NacosAiService for InMemoryNacosAiService {
    async fn release_agent_card(
        &self,
        card: &NacosAgentCard,
        set_as_latest: bool,
    ) -> AiServiceResult<()> {
        self.cards
            .lock()
            .unwrap()
            .insert(card.name.clone(), card.clone());
        self.released
            .lock()
            .unwrap()
            .push((card.name.clone(), set_as_latest));
        Ok(())
    }

    async fn register_agent_endpoint(
        &self,
        agent_name: &str,
        endpoint: &NacosAgentEndpoint,
    ) -> AiServiceResult<()> {
        self.endpoints
            .lock()
            .unwrap()
            .push((agent_name.to_string(), endpoint.clone()));
        Ok(())
    }

    async fn get_agent_card(
        &self,
        agent_name: &str,
        _version: Option<&str>,
    ) -> AiServiceResult<Option<NacosAgentCard>> {
        Ok(self.cards.lock().unwrap().get(agent_name).cloned())
    }

    async fn subscribe_agent_card(
        &self,
        agent_name: &str,
        listener: AgentCardListener,
    ) -> AiServiceResult<NacosAgentCard> {
        let card = self
            .cards
            .lock()
            .unwrap()
            .get(agent_name)
            .cloned()
            .ok_or_else(|| {
                AiServiceError::not_found(format!("agent card not found: {agent_name}"))
            })?;
        self.card_listeners
            .lock()
            .unwrap()
            .insert(agent_name.to_string(), listener);
        Ok(card)
    }

    async fn subscribe_prompt(
        &self,
        prompt_key: &str,
        _version: Option<&str>,
        _label: Option<&str>,
        listener: PromptListener,
    ) -> AiServiceResult<Option<NacosPrompt>> {
        self.prompt_listeners
            .lock()
            .unwrap()
            .insert(prompt_key.to_string(), listener);
        self.prompt_subscriptions.lock().unwrap().push((
            prompt_key.to_string(),
            _version.map(|v| v.to_string()),
            _label.map(|l| l.to_string()),
        ));
        Ok(self.prompts.lock().unwrap().get(prompt_key).cloned())
    }

    async fn download_skill_zip(&self, skill_name: &str) -> AiServiceResult<SkillZip> {
        self.skills
            .lock()
            .unwrap()
            .get(skill_name)
            .cloned()
            .ok_or_else(|| AiServiceError::not_found(format!("skill not found: {skill_name}")))
    }

    async fn download_skill_zip_by_version(
        &self,
        skill_name: &str,
        _version: &str,
    ) -> AiServiceResult<SkillZip> {
        self.download_skill_zip(skill_name).await
    }

    async fn download_skill_zip_by_label(
        &self,
        skill_name: &str,
        _label: &str,
    ) -> AiServiceResult<SkillZip> {
        self.download_skill_zip(skill_name).await
    }
}
