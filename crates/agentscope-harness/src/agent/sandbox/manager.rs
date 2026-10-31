//! Sandbox manager for lifecycle management.
use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use std::sync::Arc;
use tracing::{debug, warn};

use super::context::SandboxContext;
use super::guard::{SandboxExecutionGuard, SandboxGuard};
use super::key::SandboxIsolationKey;
use super::lease::SandboxLease;
use super::state::{SandboxState, SandboxStateStore};
use super::{Sandbox, SandboxClient, WorkspaceSpec};

/// Result of acquiring a sandbox from `SandboxManager`.
#[derive(Debug)]
pub struct SandboxAcquireResult {
    /// The acquired sandbox.
    sandbox: Option<Box<dyn Sandbox>>,

    /// Whether this is a self-managed sandbox (SDK owns lifecycle).
    self_managed: bool,

    /// Guard lease (if configured).
    lease: SandboxLease,
}

impl SandboxAcquireResult {
    /// Creates a self-managed result with a guard lease.
    pub fn self_managed(sandbox: Box<dyn Sandbox>, lease: SandboxLease) -> Self {
        Self {
            sandbox: Some(sandbox),
            self_managed: true,
            lease,
        }
    }

    /// Creates a self-managed result with no guard.
    pub fn self_managed_no_guard(sandbox: Box<dyn Sandbox>) -> Self {
        Self {
            sandbox: Some(sandbox),
            self_managed: true,
            lease: SandboxLease::noop(),
        }
    }

    /// Creates a user-managed result (caller owns lifecycle).
    pub fn user_managed(sandbox: Box<dyn Sandbox>) -> Self {
        Self {
            sandbox: Some(sandbox),
            self_managed: false,
            lease: SandboxLease::noop(),
        }
    }

    /// Returns the sandbox.
    pub fn sandbox(&self) -> Option<&dyn Sandbox> {
        self.sandbox.as_deref()
    }

    /// Returns the sandbox, consuming the result.
    pub fn into_sandbox(self) -> Option<Box<dyn Sandbox>> {
        self.sandbox
    }

    /// Takes the sandbox out of the result, leaving it empty.
    ///
    /// `Sandbox::close` needs `&mut dyn Sandbox`, and `SandboxManager::release` consumes
    /// the result, so it takes ownership of the box before closing it.
    pub fn take_sandbox(&mut self) -> Option<Box<dyn Sandbox>> {
        self.sandbox.take()
    }

    /// Returns true if this is a self-managed sandbox.
    pub fn is_self_managed(&self) -> bool {
        self.self_managed
    }

    /// Returns the guard lease.
    pub fn lease(&self) -> &SandboxLease {
        &self.lease
    }

    /// The isolation key the guard lease is held for (`None` when no guard applies).
    pub fn lease_key(&self) -> Option<&SandboxIsolationKey> {
        if self.lease.is_noop() {
            None
        } else {
            Some(self.lease.key())
        }
    }
}

/// Manages the lifecycle of `Sandbox` instances for the current call.
///
/// Acquire priority:
/// 1. `SandboxContext::external_sandbox` — user-supplied sandbox
/// 2. `SandboxContext::external_sandbox_state` — user-supplied state
/// 3. Persisted `SandboxState` — from state store
/// 4. `SandboxClient::create` — fresh sandbox
pub struct SandboxManager {
    /// Sandbox client for creating/resuming sandboxes.
    client: Arc<dyn SandboxClient>,

    /// State store for persisting sandbox state.
    state_store: Arc<dyn SandboxStateStore>,

    /// Agent ID.
    agent_id: String,

    /// Execution guard for isolation.
    execution_guard: Arc<dyn SandboxGuard>,
}

impl std::fmt::Debug for SandboxManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxManager")
            .field("agent_id", &self.agent_id)
            .field("execution_guard", &self.execution_guard)
            .finish()
    }
}

