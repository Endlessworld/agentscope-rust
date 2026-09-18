//! Control-plane hosted sandbox snapshot spec over `/api/v1/dp/snapshots/*`, mirroring
//! `ControlPlaneSnapshotSpec`.
//!
//! Upload first negotiates via `POST .../upload-url`; when the control plane returns
//! `mode:"inline"` (or the endpoint is missing), bytes are PUT directly to the control
//! plane.

use std::sync::Arc;

use agentscope_harness::agent::sandbox::snapshot::{SandboxSnapshot, SandboxSnapshotSpec};
use agentscope_core::error::{Error, Result};
use async_trait::async_trait;
use reqwest::Method;

use crate::error::AistioError;
use crate::store::control_plane_stores::tenant_query;
use crate::transport::{encode_path_segment, ControlPlaneHttpClient};

/// Control-plane hosted [`SandboxSnapshotSpec`].
#[derive(Debug, Clone)]
pub struct ControlPlaneSnapshotSpec {
    http: Arc<ControlPlaneHttpClient>,
    agent_name: String,
    namespace: String,
}

impl ControlPlaneSnapshotSpec {
    /// Creates a spec bound to the given client.
    pub fn new(
        http: Arc<ControlPlaneHttpClient>,
        agent_name: impl Into<String>,
        namespace: impl Into<String>,
    ) -> Self {
        Self {
            http,
            agent_name: agent_name.into(),
            namespace: namespace.into(),
        }
    }
}

impl SandboxSnapshotSpec for ControlPlaneSnapshotSpec {
    fn build(&self, snapshot_id: &str) -> Result<Arc<dyn SandboxSnapshot>> {
        let snapshot = ControlPlaneSandboxSnapshot {
            http: Arc::clone(&self.http),
            agent_name: self.agent_name.clone(),
            namespace: self.namespace.clone(),
            id: snapshot_id.to_string(),
        };
        Ok(Arc::new(snapshot))
    }
}

/// One control-plane hosted snapshot.
#[derive(Clone)]
pub struct ControlPlaneSandboxSnapshot {
    http: Arc<ControlPlaneHttpClient>,
    agent_name: String,
    namespace: String,
    id: String,
}

impl std::fmt::Debug for ControlPlaneSandboxSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlPlaneSandboxSnapshot").field("id", &self.id).finish()
    }
}

impl ControlPlaneSandboxSnapshot {
    fn base_path(&self) -> String {
        format!(
            "/api/v1/dp/snapshots/{}?{}",
            encode_path_segment(&self.id),
            tenant_query(&self.agent_name, &self.namespace)
        )
    }
}

#[async_trait]
impl SandboxSnapshot for ControlPlaneSandboxSnapshot {
    async fn persist(&self, workspace_archive: &[u8]) -> Result<()> {
        let path = self.base_path();
        let resp = self
            .http
            .send_bytes(Method::PUT, &path, "application/octet-stream", workspace_archive.to_vec())
            .await
            .map_err(|e| Error::Other(e.to_string()))?;
        if resp.status == 404 {
            // Negotiated/external modes are not implemented client-side.
            return Err(Error::Other(
                AistioError::Unsupported(
                    "snapshot upload mode not supported by this client".into(),
                )
                .to_string(),
            ));
        }
        if !(200..300).contains(&resp.status) {
            return Err(Error::Other(format!(
                "control-plane snapshot upload failed: HTTP {} {}",
                resp.status, resp.body
            )));
        }
        Ok(())
    }

    async fn restore(&self) -> Result<Vec<u8>> {
        let path = self.base_path();
        let resp = self
            .http
            .send_for_bytes(Method::GET, &path, None, &[])
            .await
            .map_err(|e| Error::Other(e.to_string()))?;
        if resp.status == 404 {
            return Err(Error::Other(
                AistioError::SnapshotNotFound(self.id.clone()).to_string(),
            ));
        }
        if !(200..300).contains(&resp.status) {
            return Err(Error::Other(format!(
                "control-plane snapshot download failed: HTTP {}",
                resp.status
            )));
        }
        Ok(resp.body)
    }

    async fn is_restorable(&self) -> Result<bool> {
        let path = self.base_path();
        let resp = self
            .http
            .send(Method::HEAD, &path, None)
            .await
            .map_err(|e| Error::Other(e.to_string()))?;
        Ok(resp.status == 200)
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn snapshot_type(&self) -> &str {
        "remote"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_snapshot_with_given_id() {
        let http = Arc::new(ControlPlaneHttpClient::new("http://cp", "tok").unwrap());
        let spec = ControlPlaneSnapshotSpec::new(http, "friday", "default");
        let snap = spec.build("snap-1").unwrap();
        assert_eq!(snap.id(), "snap-1");
        assert_eq!(snap.snapshot_type(), "remote");
    }
}
