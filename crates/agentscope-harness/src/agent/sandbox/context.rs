//! Sandbox configuration context.
use std::sync::Arc;

use super::super::isolation_scope::IsolationScope;
use super::state::SandboxState;
use super::Sandbox;
use super::SandboxClient;
use super::SandboxClientOptions;
use super::SandboxSnapshotSpec;
use super::WorkspaceSpec;

/// Immutable configuration for sandbox behavior.
///
/// Built with the agent and attached to `RuntimeContext` for each call.
pub struct SandboxContext {
    /// Sandbox client for creating/resuming sandboxes.
    client: Option<Box<dyn SandboxClient>>,

    /// Client options.
    client_options: Option<SandboxClientOptions>,

    /// Workspace specification.
    workspace_spec: Option<WorkspaceSpec>,

    /// Snapshot specification.
    snapshot_spec: Option<Arc<dyn SandboxSnapshotSpec>>,

    /// External sandbox (user-managed).
    external_sandbox: Option<Box<dyn Sandbox>>,

    /// External sandbox state.
    external_sandbox_state: Option<SandboxState>,

    /// Isolation scope for this sandbox.
    isolation_scope: Option<IsolationScope>,
}

impl std::fmt::Debug for SandboxContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxContext")
            .field("client", &self.client.is_some())
            .field("client_options", &self.client_options)
            .field("workspace_spec", &self.workspace_spec)
            .field("snapshot_spec", &self.snapshot_spec)
            .field("external_sandbox", &self.external_sandbox.is_some())
            .field("external_sandbox_state", &self.external_sandbox_state)
            .field("isolation_scope", &self.isolation_scope)
            .finish()
    }
}

impl Clone for SandboxContext {
    fn clone(&self) -> Self {
        // We can't clone trait objects, so we create a new context without them
        Self {
            client: None,
            client_options: self.client_options.clone(),
            workspace_spec: self.workspace_spec.clone(),
            snapshot_spec: self.snapshot_spec.clone(),
            external_sandbox: None,
            external_sandbox_state: self.external_sandbox_state.clone(),
            isolation_scope: self.isolation_scope,
        }
    }
}

impl SandboxContext {
    /// Creates a new builder.
    pub fn builder() -> SandboxContextBuilder {
        SandboxContextBuilder::default()
    }

    /// Returns the sandbox client.
    pub fn client(&self) -> Option<&dyn SandboxClient> {
        self.client.as_deref()
    }

    /// Returns the client options.
    pub fn client_options(&self) -> Option<&SandboxClientOptions> {
        self.client_options.as_ref()
    }

    /// Returns the workspace specification.
    pub fn workspace_spec(&self) -> Option<&WorkspaceSpec> {
        self.workspace_spec.as_ref()
    }

    /// Returns the snapshot specification.
    pub fn snapshot_spec(&self) -> Option<&Arc<dyn SandboxSnapshotSpec>> {
        self.snapshot_spec.as_ref()
    }

    /// Returns the external sandbox.
    pub fn external_sandbox(&self) -> Option<&dyn Sandbox> {
        self.external_sandbox.as_deref()
    }

    /// Returns the external sandbox state.
    pub fn external_sandbox_state(&self) -> Option<&SandboxState> {
        self.external_sandbox_state.as_ref()
    }

    /// Returns the isolation scope.
    pub fn isolation_scope(&self) -> Option<&IsolationScope> {
        self.isolation_scope.as_ref()
    }
}

/// Builder for `SandboxContext`.
pub struct SandboxContextBuilder {
    client: Option<Box<dyn SandboxClient>>,
    client_options: Option<SandboxClientOptions>,
    workspace_spec: Option<WorkspaceSpec>,
    snapshot_spec: Option<Arc<dyn SandboxSnapshotSpec>>,
    external_sandbox: Option<Box<dyn Sandbox>>,
    external_sandbox_state: Option<SandboxState>,
    isolation_scope: Option<IsolationScope>,
}

impl std::fmt::Debug for SandboxContextBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxContextBuilder")
            .field("client", &self.client.is_some())
            .field("client_options", &self.client_options)
            .field("workspace_spec", &self.workspace_spec)
            .field("snapshot_spec", &self.snapshot_spec)
            .field("external_sandbox", &self.external_sandbox.is_some())
            .field("external_sandbox_state", &self.external_sandbox_state)
            .field("isolation_scope", &self.isolation_scope)
            .finish()
    }
}

impl Default for SandboxContextBuilder {
    fn default() -> Self {
        Self {
            client: None,
            client_options: None,
            workspace_spec: None,
            snapshot_spec: None,
            external_sandbox: None,
            external_sandbox_state: None,
            isolation_scope: None,
        }
    }
}

impl SandboxContextBuilder {
    pub fn client(mut self, client: Box<dyn SandboxClient>) -> Self {
        self.client = Some(client);
        self
    }

    pub fn client_options(mut self, options: SandboxClientOptions) -> Self {
        self.client_options = Some(options);
        self
    }

    pub fn workspace_spec(mut self, spec: WorkspaceSpec) -> Self {
        self.workspace_spec = Some(spec);
        self
    }

    pub fn snapshot_spec(mut self, spec: Arc<dyn SandboxSnapshotSpec>) -> Self {
        self.snapshot_spec = Some(spec);
        self
    }

    pub fn external_sandbox(mut self, sandbox: Box<dyn Sandbox>) -> Self {
        self.external_sandbox = Some(sandbox);
        self
    }

    pub fn external_sandbox_state(mut self, state: SandboxState) -> Self {
        self.external_sandbox_state = Some(state);
        self
    }

    pub fn isolation_scope(mut self, scope: IsolationScope) -> Self {
        self.isolation_scope = Some(scope);
        self
    }

    pub fn build(self) -> SandboxContext {
        SandboxContext {
            client: self.client,
            client_options: self.client_options,
            workspace_spec: self.workspace_spec,
            snapshot_spec: self.snapshot_spec,
            external_sandbox: self.external_sandbox,
            external_sandbox_state: self.external_sandbox_state,
            isolation_scope: self.isolation_scope,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sandbox_context_builder() {
        let context = SandboxContext::builder()
            .client_options(SandboxClientOptions::default())
            .workspace_spec(WorkspaceSpec::default())
            .build();

        assert!(context.client().is_none());
        assert!(context.client_options().is_some());
        assert!(context.workspace_spec().is_some());
        assert!(context.snapshot_spec().is_none());
        assert!(context.external_sandbox().is_none());
        assert!(context.external_sandbox_state().is_none());
        assert!(context.isolation_scope().is_none());
    }
}
