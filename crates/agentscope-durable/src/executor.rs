//! External effect execution: the only place a side effect may leave the runtime.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use crate::types::{ExecutionRequest, ExecutionResult};
use crate::Result;

/// The name [`ExecutorRegistry::new`] registers the default executor under.
pub const DEFAULT_EXECUTOR: &str = "local";

/// What an executor can promise during recovery.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExecutorCapabilities {
    /// `query` can find an execution that was started by a previous process.
    pub lookup_by_execution_id: bool,
    /// Starting the same `execution_id` twice never produces a second effect.
    pub idempotent_start: bool,
    /// A running execution can be cancelled from another process.
    pub cancellation: bool,
}

/// Executes tool calls. Implementations must be safe to call after a crash.
#[async_trait]
pub trait Executor: Send + Sync {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities::default()
    }

    /// Starts the effect. A failure here may still have produced the effect.
    async fn start(&self, request: &ExecutionRequest) -> Result<ExecutionResult>;

    /// Looks up an execution started earlier, without starting anything new.
    async fn query(&self, _request: &ExecutionRequest) -> Result<ExecutionResult> {
        Ok(ExecutionResult::unknown(
            "query is not supported by this executor",
        ))
    }

    async fn cancel(&self, _request: &ExecutionRequest) -> Result<ExecutionResult> {
        Ok(ExecutionResult::unknown(
            "cancel is not supported by this executor",
        ))
    }
}

/// The named executors a host can route a tool call to.
///
/// The default executor is what a call without a binding uses. A named lookup never falls back
/// to it: an unregistered name is `None` and the caller reports `UNAVAILABLE`, because guessing
/// would run the effect somewhere its recovery cannot be found.
pub struct ExecutorRegistry {
    default: RwLock<Arc<dyn Executor>>,
    named: RwLock<HashMap<String, Arc<dyn Executor>>>,
}

impl ExecutorRegistry {
    pub fn new(default: Arc<dyn Executor>) -> Self {
        let registry = Self {
            default: RwLock::new(Arc::clone(&default)),
            named: RwLock::new(HashMap::new()),
        };
        registry.register(DEFAULT_EXECUTOR, default);
        registry
    }

    /// Registers `executor` under `name`; a later registration under the same name wins.
    pub fn register(&self, name: impl Into<String>, executor: Arc<dyn Executor>) {
        if let Ok(mut named) = self.named.write() {
            named.insert(name.into(), executor);
        }
    }

    /// The executor registered under `name`, or `None` if the name was never registered.
    pub fn resolve(&self, name: &str) -> Option<Arc<dyn Executor>> {
        self.named.read().ok()?.get(name).cloned()
    }

    /// The executor a call without a binding uses.
    pub fn default_executor(&self) -> Arc<dyn Executor> {
        match self.default.read() {
            Ok(default) => Arc::clone(&default),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    /// Replaces the default executor; the name it was registered under is unchanged.
    pub fn set_default(&self, executor: Arc<dyn Executor>) {
        if let Ok(mut default) = self.default.write() {
            *default = executor;
        }
    }
}
