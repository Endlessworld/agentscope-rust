//! `ScheduleAgentTask` trait, `AgentFactory` and `BaseScheduleAgentTask`,
//! mirroring `io.agentscope.extensions.scheduler.ScheduleAgentTask` and
//! `BaseScheduleAgentTask`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use agentscope_core::error::{Error, Result};
use agentscope_core::message::Msg;
use agentscope_core::model::Model;

use crate::common::config::{AgentConfigError, RuntimeAgentConfig, ScheduleConfig};

/// Builds a fresh agent for a single scheduled execution.
///
/// Java `BaseScheduleAgentTask` builds a `ReActAgent` internally; in Rust the
/// construction is delegated to this trait so the crate stays decoupled from the
/// concrete agent type (and tests can inject fakes).
#[async_trait]
pub trait AgentFactory: Send + Sync {
    /// Builds a fresh agent instance from the runtime configuration and runs a
    /// single call with the given input messages.
    async fn run(&self, config: &RuntimeAgentConfig, messages: Vec<Msg>) -> Result<Msg>;

    /// Resolves the model to execute with when the runtime config carries none.
    ///
    /// Java turns the serializable `AgentConfig.modelConfig` into a `Model`
    /// inside `BaseScheduleAgentTask#createAgent` (`getModelConfig().createModel()`).
    /// This crate has no provider registry, so only the host can map a model
    /// *name* to a `Model`; that lookup lives on the factory. When a model is
    /// resolved here it is injected into the config handed to
    /// [`run`](Self::run), so the factory can build its agent from
    /// `config.model()` exactly like Java does.
    ///
    /// The default keeps the pre-existing behaviour: the config's own model, if
    /// any (`None` when the config only carries a model name).
    fn resolve_model(&self, config: &RuntimeAgentConfig) -> Option<Arc<dyn Model>> {
        config.model().cloned()
    }
}

/// Optional control surface a backend can attach to a task.
#[async_trait]
pub trait TaskControl: Send + Sync {
    /// Pauses future executions.
    async fn pause(&self) -> Result<()>;
    /// Resumes future executions.
    async fn resume(&self) -> Result<()>;
    /// Interrupts a currently running execution, if any.
    async fn interrupt(&self) -> Result<()>;
}

/// A handle to a scheduled agent task, mirroring Java `ScheduleAgentTask`.
#[async_trait]
pub trait ScheduleAgentTask: Send + Sync {
    /// Downcast hook so backends can recover their concrete task type from an
    /// `Arc<dyn ScheduleAgentTask>`.
    fn as_any(&self) -> &dyn std::any::Any;
    /// The unique identifier of this task.
    fn id(&self) -> &str;
    /// The human-readable name (derived from the agent config).
    fn name(&self) -> &str;
    /// Executes the task immediately with the given input messages.
    async fn run(&self, messages: Vec<Msg>) -> Result<Msg>;
    /// Executes the task immediately with no input.
    async fn run_empty(&self) -> Result<Msg> {
        self.run(Vec::new()).await
    }
    /// Cancels the task permanently and removes it from the scheduler.
    fn cancel(&self);
}

/// Default [`ScheduleAgentTask`] implementation.
///
/// Each execution creates a fresh agent via the injected [`AgentFactory`], giving
/// state isolation. Execution counting and the cancelled flag mirror the Java
/// `AtomicLong` / `AtomicBoolean` fields.
pub struct BaseScheduleAgentTask {
    id: String,
    agent_config: Arc<RuntimeAgentConfig>,
    schedule_config: ScheduleConfig,
    factory: Arc<dyn AgentFactory>,
    cancelled: AtomicBool,
    execution_count: AtomicU64,
    /// Invoked on [`cancel`](Self::cancel) with the task name — used by the
    /// owning scheduler to remove the task from its registry.
    on_cancel: Option<Arc<dyn Fn(&str) + Send + Sync>>,
}

