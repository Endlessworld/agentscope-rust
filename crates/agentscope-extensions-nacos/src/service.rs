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
///
/// **2026-10-31 (NACOS-15 ruling A)**: the previous single `{code, message}` struct is now a
/// classified enum, so callers can branch on *why* a call failed instead of inferring it from
/// a synthetic `500` code:
///
/// * [`AiServiceError::Transport`] — connect/timeout/DNS failure; safe to retry (NACOS-06);
/// * [`AiServiceError::Http`] — a non-success HTTP status without a Nacos envelope;
/// * [`AiServiceError::Business`] — a Nacos business code (`code != 0`, e.g. `NOT_FOUND`);
/// * [`AiServiceError::Serde`] — the payload could not be decoded (never retryable).
///
/// The historical `code`/`message` contract is preserved through [`Self::code`] /
/// [`Self::message`] plus the [`Self::new`] / [`Self::not_found`] constructors, so existing
/// Java-side reasoning (`NacosException#getErrCode`) still applies.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AiServiceError {
    /// The transport failed before a response was read (retryable).
    #[error("nacos transport error: {message}")]
    Transport {
        /// Human-readable detail.
        message: String,
    },
    /// The server answered with a non-success HTTP status and no Nacos error envelope.
    #[error("nacos http error {status}: {message}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Human-readable detail.
        message: String,
    },
    /// The server answered with a Nacos business error code (`code != 0`).
    #[error("nacos error {code}: {message}")]
    Business {
        /// Nacos error code (`NacosException#getErrCode`).
        code: i64,
        /// Human-readable detail.
        message: String,
    },
    /// The response payload could not be decoded into the expected model.
    #[error("nacos decode error: {message}")]
    Serde {
        /// Human-readable detail.
        message: String,
    },
}

impl AiServiceError {
    /// Nacos `NOT_FOUND` error code, mirrored from `NacosException.NOT_FOUND`.
    pub const NOT_FOUND: i64 = 404;
    /// Nacos `INVALID_PARAM` error code, mirrored from `NacosException.INVALID_PARAM`.
    pub const INVALID_PARAM: i64 = 400;

    /// Builds a [`AiServiceError::Business`] error, mirroring `new NacosException(code, message)`.
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self::Business {
            code,
            message: message.into(),
        }
    }

    /// Builds a [`AiServiceError::Transport`] error.
    pub fn transport(message: impl Into<String>) -> Self {
        Self::Transport {
            message: message.into(),
        }
    }

    /// Builds a [`AiServiceError::Http`] error.
    pub fn http(status: u16, message: impl Into<String>) -> Self {
        Self::Http {
            status,
            message: message.into(),
        }
    }

    /// Builds a [`AiServiceError::Serde`] error.
    pub fn serde(message: impl Into<String>) -> Self {
        Self::Serde {
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(Self::NOT_FOUND, message)
    }

    /// The Nacos error code a caller would see from `NacosException#getErrCode`.
    ///
    /// Transport/decode failures have no Nacos code, so they report the historical `500`.
    pub fn code(&self) -> i64 {
        match self {
            Self::Transport { .. } | Self::Serde { .. } => 500,
            Self::Http { status, .. } => i64::from(*status),
            Self::Business { code, .. } => *code,
        }
    }

    /// The human-readable detail.
    pub fn message(&self) -> &str {
        match self {
            Self::Transport { message }
            | Self::Serde { message }
            | Self::Http { message, .. }
            | Self::Business { message, .. } => message.as_str(),
        }
    }

    pub fn is_not_found(&self) -> bool {
        self.code() == Self::NOT_FOUND
    }

    /// Whether the operation may succeed when retried (transport failure, 429 or 5xx).
    ///
    /// Mirrors the Nacos SDK retry policy: business codes and decode failures are terminal.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport { .. } => true,
            Self::Http { status, .. } => is_retryable_status(*status),
            Self::Business { .. } | Self::Serde { .. } => false,
        }
    }
}

