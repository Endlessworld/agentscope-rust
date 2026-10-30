//! `AgentConfig` / `RuntimeAgentConfig` / `ModelConfig`, mirroring
//! `io.agentscope.extensions.scheduler.config`.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use agentscope_core::middleware::Middleware;
use agentscope_core::model::Model;
use agentscope_core::tool::Toolkit;

/// Configuration contract for a model, mirroring Java `ModelConfig`.
///
/// Implementations convert themselves into an [`agentscope_core::model::Model`]
/// instance ready for agent execution.
pub trait ModelConfig: Send + Sync {
    /// The model name / identifier, for logging and routing.
    fn model_name(&self) -> &str;

    /// Creates the model instance described by this configuration.
    fn create_model(&self) -> Arc<dyn Model>;
}

/// A model config backed by an already-constructed [`Model`] instance.
///
/// In Java this role is filled by provider-specific configs such as
/// `DashScopeModelConfig`. Those providers live in the MODEL extension, which is
/// not yet migrated, so we accept a pre-built `Arc<dyn Model>` instead (see
/// tracker §4.2).
#[derive(Clone)]
pub struct StaticModelConfig {
    name: String,
    model: Arc<dyn Model>,
}

impl StaticModelConfig {
    /// Wraps a pre-built model with an explicit name.
    pub fn new(name: impl Into<String>, model: Arc<dyn Model>) -> Self {
        Self {
            name: name.into(),
            model,
        }
    }
}

impl ModelConfig for StaticModelConfig {
    fn model_name(&self) -> &str {
        &self.name
    }

    fn create_model(&self) -> Arc<dyn Model> {
        self.model.clone()
    }
}

/// Serializable agent metadata for scheduled tasks.
///
/// Mirrors Java `AgentConfig`: only serializable fields (`name`, `sys_prompt`,
/// and the model *name*). Runtime objects live in [`RuntimeAgentConfig`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    /// The agent name; used as the task identifier and (in xxl-job) the handler name.
    pub name: String,
    /// Optional system prompt.
    pub sys_prompt: Option<String>,
    /// The model name / identifier (serializable form of the model config).
    pub model_name: String,
}

impl AgentConfig {
    /// Creates a new builder.
    pub fn builder() -> AgentConfigBuilder {
        AgentConfigBuilder::default()
    }

    /// The agent name.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// The system prompt, if any.
    pub fn sys_prompt(&self) -> Option<&str> {
        self.sys_prompt.as_deref()
    }
    /// The model name.
    pub fn model_name(&self) -> &str {
        &self.model_name
    }
}

/// Builder for [`AgentConfig`].
#[derive(Debug, Clone, Default)]
pub struct AgentConfigBuilder {
    name: Option<String>,
    sys_prompt: Option<String>,
    model_name: Option<String>,
}

impl AgentConfigBuilder {
    /// Sets the agent name (required).
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
    /// Sets the system prompt.
    pub fn sys_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.sys_prompt = Some(prompt.into());
        self
    }
    /// Sets the model name (required).
    pub fn model_name(mut self, name: impl Into<String>) -> Self {
        self.model_name = Some(name.into());
        self
    }

    /// Sets the model name from a [`ModelConfig`].
    pub fn model_config(self, cfg: &dyn ModelConfig) -> Self {
        self.model_name(cfg.model_name())
    }

    /// Builds and validates the [`AgentConfig`].
    pub fn build(self) -> Result<AgentConfig, AgentConfigError> {
        let name = self.name.unwrap_or_default();
        if name.trim().is_empty() {
            return Err(AgentConfigError::EmptyName);
        }
        let model_name = self.model_name.unwrap_or_default();
        if model_name.trim().is_empty() {
            return Err(AgentConfigError::MissingModelConfig);
        }
        Ok(AgentConfig {
            name,
            sys_prompt: self.sys_prompt,
            model_name,
        })
    }
}

