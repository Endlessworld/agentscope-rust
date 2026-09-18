//! Training runner: registries, config, router and lifecycle controller.
//!
//! Rust port of `io.agentscope.core.training.runner`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use agentscope_core::error::{Error, Result};
use agentscope_core::message::Msg;
use agentscope_core::model::Model;
use agentscope_core::{Agent, CallableAgent, ReActAgent};

use crate::backend::dto::{CommitRequest, FeedbackRequest};
use crate::backend::{TrinityClient, TrinityModelAdapter};
use crate::reward::RewardCalculator;
use crate::strategy::{SelectionDecision, TrainingAnnotation, TrainingSelectionStrategy};

// ---------------------------------------------------------------------------
// Task / Run identifiers
// ---------------------------------------------------------------------------

/// Generates unique task ids, mirroring `TaskIdGenerator`.
pub mod task_id_generator {
    /// Prefix used for generated task ids.
    pub const PREFIX: &str = "task";

    fn now_millis() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Generates a unique task id of the form `task-{uuid}`.
    pub fn generate() -> String {
        format!("{}-{}", PREFIX, uuid::Uuid::new_v4())
    }

    /// Generates a time-sortable task id of the form `task-{timestamp}-{rand}`.
    pub fn generate_with_timestamp() -> String {
        let timestamp = now_millis();
        let uuid = uuid::Uuid::new_v4().to_string();
        let random_part: String = uuid.chars().take(8).collect();
        format!("{}-{}-{}", PREFIX, timestamp, random_part)
    }
}

// ---------------------------------------------------------------------------
// RunExecutionContext
// ---------------------------------------------------------------------------

/// Execution context for a single Run, mirroring `RunExecutionContext`.
///
/// A *Task* is a user's original request; a *Run* is one execution of it. Each
/// run collects the LLM response `msg_id`s and the messages used to compute the
/// reward.
#[derive(Debug)]
pub struct RunExecutionContext {
    task_id: String,
    run_id: String,
    msg_ids: Mutex<Vec<String>>,
    msgs: Mutex<Vec<Msg>>,
    start_time: u64,
}

impl RunExecutionContext {
    /// Creates a new run context, requiring non-empty task and run ids.
    pub fn create(task_id: impl Into<String>, run_id: impl Into<String>) -> Self {
        let task_id = task_id.into();
        let run_id = run_id.into();
        assert!(!task_id.is_empty(), "Task ID cannot be null or empty");
        assert!(!run_id.is_empty(), "Run ID cannot be null or empty");
        Self {
            task_id,
            run_id,
            msg_ids: Mutex::new(Vec::new()),
            msgs: Mutex::new(Vec::new()),
            start_time: now_millis(),
        }
    }

    /// Adds a message id (ignored when empty).
    pub fn add_msg_id(&self, msg_id: impl Into<String>) {
        let msg_id = msg_id.into();
        if !msg_id.is_empty() {
            self.msg_ids.lock().unwrap().push(msg_id);
        }
    }

    /// Returns a copy of all collected message ids.
    pub fn get_msg_ids(&self) -> Vec<String> {
        self.msg_ids.lock().unwrap().clone()
    }

    /// Number of collected message ids.
    pub fn get_msg_id_count(&self) -> usize {
        self.msg_ids.lock().unwrap().len()
    }

    /// The task id.
    pub fn get_task_id(&self) -> &str {
        &self.task_id
    }

    /// The run id.
    pub fn get_run_id(&self) -> &str {
        &self.run_id
    }

    /// The run start time (milliseconds since the Unix epoch).
    pub fn get_start_time(&self) -> u64 {
        self.start_time
    }

    /// The run duration in milliseconds.
    pub fn get_duration(&self) -> u64 {
        now_millis().saturating_sub(self.start_time)
    }

    /// Whether at least one message id has been collected.
    pub fn has_msg_ids(&self) -> bool {
        !self.msg_ids.lock().unwrap().is_empty()
    }

    /// Adds a message (ignored when `None`).
    pub fn add_msg(&self, msg: Option<Msg>) {
        if let Some(msg) = msg {
            self.msgs.lock().unwrap().push(msg);
        }
    }

    /// Replaces all messages (clears first).
    pub fn set_messages(&self, messages: Vec<Msg>) {
        let mut guard = self.msgs.lock().unwrap();
        guard.clear();
        guard.extend(messages);
    }

    /// Returns a copy of all collected messages.
    pub fn get_messages(&self) -> Vec<Msg> {
        self.msgs.lock().unwrap().clone()
    }

    /// Number of collected messages.
    pub fn get_message_count(&self) -> usize {
        self.msgs.lock().unwrap().len()
    }

    /// Whether at least one message has been collected.
    pub fn has_messages(&self) -> bool {
        !self.msgs.lock().unwrap().is_empty()
    }
}

impl PartialEq for RunExecutionContext {
    fn eq(&self, other: &Self) -> bool {
        self.task_id == other.task_id && self.run_id == other.run_id
    }
}

impl Eq for RunExecutionContext {}

impl std::hash::Hash for RunExecutionContext {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.task_id.hash(state);
        self.run_id.hash(state);
    }
}

impl std::fmt::Display for RunExecutionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RunExecution{{task={}, run={}, msgIds={}, msgs={}, duration={}ms}}",
            self.task_id,
            self.run_id,
            self.get_msg_id_count(),
            self.get_message_count(),
            self.get_duration()
        )
    }
}

// ---------------------------------------------------------------------------
// RunRegistry
// ---------------------------------------------------------------------------

/// Allocates monotonically increasing run ids per task, mirroring `RunRegistry`.
static RUN_REGISTRY: OnceLock<Mutex<HashMap<String, AtomicUsize>>> = OnceLock::new();

fn run_registry() -> &'static Mutex<HashMap<String, AtomicUsize>> {
    RUN_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Registry mapping a task id to its run counter.
pub struct RunRegistry;