impl BaseScheduleAgentTask {
    /// Creates a task without a cancel callback.
    pub fn new(
        agent_config: Arc<RuntimeAgentConfig>,
        schedule_config: ScheduleConfig,
        factory: Arc<dyn AgentFactory>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            agent_config,
            schedule_config,
            factory,
            cancelled: AtomicBool::new(false),
            execution_count: AtomicU64::new(0),
            on_cancel: None,
        }
    }

    /// Attaches a callback invoked when the task is cancelled.
    pub fn with_on_cancel(mut self, cb: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        self.on_cancel = Some(cb);
        self
    }

    /// Whether the task has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// The schedule configuration used by this task.
    pub fn schedule_config(&self) -> &ScheduleConfig {
        &self.schedule_config
    }

    /// The runtime agent configuration.
    pub fn agent_config(&self) -> &RuntimeAgentConfig {
        &self.agent_config
    }

    /// The total number of executions (manual + scheduled).
    pub fn execution_count(&self) -> u64 {
        self.execution_count.load(Ordering::SeqCst)
    }

    /// Increments the execution counter.
    pub fn increment_execution_count(&self) {
        self.execution_count.fetch_add(1, Ordering::SeqCst);
    }

    /// Executes directly using this task's stored agent config, bypassing the
    /// input-message argument (used by backend job runners).
    pub async fn run_with(&self, messages: Vec<Msg>) -> Result<Msg> {
        if self.is_cancelled() {
            return Err(Error::Agent(format!(
                "Cannot run a cancelled task: {}",
                self.name()
            )));
        }
        // Validate before creating an agent, mirroring Java `createAgent`:
        // name/sysPrompt first, then the model (`Model cannot be null.`).
        if self.agent_config.name().trim().is_empty()
            || self
                .agent_config
                .sys_prompt()
                .unwrap_or("")
                .trim()
                .is_empty()
        {
            return Err(Error::Agent(AgentConfigError::MissingSysPrompt.to_string()));
        }
        // Java `createAgent` builds the model from the config (`createModel()`)
        // before validating; the Rust equivalent is a host-side lookup, so the
        // config's model and the factory's resolution are both accepted. Only
        // when neither can produce one does the Java `Model cannot be null.`
        // error apply.
        let resolved_model = match self.agent_config.model() {
            Some(model) => Some(model.clone()),
            None => self.factory.resolve_model(&self.agent_config),
        };
        let Some(model) = resolved_model else {
            return Err(Error::Agent(AgentConfigError::MissingModel.to_string()));
        };
        // Hand the resolved model to the factory so `run` sees the same config
        // Java's `createAgent` would have built.
        let config = if self.agent_config.model().is_some() {
            self.agent_config.clone()
        } else {
            Arc::new(self.agent_config.with_model(model))
        };
        let result = self.factory.run(&config, messages).await;
        self.increment_execution_count();
        result
    }

    /// Cancels the task (idempotent).
    pub fn do_cancel(&self) {
        if !self.cancelled.swap(true, Ordering::SeqCst) {
            if let Some(cb) = &self.on_cancel {
                cb(self.name());
            }
        }
    }
}

