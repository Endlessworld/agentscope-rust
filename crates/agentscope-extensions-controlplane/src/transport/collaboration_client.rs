//! Collaboration client, mirroring
//! `io.agentscope.extensions.controlplane.transport.CollaborationClient`.
//!
//! Issue/Comment/AgentTask client shared by external-agent runtimes. Every call carries
//! the task-scoped token in the `X-Agent-Task-Token` header (the control-plane internal
//! token rides on the underlying [`ControlPlaneHttpClient`]).

use std::sync::Arc;
use std::time::Duration;

use reqwest::Method;
use serde_json::Value;

use crate::error::{ControlPlaneError, ControlPlaneResult};
use crate::transport::control_plane_http_client::{encode_path_segment, ControlPlaneHttpClient};

/// Task-scoped token header.
pub const TASK_TOKEN_HEADER: &str = "X-Agent-Task-Token";

/// Fallback media type for artifact transfers (Java uses the same literal).
pub const DEFAULT_ARTIFACT_CONTENT_TYPE: &str = "application/octet-stream";

/// Default interval between runtime-approval decision polls (Java uses 1000ms).
pub const RUNTIME_APPROVAL_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// A fenced runtime tool-approval decision, mirroring
/// `CollaborationClient.RuntimeApprovalDecision`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeApprovalDecision {
    /// Control-plane approval id (fencing token for the ack).
    pub approval_id: String,
    /// Monotonic decision version; `> 0` means the decision is final.
    pub decision_version: i64,
    /// Whether the tool call was approved.
    pub allow: bool,
    /// Reason surfaced to the model when `allow` is `false`.
    pub deny_message: String,
}

/// A downloaded shared artifact: raw bytes plus the media type the control plane reported.
///
/// Java's `downloadArtifact` returns a bare `byte[]`; the Rust port keeps the `Content-Type`
/// alongside it so callers never have to guess (and never round-trip binary through a
/// `String`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactContent {
    /// `Content-Type` of the payload, defaulting to [`DEFAULT_ARTIFACT_CONTENT_TYPE`].
    pub content_type: String,
    /// Raw payload bytes, not decoded as text.
    pub bytes: Vec<u8>,
}

/// Transport seam for the fenced runtime tool-approval handshake.
///
/// Split out from [`CollaborationClient`] so the request/poll/ack loop can be verified
/// without a control plane, mirroring the `TurnRunner` seam used by the continuation loop.
#[async_trait::async_trait]
pub trait RuntimeApprovalSource: Send + Sync {
    /// Creates a pending approval; the response must carry `approval.id`.
    async fn create_runtime_approval(
        &self,
        task_id: &str,
        token: &str,
        request: Value,
    ) -> ControlPlaneResult<Value>;

    /// Polls the decision; `decisionVersion > 0` means it is final.
    async fn poll_runtime_approval(
        &self,
        task_id: &str,
        token: &str,
        approval_id: &str,
    ) -> ControlPlaneResult<Value>;

    /// Acknowledges the final decision so the control plane stops re-delivering it.
    async fn ack_runtime_approval(
        &self,
        task_id: &str,
        token: &str,
        approval_id: &str,
        decision_version: i64,
    ) -> ControlPlaneResult<Value>;
}