impl RunRegistry {
    /// Allocates the next run id for `task_id` (starting at `0`).
    pub fn allocate_run_id(task_id: &str) -> String {
        assert!(!task_id.is_empty(), "Task ID cannot be null");
        let mut map = run_registry().lock().unwrap();
        let counter = map.entry(task_id.to_string()).or_insert_with(|| AtomicUsize::new(0));
        let run_id = counter.fetch_add(1, Ordering::SeqCst);
        run_id.to_string()
    }

    /// The current run count for `task_id` (0 when unknown).
    pub fn get_current_run_count(task_id: &str) -> usize {
        if task_id.is_empty() {
            return 0;
        }
        run_registry()
            .lock()
            .unwrap()
            .get(task_id)
            .map(|c| c.load(Ordering::SeqCst))
            .unwrap_or(0)
    }

    /// Removes the counter for `task_id`.
    pub fn cleanup(task_id: &str) {
        if !task_id.is_empty() {
            run_registry().lock().unwrap().remove(task_id);
        }
    }

    /// Removes all counters.
    pub fn clear_all() {
        run_registry().lock().unwrap().clear();
    }

    /// Number of tracked tasks.
    pub fn size() -> usize {
        run_registry().lock().unwrap().len()
    }

    /// Reserved for future time-based cleanup (mirrors `cleanupOldTasks`).
    pub fn cleanup_old_tasks(_ttl: Duration) {}
}

// ---------------------------------------------------------------------------
// TaskExecutionRegistry
// ---------------------------------------------------------------------------

/// Stores execution contexts for all runs of all tasks, mirroring
/// `TaskExecutionRegistry`.
static TASK_EXECUTION_REGISTRY: OnceLock<Mutex<HashMap<String, Vec<Arc<RunExecutionContext>>>>> =
    OnceLock::new();

fn task_execution_registry() -> &'static Mutex<HashMap<String, Vec<Arc<RunExecutionContext>>>> {
    TASK_EXECUTION_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Registry of run execution contexts grouped by task.
pub struct TaskExecutionRegistry;

impl TaskExecutionRegistry {
    /// Registers a run context.
    pub fn register(context: Arc<RunExecutionContext>) {
        let task_id = context.get_task_id().to_string();
        let mut map = task_execution_registry().lock().unwrap();
        map.entry(task_id).or_default().push(context);
    }

    /// Returns all run contexts for `task_id`, sorted ascending by numeric run id.
    pub fn get_runs_by_task(task_id: &str) -> Vec<Arc<RunExecutionContext>> {
        if task_id.is_empty() {
            return Vec::new();
        }
        let map = task_execution_registry().lock().unwrap();
        let mut contexts = map.get(task_id).cloned().unwrap_or_default();
        contexts.sort_by_key(|c| c.get_run_id().parse::<i64>().unwrap_or(0));
        contexts
    }

    /// Returns the context for a specific `(task_id, run_id)`, if present.
    pub fn get_run(task_id: &str, run_id: &str) -> Option<Arc<RunExecutionContext>> {
        if task_id.is_empty() || run_id.is_empty() {
            return None;
        }
        let map = task_execution_registry().lock().unwrap();
        map.get(task_id)
            .and_then(|v| v.iter().find(|c| c.get_run_id() == run_id).cloned())
    }

    /// All known task ids.
    pub fn get_all_task_ids() -> Vec<String> {
        task_execution_registry().lock().unwrap().keys().cloned().collect()
    }

    /// Number of runs registered for `task_id`.
    pub fn get_run_count(task_id: &str) -> usize {
        if task_id.is_empty() {
            return 0;
        }
        task_execution_registry()
            .lock()
            .unwrap()
            .get(task_id)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// Total number of tasks.
    pub fn get_task_count() -> usize {
        task_execution_registry().lock().unwrap().len()
    }

    /// Total number of runs across all tasks.
    pub fn get_total_run_count() -> usize {
        task_execution_registry()
            .lock()
            .unwrap()
            .values()
            .map(Vec::len)
            .sum()
    }

    /// Removes all data for `task_id`, returning the number of runs removed.
    pub fn cleanup(task_id: &str) -> usize {
        if task_id.is_empty() {
            return 0;
        }
        task_execution_registry()
            .lock()
            .unwrap()
            .remove(task_id)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// Removes all data.
    pub fn clear_all() {
        task_execution_registry().lock().unwrap().clear();
    }

    /// Returns a statistical summary.
    pub fn get_stats() -> RegistryStats {
        RegistryStats {
            task_count: Self::get_task_count(),
            total_run_count: Self::get_total_run_count(),
        }
    }
}

/// Statistical summary for [`TaskExecutionRegistry`], mirroring `RegistryStats`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RegistryStats {
    /// Number of tasks.
    pub task_count: usize,
    /// Total number of runs.
    pub total_run_count: usize,
}

impl RegistryStats {
    /// Number of tasks.
    pub fn get_task_count(&self) -> usize {
        self.task_count
    }

    /// Total number of runs.
    pub fn get_total_run_count(&self) -> usize {
        self.total_run_count
    }

    /// Average runs per task (0 when there are no tasks).
    pub fn get_average_runs_per_task(&self) -> f64 {
        if self.task_count > 0 {
            self.total_run_count as f64 / self.task_count as f64
        } else {
            0.0
        }
    }
}

impl std::fmt::Display for RegistryStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RegistryStats{{tasks={}, runs={}, avg={:.2}}}",
            self.task_count,
            self.total_run_count,
            self.get_average_runs_per_task()
        )
    }
}

// ---------------------------------------------------------------------------
// TrainingConfig
// ---------------------------------------------------------------------------

/// Configuration for the training runner, mirroring `TrainingConfig`.
#[derive(Clone)]
pub struct TrainingConfig {
    trinity_endpoint: String,
    trinity_api_key: String,
    model_name: String,
    selection_strategy: Arc<dyn TrainingSelectionStrategy>,
    reward_calculator: Arc<dyn RewardCalculator>,
    commit_interval_seconds: i64,
    http_timeout: Duration,
    enable_auto_commit: bool,
    shadow_pool_size: usize,
    shadow_pool_capacity: usize,
    repeat_time: i32,
}