impl SandboxManager {
    /// Creates a new sandbox manager.
    pub fn new(
        client: Arc<dyn SandboxClient>,
        state_store: Arc<dyn SandboxStateStore>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            client,
            state_store,
            agent_id: agent_id.into(),
            execution_guard: Arc::new(SandboxExecutionGuard::noop()),
        }
    }

    /// Creates a new sandbox manager with execution guard.
    ///
    /// Accepts any [`SandboxGuard`] implementation; the in-process
    /// [`SandboxExecutionGuard`] keeps working unchanged.
    pub fn with_guard<G>(
        client: Arc<dyn SandboxClient>,
        state_store: Arc<dyn SandboxStateStore>,
        agent_id: impl Into<String>,
        execution_guard: G,
    ) -> Self
    where
        G: SandboxGuard + 'static,
    {
        Self {
            client,
            state_store,
            agent_id: agent_id.into(),
            execution_guard: Arc::new(execution_guard),
        }
    }

    /// Creates a new sandbox manager with a shared (e.g. distributed) guard.
    pub fn with_shared_guard(
        client: Arc<dyn SandboxClient>,
        state_store: Arc<dyn SandboxStateStore>,
        agent_id: impl Into<String>,
        execution_guard: Arc<dyn SandboxGuard>,
    ) -> Self {
        Self {
            client,
            state_store,
            agent_id: agent_id.into(),
            execution_guard,
        }
    }

    /// Acquires a sandbox based on the context.
    pub async fn acquire(
        &self,
        sandbox_context: &SandboxContext,
        runtime_context: Option<&RuntimeContext>,
    ) -> Result<SandboxAcquireResult> {
        // Priority 1: user-supplied sandbox — guard does not apply
        // Note: We cannot return a user-managed sandbox because we don't own it.
        // The caller should manage the lifecycle directly.
        if let Some(external_sandbox) = sandbox_context.external_sandbox() {
            debug!(
                "[sandbox] Priority 1: using user-managed sandbox: {}",
                external_sandbox.state().session_id()
            );
            return Err(agentscope_core::error::Error::Other(
                "User-managed sandboxes are not supported in this implementation".to_string(),
            ));
        }

        // Priority 2: user-supplied state — guard does not apply
        if let Some(external_state) = sandbox_context.external_sandbox_state() {
            debug!(
                "[sandbox] Priority 2: resuming from explicit state: {}",
                external_state.session_id()
            );
            let sandbox = self.client.resume(external_state).await?;
            return Ok(SandboxAcquireResult::self_managed_no_guard(sandbox));
        }

        // Priority 3 / 4: harness-managed — apply guard when a scope key is present
        let scope_key = SandboxIsolationKey::resolve(
            sandbox_context.isolation_scope(),
            runtime_context,
            &self.agent_id,
        );

        let mut lease = SandboxLease::noop();
        if let Some(key) = &scope_key {
            debug!("[sandbox] Acquiring execution guard for scope {}", key);
            // Fail-closed: a guard that cannot take the lease returns `Err`, and the
            // acquisition aborts here — no sandbox is resumed/created without the lock.
            // Never fall back to a no-op lease (that would silently drop distributed
            // mutual exclusion).
            lease = self.execution_guard.try_enter(key.clone()).await?;
        }

        // Try to load persisted state (Priority 3)
        if let Some(key) = &scope_key {
            match self.state_store.load(&key.to_string()).await {
                Ok(Some(state)) => {
                    debug!(
                        "[sandbox] Priority 3: resuming from persisted state (scope={})",
                        key
                    );
                    // TODO: Update workspace spec if needed
                    match self.client.resume(&state).await {
                        Ok(sandbox) => {
                            return Ok(SandboxAcquireResult::self_managed(sandbox, lease));
                        }
                        Err(e) => {
                            warn!(
                                "[sandbox] Failed to resume from persisted state, falling through to fresh create: {}",
                                e
                            );
                        }
                    }
                }
                Ok(None) => {
                    // No persisted state, continue to create
                }
                Err(e) => {
                    warn!(
                        "[sandbox] Failed to load persisted state for scope {}, falling through to fresh create: {}",
                        key,
                        e
                    );
                }
            }
        }

        // Priority 4: create new sandbox
        debug!("[sandbox] Priority 4: creating new sandbox");
        let workspace_spec = sandbox_context
            .workspace_spec()
            .cloned()
            .unwrap_or_default();

        let snapshot_spec = sandbox_context.snapshot_spec().cloned();
        let client_options = sandbox_context.client_options().cloned();

        let sandbox = match self
            .client
            .create(
                &workspace_spec,
                snapshot_spec.as_deref(),
                client_options.as_ref(),
            )
            .await
        {
            Ok(sandbox) => sandbox,
            Err(e) => {
                // The lease was already taken for this scope: release it before bubbling
                // the error up, otherwise the isolation key stays locked forever.
                self.release_lease(&lease).await;
                return Err(e);
            }
        };

        Ok(SandboxAcquireResult::self_managed(sandbox, lease))
    }

    /// Releases a sandbox (stop + shutdown if self-managed) and closes its guard lease.
    ///
    /// Consumes the acquire result: a self-managed sandbox is **closed here and nowhere
    /// else**, so the backend resources (container / tmpdir / K8s claim) cannot outlive the
    /// call that created them. Mirrors Java `SandboxManager.release`, which calls
    /// `sandbox.stop()` + `sandbox.shutdown()` and skips user-managed sandboxes (the caller
    /// owns those and reuses them across acquire/release cycles).
    ///
    /// The guard release is **awaited here and nowhere else**: dropping a `SandboxLease`
    /// never releases (see [`SandboxLease`]), so a caller that skips this method leaves an
    /// isolation key held until the backend's lease TTL expires.
    pub async fn release(&self, result: SandboxAcquireResult) {
        let mut result = result;
        // User-managed sandboxes are owned by the caller; their lease is a no-op.
        if result.is_self_managed() {
            if let Some(mut sandbox) = result.take_sandbox() {
                debug!(
                    "[sandbox] Releasing self-managed sandbox: {}",
                    sandbox.state().session_id()
                );
                // `close()` = stop (persist snapshot) + shutdown (destroy resources). A
                // failed snapshot must not leave the backend alive, so `shutdown` is still
                // attempted when `close()` fails: teardown is fail-closed.
                if let Err(e) = sandbox.close().await {
                    warn!("[sandbox] Failed to close sandbox: {}", e);
                    if let Err(e) = sandbox.shutdown().await {
                        warn!("[sandbox] Failed to shut down sandbox: {}", e);
                    }
                }
            }
        }

        self.release_lease(result.lease()).await;
    }

    /// Releases the guard lease exactly once (no-op leases are ignored).
    async fn release_lease(&self, lease: &SandboxLease) {
        if lease.is_noop() || !lease.mark_released() {
            return;
        }
        debug!(
            "[sandbox] Releasing execution guard for scope {}",
            lease.key()
        );
        self.execution_guard.release(lease.key()).await;
    }

    /// Persists sandbox state.
    pub async fn persist_state(
        &self,
        result: &SandboxAcquireResult,
        sandbox_context: &SandboxContext,
        runtime_context: Option<&RuntimeContext>,
    ) {
        let sandbox = match result.sandbox() {
            Some(s) => s,
            None => return,
        };

        // User-managed sandboxes carry their own persistence story
        if !result.is_self_managed() {
            return;
        }

        let state = sandbox.state();

        let scope_key = SandboxIsolationKey::resolve(
            sandbox_context.isolation_scope(),
            runtime_context,
            &self.agent_id,
        );

        let key = match scope_key {
            Some(k) => k,
            None => {
                debug!("[sandbox] No scope key available, skipping state persistence");
                return;
            }
        };

        match self.state_store.save(&key.to_string(), state).await {
            Ok(()) => {
                debug!(
                    "[sandbox] Persisted sandbox state for scope {}: sessionId={}",
                    key,
                    state.session_id()
                );
            }
            Err(e) => {
                warn!("[sandbox] Failed to persist sandbox state: {}", e);
            }
        }
    }

    /// Clears persisted sandbox state.
    pub async fn clear_state(
        &self,
        sandbox_context: &SandboxContext,
        runtime_context: Option<&RuntimeContext>,
    ) {
        let scope_key = SandboxIsolationKey::resolve(
            sandbox_context.isolation_scope(),
            runtime_context,
            &self.agent_id,
        );

        let key = match scope_key {
            Some(k) => k,
            None => return,
        };

        if let Err(e) = self.state_store.delete(&key.to_string()).await {
            warn!("[sandbox] Failed to clear sandbox state: {}", e);
        }
    }
}