/// Drives the fenced approval handshake: request → poll → ack, mirroring
/// `CollaborationClient.awaitRuntimeToolApproval`.
///
/// Blocks (asynchronously) until the control plane publishes a final decision, so a
/// `PermissionBehavior::Ask` tool call resolves through a real human decision.
pub async fn await_runtime_tool_approval(
    source: &dyn RuntimeApprovalSource,
    task_id: &str,
    token: &str,
    tool_use_id: &str,
    tool_name: &str,
    input_preview: Value,
    poll_interval: Duration,
) -> ControlPlaneResult<RuntimeApprovalDecision> {
    let request = serde_json::json!({
        "kind": "tool_confirmation",
        "toolUseId": tool_use_id,
        "toolName": tool_name,
        "inputPreview": if input_preview.is_null() {
            serde_json::json!({})
        } else {
            input_preview
        },
    });
    let created = source
        .create_runtime_approval(task_id, token, request)
        .await?;
    let approval_id = created
        .pointer("/approval/id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if approval_id.trim().is_empty() {
        return Err(ControlPlaneError::Unsupported(
            "runtime-approval.request returned no approval id".to_string(),
        ));
    }
    loop {
        let decision = source
            .poll_runtime_approval(task_id, token, &approval_id)
            .await?;
        let decision_version = decision
            .get("decisionVersion")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if decision_version > 0 {
            source
                .ack_runtime_approval(task_id, token, &approval_id, decision_version)
                .await?;
            return Ok(RuntimeApprovalDecision {
                approval_id,
                decision_version,
                allow: decision
                    .get("allow")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                deny_message: decision
                    .get("denyMessage")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        tokio::time::sleep(poll_interval).await;
    }
}

/// Collaboration API client.
#[derive(Clone)]
pub struct CollaborationClient {
    http: Arc<ControlPlaneHttpClient>,
}

impl CollaborationClient {
    /// Creates a client over the shared HTTP client.
    pub fn new(http: Arc<ControlPlaneHttpClient>) -> Self {
        Self { http }
    }

    /// `GET /api/v1/issues/{id}` (`issue.get`).
    pub async fn issue(&self, issue_id: &str, token: &str) -> ControlPlaneResult<Value> {
        self.send(
            Method::GET,
            &format!("/api/v1/issues/{}", seg(issue_id)),
            None,
            token,
            "issue.get",
        )
        .await
    }

    /// `GET /api/v1/issues/{id}/comments` (`issue.comment.list`).
    pub async fn comments(&self, issue_id: &str, token: &str) -> ControlPlaneResult<Value> {
        self.send(
            Method::GET,
            &format!("/api/v1/issues/{}/comments", seg(issue_id)),
            None,
            token,
            "issue.comment.list",
        )
        .await
    }

    /// `POST /api/v1/issues/{id}/comments` (`issue.comment.add`).
    pub async fn add_comment(
        &self,
        issue_id: &str,
        token: &str,
        content: &str,
        mentions: Vec<Value>,
        parent_id: Option<&str>,
    ) -> ControlPlaneResult<Value> {
        let body = serde_json::json!({
            "content": content,
            "mentions": mentions,
            "parentId": parent_id.unwrap_or_default(),
        });
        self.send(
            Method::POST,
            &format!("/api/v1/issues/{}/comments", seg(issue_id)),
            Some(body),
            token,
            "issue.comment.add",
        )
        .await
    }

    /// `GET /api/v1/teams/{id}` (`team.get`).
    pub async fn team(&self, team_id: &str, token: &str) -> ControlPlaneResult<Value> {
        self.send(
            Method::GET,
            &format!("/api/v1/teams/{}", seg(team_id)),
            None,
            token,
            "team.get",
        )
        .await
    }

    /// `POST /api/v1/approvals` (`approval.request`).
    pub async fn request_approval(&self, token: &str, request: Value) -> ControlPlaneResult<Value> {
        self.send(
            Method::POST,
            "/api/v1/approvals",
            Some(request),
            token,
            "approval.request",
        )
        .await
    }

    /// Requests a fenced runtime tool decision and waits for it, mirroring
    /// `CollaborationClient.awaitRuntimeToolApproval`.
    pub async fn await_runtime_tool_approval(
        &self,
        task_id: &str,
        token: &str,
        tool_use_id: &str,
        tool_name: &str,
        input_preview: Value,
    ) -> ControlPlaneResult<RuntimeApprovalDecision> {
        await_runtime_tool_approval(
            self,
            task_id,
            token,
            tool_use_id,
            tool_name,
            input_preview,
            RUNTIME_APPROVAL_POLL_INTERVAL,
        )
        .await
    }

    /// `GET /api/v1/agent-tasks/{id}/context` (`task.context`).
    pub async fn task_context(&self, task_id: &str, token: &str) -> ControlPlaneResult<Value> {
        self.task_send(Method::GET, task_id, "context", token, None, "task.context")
            .await
    }

    /// Lists the canonical task-scoped collaboration tools (`tools/list`).
    pub async fn tools(&self, task_id: &str, token: &str) -> ControlPlaneResult<Value> {
        let response = self
            .mcp(
                token,
                "tools/list",
                serde_json::json!({ "taskId": task_id }),
            )
            .await?;
        Ok(response.get("tools").cloned().unwrap_or(Value::Null))
    }

    /// Invokes one canonical collaboration tool (`tools/call`), mirroring the Java
    /// `callTool(taskId, token, toolName, arguments, toolCallId)` overload.
    pub async fn call_tool(
        &self,
        task_id: &str,
        token: &str,
        tool_name: &str,
        arguments: Option<Value>,
        tool_call_id: Option<&str>,
    ) -> ControlPlaneResult<Value> {
        let mut scoped = match arguments {
            Some(Value::Object(map)) => map,
            Some(other) => {
                let mut map = serde_json::Map::new();
                map.insert("value".into(), other);
                map
            }
            None => serde_json::Map::new(),
        };
        scoped.insert("taskId".into(), Value::String(task_id.to_string()));
        if let Some(id) = tool_call_id.filter(|s| !s.trim().is_empty()) {
            scoped.insert("_toolCallId".into(), Value::String(id.to_string()));
        }
        let params = serde_json::json!({
            "name": tool_name,
            "arguments": Value::Object(scoped),
        });
        self.mcp(token, "tools/call", params).await
    }

    /// `POST /api/v1/agent-tasks/{id}/children` (`issue.child.create`).
    pub async fn create_child(
        &self,
        task_id: &str,
        token: &str,
        request: Value,
    ) -> ControlPlaneResult<Value> {
        self.task_send(
            Method::POST,
            task_id,
            "children",
            token,
            Some(request),
            "issue.child.create",
        )
        .await
    }

    /// `POST /api/v1/agent-tasks/{id}/ack` (`task.ack`).
    pub async fn acknowledge(
        &self,
        task_id: &str,
        token: &str,
        input_ids: Vec<String>,
    ) -> ControlPlaneResult<Value> {
        let body = serde_json::json!({ "inputIds": input_ids });
        self.task_send(Method::POST, task_id, "ack", token, Some(body), "task.ack")
            .await
    }

    /// `POST /api/v1/agent-tasks/{id}/start` (`task.start`).
    pub async fn start(
        &self,
        task_id: &str,
        token: &str,
        expected_version: i64,
    ) -> ControlPlaneResult<Value> {
        let body = serde_json::json!({ "expectedVersion": expected_version });
        self.task_send(
            Method::POST,
            task_id,
            "start",
            token,
            Some(body),
            "task.start",
        )
        .await
    }

    /// `POST /api/v1/agent-tasks/{id}/progress` (`task.progress`).
    pub async fn progress(
        &self,
        task_id: &str,
        token: &str,
        content: &str,
    ) -> ControlPlaneResult<Value> {
        let body = serde_json::json!({ "content": content });
        self.task_send(
            Method::POST,
            task_id,
            "progress",
            token,
            Some(body),
            "task.progress",
        )
        .await
    }

    /// `POST /api/v1/agent-tasks/{id}/respond` (`task.respond`).
    pub async fn respond(
        &self,
        task_id: &str,
        token: &str,
        content: &str,
    ) -> ControlPlaneResult<Value> {
        let body = serde_json::json!({ "content": content, "type": "result" });
        self.task_send(
            Method::POST,
            task_id,
            "respond",
            token,
            Some(body),
            "task.respond",
        )
        .await
    }

    /// `POST /api/v1/agent-tasks/{id}/workspace-application` (`workspace.applied`).
    pub async fn workspace_applied(
        &self,
        task_id: &str,
        token: &str,
        digest: &str,
    ) -> ControlPlaneResult<Value> {
        let body = serde_json::json!({ "digest": digest });
        self.task_send(
            Method::POST,
            task_id,
            "workspace-application",
            token,
            Some(body),
            "workspace.applied",
        )
        .await
    }

    /// `POST /api/v1/agent-tasks/{id}/complete` (`task.complete`).
    #[allow(clippy::too_many_arguments)]
    pub async fn complete(
        &self,
        task_id: &str,
        token: &str,
        expected_version: i64,
        summary: &str,
        result: Value,
        processed_input_ids: Vec<String>,
        deferred_input_ids: Vec<String>,
    ) -> ControlPlaneResult<Value> {
        let body = serde_json::json!({
            "expectedVersion": expected_version,
            "summary": summary,
            "result": result,
            "processedInputIds": processed_input_ids,
            "deferredInputIds": deferred_input_ids,
        });
        self.task_send(
            Method::POST,
            task_id,
            "complete",
            token,
            Some(body),
            "task.complete",
        )
        .await
    }

    /// `POST /api/v1/agent-tasks/{id}/complete` carrying an explicit business outcome.
    ///
    /// The server preserves partial results on failure.
    #[allow(clippy::too_many_arguments)]
    pub async fn finish(
        &self,
        task_id: &str,
        token: &str,
        expected_version: i64,
        outcome: &str,
        reason: &str,
        result: Value,
        processed_input_ids: Vec<String>,
        deferred_input_ids: Vec<String>,
    ) -> ControlPlaneResult<Value> {
        let body = serde_json::json!({
            "expectedVersion": expected_version,
            "outcome": outcome,
            "summary": reason,
            "result": result,
            "processedInputIds": processed_input_ids,
            "deferredInputIds": deferred_input_ids,
        });
        self.task_send(
            Method::POST,
            task_id,
            "complete",
            token,
            Some(body),
            "task.complete",
        )
        .await
    }

    /// `POST /api/v1/agent-tasks/{id}/fail` (`task.fail`).
    pub async fn fail(
        &self,
        task_id: &str,
        token: &str,
        expected_version: i64,
        code: &str,
        message: &str,
    ) -> ControlPlaneResult<Value> {
        let body = serde_json::json!({
            "expectedVersion": expected_version,
            "code": if code.is_empty() { "agent_failed" } else { code },
            "message": message,
        });
        self.task_send(
            Method::POST,
            task_id,
            "fail",
            token,
            Some(body),
            "task.fail",
        )
        .await
    }

    /// `GET /api/v1/agent-tasks/{id}/run` (`run.get`).
    pub async fn run(&self, task_id: &str, token: &str) -> ControlPlaneResult<Value> {
        self.task_send(Method::GET, task_id, "run", token, None, "run.get")
            .await
    }

    /// `GET /api/v1/agent-tasks/{id}/run/graph` (`run.graph`).
    pub async fn run_graph(&self, task_id: &str, token: &str) -> ControlPlaneResult<Value> {
        self.task_send(Method::GET, task_id, "run/graph", token, None, "run.graph")
            .await
    }

    /// `POST /api/v1/agent-tasks/{id}/run/node/complete` (`run.node.complete`).
    pub async fn complete_run_node(
        &self,
        task_id: &str,
        token: &str,
        output: Value,
    ) -> ControlPlaneResult<Value> {
        self.task_send(
            Method::POST,
            task_id,
            "run/node/complete",
            token,
            Some(output),
            "run.node.complete",
        )
        .await
    }

    /// `POST /api/v1/agent-tasks/{id}/run/node/fail` (`run.node.fail`).
    pub async fn fail_run_node(
        &self,
        task_id: &str,
        token: &str,
        code: &str,
        message: &str,
    ) -> ControlPlaneResult<Value> {
        let body = serde_json::json!({ "code": code, "message": message });
        self.task_send(
            Method::POST,
            task_id,
            "run/node/fail",
            token,
            Some(body),
            "run.node.fail",
        )
        .await
    }

    /// `GET /api/v1/agent-tasks/{id}/run/artifacts` (`run.artifacts`).
    pub async fn run_artifacts(&self, task_id: &str, token: &str) -> ControlPlaneResult<Value> {
        self.task_send(
            Method::GET,
            task_id,
            "run/artifacts",
            token,
            None,
            "run.artifacts",
        )
        .await
    }

    /// `POST /api/v1/agent-tasks/{id}/run/replan` (`run.replan`).
    ///
    /// `node` is the replanned run-graph node, forwarded verbatim.
    pub async fn replan_run(
        &self,
        task_id: &str,
        token: &str,
        node: Value,
    ) -> ControlPlaneResult<Value> {
        self.task_send(
            Method::POST,
            task_id,
            "run/replan",
            token,
            Some(node),
            "run.replan",
        )
        .await
    }

    /// `POST /api/v1/agent-tasks/{id}/run/signals/{name}` (`run.signal`).
    ///
    /// A missing payload is sent as `{}`, mirroring Java `payload == null ? Map.of() : payload`.
    pub async fn signal_run(
        &self,
        task_id: &str,
        token: &str,
        name: &str,
        idempotency_key: &str,
        payload: Option<Value>,
    ) -> ControlPlaneResult<Value> {
        let body = serde_json::json!({
            "idempotencyKey": idempotency_key,
            "payload": match payload {
                Some(value) if !value.is_null() => value,
                _ => serde_json::json!({}),
            },
        });
        self.task_send(
            Method::POST,
            task_id,
            &format!("run/signals/{}", seg(name)),
            token,
            Some(body),
            "run.signal",
        )
        .await
    }

    /// `POST /api/v1/artifacts/uploads` (`artifact.upload`) as `multipart/form-data`.
    ///
    /// Shared bytes only — local runtime paths never cross the collaboration boundary.
    /// `target_type` defaults to `agent-task`, `target_ref` to `task_id`, and a missing
    /// `content_type` to [`DEFAULT_ARTIFACT_CONTENT_TYPE`], all matching Java.
    pub async fn upload_artifact(
        &self,
        task_id: &str,
        token: &str,
        filename: &str,
        content_type: Option<&str>,
        content: &[u8],
        target_type: Option<&str>,
        target_ref: Option<&str>,
    ) -> ControlPlaneResult<Value> {
        let boundary = format!("controlplane-{}", uuid::Uuid::new_v4());
        let body = multipart_artifact_body(
            &boundary,
            task_id,
            filename,
            content_type,
            content,
            target_type,
            target_ref,
        );
        let headers = [(TASK_TOKEN_HEADER, token)];
        let response = self
            .http
            .send_bytes_with_headers(
                Method::POST,
                "/api/v1/artifacts/uploads",
                &format!("multipart/form-data; boundary={boundary}"),
                body,
                &headers,
            )
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(ControlPlaneError::Http(format!(
                "artifact.upload failed: HTTP {} {}",
                response.status, response.body
            )));
        }
        parse_json_body(&response.body, "artifact.upload")
    }

    /// `POST /api/v1/artifacts/{id}/download?taskId={taskId}` (`artifact.download`).
    ///
    /// The only binary response on this client: the payload is returned as raw bytes and
    /// is never converted through a `String`.
    pub async fn download_artifact(
        &self,
        artifact_id: &str,
        task_id: &str,
        token: &str,
    ) -> ControlPlaneResult<ArtifactContent> {
        let path = format!(
            "/api/v1/artifacts/{}/download?taskId={}",
            seg(artifact_id),
            crate::transport::encode_query(task_id)
        );
        let headers = [(TASK_TOKEN_HEADER, token)];
        let response = self
            .http
            .send_for_bytes(Method::POST, &path, None, &headers)
            .await?;
        if !(200..300).contains(&response.status) {
            let detail = String::from_utf8_lossy(&response.body).to_string();
            return Err(ControlPlaneError::Http(format!(
                "artifact.download failed: HTTP {} {detail}",
                response.status
            )));
        }
        Ok(ArtifactContent {
            content_type: response
                .content_type
                .unwrap_or_else(|| DEFAULT_ARTIFACT_CONTENT_TYPE.to_string()),
            bytes: response.body,
        })
    }

    /// Sends a task-scoped request.
    async fn task_send(
        &self,
        method: Method,
        task_id: &str,
        action: &str,
        token: &str,
        body: Option<Value>,
        operation: &str,
    ) -> ControlPlaneResult<Value> {
        self.send(
            method,
            &format!("/api/v1/agent-tasks/{}/{action}", seg(task_id)),
            body,
            token,
            operation,
        )
        .await
    }

    /// Sends a control-plane MCP request (`/api/v1/agent-tasks/mcp`).
    async fn mcp(&self, token: &str, method: &str, params: Value) -> ControlPlaneResult<Value> {
        let body = serde_json::json!({ "method": method, "params": params });
        self.send(
            Method::POST,
            "/api/v1/agent-tasks/mcp",
            Some(body),
            token,
            "mcp",
        )
        .await
    }

    /// Sends a request, maps non-2xx to a collaboration error and parses the JSON body.
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        token: &str,
        operation: &str,
    ) -> ControlPlaneResult<Value> {
        let headers = [(TASK_TOKEN_HEADER, token)];
        let response = self
            .http
            .send_with_headers(method, path, body, &headers)
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(ControlPlaneError::Http(format!(
                "{operation} failed: HTTP {} {}",
                response.status, response.body
            )));
        }
        parse_json_body(&response.body, operation)
    }
}