impl TrainingConfig {
    /// Returns a new builder.
    pub fn builder() -> TrainingConfigBuilder {
        TrainingConfigBuilder::default()
    }

    /// Trinity service endpoint.
    pub fn trinity_endpoint(&self) -> &str {
        &self.trinity_endpoint
    }
    /// Trinity API key.
    pub fn trinity_api_key(&self) -> &str {
        &self.trinity_api_key
    }
    /// Training model name/path.
    pub fn model_name(&self) -> &str {
        &self.model_name
    }
    /// Selection strategy.
    pub fn selection_strategy(&self) -> Arc<dyn TrainingSelectionStrategy> {
        Arc::clone(&self.selection_strategy)
    }
    /// Reward calculator.
    pub fn reward_calculator(&self) -> Arc<dyn RewardCalculator> {
        Arc::clone(&self.reward_calculator)
    }
    /// Commit interval in seconds.
    pub fn commit_interval_seconds(&self) -> i64 {
        self.commit_interval_seconds
    }
    /// HTTP timeout.
    pub fn http_timeout(&self) -> Duration {
        self.http_timeout
    }
    /// Whether periodic auto-commit is enabled.
    pub fn is_enable_auto_commit(&self) -> bool {
        self.enable_auto_commit
    }
    /// Shadow agent pool size.
    pub fn shadow_pool_size(&self) -> usize {
        self.shadow_pool_size
    }
    /// Shadow agent queue capacity.
    pub fn shadow_pool_capacity(&self) -> usize {
        self.shadow_pool_capacity
    }
    /// Repeat execution count per task.
    pub fn repeat_time(&self) -> i32 {
        self.repeat_time
    }

    /// Current sampling rate, or `-1` when a non-sampling strategy is used.
    pub fn get_sample_rate(&self) -> f64 {
        self.selection_strategy
            .as_any()
            .downcast_ref::<crate::strategy::SamplingRateStrategy>()
            .map(|s| s.get_sample_rate())
            .unwrap_or(-1.0)
    }
}

/// Builder for [`TrainingConfig`], mirroring `TrainingConfig.Builder`.
#[derive(Clone)]
pub struct TrainingConfigBuilder {
    trinity_endpoint: Option<String>,
    trinity_api_key: String,
    model_name: String,
    selection_strategy: Option<Arc<dyn TrainingSelectionStrategy>>,
    reward_calculator: Option<Arc<dyn RewardCalculator>>,
    commit_interval_seconds: i64,
    http_timeout: Duration,
    enable_auto_commit: bool,
    shadow_pool_size: usize,
    shadow_pool_capacity: usize,
    repeat_time: i32,
}

impl Default for TrainingConfigBuilder {
    fn default() -> Self {
        Self {
            trinity_endpoint: None,
            trinity_api_key: "dummy".to_string(),
            model_name: "training-model".to_string(),
            selection_strategy: None,
            reward_calculator: None,
            commit_interval_seconds: 300,
            http_timeout: Duration::from_secs(300),
            enable_auto_commit: true,
            shadow_pool_size: 10,
            shadow_pool_capacity: 1000,
            repeat_time: 1,
        }
    }
}

impl TrainingConfigBuilder {
    pub fn trinity_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.trinity_endpoint = Some(endpoint.into());
        self
    }
    pub fn trinity_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.trinity_api_key = api_key.into();
        self
    }
    pub fn model_name(mut self, model_name: impl Into<String>) -> Self {
        self.model_name = model_name.into();
        self
    }
    pub fn selection_strategy(mut self, strategy: Arc<dyn TrainingSelectionStrategy>) -> Self {
        self.selection_strategy = Some(strategy);
        self
    }
    /// Shortcut: set a plain sampling-rate strategy.
    pub fn sample_rate(self, sample_rate: f64) -> Self {
        assert!(
            (0.0..=1.0).contains(&sample_rate),
            "sampleRate must be between 0 and 1"
        );
        self.selection_strategy(Arc::new(crate::strategy::SamplingRateStrategy::of(sample_rate)))
    }
    pub fn reward_calculator(mut self, calculator: Arc<dyn RewardCalculator>) -> Self {
        self.reward_calculator = Some(calculator);
        self
    }
    pub fn commit_interval_seconds(mut self, seconds: i64) -> Self {
        self.commit_interval_seconds = seconds;
        self
    }
    pub fn http_timeout(mut self, timeout: Duration) -> Self {
        self.http_timeout = timeout;
        self
    }
    pub fn enable_auto_commit(mut self, enable: bool) -> Self {
        self.enable_auto_commit = enable;
        self
    }
    pub fn shadow_pool_size(mut self, size: usize) -> Self {
        self.shadow_pool_size = size;
        self
    }
    pub fn shadow_pool_capacity(mut self, capacity: usize) -> Self {
        self.shadow_pool_capacity = capacity;
        self
    }
    /// Sets the repeat execution count; must be `>= 1`.
    pub fn repeat_time(mut self, repeat_time: i32) -> Self {
        assert!(repeat_time >= 1, "repeatTime must be >= 1");
        self.repeat_time = repeat_time;
        self
    }

    /// Builds the config, requiring an endpoint and a reward calculator.
    pub fn build(self) -> Result<TrainingConfig> {
        let trinity_endpoint = self
            .trinity_endpoint
            .filter(|e| !e.is_empty())
            .ok_or_else(|| Error::Agent("Trinity endpoint must be specified".to_string()))?;
        let reward_calculator = self
            .reward_calculator
            .ok_or_else(|| Error::Agent("RewardCalculator must be specified".to_string()))?;
        let selection_strategy = self.selection_strategy.unwrap_or_else(|| {
            Arc::new(crate::strategy::SamplingRateStrategy::of(0.1))
        });
        Ok(TrainingConfig {
            trinity_endpoint,
            trinity_api_key: self.trinity_api_key,
            model_name: self.model_name,
            selection_strategy,
            reward_calculator,
            commit_interval_seconds: self.commit_interval_seconds,
            http_timeout: self.http_timeout,
            enable_auto_commit: self.enable_auto_commit,
            shadow_pool_size: self.shadow_pool_size,
            shadow_pool_capacity: self.shadow_pool_capacity,
            repeat_time: self.repeat_time,
        })
    }
}