/// Whether an HTTP status is worth retrying (`429` or `5xx`).
pub(crate) fn is_retryable_status(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

/// Exponential backoff for a background loop: `base` while healthy, `base * 2^failures`
/// (capped at `cap`) while failing.
///
/// Shared by the Nacos polling loop and the registry heartbeat so both recover from an outage
/// at the same pace, mirroring the model-call retry schedule (`2, 4, 8, … 60s`).
pub(crate) fn exponential_backoff(
    base: std::time::Duration,
    failures: u32,
    cap: std::time::Duration,
) -> std::time::Duration {
    if failures == 0 {
        return base;
    }
    let factor = 2f64.powi(failures.min(16) as i32);
    let backoff = std::time::Duration::from_secs_f64(base.as_secs_f64() * factor);
    backoff.min(cap.max(base))
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
    /// The template's declared variables, mirroring `Prompt#getVariables()`.
    ///
    /// Their defaults are applied when rendering and are overridden by the caller's args.
    pub variables: Option<Vec<NacosPromptVariable>>,
}

impl NacosPrompt {
    pub fn new(template: impl Into<String>) -> Self {
        Self {
            template: template.into(),
            version: None,
            label: None,
            variables: None,
        }
    }

    /// Renders the template, replacing `{{key}}` and `${key}` placeholders with
    /// the supplied values. Mirrors `Prompt#render(Map)`.
    pub fn render(&self, args: Option<&HashMap<String, String>>) -> String {
        render_template(&self.template, args)
    }
}

/// A prompt template variable, mirroring
/// `com.alibaba.nacos.api.ai.model.prompt.PromptVariable`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct NacosPromptVariable {
    /// Variable name, replaced as `{{name}}` when rendering.
    pub name: String,
    /// Default value applied when the caller supplies no override (`None`: no default).
    #[serde(default, rename = "defaultValue")]
    pub default_value: Option<String>,
}

/// A `(name, version, label)` selector triple: the identity of a prompt subscription or a
/// skill download in Nacos.
///
/// Version and label are mutually exclusive, so at most one side is populated.
pub type SelectionKey = (String, Option<String>, Option<String>);