/// Parses a collaboration response body, treating an empty body as `{}`.
fn parse_json_body(body: &str, operation: &str) -> ControlPlaneResult<Value> {
    if body.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(body)
        .map_err(|e| ControlPlaneError::Http(format!("{operation} returned invalid JSON: {e}")))
}

/// Builds the artifact-upload body, mirroring Java `CollaborationClient.uploadArtifact`.
///
/// Form fields come first (`sourceTaskId`, `targetType`, `targetRef`), then a single `file`
/// part whose filename is stripped of quote / CR / LF, then the closing boundary.
fn multipart_artifact_body(
    boundary: &str,
    task_id: &str,
    filename: &str,
    content_type: Option<&str>,
    content: &[u8],
    target_type: Option<&str>,
    target_ref: Option<&str>,
) -> Vec<u8> {
    let mut body = Vec::new();
    write_form_field(&mut body, boundary, "sourceTaskId", task_id);
    write_form_field(
        &mut body,
        boundary,
        "targetType",
        target_type.unwrap_or("agent-task"),
    );
    write_form_field(
        &mut body,
        boundary,
        "targetRef",
        target_ref.unwrap_or(task_id),
    );
    let safe_name = filename.replace(|c: char| c == '"' || c == '\r' || c == '\n', "_");
    let header = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{safe_name}\"\r\nContent-Type: {}\r\n\r\n",
        content_type.unwrap_or(DEFAULT_ARTIFACT_CONTENT_TYPE)
    );
    body.extend_from_slice(header.as_bytes());
    body.extend_from_slice(content);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

