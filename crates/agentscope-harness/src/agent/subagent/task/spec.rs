//! Discriminated union describing how a background subagent task should execute,
//! mirroring `TaskRunSpec`.

use std::collections::BTreeMap;

use agentscope_core::error::Result;

/// Describes how a background subagent task should execute.
pub enum TaskRunSpec {
    /// In-process execution via a blocking closure returning a text result.
    Local {
        execution: Box<dyn Fn() -> Result<String> + Send + Sync>,
    },
    /// Remote HTTP task execution (agent-protocol transport). The `task_id` is chosen by
    /// the client and used as the remote task key end-to-end.
    Remote {
        base_url: String,
        headers: BTreeMap<String, String>,
        agent_id: String,
        input: String,
    },
    /// Adopts an already-running tokio task as a tracked background task (used when a sync
    /// execution is promoted to async on timeout).
    Adopted {
        future: tokio::task::JoinHandle<Result<String>>,
    },
}

impl TaskRunSpec {
    /// Creates a local in-process execution spec.
    pub fn local<F>(execution: F) -> Self
    where
        F: Fn() -> Result<String> + Send + Sync + 'static,
    {
        TaskRunSpec::Local {
            execution: Box::new(execution),
        }
    }

    /// Creates a remote agent-protocol execution spec.
    pub fn remote(
        base_url: impl Into<String>,
        headers: BTreeMap<String, String>,
        agent_id: impl Into<String>,
        input: impl Into<String>,
    ) -> Self {
        TaskRunSpec::Remote {
            base_url: base_url.into(),
            headers,
            agent_id: agent_id.into(),
            input: input.into(),
        }
    }
}