// ---------------------------------------------------------------------------
// AgentCloner
// ---------------------------------------------------------------------------

/// Extra configuration for a shadow agent that cannot be read back from an
/// existing agent (Rust has no reflection).
#[derive(Debug, Clone, Default)]
pub struct ShadowAgentSpec {
    /// System prompt copied onto the shadow agent.
    pub sys_prompt: Option<String>,
    /// Max reasoning iterations copied onto the shadow agent.
    pub max_iters: Option<i32>,
}

/// Clones an agent with a replacement model, mirroring `AgentCloner`.
///
/// The Java version uses reflection to read private fields; Rust instead copies
/// the fields exposed through the [`agentscope_core::Agent`] trait (name,
/// description, toolkit) and rebuilds the agent with a fresh (empty) state. Any
/// field that cannot be read back (`sys_prompt`, `max_iters`) can be supplied via
/// [`ShadowAgentSpec`].
pub struct AgentCloner;

impl AgentCloner {
    /// Clones `original`, replacing its model with `new_model`.
    ///
    /// The clone gets a `-shadow` name suffix and a fresh, empty conversation
    /// state (no shared memory with the original).
    pub fn clone_with_model(
        original: &ReActAgent,
        new_model: Arc<dyn Model>,
        spec: ShadowAgentSpec,
    ) -> Result<ReActAgent> {
        let mut builder = ReActAgent::builder()
            .name(format!("{}-shadow", original.name()))
            .description(original.description().to_string())
            .model(new_model)
            .toolkit(original.toolkit().clone())
            .todo_tool(false);
        if let Some(sys_prompt) = spec.sys_prompt {
            builder = builder.sys_prompt(sys_prompt);
        }
        if let Some(max_iters) = spec.max_iters {
            builder = builder.max_iters(max_iters);
        }
        builder
            .build()
            .map_err(|e| Error::Agent(format!("Failed to clone ReActAgent: {}", original.name()).into()).with_source_boxed(e))
    }
}

// Small helper so we can attach a source error message without exposing internals.
trait ErrorExt {
    fn with_source_boxed(self, source: Error) -> Error;
}

impl ErrorExt for Error {
    fn with_source_boxed(self, source: Error) -> Error {
        Error::Other(format!("{self}: {source}"))
    }
}

// ---------------------------------------------------------------------------
// TrainingRouter
// ---------------------------------------------------------------------------

/// The outcome of running one shadow agent, exposed for observability/testing.
#[derive(Debug, Clone, PartialEq)]
pub struct ShadowRunReport {
    pub task_id: String,
    pub run_id: String,
    pub msg_id_count: usize,
    pub reward: Option<f64>,
    pub feedback_submitted: bool,
}

/// Runs shadow agents to collect training data, mirroring `TrainingRouter`.
///
/// The Java hook reacts to `PreCall`/`PostCall` events. Rust has no equivalent
/// post-call hook, so this router is driven explicitly: callers hand it the
/// production agent, the input messages and an optional training annotation, and
/// it decides whether to train, then spawns the configured number of shadow runs.
pub struct TrainingRouter {
    config: TrainingConfig,
    trinity_client: TrinityClient,
}

impl TrainingRouter {
    /// Creates a router from a config and a Trinity client.
    pub fn new(config: TrainingConfig, trinity_client: TrinityClient) -> Self {
        Self {
            config,
            trinity_client,
        }
    }

    /// The underlying config.
    pub fn config(&self) -> &TrainingConfig {
        &self.config
    }

    /// Makes a selection decision for a call, mirroring the branch of `handlePostCall`.
    pub fn decide(
        &self,
        agent_name: &str,
        inputs: &[Msg],
        output: &Msg,
        annotation: Option<&TrainingAnnotation>,
    ) -> SelectionDecision {
        // Prevent shadow agents from re-triggering training (recursion guard).
        if agent_name.contains("-shadow") {
            return SelectionDecision::reject("shadow-agent");
        }
        self.config
            .selection_strategy()
            .should_select(agent_name, inputs, output, annotation)
    }