fn write_form_field(body: &mut Vec<u8>, boundary: &str, name: &str, value: &str) {
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        )
        .as_bytes(),
    );
}

#[async_trait::async_trait]
impl RuntimeApprovalSource for CollaborationClient {
    async fn create_runtime_approval(
        &self,
        task_id: &str,
        token: &str,
        request: Value,
    ) -> ControlPlaneResult<Value> {
        self.task_send(
            Method::POST,
            task_id,
            "runtime-approvals",
            token,
            Some(request),
            "runtime-approval.request",
        )
        .await
    }

    async fn poll_runtime_approval(
        &self,
        task_id: &str,
        token: &str,
        approval_id: &str,
    ) -> ControlPlaneResult<Value> {
        self.task_send(
            Method::GET,
            task_id,
            &format!("runtime-approvals/{}/decision", seg(approval_id)),
            token,
            None,
            "runtime-approval.poll",
        )
        .await
    }

    async fn ack_runtime_approval(
        &self,
        task_id: &str,
        token: &str,
        approval_id: &str,
        decision_version: i64,
    ) -> ControlPlaneResult<Value> {
        let body = serde_json::json!({ "decisionVersion": decision_version });
        self.task_send(
            Method::POST,
            task_id,
            &format!("runtime-approvals/{}/ack", seg(approval_id)),
            token,
            Some(body),
            "runtime-approval.ack",
        )
        .await
    }
}

