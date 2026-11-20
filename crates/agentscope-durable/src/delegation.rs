//! Delegation is fenced by the parent turn's lease.
//!
//! A delegation must be recorded by the same writer that owns the parent turn: it creates a
//! child thread, admits its first turn and links both back to the parent's tool call. That is
//! why it is not an [`Executor`](crate::Executor) — an executor has no lease, so it could not
//! prove it still owns the parent when the child is created.

use sha2::{Digest, Sha256};

use crate::error::{DurableError, ErrorCode, Result};
use crate::session_backend::SessionBackend;
use crate::types::{
    DelegationTarget, DelegationView, Lease, Restoration, ThreadKey, ThreadSpec, ThreadView,
};

/// The child thread id a parent/request pair always derives.
///
/// The name is content-addressed so a replay of the same request finds the child it already
/// created instead of starting a second one.
pub fn child_thread_id(parent: &ThreadKey, request_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(parent.thread_id.as_bytes());
    hasher.update(request_id.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("child-{}", &hex[..12])
}

/// Installs the parent's current checkpoint into `spec`'s thread as a settled baseline.
///
/// Idempotent: an existing child is returned as-is, which is what lets a recovered parent tick
/// find the child its previous attempt created. A parent without a checkpoint has no baseline
/// to fork, and says so rather than starting the child from nothing.
pub async fn fork_thread(
    backend: &dyn SessionBackend,
    parent: &ThreadKey,
    request_id: &str,
    spec: ThreadSpec,
) -> Result<ThreadView> {
    match backend.thread(&spec.key).await {
        Ok(view) => return Ok(view),
        Err(error) if error.code() == ErrorCode::NotFound => {}
        Err(error) => return Err(error),
    }
    let state = backend.thread_state(parent).await?;
    let Some(metadata) = state.checkpoint_metadata else {
        return Err(DurableError::invalid(
            "The parent thread has no checkpoint to fork",
        ));
    };
    let position = metadata.position.parse::<u64>().map_err(|_| {
        DurableError::invalid(format!(
            "Invalid checkpoint position: {}",
            metadata.position
        ))
    })?;
    let checkpoint = backend.checkpoint(parent, position).await?;
    backend.create_thread(spec.clone()).await?;
    backend
        .restore(Restoration {
            request_id: request_id.to_string(),
            expected_position: 0,
            target: spec,
            checkpoint: checkpoint.checkpoint,
            journal: Vec::new(),
            resources: Vec::new(),
        })
        .await
}

/// The delegation entry point of one tick: it holds the lease the parent turn already owns.
pub struct DelegationExecutor<'a> {
    backend: &'a dyn SessionBackend,
    lease: &'a Lease,
}

impl<'a> DelegationExecutor<'a> {
    pub fn new(backend: &'a dyn SessionBackend, lease: &'a Lease) -> Self {
        Self { backend, lease }
    }

    pub async fn delegate(
        &self,
        execution_id: &str,
        request_id: &str,
        target: DelegationTarget,
    ) -> Result<DelegationView> {
        self.backend
            .delegate(self.lease, execution_id, request_id, target)
            .await
    }
}
