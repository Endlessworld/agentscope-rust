//! Orchestration client, mirroring
//! `io.agentscope.extensions.controlplane.transport.OrchestrationClient`.
//!
//! Definition/run/attempt/runtime-policy client. Tenant- and namespace-scoped list
//! endpoints append `?tenant=…&namespace=…` (both percent-encoded).

use std::sync::Arc;

use reqwest::Method;
use serde_json::Value;

use crate::error::{ControlPlaneError, ControlPlaneResult};
use crate::transport::control_plane_http_client::{encode_path_segment, ControlPlaneHttpClient};

/// Orchestration API client.
#[derive(Clone)]
pub struct OrchestrationClient {
    http: Arc<ControlPlaneHttpClient>,
    tenant: String,
    namespace: String,
}

impl OrchestrationClient {
    /// Creates a client scoped to `tenant`/`namespace`.
    pub fn new(
        http: Arc<ControlPlaneHttpClient>,
        tenant: impl Into<String>,
        namespace: impl Into<String>,
    ) -> Self {
        Self {
            http,
            tenant: tenant.into(),
            namespace: namespace.into(),
        }
    }

    /// `GET /api/v1/orchestration-definitions`.
    pub async fn definitions(&self) -> ControlPlaneResult<Value> {
        self.get(&self.scope("/api/v1/orchestration-definitions"))
            .await
    }

    /// `GET /api/v1/orchestration-definitions/{id}`.
    pub async fn definition(&self, id: &str) -> ControlPlaneResult<Value> {
        self.get(&format!("/api/v1/orchestration-definitions/{}", seg(id)))
            .await
    }

    /// `POST /api/v1/orchestration-definitions`.
    pub async fn create_definition(&self, body: Value) -> ControlPlaneResult<Value> {
        self.send(
            Method::POST,
            "/api/v1/orchestration-definitions",
            Some(body),
        )
        .await
    }

    /// `PATCH /api/v1/orchestration-definitions/{id}`.
    pub async fn update_definition(&self, id: &str, body: Value) -> ControlPlaneResult<Value> {
        self.send(
            Method::PATCH,
            &format!("/api/v1/orchestration-definitions/{}", seg(id)),
            Some(body),
        )
        .await
    }

    /// `POST /api/v1/orchestration-definitions/{id}/validate`.
    pub async fn validate_definition(&self, id: &str, spec: Value) -> ControlPlaneResult<Value> {
        self.send(
            Method::POST,
            &format!("/api/v1/orchestration-definitions/{}/validate", seg(id)),
            Some(spec),
        )
        .await
    }

    /// `POST /api/v1/orchestration-definitions/{id}/publish`.
    pub async fn publish_definition(&self, id: &str) -> ControlPlaneResult<Value> {
        self.send(
            Method::POST,
            &format!("/api/v1/orchestration-definitions/{}/publish", seg(id)),
            Some(serde_json::json!({})),
        )
        .await
    }

    /// `GET /api/v1/orchestration-definitions/{id}/revisions`.
    pub async fn revisions(&self, id: &str) -> ControlPlaneResult<Value> {
        self.get(&format!(
            "/api/v1/orchestration-definitions/{}/revisions",
            seg(id)
        ))
        .await
    }

    /// `POST /api/v1/orchestration-definitions/{id}/runs`.
    pub async fn start(&self, id: &str, request: Value) -> ControlPlaneResult<Value> {
        self.send(
            Method::POST,
            &format!("/api/v1/orchestration-definitions/{}/runs", seg(id)),
            Some(request),
        )
        .await
    }

    /// `GET /api/v1/orchestration-runs`.
    pub async fn runs(&self) -> ControlPlaneResult<Value> {
        self.get(&self.scope("/api/v1/orchestration-runs")).await
    }

    /// `GET /api/v1/orchestration-runs/{id}`.
    pub async fn run(&self, id: &str) -> ControlPlaneResult<Value> {
        self.get(&format!("/api/v1/orchestration-runs/{}", seg(id)))
            .await
    }

    /// `GET /api/v1/orchestration-runs/{id}/graph`.
    pub async fn graph(&self, id: &str) -> ControlPlaneResult<Value> {
        self.get(&format!("/api/v1/orchestration-runs/{}/graph", seg(id)))
            .await
    }

    /// `GET /api/v1/orchestration-runs/{id}/events`.
    pub async fn events(&self, id: &str) -> ControlPlaneResult<Value> {
        self.get(&format!("/api/v1/orchestration-runs/{}/events", seg(id)))
            .await
    }