/// Validation error for [`AgentConfig`] / [`RuntimeAgentConfig`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentConfigError {
    /// Agent name must not be null or empty.
    #[error("Agent name must not be null or empty")]
    EmptyName,
    /// ModelConfig must not be null.
    #[error("ModelConfig must not be null")]
    MissingModelConfig,
    /// Model must not be null when building `RuntimeAgentConfig`.
    ///
    /// Message is byte-identical to Java `BaseScheduleAgentTask#createAgent`
    /// (`throw new IllegalArgumentException("Model cannot be null.")`).
    #[error("Model cannot be null.")]
    MissingModel,
    /// System prompt must not be empty when creating an agent.
    #[error("Parameter configuration error: name or sysPrompt cannot be empty.")]
    MissingSysPrompt,
}

/// Runtime agent configuration: serializable metadata plus non-serializable
/// runtime objects (`model`, `toolkit`, `middleware`).
///
/// Mirrors Java `RuntimeAgentConfig extends AgentConfig`; in Rust we **embed**
/// the `AgentConfig` (no class inheritance) and expose delegating accessors.
#[derive(Clone)]
pub struct RuntimeAgentConfig {
    meta: AgentConfig,
    model: Option<Arc<dyn Model>>,
    toolkit: Option<Toolkit>,
    middleware: Vec<Arc<dyn Middleware>>,
}

impl RuntimeAgentConfig {
    /// Creates a new builder.
    pub fn builder() -> RuntimeAgentConfigBuilder {
        RuntimeAgentConfigBuilder::default()
    }

    /// The embedded serializable metadata.
    pub fn meta(&self) -> &AgentConfig {
        &self.meta
    }
    /// The agent name.
    pub fn name(&self) -> &str {
        &self.meta.name
    }
    /// The system prompt, if any.
    pub fn sys_prompt(&self) -> Option<&str> {
        self.meta.sys_prompt.as_deref()
    }
    /// The model name.
    pub fn model_name(&self) -> &str {
        &self.meta.model_name
    }
    /// The runtime model, if configured.
    pub fn model(&self) -> Option<&Arc<dyn Model>> {
        self.model.as_ref()
    }
    /// The runtime toolkit, if configured.
    pub fn toolkit(&self) -> Option<&Toolkit> {
        self.toolkit.as_ref()
    }
    /// The runtime middleware chain.
    pub fn middleware(&self) -> &[Arc<dyn Middleware>] {
        &self.middleware
    }

    /// Returns a copy of this config with `middleware` appended.
    ///
    /// Java exposes hooks as a mutable `CopyOnWriteArrayList` so a backend can
    /// attach one to an existing config (xxl-job appends its `PostReasoningEvent`
    /// log hook in `XxlJobAgentScheduler`). Rust configs are immutable, so the
    /// equivalent is a copy carrying the extra middleware: the caller's config
    /// keeps its original chain.
    pub fn with_middleware(&self, middleware: Arc<dyn Middleware>) -> Self {
        let mut next = self.clone();
        next.middleware.push(middleware);
        next
    }

    /// Returns a copy with the model replaced.
    pub fn with_model(&self, model: Arc<dyn Model>) -> Self {
        let mut next = self.clone();
        next.model = Some(model);
        next
    }
}

impl From<AgentConfig> for RuntimeAgentConfig {
    fn from(meta: AgentConfig) -> Self {
        Self {
            meta,
            model: None,
            toolkit: None,
            middleware: Vec::new(),
        }
    }
}

/// Builder for [`RuntimeAgentConfig`].
#[derive(Default)]
pub struct RuntimeAgentConfigBuilder {
    meta: Option<AgentConfig>,
    name: Option<String>,
    sys_prompt: Option<String>,
    model_name: Option<String>,
    model: Option<Arc<dyn Model>>,
    toolkit: Option<Toolkit>,
    middleware: Vec<Arc<dyn Middleware>>,
}

