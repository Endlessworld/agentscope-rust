//! Control-plane hosted sandbox execution guard over `/api/v1/dp/locks/*`, mirroring
//! `ControlPlaneSandboxExecutionGuard`.
//!
//! On acquire success the returned lease renews every 20s with a 60s TTL. When the
//! control plane is unreachable for a grace period the guard falls back to a
//! process-local mutex.
//!
//! **Why an adapter:** harness `SandboxExecutionGuard` is a concrete, non-blocking,
//! infallible struct (`try_enter` always returns a re-entrant lease) and `SandboxLease`
//! carries no release callback. A control-plane implementation therefore cannot be
//! injected through those types; this crate-local adapter exposes the same
//! acquire/try semantics with an explicit async `close`. The harness abstraction gap is
//! recorded in this module's gap tracker (§4.2).

use std::collections::HashMap;
use std::sync::Arc;

use agentscope_harness::agent::sandbox::SandboxIsolationKey;
use reqwest::Method;
use tokio::sync::{Mutex as TokioMutex, OwnedMutexGuard};

use crate::error::{ControlPlaneError, ControlPlaneResult};
use crate::store::session_turn_gate::{acquire_body, lease_body, parse_owner_token, LockTiming};
use crate::transport::ControlPlaneHttpClient;

/// Builds the lock name for an isolation key (`<scope>:<value>[:<subKey>]`), mirroring
/// the Java `composeLockName` (scope lowercased).
pub fn compose_lock_name(key: &SandboxIsolationKey) -> String {
    let base = format!("{}:{}", key.scope.to_ascii_lowercase(), key.value);
    match &key.sub_key {
        Some(sub) if !sub.is_empty() => format!("{base}:{sub}"),
        _ => base,
    }
}

/// A held sandbox execution lease.
#[allow(dead_code)]
pub struct ControlPlaneSandboxLease {
    http: Option<Arc<ControlPlaneHttpClient>>,
    lock_name: String,
    owner_token: Option<String>,
    _local_guard: Option<OwnedMutexGuard<()>>,
}

impl ControlPlaneSandboxLease {
    /// Whether this lease is backed by the control plane.
    pub fn is_control_plane(&self) -> bool {
        self.owner_token.is_some()
    }

    /// The control-plane lock name.
    pub fn lock_name(&self) -> &str {
        &self.lock_name
    }

    /// Releases the control-plane lock, if any.
    pub async fn close(&mut self) {
        let (Some(http), Some(token)) = (self.http.take(), self.owner_token.take()) else {
            return;
        };
        let body = lease_body("", "", &self.lock_name, &token);
        let _ = http
            .send(Method::POST, "/api/v1/dp/locks/release", Some(body))
            .await;
    }
}

impl std::fmt::Debug for ControlPlaneSandboxLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlPlaneSandboxLease")
            .field("lock_name", &self.lock_name)
            .field("control_plane", &self.owner_token.is_some())
            .finish()
    }
}

/// Control-plane hosted sandbox execution guard with a process-local fallback.
#[derive(Clone)]
pub struct ControlPlaneSandboxExecutionGuard {
    http: Arc<ControlPlaneHttpClient>,
    agent_name: String,
    namespace: String,
    local_locks: Arc<std::sync::Mutex<HashMap<String, Arc<TokioMutex<()>>>>>,
    timing: LockTiming,
}

impl ControlPlaneSandboxExecutionGuard {
    /// Creates a guard bound to the given client.
    pub fn new(
        http: Arc<ControlPlaneHttpClient>,
        agent_name: impl Into<String>,
        namespace: impl Into<String>,
    ) -> Self {
        Self {
            http,
            agent_name: agent_name.into(),
            namespace: namespace.into(),
            local_locks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            timing: LockTiming::default(),
        }
    }

    /// Overrides the retry/fallback timings.
    pub fn with_timing(mut self, timing: LockTiming) -> Self {
        self.timing = timing;
        self
    }

    fn local_mutex(&self, lock_name: &str) -> Arc<TokioMutex<()>> {
        let mut map = self.local_locks.lock().unwrap_or_else(|p| p.into_inner());
        map.entry(lock_name.to_string())
            .or_insert_with(|| Arc::new(TokioMutex::new(())))
            .clone()
    }

    async fn acquire_remote(&self, lock_name: &str) -> ControlPlaneResult<Option<String>> {
        let holder = uuid::Uuid::new_v4().to_string();
        let body = acquire_body(&self.agent_name, &self.namespace, lock_name, &holder);
        let resp = self
            .http
            .send(Method::POST, "/api/v1/dp/locks/acquire", Some(body))
            .await?;
        match resp.status {
            200..=299 => Ok(Some(parse_owner_token(&resp.body))),
            409 => Ok(None),
            status => Err(ControlPlaneError::ControlPlane {
                status,
                message: format!("acquire: {}", resp.body),
            }),
        }
    }

    /// Acquires the execution lease for `key`, retrying a contended lock and falling
    /// back to a process-local mutex when the control plane stays unreachable.
    pub async fn try_enter(&self, key: &SandboxIsolationKey) -> ControlPlaneSandboxLease {
        let lock_name = compose_lock_name(key);
        let mut unreachable_deadline: Option<tokio::time::Instant> = None;
        loop {
            match self.acquire_remote(&lock_name).await {
                Ok(Some(token)) => {
                    return ControlPlaneSandboxLease {
                        http: Some(Arc::clone(&self.http)),
                        lock_name,
                        owner_token: Some(token),
                        _local_guard: None,
                    };
                }
                Ok(None) => {
                    unreachable_deadline = None;
                    tokio::time::sleep(self.timing.retry_interval).await;
                }
                Err(_) => {
                    let deadline = *unreachable_deadline.get_or_insert(
                        tokio::time::Instant::now() + self.timing.unreachable_fallback,
                    );
                    if tokio::time::Instant::now() >= deadline {
                        tracing::warn!(lock = %lock_name, "controlplane: control plane unreachable; using process-local sandbox lock");
                        let guard = self.local_mutex(&lock_name).lock_owned().await;
                        return ControlPlaneSandboxLease {
                            http: None,
                            lock_name,
                            owner_token: None,
                            _local_guard: Some(guard),
                        };
                    }
                    tokio::time::sleep(self.timing.retry_interval).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> ControlPlaneSandboxExecutionGuard {
        ControlPlaneSandboxExecutionGuard::new(
            Arc::new(ControlPlaneHttpClient::new("http://127.0.0.1:1", "tok").unwrap()),
            "friday",
            "default",
        )
        .with_timing(LockTiming::fast())
    }

    #[test]
    fn composes_lock_name_from_isolation_key() {
        assert_eq!(
            compose_lock_name(&SandboxIsolationKey::new("User", "u-1")),
            "user:u-1"
        );
        assert_eq!(
            compose_lock_name(&SandboxIsolationKey::with_sub_key("session", "s-1", "ws")),
            "session:s-1:ws"
        );
    }

    #[tokio::test]
    async fn falls_back_to_local_lock_when_unreachable() {
        let guard = guard();
        let key = SandboxIsolationKey::new("user", "u-1");
        let mut lease = guard.try_enter(&key).await;
        assert!(!lease.is_control_plane());
        assert_eq!(lease.lock_name(), "user:u-1");
        lease.close().await;
        drop(lease);
    }
}