    /// Runs the shadow training flow for a single selected call.
    ///
    /// Runs `repeat_time` iterations with the same task id and incrementing run
    /// ids. This is blocking (awaited) to remain testable; production callers may
    /// wrap it in `tokio::spawn`.
    pub async fn run_shadow(
        &self,
        agent_name: &str,
        production_agent: &ReActAgent,
        inputs: Vec<Msg>,
        output: Msg,
        annotation: Option<&TrainingAnnotation>,
        spec: ShadowAgentSpec,
    ) -> Vec<ShadowRunReport> {
        let mut reports = Vec::new();
        let decision = self.decide(agent_name, &inputs, &output, annotation);
        if !decision.should_train() {
            return reports;
        }

        let task_id = decision
            .metadata()
            .get("taskId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(task_id_generator::generate);

        let repeat = self.config.repeat_time();
        for i in 0..repeat {
            let run_id = RunRegistry::allocate_run_id(&task_id);
            let context = Arc::new(RunExecutionContext::create(task_id.clone(), run_id.clone()));

            let report = self
                .run_single_shadow(production_agent, inputs.clone(), context.clone(), spec.clone())
                .await;
            reports.push(report);

            if i == repeat - 1 {
                RunRegistry::cleanup(&task_id);
            }
        }
        reports
    }

    async fn run_single_shadow(
        &self,
        production_agent: &ReActAgent,
        inputs: Vec<Msg>,
        context: Arc<RunExecutionContext>,
        spec: ShadowAgentSpec,
    ) -> ShadowRunReport {
        let mut report = ShadowRunReport {
            task_id: context.get_task_id().to_string(),
            run_id: context.get_run_id().to_string(),
            msg_id_count: 0,
            reward: None,
            feedback_submitted: false,
        };

        // Build the Trinity-backed model adapter, pointing the shadow agent at the
        // configured training model (`{endpoint}/v1`), exactly like the Java router.
        let base_url = format!("{}/v1", self.trinity_client.endpoint());
        let adapter = match TrinityModelAdapter::builder()
            .base_url(base_url)
            .model_name(self.config.model_name())
            .api_key(self.config.trinity_api_key())
            .execution_context(context.clone())
            .build()
        {
            Ok(a) => Arc::new(a),
            Err(e) => {
                tracing::error!("Failed to create Trinity model adapter: {e}");
                return report;
            }
        };

        // Clone the production agent, replacing its model with the adapter.
        let shadow = match AgentCloner::clone_with_model(production_agent, adapter, spec) {
            Ok(a) => a,
            Err(e) => {
                tracing::error!("Failed to clone shadow agent: {e}");
                return report;
            }
        };

        // Execute the shadow agent.
        match CallableAgent::call(&shadow, inputs).await {
            Ok(_msg) => {
                context.set_messages(shadow.agent_state().context().to_vec());
            }
            Err(e) => {
                tracing::error!("Shadow agent failed for {context}: {e}");
                return report;
            }
        }

        let msg_ids = context.get_msg_ids();
        report.msg_id_count = msg_ids.len();
        if msg_ids.is_empty() {
            tracing::warn!("No msg_ids collected for {context}");
            return report;
        }

        let reward = self.config.reward_calculator().calculate(&shadow);
        report.reward = Some(reward);

        let request = FeedbackRequest::builder()
            .task_id(context.get_task_id())
            .run_id(context.get_run_id())
            .msg_ids(msg_ids)
            .reward(reward)
            .build();

        match self.trinity_client.feedback(&request).await {
            Ok(()) => {
                report.feedback_submitted = true;
                TaskExecutionRegistry::register(context);
            }
            Err(e) => tracing::error!("Failed to submit feedback: {e}"),
        }

        report
    }
}

// ---------------------------------------------------------------------------
// TrainingRunner
// ---------------------------------------------------------------------------

/// Controls the training lifecycle, mirroring `TrainingRunner`.
pub struct TrainingRunner {
    config: TrainingConfig,
    trinity_client: TrinityClient,
    router: Arc<TrainingRouter>,
    running: std::sync::atomic::AtomicBool,
    commit_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl TrainingRunner {
    /// Returns a new builder.
    pub fn builder() -> TrainingRunnerBuilder {
        TrainingRunnerBuilder::default()
    }

    /// The runner config.
    pub fn get_config(&self) -> &TrainingConfig {
        &self.config
    }

    /// Whether the runner is currently active.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// The underlying router.
    pub fn router(&self) -> Arc<TrainingRouter> {
        Arc::clone(&self.router)
    }

    /// Starts the runner (idempotent) and any periodic-commit task.
    pub fn start(&self) {
        if self.running.swap(true, Ordering::SeqCst) {
            tracing::warn!("TrainingRunner already running");
            return;
        }
        self.start_commit_scheduler();
        tracing::info!("TrainingRunner started successfully");
    }

    /// Stops the runner (idempotent), performs a final commit and aborts the
    /// periodic-commit task.
    pub async fn stop(&self) {
        if !self.running.swap(false, Ordering::SeqCst) {
            tracing::warn!("TrainingRunner not running");
            return;
        }
        if let Some(handle) = self.commit_task.lock().unwrap().take() {
            handle.abort();
        }
        let _ = self.commit().await;
        tracing::info!("TrainingRunner stopped");
    }

    /// Triggers a commit, mirroring `commit`.
    pub async fn commit(&self) -> Result<()> {
        match self.trinity_client.commit(&CommitRequest::builder().build()).await {
            Ok(()) => {
                tracing::info!("Training commit successful");
                Ok(())
            }
            Err(e) => {
                tracing::error!("Failed to commit training: {e}");
                Err(e)
            }
        }
    }

    fn start_commit_scheduler(&self) {
        let interval = self.config.commit_interval_seconds();
        if interval <= 0 || !self.config.is_enable_auto_commit() {
            tracing::info!("Commit scheduler disabled (interval <= 0 or auto-commit off)");
            return;
        }
        let client = self.trinity_client.clone();
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(interval as u64));
            // The first tick completes immediately; skip it so the first commit
            // happens after one full interval, matching the Java scheduler.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if let Err(e) = client.commit(&CommitRequest::builder().build()).await {
                    tracing::error!("Error in scheduled commit: {e}");
                }
            }
        });
        *self.commit_task.lock().unwrap() = Some(handle);
        tracing::info!("Commit scheduler started with interval: {interval} seconds");
    }
}

/// Builder for [`TrainingRunner`], mirroring `TrainingRunner.Builder`.
#[derive(Default)]
pub struct TrainingRunnerBuilder {
    config: Option<TrainingConfig>,
    config_builder: Option<TrainingConfigBuilder>,
}

impl TrainingRunnerBuilder {
    /// Sets a ready-made config (mutually exclusive with the incremental setters).
    pub fn config(mut self, config: TrainingConfig) -> Self {
        self.config = Some(config);
        self
    }

    fn config_builder(&mut self) -> Result<&mut TrainingConfigBuilder> {
        if self.config.is_some() {
            return Err(Error::Other(
                "Cannot use builder methods when config is already set".to_string(),
            ));
        }
        Ok(self.config_builder.get_or_insert_with(TrainingConfigBuilder::default))
    }