impl RuntimeAgentConfigBuilder {
    /// Sets the agent name (required).
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
    /// Sets the system prompt.
    pub fn sys_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.sys_prompt = Some(prompt.into());
        self
    }
    /// Sets the model name (required).
    pub fn model_name(mut self, name: impl Into<String>) -> Self {
        self.model_name = Some(name.into());
        self
    }
    /// Seeds metadata from an existing [`AgentConfig`].
    pub fn meta(mut self, meta: AgentConfig) -> Self {
        self.meta = Some(meta);
        self
    }
    /// Sets the runtime model (required in practice for execution).
    pub fn model(mut self, model: Arc<dyn Model>) -> Self {
        self.model = Some(model);
        self
    }
    /// Sets the runtime toolkit.
    pub fn toolkit(mut self, toolkit: Toolkit) -> Self {
        self.toolkit = Some(toolkit);
        self
    }
    /// Adds a middleware to the chain.
    pub fn middleware(mut self, mw: Arc<dyn Middleware>) -> Self {
        self.middleware.push(mw);
        self
    }

    /// Builds and validates the [`RuntimeAgentConfig`].
    pub fn build(self) -> Result<RuntimeAgentConfig, AgentConfigError> {
        let meta = match self.meta {
            Some(m) => m,
            None => AgentConfig::builder()
                .name(self.name.clone().unwrap_or_default())
                .model_name(self.model_name.clone().unwrap_or_default())
                .sys_prompt_opt(self.sys_prompt.clone())
                .build()?,
        };
        Ok(RuntimeAgentConfig {
            meta,
            model: self.model,
            toolkit: self.toolkit,
            middleware: self.middleware,
        })
    }
}

// Internal helper so `RuntimeAgentConfigBuilder` can forward an optional prompt.
impl AgentConfigBuilder {
    fn sys_prompt_opt(mut self, prompt: Option<String>) -> Self {
        self.sys_prompt = prompt;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_config_requires_name_and_model() {
        assert_eq!(
            AgentConfig::builder().build().unwrap_err(),
            AgentConfigError::EmptyName
        );
        assert_eq!(
            AgentConfig::builder().name("A").build().unwrap_err(),
            AgentConfigError::MissingModelConfig
        );
        let cfg = AgentConfig::builder()
            .name("MyAgent")
            .model_name("qwen-max")
            .sys_prompt("be nice")
            .build()
            .unwrap();
        assert_eq!(cfg.name(), "MyAgent");
        assert_eq!(cfg.sys_prompt(), Some("be nice"));
    }

    #[test]
    fn agent_config_serde_roundtrip() {
        let cfg = AgentConfig::builder()
            .name("A")
            .model_name("m")
            .build()
            .unwrap();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn runtime_config_embeds_meta() {
        let rt = RuntimeAgentConfig::builder()
            .name("A")
            .model_name("m")
            .sys_prompt("p")
            .build()
            .unwrap();
        assert_eq!(rt.name(), "A");
        assert_eq!(rt.meta().model_name, "m");
        assert!(rt.model().is_none());
    }

    #[test]
    fn runtime_config_from_agent_config() {
        let cfg = AgentConfig::builder()
            .name("A")
            .model_name("m")
            .build()
            .unwrap();
        let rt = RuntimeAgentConfig::from(cfg);
        assert_eq!(rt.name(), "A");
    }

    struct NoopMiddleware;

    impl Middleware for NoopMiddleware {}

    #[test]
    fn with_middleware_returns_a_copy_and_keeps_the_original() {
        let base = RuntimeAgentConfig::builder()
            .name("A")
            .model_name("m")
            .build()
            .unwrap();
        assert!(base.middleware().is_empty());

        let with_mw = base.with_middleware(Arc::new(NoopMiddleware));
        assert_eq!(with_mw.middleware().len(), 1);
        assert!(
            base.middleware().is_empty(),
            "with_middleware must not mutate the original config"
        );
    }
}