#[async_trait]
impl ScheduleAgentTask for BaseScheduleAgentTask {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        self.agent_config.name()
    }

    async fn run(&self, messages: Vec<Msg>) -> Result<Msg> {
        self.run_with(messages).await
    }

    fn cancel(&self) {
        self.do_cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    use crate::common::config::AgentConfig;

    struct CountingFactory {
        calls: AtomicUsize,
        fail: bool,
    }

    #[async_trait]
    impl AgentFactory for CountingFactory {
        async fn run(&self, _config: &RuntimeAgentConfig, messages: Vec<Msg>) -> Result<Msg> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(Error::Agent("boom".into()));
            }
            Ok(Msg::assistant(format!("echo:{}", messages.len())))
        }
    }

    fn runtime_config() -> RuntimeAgentConfig {
        RuntimeAgentConfig::builder()
            .name("Agent")
            .model_name("m")
            .sys_prompt("sys")
            .model(crate::test_support::stub_model("ok"))
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn run_increments_count_and_delegates() {
        let factory = Arc::new(CountingFactory {
            calls: AtomicUsize::new(0),
            fail: false,
        });
        let task = BaseScheduleAgentTask::new(
            Arc::new(runtime_config()),
            ScheduleConfig::none(),
            factory.clone(),
        );
        let reply = task.run(vec![Msg::user("hi")]).await.unwrap();
        assert_eq!(reply.text_content(), "echo:1");
        assert_eq!(task.execution_count(), 1);
        assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelled_task_cannot_run() {
        let factory = Arc::new(CountingFactory {
            calls: AtomicUsize::new(0),
            fail: false,
        });
        let task =
            BaseScheduleAgentTask::new(Arc::new(runtime_config()), ScheduleConfig::none(), factory);
        task.cancel();
        assert!(task.is_cancelled());
        let err = task.run_empty().await.unwrap_err();
        assert!(err.to_string().contains("cancelled"));
    }

    #[tokio::test]
    async fn cancel_is_idempotent_and_invokes_callback_once() {
        let factory = Arc::new(CountingFactory {
            calls: AtomicUsize::new(0),
            fail: false,
        });
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = hits.clone();
        let task =
            BaseScheduleAgentTask::new(Arc::new(runtime_config()), ScheduleConfig::none(), factory)
                .with_on_cancel(Arc::new(move |_name| {
                    hits_clone.fetch_add(1, Ordering::SeqCst);
                }));
        task.cancel();
        task.cancel();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn missing_sys_prompt_fails_without_calling_factory() {
        let factory = Arc::new(CountingFactory {
            calls: AtomicUsize::new(0),
            fail: false,
        });
        let cfg = RuntimeAgentConfig::builder()
            .name("A")
            .model_name("m")
            .build()
            .unwrap();
        let task =
            BaseScheduleAgentTask::new(Arc::new(cfg), ScheduleConfig::none(), factory.clone());
        assert!(task.run_empty().await.is_err());
        assert_eq!(factory.calls.load(Ordering::SeqCst), 0);
        assert_eq!(task.execution_count(), 0);
    }

    #[tokio::test]
    async fn missing_model_fails_with_the_java_message() {
        // Java `BaseScheduleAgentTask#createAgent` throws
        // `IllegalArgumentException("Model cannot be null.")` after the name/sysPrompt
        // check, before any agent is built.
        let factory = Arc::new(CountingFactory {
            calls: AtomicUsize::new(0),
            fail: false,
        });
        let cfg = RuntimeAgentConfig::builder()
            .name("A")
            .model_name("m")
            .sys_prompt("sys")
            .build()
            .unwrap();
        assert!(cfg.model().is_none());
        let task =
            BaseScheduleAgentTask::new(Arc::new(cfg), ScheduleConfig::none(), factory.clone());

        let err = task.run_empty().await.unwrap_err();
        // `Error::Agent` renders as `Agent error: {0}`; the Java text is the payload.
        match err {
            Error::Agent(message) => assert_eq!(message, "Model cannot be null."),
            other => panic!("expected an agent error, got {other:?}"),
        }
        assert_eq!(factory.calls.load(Ordering::SeqCst), 0);
        assert_eq!(task.execution_count(), 0);
    }

    #[tokio::test]
    async fn failed_execution_still_counts() {
        // Java increments the counter after `run` regardless of the outcome, so a
        // failing execution is counted too.
        let factory = Arc::new(CountingFactory {
            calls: AtomicUsize::new(0),
            fail: true,
        });
        let task = BaseScheduleAgentTask::new(
            Arc::new(runtime_config()),
            ScheduleConfig::none(),
            factory.clone(),
        );
        assert!(task.run_empty().await.is_err());
        assert_eq!(task.execution_count(), 1);
        assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
    }

    /// Factory that resolves a model itself, the way a host with a provider
    /// registry does, and records what the config carried when `run` was called.
    struct ResolvingFactory {
        calls: AtomicUsize,
        saw_model: AtomicBool,
    }

    #[async_trait]
    impl AgentFactory for ResolvingFactory {
        fn resolve_model(&self, config: &RuntimeAgentConfig) -> Option<Arc<dyn Model>> {
            // Only the serializable model *name* is available on this path.
            (config.model_name() == "qwen-max").then(|| crate::test_support::stub_model("resolved"))
        }

        async fn run(&self, config: &RuntimeAgentConfig, _messages: Vec<Msg>) -> Result<Msg> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.saw_model
                .store(config.model().is_some(), Ordering::SeqCst);
            Ok(Msg::assistant("ran"))
        }
    }

    #[tokio::test]
    async fn factory_resolved_model_is_accepted_and_handed_to_run() {
        // `RuntimeAgentConfig::from(AgentConfig)` carries no model (that is the
        // serializable `schedule(AgentConfig)` path), so before
        // `AgentFactory::resolve_model` every run aborted with
        // `Model cannot be null.` even though the host knew how to build one.
        let factory = Arc::new(ResolvingFactory {
            calls: AtomicUsize::new(0),
            saw_model: AtomicBool::new(false),
        });
        let cfg = RuntimeAgentConfig::from(
            AgentConfig::builder()
                .name("A")
                .model_name("qwen-max")
                .sys_prompt("sys")
                .build()
                .unwrap(),
        );
        assert!(cfg.model().is_none(), "serializable path carries no model");
        let task =
            BaseScheduleAgentTask::new(Arc::new(cfg), ScheduleConfig::none(), factory.clone());

        let reply = task.run_empty().await.unwrap();
        assert_eq!(reply.text_content(), "ran");
        assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
        assert!(
            factory.saw_model.load(Ordering::SeqCst),
            "the factory-resolved model must be injected into the config handed to `run`"
        );
        assert_eq!(task.execution_count(), 1);
    }
}