use super::ExecResult;

#[cfg(test)]
mod tests {
    use super::super::state::InMemorySandboxStateStore;
    use super::super::{SandboxClientOptions, SandboxSnapshotSpec};
    use super::*;
    use std::sync::Arc;

    // Mock sandbox for testing
    #[derive(Debug)]
    struct MockSandbox {
        state: SandboxState,
        running: bool,
    }

    impl MockSandbox {
        fn new() -> Self {
            Self {
                state: SandboxState::Local(super::super::state::LocalSandboxState {
                    session_id: "test-session".to_string(),
                    temp_dir: "/tmp/test".to_string(),
                    workspace_root_ready: false,
                    workspace_projection_hash: None,
                    metadata: std::collections::HashMap::new(),
                    snapshot: None,
                }),
                running: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl Sandbox for MockSandbox {
        async fn start(&mut self) -> Result<()> {
            self.running = true;
            Ok(())
        }

        async fn stop(&mut self) -> Result<()> {
            self.running = false;
            Ok(())
        }

        fn is_running(&self) -> bool {
            self.running
        }

        fn state(&self) -> &SandboxState {
            &self.state
        }

        async fn exec(
            &self,
            _runtime_context: Option<&RuntimeContext>,
            _command: &str,
            _timeout_seconds: Option<u64>,
        ) -> Result<ExecResult> {
            Ok(ExecResult {
                stdout: "mock output".to_string(),
                stderr: String::new(),
                exit_code: 0,
                truncated: false,
            })
        }

        async fn persist_workspace(&self) -> Result<Vec<u8>> {
            Ok(vec![])
        }

        async fn hydrate_workspace(&self, _archive: &[u8]) -> Result<()> {
            Ok(())
        }
    }

    // Mock client for testing
    struct MockSandboxClient;

    #[async_trait::async_trait]
    impl SandboxClient for MockSandboxClient {
        async fn create(
            &self,
            _workspace_spec: &WorkspaceSpec,
            _snapshot_spec: Option<&dyn SandboxSnapshotSpec>,
            _options: Option<&SandboxClientOptions>,
        ) -> Result<Box<dyn Sandbox>> {
            Ok(Box::new(MockSandbox::new()))
        }

        async fn resume(&self, _state: &SandboxState) -> Result<Box<dyn Sandbox>> {
            Ok(Box::new(MockSandbox::new()))
        }

        async fn delete(&self, _sandbox: &dyn Sandbox) -> Result<()> {
            Ok(())
        }

        fn serialize_state(&self, state: &SandboxState) -> Result<String> {
            serde_json::to_string(state)
                .map_err(|e| agentscope_core::error::Error::Other(e.to_string()))
        }

        fn deserialize_state(&self, json: &str) -> Result<SandboxState> {
            serde_json::from_str(json)
                .map_err(|e| agentscope_core::error::Error::Other(e.to_string()))
        }

        fn deserialize_state_with_spec(
            &self,
            json: &str,
            _snapshot_spec: Option<&dyn SandboxSnapshotSpec>,
        ) -> Result<SandboxState> {
            self.deserialize_state(json)
        }
    }

    #[tokio::test]
    async fn test_sandbox_manager_acquire_priority_4() {
        let client = Arc::new(MockSandboxClient);
        let state_store = Arc::new(InMemorySandboxStateStore::new());
        let manager = SandboxManager::new(client, state_store, "test-agent");

        let context = SandboxContext::builder().build();

        let result = manager.acquire(&context, None).await.unwrap();

        assert!(result.is_self_managed());
        assert!(result.sandbox().is_some());
        assert!(result.lease().is_noop());
    }

    #[tokio::test]
    async fn test_sandbox_manager_persist_state() {
        let client = Arc::new(MockSandboxClient);
        let state_store = Arc::new(InMemorySandboxStateStore::new());
        let manager = SandboxManager::new(client, state_store.clone(), "test-agent");

        let context = SandboxContext::builder().build();
        let result = manager.acquire(&context, None).await.unwrap();

        // Persist state
        manager.persist_state(&result, &context, None).await;

        // Note: This test doesn't verify persistence because the mock sandbox
        // doesn't have a proper isolation key resolution. In a real test,
        // we'd need to set up the runtime context properly.
    }

    // -----------------------------------------------------------------------
    //  Guard lease lifecycle (M4: `SandboxManager::release` must close the lease)
    // -----------------------------------------------------------------------

    use crate::agent::isolation_scope::IsolationScope;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, Default)]
    struct RecordingGuardState {
        enters: AtomicUsize,
        releases: AtomicUsize,
        held: std::sync::Mutex<std::collections::HashMap<SandboxIsolationKey, usize>>,
    }