    /// `POST /api/v1/orchestration-runs/{id}/{action}`.
    pub async fn control(&self, id: &str, action: &str) -> ControlPlaneResult<Value> {
        self.send(
            Method::POST,
            &format!("/api/v1/orchestration-runs/{}/{}", seg(id), seg(action)),
            Some(serde_json::json!({})),
        )
        .await
    }

    /// `POST /api/v1/orchestration-runs/{id}/rerun`.
    pub async fn rerun(
        &self,
        id: &str,
        idempotency_key: &str,
        input: Value,
    ) -> ControlPlaneResult<Value> {
        let body = serde_json::json!({ "idempotencyKey": idempotency_key, "input": input });
        self.send(
            Method::POST,
            &format!("/api/v1/orchestration-runs/{}/rerun", seg(id)),
            Some(body),
        )
        .await
    }

    /// `POST /api/v1/orchestration-runs/{id}/signals/{name}`.
    pub async fn signal(
        &self,
        id: &str,
        name: &str,
        idempotency_key: &str,
        payload: Value,
    ) -> ControlPlaneResult<Value> {
        let body = serde_json::json!({ "idempotencyKey": idempotency_key, "payload": payload });
        self.send(
            Method::POST,
            &format!(
                "/api/v1/orchestration-runs/{}/signals/{}",
                seg(id),
                seg(name)
            ),
            Some(body),
        )
        .await
    }

    /// `GET /api/v1/execution-attempts`.
    pub async fn attempts(&self) -> ControlPlaneResult<Value> {
        self.get(&self.scope("/api/v1/execution-attempts")).await
    }

    /// `GET /api/v1/execution-attempts/{id}`.
    pub async fn attempt(&self, id: &str) -> ControlPlaneResult<Value> {
        self.get(&format!("/api/v1/execution-attempts/{}", seg(id)))
            .await
    }

    /// `GET /api/v1/agent-runtime-policies/{agentRef}`.
    pub async fn runtime_policy(&self, agent_ref: &str) -> ControlPlaneResult<Value> {
        self.get(&self.scope(&format!(
            "/api/v1/agent-runtime-policies/{}",
            seg(agent_ref)
        )))
        .await
    }

    /// `PUT /api/v1/agent-runtime-policies/{agentRef}`.
    pub async fn put_runtime_policy(
        &self,
        agent_ref: &str,
        policy: Value,
    ) -> ControlPlaneResult<Value> {
        self.send(
            Method::PUT,
            &format!("/api/v1/agent-runtime-policies/{}", seg(agent_ref)),
            Some(policy),
        )
        .await
    }

    /// Appends the tenant/namespace scope to a list endpoint.
    fn scope(&self, path: &str) -> String {
        format!(
            "{path}?tenant={}&namespace={}",
            seg(&self.tenant),
            seg(&self.namespace)
        )
    }

    async fn get(&self, path: &str) -> ControlPlaneResult<Value> {
        self.send(Method::GET, path, None).await
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> ControlPlaneResult<Value> {
        let response = self.http.send(method, path, body).await?;
        if !(200..300).contains(&response.status) {
            return Err(ControlPlaneError::Http(format!(
                "orchestration request failed: HTTP {}: {}",
                response.status, response.body
            )));
        }
        if response.body.trim().is_empty() {
            return Ok(serde_json::json!({}));
        }
        serde_json::from_str(&response.body).map_err(|e| {
            ControlPlaneError::Http(format!("orchestration returned invalid JSON: {e}"))
        })
    }
}

/// Percent-encodes one path/query segment.
fn seg(value: &str) -> String {
    encode_path_segment(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> OrchestrationClient {
        let http = Arc::new(ControlPlaneHttpClient::new("http://localhost:9", "tok").unwrap());
        OrchestrationClient::new(http, "acme", "prod")
    }

    #[test]
    fn scope_appends_tenant_and_namespace() {
        let c = client();
        assert_eq!(
            c.scope("/api/v1/orchestration-runs"),
            "/api/v1/orchestration-runs?tenant=acme&namespace=prod"
        );
    }

    #[test]
    fn scope_encodes_reserved_characters() {
        let http = Arc::new(ControlPlaneHttpClient::new("http://localhost:9", "tok").unwrap());
        let c = OrchestrationClient::new(http, "a/b c", "ns&x");
        assert_eq!(c.scope("/p"), "/p?tenant=a%2Fb%20c&namespace=ns%26x");
    }

    #[test]
    fn client_retains_scope_fields() {
        let c = client();
        assert_eq!(c.tenant, "acme");
        assert_eq!(c.namespace, "prod");
    }
}