/// Percent-encodes one path segment.
fn seg(value: &str) -> String {
    encode_path_segment(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn task_token_header_name_is_stable() {
        assert_eq!(TASK_TOKEN_HEADER, "X-Agent-Task-Token");
    }

    #[test]
    fn path_segment_is_encoded() {
        assert_eq!(seg("a b/c"), "a%20b%2Fc");
    }

    /// Mirrors the Java `AgentTaskCollaborationToolTest` runtime-approval scenarios offline.
    struct FakeApprovalSource {
        created: Value,
        decisions: Mutex<Vec<Value>>,
        acked: Mutex<Vec<i64>>,
    }

    impl FakeApprovalSource {
        fn new(created: Value, decisions: Vec<Value>) -> Self {
            Self {
                created,
                decisions: Mutex::new(decisions),
                acked: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl RuntimeApprovalSource for FakeApprovalSource {
        async fn create_runtime_approval(
            &self,
            _task_id: &str,
            _token: &str,
            _request: Value,
        ) -> ControlPlaneResult<Value> {
            Ok(self.created.clone())
        }

        async fn poll_runtime_approval(
            &self,
            _task_id: &str,
            _token: &str,
            _approval_id: &str,
        ) -> ControlPlaneResult<Value> {
            let mut decisions = self.decisions.lock().unwrap();
            if decisions.is_empty() {
                return Ok(serde_json::json!({ "decisionVersion": 0 }));
            }
            Ok(decisions.remove(0))
        }

        async fn ack_runtime_approval(
            &self,
            _task_id: &str,
            _token: &str,
            _approval_id: &str,
            decision_version: i64,
        ) -> ControlPlaneResult<Value> {
            self.acked.lock().unwrap().push(decision_version);
            Ok(serde_json::json!({}))
        }
    }

    #[tokio::test]
    async fn runtime_approval_polls_until_final_then_acks() {
        let source = FakeApprovalSource::new(
            serde_json::json!({ "approval": { "id": "appr-1" } }),
            vec![
                serde_json::json!({ "decisionVersion": 0 }),
                serde_json::json!({ "decisionVersion": 3, "allow": true }),
            ],
        );
        let decision = await_runtime_tool_approval(
            &source,
            "task-1",
            "tok",
            "call-1",
            "read_file",
            serde_json::json!({ "path": "a.txt" }),
            Duration::from_millis(0),
        )
        .await
        .unwrap();
        assert_eq!(
            decision,
            RuntimeApprovalDecision {
                approval_id: "appr-1".to_string(),
                decision_version: 3,
                allow: true,
                deny_message: String::new(),
            }
        );
        assert_eq!(*source.acked.lock().unwrap(), vec![3]);
    }

    #[tokio::test]
    async fn runtime_approval_surfaces_deny_message() {
        let source = FakeApprovalSource::new(
            serde_json::json!({ "approval": { "id": "appr-2" } }),
            vec![
                serde_json::json!({ "decisionVersion": 1, "allow": false, "denyMessage": "nope" }),
            ],
        );
        let decision = await_runtime_tool_approval(
            &source,
            "task-1",
            "tok",
            "call-1",
            "execute",
            Value::Null,
            Duration::from_millis(0),
        )
        .await
        .unwrap();
        assert!(!decision.allow);
        assert_eq!(decision.deny_message, "nope");
    }

    #[tokio::test]
    async fn runtime_approval_requires_an_approval_id() {
        let source = FakeApprovalSource::new(serde_json::json!({ "approval": {} }), vec![]);
        let err = await_runtime_tool_approval(
            &source,
            "task-1",
            "tok",
            "call-1",
            "execute",
            Value::Null,
            Duration::from_millis(0),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no approval id"), "{err}");
    }

    #[test]
    fn client_is_constructible_from_http() {
        let http = Arc::new(ControlPlaneHttpClient::new("http://localhost:8080", "tok").unwrap());
        let client = CollaborationClient::new(http);
        // Cloning must share the same underlying handle.
        let _clone = client.clone();
        assert_eq!(client.http.base_url(), "http://localhost:8080");
    }

    // ---- offline loopback harness for the artifact / run-control endpoints ----

    /// One request as the loopback control plane observed it.
    #[derive(Clone, Debug)]
    struct RecordedRequest {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl RecordedRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }

        fn json(&self) -> Value {
            serde_json::from_slice(&self.body).expect("json body")
        }
    }

    struct ControlPlaneStub {
        requests: Mutex<Vec<RecordedRequest>>,
        response: Mutex<(u16, Option<String>, Vec<u8>)>,
    }

    impl ControlPlaneStub {
        fn requests(&self) -> Vec<RecordedRequest> {
            self.requests.lock().unwrap().clone()
        }

        fn only_request(&self) -> RecordedRequest {
            let requests = self.requests();
            assert_eq!(
                requests.len(),
                1,
                "expected exactly one request: {requests:?}"
            );
            requests[0].clone()
        }
    }

    async fn control_plane_stub(
        status: u16,
        content_type: Option<&str>,
        body: Vec<u8>,
    ) -> (Arc<ControlPlaneStub>, CollaborationClient) {
        use axum::extract::{Request, State};

        async fn handle(
            State(stub): State<Arc<ControlPlaneStub>>,
            request: Request,
        ) -> axum::response::Response {
            let method = request.method().to_string();
            let path = request
                .uri()
                .path_and_query()
                .map(|p| p.as_str().to_string())
                .unwrap_or_default();
            let headers = request
                .headers()
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str().to_string(),
                        String::from_utf8_lossy(v.as_bytes()).into_owned(),
                    )
                })
                .collect();
            let body = axum::body::to_bytes(request.into_body(), usize::MAX)
                .await
                .map(|b| b.to_vec())
                .unwrap_or_default();
            stub.requests.lock().unwrap().push(RecordedRequest {
                method,
                path,
                headers,
                body,
            });
            let (status, content_type, body) = stub.response.lock().unwrap().clone();
            let mut builder = axum::response::Response::builder().status(status);
            if let Some(content_type) = content_type {
                builder = builder.header("content-type", content_type);
            }
            builder.body(axum::body::Body::from(body)).unwrap()
        }

        let stub = Arc::new(ControlPlaneStub {
            requests: Mutex::new(Vec::new()),
            response: Mutex::new((status, content_type.map(str::to_string), body)),
        });
        let app = axum::Router::new()
            .fallback(handle)
            .with_state(Arc::clone(&stub));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind stub");
        let addr = listener.local_addr().expect("stub addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let http = Arc::new(
            ControlPlaneHttpClient::new(format!("http://{addr}"), "internal-token").unwrap(),
        );
        (stub, CollaborationClient::new(http))
    }

    #[tokio::test]
    async fn replan_run_posts_the_node_to_run_replan() {
        let (stub, client) = control_plane_stub(
            200,
            Some("application/json"),
            br#"{"accepted":true}"#.to_vec(),
        )
        .await;
        let node = serde_json::json!({ "nodeId": "n-2", "kind": "agent" });
        let value = client
            .replan_run("task 1", "task-token", node.clone())
            .await
            .unwrap();
        assert_eq!(value, serde_json::json!({ "accepted": true }));

        let request = stub.only_request();
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.path, "/api/v1/agent-tasks/task%201/run/replan",
            "task ids must be percent-encoded"
        );
        assert_eq!(request.header(TASK_TOKEN_HEADER), Some("task-token"));
        assert_eq!(
            request.header(crate::transport::INTERNAL_TOKEN_HEADER),
            Some("internal-token")
        );
        assert_eq!(request.json(), node);
    }

    #[tokio::test]
    async fn signal_run_posts_the_idempotency_key_and_defaults_a_missing_payload() {
        let (stub, client) =
            control_plane_stub(202, Some("application/json"), b"{}".to_vec()).await;
        client
            .signal_run(
                "t-1",
                "task-token",
                "pause now",
                "idem-7",
                Some(serde_json::json!({ "at": 3 })),
            )
            .await
            .unwrap();
        let first = stub.requests()[0].clone();
        assert_eq!(first.method, "POST");
        assert_eq!(
            first.path,
            "/api/v1/agent-tasks/t-1/run/signals/pause%20now"
        );
        assert_eq!(
            first.json(),
            serde_json::json!({ "idempotencyKey": "idem-7", "payload": { "at": 3 } })
        );

        // A missing payload must be sent as `{}` (Java `payload == null ? Map.of() : payload`).
        let (_stub, client) =
            control_plane_stub(202, Some("application/json"), b"{}".to_vec()).await;
        client
            .signal_run("t-1", "task-token", "pause", "idem-8", None)
            .await
            .unwrap();
        assert_eq!(
            _stub.only_request().json(),
            serde_json::json!({ "idempotencyKey": "idem-8", "payload": {} })
        );
    }

    #[tokio::test]
    async fn upload_artifact_posts_a_multipart_form_with_binary_content() {
        let (stub, client) = control_plane_stub(
            201,
            Some("application/json"),
            br#"{"artifactId":"art-1"}"#.to_vec(),
        )
        .await;
        // 0xFF / 0xFE are not valid UTF-8: the body must survive byte-for-byte.
        let content = vec![0xFFu8, 0x00, 0xFE, 0x01];
        let value = client
            .upload_artifact(
                "t-1",
                "task-token",
                "re\"port\n.bin",
                Some("application/pdf"),
                &content,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(value, serde_json::json!({ "artifactId": "art-1" }));

        let request = stub.only_request();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/v1/artifacts/uploads");
        assert_eq!(request.header(TASK_TOKEN_HEADER), Some("task-token"));
        let content_type = request.header("content-type").expect("content-type");
        let boundary = content_type
            .strip_prefix("multipart/form-data; boundary=")
            .expect("multipart content type");
        assert!(boundary.starts_with("controlplane-"), "{boundary}");

        let body = String::from_utf8_lossy(&request.body).into_owned();
        assert!(body.contains(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"sourceTaskId\"\r\n\r\nt-1\r\n"
        )));
        // Java defaults: targetType=agent-task, targetRef=taskId.
        assert!(body.contains("name=\"targetType\"\r\n\r\nagent-task\r\n"));
        assert!(body.contains("name=\"targetRef\"\r\n\r\nt-1\r\n"));
        // Quote / CR / LF in the filename are neutralised.
        assert!(body.contains("filename=\"re_port_.bin\""), "{body}");
        assert!(body.contains("Content-Type: application/pdf"));
        assert!(body.ends_with(&format!("\r\n--{boundary}--\r\n")));
        // Binary content is intact, not lossily decoded.
        assert!(
            request
                .body
                .windows(content.len())
                .any(|w| w == content.as_slice()),
            "binary payload must be copied verbatim"
        );
    }

    #[tokio::test]
    async fn upload_artifact_maps_error_statuses() {
        let (_stub, client) = control_plane_stub(
            409,
            Some("application/json"),
            br#"{"error":"conflict"}"#.to_vec(),
        )
        .await;
        let err = client
            .upload_artifact("t-1", "tok", "a.bin", None, b"x", None, None)
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("artifact.upload failed"), "{message}");
        assert!(message.contains("409"), "{message}");
        assert!(message.contains("conflict"), "{message}");
    }

    #[tokio::test]
    async fn download_artifact_round_trips_non_utf8_bytes_with_the_content_type() {
        let payload = vec![0xFFu8, 0xFE, 0x00, 0x80, 0x7F];
        let (stub, client) =
            control_plane_stub(200, Some("application/octet-stream"), payload.clone()).await;
        let artifact = client
            .download_artifact("art-1", "t 1", "task-token")
            .await
            .unwrap();
        assert_eq!(artifact.bytes, payload);
        assert_eq!(artifact.content_type, "application/octet-stream");

        let request = stub.only_request();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/v1/artifacts/art-1/download?taskId=t+1");
        assert_eq!(request.header(TASK_TOKEN_HEADER), Some("task-token"));
        assert!(request.body.is_empty(), "download must not send a body");
        assert_eq!(request.header("content-type"), None);
    }

    #[tokio::test]
    async fn download_artifact_defaults_the_content_type_and_surfaces_errors() {
        let (_stub, client) = control_plane_stub(200, None, vec![1, 2, 3]).await;
        let artifact = client.download_artifact("a", "t", "tok").await.unwrap();
        assert_eq!(artifact.content_type, DEFAULT_ARTIFACT_CONTENT_TYPE);
        assert_eq!(artifact.bytes, vec![1, 2, 3]);

        let (_stub, client) =
            control_plane_stub(404, Some("text/plain"), b"artifact missing".to_vec()).await;
        let err = client.download_artifact("a", "t", "tok").await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("artifact.download failed"), "{message}");
        assert!(message.contains("404"), "{message}");
        assert!(message.contains("artifact missing"), "{message}");
    }

    #[tokio::test]
    async fn download_artifact_percent_encodes_the_artifact_id() {
        let (_stub, client) = control_plane_stub(200, None, vec![]).await;
        client
            .download_artifact("a/b c", "t-1", "tok")
            .await
            .unwrap();
        assert_eq!(
            _stub.only_request().path,
            "/api/v1/artifacts/a%2Fb%20c/download?taskId=t-1"
        );
    }
}