    /// A fake distributed guard: records enter/release calls and tracks held keys so the
    /// manager's acquire/release pairing can be observed from the outside.
    #[derive(Debug, Clone, Default)]
    struct RecordingGuard {
        state: Arc<RecordingGuardState>,
    }

    impl RecordingGuard {
        fn enters(&self) -> usize {
            self.state.enters.load(Ordering::SeqCst)
        }

        fn releases(&self) -> usize {
            self.state.releases.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl SandboxGuard for RecordingGuard {
        async fn try_enter(
            &self,
            key: SandboxIsolationKey,
        ) -> agentscope_core::error::Result<SandboxLease> {
            self.state.enters.fetch_add(1, Ordering::SeqCst);
            *self
                .state
                .held
                .lock()
                .expect("guard state")
                .entry(key.clone())
                .or_insert(0) += 1;
            Ok(SandboxLease::new(key))
        }

        async fn release(&self, key: &SandboxIsolationKey) {
            self.state.releases.fetch_add(1, Ordering::SeqCst);
            let mut held = self.state.held.lock().expect("guard state");
            if let Some(count) = held.get_mut(key) {
                if *count > 1 {
                    *count -= 1;
                } else {
                    held.remove(key);
                }
            }
        }

        fn is_held(&self, key: &SandboxIsolationKey) -> bool {
            self.state
                .held
                .lock()
                .expect("guard state")
                .contains_key(key)
        }
    }

    /// A client whose `create` always fails (state store empty, so priority 3 is skipped).
    struct FailingSandboxClient;

    #[async_trait::async_trait]
    impl SandboxClient for FailingSandboxClient {
        async fn create(
            &self,
            _workspace_spec: &WorkspaceSpec,
            _snapshot_spec: Option<&dyn SandboxSnapshotSpec>,
            _options: Option<&SandboxClientOptions>,
        ) -> Result<Box<dyn Sandbox>> {
            Err(agentscope_core::error::Error::Other("boom".to_string()))
        }

        async fn resume(&self, _state: &SandboxState) -> Result<Box<dyn Sandbox>> {
            Err(agentscope_core::error::Error::Other("boom".to_string()))
        }

        async fn delete(&self, _sandbox: &dyn Sandbox) -> Result<()> {
            Ok(())
        }

        fn serialize_state(&self, state: &SandboxState) -> Result<String> {
            serde_json::to_string(state)
                .map_err(|e| agentscope_core::error::Error::Other(e.to_string()))
        }

        fn deserialize_state(&self, json: &str) -> Result<SandboxState> {
            serde_json::from_str(json)
                .map_err(|e| agentscope_core::error::Error::Other(e.to_string()))
        }

        fn deserialize_state_with_spec(
            &self,
            json: &str,
            _snapshot_spec: Option<&dyn SandboxSnapshotSpec>,
        ) -> Result<SandboxState> {
            self.deserialize_state(json)
        }
    }

    fn agent_scoped_context() -> SandboxContext {
        SandboxContext::builder()
            .isolation_scope(IsolationScope::Agent)
            .build()
    }

    #[tokio::test]
    async fn release_frees_the_isolation_key_exactly_once() {
        let guard = RecordingGuard::default();
        let observed = guard.clone();
        let manager = SandboxManager::with_guard(
            Arc::new(MockSandboxClient),
            Arc::new(InMemorySandboxStateStore::new()),
            "test-agent",
            guard,
        );
        let context = agent_scoped_context();
        let key = SandboxIsolationKey::new("agent", "test-agent");

        let result = manager.acquire(&context, None).await.unwrap();
        assert!(!result.lease().is_noop(), "guard lease must be taken");
        assert_eq!(result.lease_key(), Some(&key));
        assert!(
            observed.is_held(&key),
            "the key is held while the call runs"
        );
        assert_eq!(observed.enters(), 1);

        manager.release(result).await;
        assert!(
            !observed.is_held(&key),
            "release must free the isolation key"
        );
        assert_eq!(
            (observed.enters(), observed.releases()),
            (1, 1),
            "fake guard must see exactly one enter and one release"
        );

        // `release` consumes the acquire result, so a double release is not expressible;
        // the key is free again and a new acquire re-enters exactly once.
        let again = manager.acquire(&context, None).await.unwrap();
        assert!(observed.is_held(&key), "the key can be re-acquired");
        assert_eq!((observed.enters(), observed.releases()), (2, 1));
        manager.release(again).await;
        assert_eq!((observed.enters(), observed.releases()), (2, 2));
    }

    #[tokio::test]
    async fn failed_acquisition_releases_the_guard_lease() {
        let guard = RecordingGuard::default();
        let observed = guard.clone();
        let manager = SandboxManager::with_guard(
            Arc::new(FailingSandboxClient),
            Arc::new(InMemorySandboxStateStore::new()),
            "test-agent",
            guard,
        );
        let context = agent_scoped_context();
        let key = SandboxIsolationKey::new("agent", "test-agent");

        let error = manager.acquire(&context, None).await.unwrap_err();
        assert!(format!("{error}").contains("boom"), "{error}");
        assert!(
            !observed.is_held(&key),
            "a failed acquisition must not leak the isolation key"
        );
        assert_eq!((observed.enters(), observed.releases()), (1, 1));
    }

    // -----------------------------------------------------------------------
    //  Fail-closed guard acquisition (SANDBOX-09)
    // -----------------------------------------------------------------------

    /// A guard whose acquisition always fails, as a distributed backend does when its lock
    /// store is unreachable or the lock cannot be taken before the deadline.
    #[derive(Debug, Clone, Default)]
    struct FailingGuard {
        enters: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl SandboxGuard for FailingGuard {
        async fn try_enter(
            &self,
            _key: SandboxIsolationKey,
        ) -> agentscope_core::error::Result<SandboxLease> {
            self.enters.fetch_add(1, Ordering::SeqCst);
            Err(agentscope_core::error::Error::Other(
                "lock store unreachable".to_string(),
            ))
        }

        async fn release(&self, _key: &SandboxIsolationKey) {}

        fn is_held(&self, _key: &SandboxIsolationKey) -> bool {
            false
        }
    }

    /// A client that records how many sandboxes it was asked to create.
    #[derive(Debug, Clone, Default)]
    struct CountingSandboxClient {
        creates: Arc<AtomicUsize>,
    }

    impl CountingSandboxClient {
        fn creates(&self) -> usize {
            self.creates.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl SandboxClient for CountingSandboxClient {
        async fn create(
            &self,
            _workspace_spec: &WorkspaceSpec,
            _snapshot_spec: Option<&dyn SandboxSnapshotSpec>,
            _options: Option<&SandboxClientOptions>,
        ) -> Result<Box<dyn Sandbox>> {
            self.creates.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(MockSandbox::new()))
        }

        async fn resume(&self, _state: &SandboxState) -> Result<Box<dyn Sandbox>> {
            Ok(Box::new(MockSandbox::new()))
        }

        async fn delete(&self, _sandbox: &dyn Sandbox) -> Result<()> {
            Ok(())
        }

        fn serialize_state(&self, state: &SandboxState) -> Result<String> {
            serde_json::to_string(state)
                .map_err(|e| agentscope_core::error::Error::Other(e.to_string()))
        }

        fn deserialize_state(&self, json: &str) -> Result<SandboxState> {
            serde_json::from_str(json)
                .map_err(|e| agentscope_core::error::Error::Other(e.to_string()))
        }

        fn deserialize_state_with_spec(
            &self,
            json: &str,
            _snapshot_spec: Option<&dyn SandboxSnapshotSpec>,
        ) -> Result<SandboxState> {
            self.deserialize_state(json)
        }
    }

    /// Fail-closed: a guard that cannot take the isolation lease must abort the acquire.
    ///
    /// Before the `Result` contract the guard's `Err` was downgraded to
    /// [`SandboxLease::noop`] and the manager happily created a sandbox while another
    /// process held the same key. This test pins both halves of the fix: the error reaches
    /// the caller, and no sandbox is created (that half is what makes it fail-closed rather
    /// than merely louder).
    #[tokio::test]
    async fn guard_acquisition_failure_aborts_the_acquire() {
        let guard = FailingGuard::default();
        let observed_guard = guard.clone();
        let client = CountingSandboxClient::default();
        let observed_client = client.clone();
        let manager = SandboxManager::with_guard(
            Arc::new(client),
            Arc::new(InMemorySandboxStateStore::new()),
            "test-agent",
            guard,
        );
        let context = agent_scoped_context();

        let error = manager.acquire(&context, None).await.unwrap_err();
        assert!(
            format!("{error}").contains("lock store unreachable"),
            "the guard's error must reach the caller: {error}"
        );
        assert_eq!(
            observed_guard.enters.load(Ordering::SeqCst),
            1,
            "the guard is consulted exactly once"
        );
        assert_eq!(
            observed_client.creates(),
            0,
            "no sandbox may be created when the isolation lease is unavailable"
        );
    }

    // -----------------------------------------------------------------------
    //  Fail-closed teardown (SANDBOX-09: `release` must `close()` the sandbox)
    // -----------------------------------------------------------------------

    #[derive(Debug, Default)]
    struct CloseCounters {
        stops: AtomicUsize,
        shutdowns: AtomicUsize,
    }

    /// A self-managed sandbox whose teardown is observable.
    #[derive(Debug)]
    struct ClosingSandbox {
        state: SandboxState,
        counters: Arc<CloseCounters>,
    }

    #[async_trait::async_trait]
    impl Sandbox for ClosingSandbox {
        async fn start(&mut self) -> Result<()> {
            Ok(())
        }

        async fn stop(&mut self) -> Result<()> {
            self.counters.stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn shutdown(&mut self) -> Result<()> {
            self.counters.shutdowns.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn is_running(&self) -> bool {
            true
        }

        fn state(&self) -> &SandboxState {
            &self.state
        }

        async fn exec(
            &self,
            _runtime_context: Option<&RuntimeContext>,
            _command: &str,
            _timeout_seconds: Option<u64>,
        ) -> Result<ExecResult> {
            unreachable!("the teardown tests never execute commands")
        }

        async fn persist_workspace(&self) -> Result<Vec<u8>> {
            Ok(vec![])
        }

        async fn hydrate_workspace(&self, _archive: &[u8]) -> Result<()> {
            Ok(())
        }
    }

    fn closing_state() -> SandboxState {
        SandboxState::Local(super::super::state::LocalSandboxState {
            session_id: "close-me".to_string(),
            temp_dir: "/tmp/close-me".to_string(),
            workspace_root_ready: true,
            workspace_projection_hash: None,
            metadata: std::collections::HashMap::new(),
            snapshot: None,
        })
    }

    #[tokio::test]
    async fn release_closes_a_self_managed_sandbox() {
        let counters = Arc::new(CloseCounters::default());
        let manager = SandboxManager::new(
            Arc::new(MockSandboxClient),
            Arc::new(InMemorySandboxStateStore::new()),
            "test-agent",
        );
        let result = SandboxAcquireResult::self_managed_no_guard(Box::new(ClosingSandbox {
            state: closing_state(),
            counters: Arc::clone(&counters),
        }));

        manager.release(result).await;

        assert_eq!(
            counters.stops.load(Ordering::SeqCst),
            1,
            "release must call stop() (snapshot persistence)"
        );
        assert_eq!(
            counters.shutdowns.load(Ordering::SeqCst),
            1,
            "release must call shutdown() (destroy backend resources)"
        );
    }

    #[tokio::test]
    async fn release_leaves_a_user_managed_sandbox_alive() {
        let counters = Arc::new(CloseCounters::default());
        let manager = SandboxManager::new(
            Arc::new(MockSandboxClient),
            Arc::new(InMemorySandboxStateStore::new()),
            "test-agent",
        );
        let result = SandboxAcquireResult::user_managed(Box::new(ClosingSandbox {
            state: closing_state(),
            counters: Arc::clone(&counters),
        }));

        manager.release(result).await;

        assert_eq!(
            counters.stops.load(Ordering::SeqCst),
            0,
            "the caller owns a user-managed sandbox and reuses it"
        );
        assert_eq!(counters.shutdowns.load(Ordering::SeqCst), 0);
    }
}
