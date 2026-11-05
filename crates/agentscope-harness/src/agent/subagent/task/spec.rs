//! Discriminated union describing how a background subagent task should execute,
//! mirroring `TaskRunSpec`.

use std::collections::BTreeMap;

use agentscope_core::error::Result;

use super::protocol_client::RemoteSubmitContext;

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
        /// Optional submission context (permissions, streaming preferences, parent identity).
        context: Option<RemoteSubmitContext>,
    },
    /// Adopts an already-running tokio task as a tracked background task (used when a sync
    /// execution is promoted to async on timeout).
    Adopted {
        future: tokio::task::JoinHandle<Result<String>>,
        /// Host-visible session id of the subagent this run belongs to (`sub-<uuid>`).
        ///
        /// Persisted so `session/load` can rebuild the exact child the live client saw; without
        /// it the replay has to invent an id and the card cannot be matched to the original.
        /// `None` for adopted runs that are not subagent turns.
        child_session: Option<String>,
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
            context: None,
        }
    }

    /// Creates a remote agent-protocol execution spec with an explicit submission context.
    pub fn remote_with_context(
        base_url: impl Into<String>,
        headers: BTreeMap<String, String>,
        agent_id: impl Into<String>,
        input: impl Into<String>,
        context: RemoteSubmitContext,
    ) -> Self {
        TaskRunSpec::Remote {
            base_url: base_url.into(),
            headers,
            agent_id: agent_id.into(),
            input: input.into(),
            context: Some(context),
        }
    }
}