    pub fn trinity_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        if let Ok(b) = self.config_builder() {
            b.trinity_endpoint = Some(endpoint.into());
        }
        self
    }
    pub fn model_name(mut self, model_name: impl Into<String>) -> Self {
        if let Ok(b) = self.config_builder() {
            b.model_name = model_name.into();
        }
        self
    }
    pub fn selection_strategy(mut self, strategy: Arc<dyn TrainingSelectionStrategy>) -> Self {
        if let Ok(b) = self.config_builder() {
            b.selection_strategy = Some(strategy);
        }
        self
    }
    pub fn reward_calculator(mut self, calculator: Arc<dyn RewardCalculator>) -> Self {
        if let Ok(b) = self.config_builder() {
            b.reward_calculator = Some(calculator);
        }
        self
    }
    pub fn commit_interval_seconds(mut self, seconds: i64) -> Self {
        if let Ok(b) = self.config_builder() {
            b.commit_interval_seconds = seconds;
        }
        self
    }
    pub fn repeat_time(mut self, repeat_time: i32) -> Self {
        if let Ok(b) = self.config_builder() {
            if repeat_time >= 1 {
                b.repeat_time = repeat_time;
            }
        }
        self
    }

    /// Builds the runner, requiring a config.
    pub fn build(self) -> Result<TrainingRunner> {
        let config = match self.config {
            Some(c) => c,
            None => self
                .config_builder
                .ok_or_else(|| {
                    Error::Other(
                        "TrainingConfig must be set. Use config() or builder methods.".to_string(),
                    )
                })?
                .build()?,
        };
        let trinity_client = TrinityClient::builder()
            .endpoint(config.trinity_endpoint())
            .timeout(config.http_timeout())
            .build()?;
        let router = Arc::new(TrainingRouter::new(config.clone(), trinity_client.clone()));
        Ok(TrainingRunner {
            config,
            trinity_client,
            router,
            running: std::sync::atomic::AtomicBool::new(false),
            commit_task: Mutex::new(None),
        })
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reward::ZeroRewardCalculator;
    use crate::strategy::{SamplingRateStrategy, TrainingAnnotation};

    #[test]
    fn run_execution_context_collects() {
        let ctx = RunExecutionContext::create("t1", "0");
        assert!(!ctx.has_msg_ids());
        ctx.add_msg_id("m1");
        ctx.add_msg_id("");
        ctx.add_msg_id("m2");
        assert_eq!(ctx.get_msg_ids(), vec!["m1".to_string(), "m2".to_string()]);
        assert_eq!(ctx.get_msg_id_count(), 2);
        assert!(ctx.has_msg_ids());
        ctx.add_msg(Some(Msg::user("hi")));
        ctx.add_msg(None);
        assert_eq!(ctx.get_message_count(), 1);
        assert!(ctx.has_messages());
        assert!(ctx.get_duration() < 5_000);
        assert_eq!(ctx.get_task_id(), "t1");
        assert_eq!(ctx.get_run_id(), "0");
    }

    #[test]
    fn run_execution_context_set_messages() {
        let ctx = RunExecutionContext::create("t1", "0");
        ctx.add_msg(Some(Msg::user("a")));
        ctx.set_messages(vec![Msg::user("b"), Msg::user("c")]);
        assert_eq!(ctx.get_message_count(), 2);
        assert_eq!(ctx.get_messages()[0].text_content(), "b");
    }

    #[test]
    fn run_registry_allocates_incrementing_ids() {
        RunRegistry::clear_all();
        assert_eq!(RunRegistry::allocate_run_id("task-a"), "0");
        assert_eq!(RunRegistry::allocate_run_id("task-a"), "1");
        assert_eq!(RunRegistry::get_current_run_count("task-a"), 2);
        assert_eq!(RunRegistry::allocate_run_id("task-b"), "0");
        assert_eq!(RunRegistry::size(), 2);
        RunRegistry::cleanup("task-a");
        assert_eq!(RunRegistry::get_current_run_count("task-a"), 0);
    }

    #[test]
    fn task_execution_registry_flow() {
        TaskExecutionRegistry::clear_all();
        let c1 = Arc::new(RunExecutionContext::create("t", "1"));
        let c0 = Arc::new(RunExecutionContext::create("t", "0"));
        TaskExecutionRegistry::register(c1);
        TaskExecutionRegistry::register(c0);
        let runs = TaskExecutionRegistry::get_runs_by_task("t");
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].get_run_id(), "0");
        assert_eq!(runs[1].get_run_id(), "1");
        assert!(TaskExecutionRegistry::get_run("t", "1").is_some());
        assert_eq!(TaskExecutionRegistry::get_run_count("t"), 2);
        assert_eq!(TaskExecutionRegistry::get_task_count(), 1);
        assert_eq!(TaskExecutionRegistry::get_total_run_count(), 2);
        let stats = TaskExecutionRegistry::get_stats();
        assert_eq!(stats.get_task_count(), 1);
        assert_eq!(stats.get_total_run_count(), 2);
        assert!((stats.get_average_runs_per_task() - 2.0).abs() < 1e-9);
        assert_eq!(TaskExecutionRegistry::cleanup("t"), 2);
        assert_eq!(TaskExecutionRegistry::get_task_count(), 0);
    }

    #[test]
    fn task_id_generator_formats() {
        let id = task_id_generator::generate();
        assert!(id.starts_with("task-"));
        let id2 = task_id_generator::generate_with_timestamp();
        assert!(id2.starts_with("task-"));
        assert_ne!(id, id2);
    }

    #[test]
    fn config_builder_validates_and_defaults() {
        assert!(TrainingConfig::builder().build().is_err());
        assert!(TrainingConfig::builder()
            .trinity_endpoint("http://x")
            .build()
            .is_err());
        let config = TrainingConfig::builder()
            .trinity_endpoint("http://x")
            .reward_calculator(Arc::new(ZeroRewardCalculator))
            .sample_rate(0.25)
            .repeat_time(3)
            .build()
            .unwrap();
        assert_eq!(config.get_sample_rate(), 0.25);
        assert_eq!(config.repeat_time(), 3);
        assert_eq!(config.trinity_api_key(), "dummy");
        assert_eq!(config.model_name(), "training-model");
        assert_eq!(config.commit_interval_seconds(), 300);
        assert_eq!(config.shadow_pool_size(), 10);
        assert_eq!(config.shadow_pool_capacity(), 1000);
        assert!(config.is_enable_auto_commit());
    }

    #[test]
    fn config_sample_rate_negative_for_non_sampling() {
        let config = TrainingConfig::builder()
            .trinity_endpoint("http://x")
            .reward_calculator(Arc::new(ZeroRewardCalculator))
            .selection_strategy(Arc::new(
                crate::strategy::ExplicitMarkingStrategy::create(),
            ))
            .build()
            .unwrap();
        assert_eq!(config.get_sample_rate(), -1.0);
    }

    #[test]
    fn config_repeat_time_rejects_zero() {
        let result = std::panic::catch_unwind(|| {
            TrainingConfig::builder()
                .trinity_endpoint("http://x")
                .reward_calculator(Arc::new(ZeroRewardCalculator))
                .repeat_time(0)
        });
        assert!(result.is_err());
    }

    #[test]
    fn training_runner_builder_requires_config() {
        assert!(TrainingRunner::builder().build().is_err());
    }

    #[test]
    fn training_runner_builds_from_incremental_builder() {
        let runner = TrainingRunner::builder()
            .trinity_endpoint("http://localhost:9")
            .model_name("m")
            .selection_strategy(Arc::new(SamplingRateStrategy::of(0.1)))
            .reward_calculator(Arc::new(ZeroRewardCalculator))
            .commit_interval_seconds(0)
            .repeat_time(2)
            .build()
            .unwrap();
        assert!(!runner.is_running());
        assert_eq!(runner.get_config().model_name(), "m");
        runner.start();
        assert!(runner.is_running());
        runner.start();
        assert!(runner.is_running());
    }

    #[tokio::test]
    async fn training_runner_stop_is_idempotent() {
        let runner = TrainingRunner::builder()
            .trinity_endpoint("http://127.0.0.1:1")
            .reward_calculator(Arc::new(ZeroRewardCalculator))
            .commit_interval_seconds(0)
            .build()
            .unwrap();
        runner.start();
        // stop performs a final commit which will fail against an unreachable
        // endpoint; that is fine, the runner must still stop cleanly.
        runner.stop().await;
        assert!(!runner.is_running());
        runner.stop().await;
        assert!(!runner.is_running());
    }

    #[test]
    fn router_decide_rejects_shadow_agent() {
        let config = TrainingConfig::builder()
            .trinity_endpoint("http://x")
            .selection_strategy(Arc::new(SamplingRateStrategy::of(1.0)))
            .reward_calculator(Arc::new(ZeroRewardCalculator))
            .build()
            .unwrap();
        let client = TrinityClient::builder().endpoint("http://x").build().unwrap();
        let router = TrainingRouter::new(config, client);
        let decision = router.decide("agent-shadow", &[], &Msg::assistant("x"), None);
        assert!(!decision.should_train());
        assert_eq!(decision.reason(), "shadow-agent");
    }

    #[test]
    fn router_decide_uses_strategy() {
        let config = TrainingConfig::builder()
            .trinity_endpoint("http://x")
            .selection_strategy(Arc::new(SamplingRateStrategy::of(1.0)))
            .reward_calculator(Arc::new(ZeroRewardCalculator))
            .build()
            .unwrap();
        let client = TrinityClient::builder().endpoint("http://x").build().unwrap();
        let router = TrainingRouter::new(config, client);
        let decision = router.decide("agent", &[], &Msg::assistant("x"), None);
        assert!(decision.should_train());
    }

    #[test]
    fn router_decide_with_explicit_marking() {
        let config = TrainingConfig::builder()
            .trinity_endpoint("http://x")
            .selection_strategy(Arc::new(
                crate::strategy::ExplicitMarkingStrategy::create(),
            ))
            .reward_calculator(Arc::new(ZeroRewardCalculator))
            .build()
            .unwrap();
        let client = TrinityClient::builder().endpoint("http://x").build().unwrap();
        let router = TrainingRouter::new(config, client);
        let annotation = TrainingAnnotation::builder()
            .enabled(true)
            .task_id("explicit-task")
            .build();
        let decision = router.decide("agent", &[], &Msg::assistant("x"), Some(&annotation));
        assert!(decision.should_train());
        assert_eq!(
            decision.metadata().get("taskId").unwrap(),
            "explicit-task"
        );
    }

    // --- Additional offline coverage -------------------------------------------------

    #[test]
    #[should_panic]
    fn run_execution_context_rejects_empty_task_id() {
        let _ = RunExecutionContext::create("", "0");
    }

    #[test]
    #[should_panic]
    fn run_execution_context_rejects_empty_run_id() {
        let _ = RunExecutionContext::create("t", "");
    }

    #[test]
    fn run_execution_context_equality_and_display() {
        let a = RunExecutionContext::create("t", "0");
        let b = RunExecutionContext::create("t", "0");
        let c = RunExecutionContext::create("t", "1");
        assert_eq!(a, b);
        assert_ne!(a, c);
        a.add_msg_id("m");
        assert!(format!("{a}").contains("task=t, run=0, msgIds=1"));
    }

    #[test]
    fn run_registry_handles_empty_ids_and_cleanup_old_tasks() {
        RunRegistry::clear_all();
        assert_eq!(RunRegistry::get_current_run_count(""), 0);
        RunRegistry::cleanup(""); // no-op
        // Reserved interface mirrors Java's no-op TODO.
        RunRegistry::cleanup_old_tasks(Duration::from_secs(60));
        assert_eq!(RunRegistry::size(), 0);
    }

    #[test]
    #[should_panic]
    fn run_registry_rejects_empty_task_id() {
        let _ = RunRegistry::allocate_run_id("");
    }

    #[test]
    fn task_execution_registry_empty_lookups() {
        TaskExecutionRegistry::clear_all();
        assert!(TaskExecutionRegistry::get_runs_by_task("").is_empty());
        assert!(TaskExecutionRegistry::get_runs_by_task("missing").is_empty());
        assert!(TaskExecutionRegistry::get_run("", "0").is_none());
        assert!(TaskExecutionRegistry::get_run("t", "").is_none());
        assert!(TaskExecutionRegistry::get_run("missing", "0").is_none());
        assert_eq!(TaskExecutionRegistry::get_run_count(""), 0);
        assert_eq!(TaskExecutionRegistry::cleanup(""), 0);
        assert_eq!(TaskExecutionRegistry::cleanup("missing"), 0);
        assert_eq!(TaskExecutionRegistry::get_task_count(), 0);
        assert_eq!(TaskExecutionRegistry::get_total_run_count(), 0);
        assert!(TaskExecutionRegistry::get_all_task_ids().is_empty());
    }

    #[test]
    fn task_execution_registry_lists_task_ids() {
        TaskExecutionRegistry::clear_all();
        TaskExecutionRegistry::register(Arc::new(RunExecutionContext::create("t-a", "0")));
        TaskExecutionRegistry::register(Arc::new(RunExecutionContext::create("t-b", "0")));
        let mut ids = TaskExecutionRegistry::get_all_task_ids();
        ids.sort();
        assert_eq!(ids, vec!["t-a".to_string(), "t-b".to_string()]);
        TaskExecutionRegistry::clear_all();
    }

    #[test]
    fn registry_stats_zero_and_display() {
        let stats = RegistryStats {
            task_count: 0,
            total_run_count: 0,
        };
        assert_eq!(stats.get_average_runs_per_task(), 0.0);
        assert!(format!("{stats}").contains("tasks=0, runs=0, avg=0.00"));
        let stats = RegistryStats {
            task_count: 2,
            total_run_count: 3,
        };
        assert!((stats.get_average_runs_per_task() - 1.5).abs() < 1e-9);
    }

    #[test]
    fn config_optional_setters_are_applied() {
        let config = TrainingConfig::builder()
            .trinity_endpoint("http://x")
            .trinity_api_key("secret")
            .model_name("m")
            .reward_calculator(Arc::new(ZeroRewardCalculator))
            .commit_interval_seconds(60)
            .http_timeout(Duration::from_secs(10))
            .enable_auto_commit(false)
            .shadow_pool_size(4)
            .shadow_pool_capacity(8)
            .build()
            .unwrap();
        assert_eq!(config.trinity_api_key(), "secret");
        assert_eq!(config.commit_interval_seconds(), 60);
        assert_eq!(config.http_timeout(), Duration::from_secs(10));
        assert!(!config.is_enable_auto_commit());
        assert_eq!(config.shadow_pool_size(), 4);
        assert_eq!(config.shadow_pool_capacity(), 8);
        // Default strategy (10% sampling) is used when none is supplied.
        assert_eq!(config.get_sample_rate(), 0.1);
    }

    #[test]
    fn config_endpoint_and_reward_are_required() {
        assert!(TrainingConfig::builder()
            .trinity_endpoint("")
            .reward_calculator(Arc::new(ZeroRewardCalculator))
            .build()
            .is_err());
        assert!(TrainingConfig::builder()
            .trinity_endpoint("http://x")
            .build()
            .is_err());
    }

    #[test]
    fn training_runner_keeps_config_when_setters_mixed() {
        // Mirrors Java: once a config object is set, incremental setters are ignored.
        let config = TrainingConfig::builder()
            .trinity_endpoint("http://x")
            .reward_calculator(Arc::new(ZeroRewardCalculator))
            .build()
            .unwrap();
        let runner = TrainingRunner::builder()
            .config(config)
            .trinity_endpoint("http://y")
            .build()
            .unwrap();
        assert_eq!(runner.get_config().trinity_endpoint(), "http://x");
        assert!(!runner.is_running());
    }

    #[test]
    fn shadow_agent_spec_defaults_and_clone() {
        let spec = ShadowAgentSpec::default();
        assert!(spec.sys_prompt.is_none());
        assert!(spec.max_iters.is_none());
        let spec2 = ShadowAgentSpec {
            sys_prompt: Some("you are shadow".to_string()),
            max_iters: Some(3),
        };
        assert_eq!(spec2.clone().max_iters, Some(3));
    }

    mod model_support {
        use super::*;
        use agentscope_core::error::Result as CoreResult;
        use agentscope_core::message::ChatUsage;
        use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
        use agentscope_core::ReActAgent;
        use futures::stream;

        pub struct FakeModel;
        impl Model for FakeModel {
            fn stream(
                &self,
                _m: Vec<Msg>,
                _t: Vec<ToolSchema>,
                _o: GenerateOptions,
            ) -> futures::stream::BoxStream<'_, CoreResult<ChatResponse>> {
                Box::pin(stream::once(async {
                    Ok(ChatResponse::new("id", vec![], ChatUsage::default(), None, None))
                }))
            }
            fn model_name(&self) -> &str {
                "fake"
            }
        }

        pub fn agent(name: &str) -> ReActAgent {
            ReActAgent::builder()
                .name(name)
                .model(Arc::new(FakeModel))
                .build()
                .unwrap()
        }
    }

    #[test]
    fn agent_cloner_rebuilds_shadow_with_spec() {
        let original = model_support::agent("prod");
        let spec = ShadowAgentSpec {
            sys_prompt: Some("shadow prompt".to_string()),
            max_iters: Some(2),
        };
        let shadow =
            AgentCloner::clone_with_model(&original, Arc::new(model_support::FakeModel), spec)
                .unwrap();
        // Name suffix mirrors Java (`original + "-shadow"`).
        assert_eq!(shadow.name(), "prod-shadow");
        // A freshly cloned agent starts with an empty context (no shared memory).
        assert!(shadow.agent_state().context().is_empty());
    }

    #[tokio::test]
    async fn run_shadow_is_noop_when_not_selected() {
        let config = TrainingConfig::builder()
            .trinity_endpoint("http://127.0.0.1:1")
            .selection_strategy(Arc::new(SamplingRateStrategy::of(0.0)))
            .reward_calculator(Arc::new(ZeroRewardCalculator))
            .repeat_time(2)
            .build()
            .unwrap();
        let client = TrinityClient::builder()
            .endpoint("http://127.0.0.1:1")
            .build()
            .unwrap();
        let router = TrainingRouter::new(config, client);
        let prod = model_support::agent("prod");
        let reports = router
            .run_shadow(
                "prod",
                &prod,
                vec![Msg::user("hi")],
                Msg::assistant("ok"),
                None,
                ShadowAgentSpec::default(),
            )
            .await;
        // Rejected by the strategy => no shadow runs, no network calls.
        assert!(reports.is_empty());
    }
}
