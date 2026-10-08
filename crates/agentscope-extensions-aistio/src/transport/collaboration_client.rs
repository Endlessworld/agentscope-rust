//! Collaboration client, mirroring
//! `io.agentscope.extensions.aistio.transport.CollaborationClient`.
//!
//! Issue/Comment/AgentTask client shared by external-agent runtimes. Every call carries
//! the task-scoped token in the `X-Agent-Task-Token` header (the control-plane internal
//! token rides on the underlying [`ControlPlaneHttpClient`]).

use std::sync::Arc;
use std::time::Duration;

use reqwest::Method;
use serde_json::Value;

use crate::error::{AistioError, AistioResult};
use crate::transport::control_plane_http_client::{encode_path_segment, ControlPlaneHttpClient};

/// Task-scoped token header.
pub const TASK_TOKEN_HEADER: &str = "X-Agent-Task-Token";

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
    ) -> AistioResult<Value>;

    /// Polls the decision; `decisionVersion > 0` means it is final.
    async fn poll_runtime_approval(
        &self,
        task_id: &str,
        token: &str,
        approval_id: &str,
    ) -> AistioResult<Value>;

    /// Acknowledges the final decision so the control plane stops re-delivering it.
    async fn ack_runtime_approval(
        &self,
        task_id: &str,
        token: &str,
        approval_id: &str,
        decision_version: i64,
    ) -> AistioResult<Value>;
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
) -> AistioResult<RuntimeApprovalDecision> {
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
        return Err(AistioError::Unsupported(
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
    pub async fn issue(&self, issue_id: &str, token: &str) -> AistioResult<Value> {
        self.send(Method::GET, &format!("/api/v1/issues/{}", seg(issue_id)), None, token, "issue.get")
            .await
    }

    /// `GET /api/v1/issues/{id}/comments` (`issue.comment.list`).
    pub async fn comments(&self, issue_id: &str, token: &str) -> AistioResult<Value> {
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
    ) -> AistioResult<Value> {
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
    pub async fn team(&self, team_id: &str, token: &str) -> AistioResult<Value> {
        self.send(Method::GET, &format!("/api/v1/teams/{}", seg(team_id)), None, token, "team.get")
            .await
    }

    /// `POST /api/v1/approvals` (`approval.request`).
    pub async fn request_approval(&self, token: &str, request: Value) -> AistioResult<Value> {
        self.send(Method::POST, "/api/v1/approvals", Some(request), token, "approval.request")
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
    ) -> AistioResult<RuntimeApprovalDecision> {
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
    pub async fn task_context(&self, task_id: &str, token: &str) -> AistioResult<Value> {
        self.task_send(Method::GET, task_id, "context", token, None, "task.context")
            .await
    }

    /// Lists the canonical task-scoped collaboration tools (`tools/list`).
    pub async fn tools(&self, task_id: &str, token: &str) -> AistioResult<Value> {
        let response = self
            .mcp(token, "tools/list", serde_json::json!({ "taskId": task_id }))
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
    ) -> AistioResult<Value> {
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
    ) -> AistioResult<Value> {
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
    ) -> AistioResult<Value> {
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
    ) -> AistioResult<Value> {
        let body = serde_json::json!({ "expectedVersion": expected_version });
        self.task_send(Method::POST, task_id, "start", token, Some(body), "task.start")
            .await
    }

    /// `POST /api/v1/agent-tasks/{id}/progress` (`task.progress`).
    pub async fn progress(&self, task_id: &str, token: &str, content: &str) -> AistioResult<Value> {
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
    pub async fn respond(&self, task_id: &str, token: &str, content: &str) -> AistioResult<Value> {
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
    ) -> AistioResult<Value> {
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
    ) -> AistioResult<Value> {
        let body = serde_json::json!({
            "expectedVersion": expected_version,
            "summary": summary,
            "result": result,
            "processedInputIds": processed_input_ids,
            "deferredInputIds": deferred_input_ids,
        });
        self.task_send(Method::POST, task_id, "complete", token, Some(body), "task.complete")
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
    ) -> AistioResult<Value> {
        let body = serde_json::json!({
            "expectedVersion": expected_version,
            "outcome": outcome,
            "summary": reason,
            "result": result,
            "processedInputIds": processed_input_ids,
            "deferredInputIds": deferred_input_ids,
        });
        self.task_send(Method::POST, task_id, "complete", token, Some(body), "task.complete")
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
    ) -> AistioResult<Value> {
        let body = serde_json::json!({
            "expectedVersion": expected_version,
            "code": if code.is_empty() { "agent_failed" } else { code },
            "message": message,
        });
        self.task_send(Method::POST, task_id, "fail", token, Some(body), "task.fail")
            .await
    }

    /// `GET /api/v1/agent-tasks/{id}/run` (`run.get`).
    pub async fn run(&self, task_id: &str, token: &str) -> AistioResult<Value> {
        self.task_send(Method::GET, task_id, "run", token, None, "run.get")
            .await
    }

    /// `GET /api/v1/agent-tasks/{id}/run/graph` (`run.graph`).
    pub async fn run_graph(&self, task_id: &str, token: &str) -> AistioResult<Value> {
        self.task_send(Method::GET, task_id, "run/graph", token, None, "run.graph")
            .await
    }

    /// `POST /api/v1/agent-tasks/{id}/run/node/complete` (`run.node.complete`).
    pub async fn complete_run_node(
        &self,
        task_id: &str,
        token: &str,
        output: Value,
    ) -> AistioResult<Value> {
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
    ) -> AistioResult<Value> {
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
    pub async fn run_artifacts(&self, task_id: &str, token: &str) -> AistioResult<Value> {
        self.task_send(Method::GET, task_id, "run/artifacts", token, None, "run.artifacts")
            .await
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
    ) -> AistioResult<Value> {
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
    async fn mcp(&self, token: &str, method: &str, params: Value) -> AistioResult<Value> {
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
    ) -> AistioResult<Value> {
        let headers = [(TASK_TOKEN_HEADER, token)];
        let response = self
            .http
            .send_with_headers(method, path, body, &headers)
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(AistioError::Http(format!(
                "{operation} failed: HTTP {} {}",
                response.status, response.body
            )));
        }
        if response.body.trim().is_empty() {
            return Ok(serde_json::json!({}));
        }
        serde_json::from_str(&response.body)
            .map_err(|e| AistioError::Http(format!("{operation} returned invalid JSON: {e}")))
    }
}

#[async_trait::async_trait]
impl RuntimeApprovalSource for CollaborationClient {
    async fn create_runtime_approval(
        &self,
        task_id: &str,
        token: &str,
        request: Value,
    ) -> AistioResult<Value> {
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
    ) -> AistioResult<Value> {
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
    ) -> AistioResult<Value> {
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
        ) -> AistioResult<Value> {
            Ok(self.created.clone())
        }

        async fn poll_runtime_approval(
            &self,
            _task_id: &str,
            _token: &str,
            _approval_id: &str,
        ) -> AistioResult<Value> {
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
        ) -> AistioResult<Value> {
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
            vec![serde_json::json!({ "decisionVersion": 1, "allow": false, "denyMessage": "nope" })],
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
}