/// Normalizes one prompt/skill selector: `None`/blank means "unspecified" (trimmed).
pub fn normalize_selector(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Normalizes the `(version, label)` pair, rejecting both at once (Nacos: version and label
/// are mutually exclusive selectors).
pub(crate) fn normalize_selectors(
    version: Option<&str>,
    label: Option<&str>,
) -> AiServiceResult<(Option<String>, Option<String>)> {
    let version = normalize_selector(version);
    let label = normalize_selector(label);
    if version.is_some() && label.is_some() {
        return Err(AiServiceError::new(
            AiServiceError::INVALID_PARAM,
            "prompt version and label are mutually exclusive",
        ));
    }
    Ok((version, label))
}

/// Builds a [`SelectionKey`] with normalized selectors.
pub fn selection_key(name: &str, version: Option<&str>, label: Option<&str>) -> SelectionKey {
    (
        name.to_string(),
        normalize_selector(version),
        normalize_selector(label),
    )
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

/// Endpoint type released with an AgentCard, mirroring `AiConstants.A2a.A2A_ENDPOINT_TYPE_SERVICE`.
///
/// Nacos expects `releaseAgentCard(card, endpointType, setAsLatest)`; the Java module always
/// passes the `service` type (NACOS-05). The value is a trait parameter, so a caller that
/// publishes some other Nacos endpoint type can pass its own string.
pub const A2A_ENDPOINT_TYPE_SERVICE: &str = "service";

/// A recorded [`NacosAiService::release_agent_card`] call (see [`InMemoryNacosAiService`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasedAgentCard {
    /// Agent name the card was published for.
    pub name: String,
    /// Endpoint type sent with the release (`service` in the Java module).
    pub endpoint_type: String,
    /// Whether the release asked Nacos to mark the card as the latest version.
    pub set_as_latest: bool,
}

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
    ///
    /// `endpoint_type` is the Nacos endpoint type; [`A2A_ENDPOINT_TYPE_SERVICE`] is what the
    /// Java module sends (NACOS-05).
    async fn release_agent_card(
        &self,
        card: &NacosAgentCard,
        endpoint_type: &str,
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

    /// Cancels the subscription registered for exactly `(prompt_key, version, label)`.
    ///
    /// Nacos has no unsubscribe call (the Java module disposes the whole `AiService`), so a
    /// client only drops its local bookkeeping: further pushes for that exact selection are no
    /// longer delivered. The default implementation is a no-op for services that keep no local
    /// subscription state.
    async fn unsubscribe_prompt(
        &self,
        _prompt_key: &str,
        _version: Option<&str>,
        _label: Option<&str>,
    ) -> AiServiceResult<()> {
        Ok(())
    }

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
    released: Mutex<Vec<ReleasedAgentCard>>,
    endpoints: Mutex<Vec<(String, NacosAgentEndpoint)>>,
    prompts: Mutex<HashMap<String, NacosPrompt>>,
    /// Prompts seeded for exactly one version/label selection.
    prompt_selections: Mutex<HashMap<SelectionKey, NacosPrompt>>,
    prompt_listeners: Mutex<HashMap<SelectionKey, PromptListener>>,
    prompt_subscriptions: Mutex<Vec<SelectionKey>>,
    prompt_unsubscriptions: Mutex<Vec<SelectionKey>>,
    /// One-shot failure the next `subscribe_prompt` call returns.
    prompt_subscribe_error: Mutex<Option<AiServiceError>>,
    /// One-shot gate the next `subscribe_prompt` call awaits (slow-server tests).
    prompt_subscribe_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// Failure `get_agent_card` keeps returning until it is cleared.
    agent_card_error: Mutex<Option<AiServiceError>>,
    card_listeners: Mutex<HashMap<String, AgentCardListener>>,
    skills: Mutex<HashMap<String, SkillZip>>,
    skill_downloads: Mutex<Vec<SelectionKey>>,
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

    /// Seeds a prompt for exactly one version/label selection, mirroring a Nacos prompt that
    /// has several published versions / labels.
    pub fn set_prompt_selection(
        &self,
        key: impl Into<String>,
        version: Option<&str>,
        label: Option<&str>,
        prompt: NacosPrompt,
    ) {
        let key = key.into();
        self.prompt_selections
            .lock()
            .unwrap()
            .insert(selection_key(&key, version, label), prompt);
    }

    /// Makes the next [`NacosAiService::subscribe_prompt`] call fail with `error`.
    pub fn fail_next_prompt_subscribe(&self, error: AiServiceError) {
        *self.prompt_subscribe_error.lock().unwrap() = Some(error);
    }

    /// Gates the next [`NacosAiService::subscribe_prompt`] call on the returned semaphore
    /// (created without permits), so a test can hold a subscription open and assert what other
    /// selections may do meanwhile.
    pub fn gate_next_prompt_subscribe(&self) -> Arc<tokio::sync::Semaphore> {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *self.prompt_subscribe_gate.lock().unwrap() = Some(Arc::clone(&gate));
        gate
    }

    /// Makes [`NacosAiService::get_agent_card`] fail with `error` until cleared with `None`.
    pub fn set_agent_card_error(&self, error: Option<AiServiceError>) {
        *self.agent_card_error.lock().unwrap() = error;
    }

    /// Seeds a skill ZIP served by the `download_skill_zip*` methods.
    pub fn set_skill_zip(&self, name: impl Into<String>, zip: SkillZip) {
        self.skills.lock().unwrap().insert(name.into(), zip);
    }

    /// Records released AgentCards `(name, set_as_latest)`.
    pub fn released_cards(&self) -> Vec<(String, bool)> {
        self.released
            .lock()
            .unwrap()
            .iter()
            .map(|released| (released.name.clone(), released.set_as_latest))
            .collect()
    }

    /// Records the full release payloads, including the `endpointType` (NACOS-05).
    pub fn released_cards_detailed(&self) -> Vec<ReleasedAgentCard> {
        self.released.lock().unwrap().clone()
    }

    /// Records registered endpoints.
    pub fn registered_endpoints(&self) -> Vec<(String, NacosAgentEndpoint)> {
        self.endpoints.lock().unwrap().clone()
    }

    /// Records prompt subscriptions `(key, version, label)` for assertions.
    pub fn prompt_subscriptions(&self) -> Vec<SelectionKey> {
        self.prompt_subscriptions.lock().unwrap().clone()
    }

    /// Records prompt unsubscriptions `(key, version, label)` for assertions.
    pub fn prompt_unsubscriptions(&self) -> Vec<SelectionKey> {
        self.prompt_unsubscriptions.lock().unwrap().clone()
    }

    /// Records the selection of every skill download `(name, version, label)`.
    pub fn skill_downloads(&self) -> Vec<SelectionKey> {
        self.skill_downloads.lock().unwrap().clone()
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

    /// Pushes an updated prompt to every subscription of `key`, mirroring a Nacos config change
    /// push. Returns whether at least one subscription existed.
    pub fn fire_prompt_update(&self, key: &str, prompt: NacosPrompt) -> bool {
        self.prompts
            .lock()
            .unwrap()
            .insert(key.to_string(), prompt.clone());
        let listeners: Vec<PromptListener> = self
            .prompt_listeners
            .lock()
            .unwrap()
            .iter()
            .filter(|(selection, _)| selection.0 == key)
            .map(|(_, listener)| Arc::clone(listener))
            .collect();
        for listener in &listeners {
            listener(key, &prompt);
        }
        !listeners.is_empty()
    }

    /// Pushes an update to exactly one selection's subscription, the way a Nacos server routes
    /// an event to the subscription that selected it. Returns whether one existed.
    pub fn fire_prompt_update_for(
        &self,
        key: &str,
        version: Option<&str>,
        label: Option<&str>,
        prompt: NacosPrompt,
    ) -> bool {
        let selection = selection_key(key, version, label);
        self.prompt_selections
            .lock()
            .unwrap()
            .insert(selection.clone(), prompt.clone());
        let listener = self
            .prompt_listeners
            .lock()
            .unwrap()
            .get(&selection)
            .cloned();
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
        endpoint_type: &str,
        set_as_latest: bool,
    ) -> AiServiceResult<()> {
        self.cards
            .lock()
            .unwrap()
            .insert(card.name.clone(), card.clone());
        self.released.lock().unwrap().push(ReleasedAgentCard {
            name: card.name.clone(),
            endpoint_type: endpoint_type.to_string(),
            set_as_latest,
        });
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
        if let Some(error) = self.agent_card_error.lock().unwrap().clone() {
            return Err(error);
        }
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
        version: Option<&str>,
        label: Option<&str>,
        listener: PromptListener,
    ) -> AiServiceResult<Option<NacosPrompt>> {
        if let Some(error) = self.prompt_subscribe_error.lock().unwrap().take() {
            return Err(error);
        }
        let gate = self.prompt_subscribe_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            // One permit is added by the test once it wants the subscription to complete.
            let _permit = gate
                .acquire()
                .await
                .map_err(|_| AiServiceError::transport("prompt subscribe gate closed"))?;
        }
        let selection = selection_key(prompt_key, version, label);
        let prompt = self
            .prompt_selections
            .lock()
            .unwrap()
            .get(&selection)
            .cloned()
            .or_else(|| self.prompts.lock().unwrap().get(prompt_key).cloned());
        self.prompt_listeners
            .lock()
            .unwrap()
            .insert(selection.clone(), listener);
        self.prompt_subscriptions.lock().unwrap().push(selection);
        Ok(prompt)
    }

    async fn unsubscribe_prompt(
        &self,
        prompt_key: &str,
        version: Option<&str>,
        label: Option<&str>,
    ) -> AiServiceResult<()> {
        let selection = selection_key(prompt_key, version, label);
        self.prompt_listeners.lock().unwrap().remove(&selection);
        self.prompt_unsubscriptions.lock().unwrap().push(selection);
        Ok(())
    }

    async fn download_skill_zip(&self, skill_name: &str) -> AiServiceResult<SkillZip> {
        self.record_skill_download(skill_name, None, None);
        self.skill(skill_name)
    }

    async fn download_skill_zip_by_version(
        &self,
        skill_name: &str,
        version: &str,
    ) -> AiServiceResult<SkillZip> {
        self.record_skill_download(skill_name, Some(version), None);
        self.skill(skill_name)
    }

    async fn download_skill_zip_by_label(
        &self,
        skill_name: &str,
        label: &str,
    ) -> AiServiceResult<SkillZip> {
        self.record_skill_download(skill_name, None, Some(label));
        self.skill(skill_name)
    }
}

impl InMemoryNacosAiService {
    fn skill(&self, skill_name: &str) -> AiServiceResult<SkillZip> {
        self.skills
            .lock()
            .unwrap()
            .get(skill_name)
            .cloned()
            .ok_or_else(|| AiServiceError::not_found(format!("skill not found: {skill_name}")))
    }

    fn record_skill_download(&self, skill_name: &str, version: Option<&str>, label: Option<&str>) {
        self.skill_downloads
            .lock()
            .unwrap()
            .push(selection_key(skill_name, version, label));
    }
}
