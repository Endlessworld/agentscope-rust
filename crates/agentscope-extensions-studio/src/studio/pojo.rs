//! Request/response POJOs for the Studio REST API, mirroring `studio.pojo`.
use serde::{Deserialize, Serialize};

use agentscope_core::message::{JsonMap, Msg};

/// Request payload for registering a run with Studio (`POST /trpc/registerRun`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegisterRunRequest {
    /// Unique identifier for this run.
    pub id: String,
    /// Project name.
    pub project: String,
    /// Run name.
    pub name: String,
    /// Creation timestamp (`yyyy-MM-dd HH:mm:ss.SSS`).
    pub timestamp: String,
    /// Process id of the running application.
    pub pid: u64,
    /// Run status (e.g. `running`).
    pub status: String,
    /// Run directory (empty by default in Java).
    #[serde(rename = "run_dir")]
    pub run_dir: String,
}

impl RegisterRunRequest {
    /// Creates a new builder.
    pub fn builder() -> RegisterRunRequestBuilder {
        RegisterRunRequestBuilder::default()
    }
}

/// Builder for [`RegisterRunRequest`].
#[derive(Debug, Default, Clone)]
pub struct RegisterRunRequestBuilder {
    id: Option<String>,
    project: Option<String>,
    name: Option<String>,
    timestamp: Option<String>,
    pid: Option<u64>,
    status: Option<String>,
    run_dir: Option<String>,
}

impl RegisterRunRequestBuilder {
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }
    pub fn project(mut self, project: impl Into<String>) -> Self {
        self.project = Some(project.into());
        self
    }
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
    pub fn timestamp(mut self, timestamp: impl Into<String>) -> Self {
        self.timestamp = Some(timestamp.into());
        self
    }
    pub fn pid(mut self, pid: u64) -> Self {
        self.pid = Some(pid);
        self
    }
    pub fn status(mut self, status: impl Into<String>) -> Self {
        self.status = Some(status.into());
        self
    }
    pub fn run_dir(mut self, run_dir: impl Into<String>) -> Self {
        self.run_dir = Some(run_dir.into());
        self
    }

    /// Builds the request, filling in Java-compatible defaults.
    pub fn build(self) -> RegisterRunRequest {
        RegisterRunRequest {
            id: self.id.unwrap_or_default(),
            project: self.project.unwrap_or_default(),
            name: self.name.unwrap_or_default(),
            timestamp: self.timestamp.unwrap_or_default(),
            pid: self.pid.unwrap_or_default(),
            status: self.status.unwrap_or_else(|| "running".to_string()),
            run_dir: self.run_dir.unwrap_or_default(),
        }
    }
}

/// Request payload for pushing a message to Studio (`POST /trpc/pushMessage`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushMessageRequest {
    /// Run id this message belongs to.
    pub run_id: String,
    /// Reply id for this message.
    pub reply_id: String,
    /// Agent name that produced the message.
    pub name: String,
    /// Lowercased role (e.g. `assistant`, `user`).
    pub role: String,
    /// The message body.
    pub msg: Msg,
}

impl PushMessageRequest {
    /// Creates a new builder.
    pub fn builder() -> PushMessageRequestBuilder {
        PushMessageRequestBuilder::default()
    }
}

/// Builder for [`PushMessageRequest`].
#[derive(Debug, Default, Clone)]
pub struct PushMessageRequestBuilder {
    run_id: Option<String>,
    reply_id: Option<String>,
    name: Option<String>,
    role: Option<String>,
    msg: Option<Msg>,
}

impl PushMessageRequestBuilder {
    pub fn run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }
    pub fn reply_id(mut self, reply_id: impl Into<String>) -> Self {
        self.reply_id = Some(reply_id.into());
        self
    }
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
    pub fn role(mut self, role: impl Into<String>) -> Self {
        self.role = Some(role.into());
        self
    }
    pub fn msg(mut self, msg: Msg) -> Self {
        self.msg = Some(msg);
        self
    }

    /// Builds the request; `name` falls back to the generated reply id when empty
    /// (mirrors Java `msg.getName() != null ? msg.getName() : messageId`).
    pub fn build(self) -> PushMessageRequest {
        let reply_id = self.reply_id.unwrap_or_default();
        let name = self
            .name
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| reply_id.clone());
        PushMessageRequest {
            run_id: self.run_id.unwrap_or_default(),
            reply_id,
            name,
            role: self.role.unwrap_or_default(),
            msg: self.msg.unwrap_or_else(|| Msg::user("")),
        }
    }
}

/// Request payload for requesting user input (`POST /trpc/requestUserInput`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestUserInputRequest {
    /// Unique identifier for this user input request.
    pub request_id: String,
    /// Run id associated with the request.
    pub run_id: String,
    /// Agent requesting input.
    pub agent_id: String,
    /// Agent name requesting input.
    pub agent_name: String,
    /// Optional JSON schema for structured input (`null` when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_input: Option<serde_json::Value>,
}

impl RequestUserInputRequest {
    /// Creates a new builder.
    pub fn builder() -> RequestUserInputRequestBuilder {
        RequestUserInputRequestBuilder::default()
    }
}

/// Builder for [`RequestUserInputRequest`].
#[derive(Debug, Default, Clone)]
pub struct RequestUserInputRequestBuilder {
    request_id: Option<String>,
    run_id: Option<String>,
    agent_id: Option<String>,
    agent_name: Option<String>,
    structured_input: Option<serde_json::Value>,
}

impl RequestUserInputRequestBuilder {
    pub fn request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }
    pub fn run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }
    pub fn agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }
    pub fn agent_name(mut self, agent_name: impl Into<String>) -> Self {
        self.agent_name = Some(agent_name.into());
        self
    }
    pub fn structured_input(mut self, value: serde_json::Value) -> Self {
        self.structured_input = Some(value);
        self
    }

    /// Builds the request.
    pub fn build(self) -> RequestUserInputRequest {
        RequestUserInputRequest {
            request_id: self.request_id.unwrap_or_default(),
            run_id: self.run_id.unwrap_or_default(),
            agent_id: self.agent_id.unwrap_or_default(),
            agent_name: self.agent_name.unwrap_or_default(),
            structured_input: self.structured_input,
        }
    }
}

/// Metadata for user input messages from Studio.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputMetadata {
    /// Source identifier (typically `studio`).
    pub source: String,
    /// Request id this input responds to (omitted when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Structured input data (omitted when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_input: Option<JsonMap>,
}

impl Default for UserInputMetadata {
    fn default() -> Self {
        Self {
            source: "studio".to_string(),
            request_id: None,
            structured_input: None,
        }
    }
}

impl UserInputMetadata {
    /// Creates a new builder.
    pub fn builder() -> UserInputMetadataBuilder {
        UserInputMetadataBuilder::default()
    }

    /// Converts this metadata into a plain [`JsonMap`] (for `Msg.metadata`).
    pub fn to_map(&self) -> JsonMap {
        match serde_json::to_value(self) {
            Ok(serde_json::Value::Object(map)) => map,
            _ => JsonMap::new(),
        }
    }
}

/// Builder for [`UserInputMetadata`].
#[derive(Debug, Clone)]
pub struct UserInputMetadataBuilder {
    source: String,
    request_id: Option<String>,
    structured_input: Option<JsonMap>,
}

impl Default for UserInputMetadataBuilder {
    fn default() -> Self {
        Self {
            source: "studio".to_string(),
            request_id: None,
            structured_input: None,
        }
    }
}

impl UserInputMetadataBuilder {
    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }
    pub fn request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }
    pub fn structured_input(mut self, structured_input: JsonMap) -> Self {
        self.structured_input = Some(structured_input);
        self
    }

    /// Builds the metadata.
    pub fn build(self) -> UserInputMetadata {
        UserInputMetadata {
            source: self.source,
            request_id: self.request_id,
            structured_input: self.structured_input,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::Msg;

    #[test]
    fn register_run_request_serde_roundtrip() {
        let req = RegisterRunRequest::builder()
            .id("run-1")
            .project("proj")
            .name("my run")
            .timestamp("2026-09-18 12:00:00.000")
            .pid(4321)
            .status("running")
            .run_dir("/tmp/run")
            .build();
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"run_dir\""));
        let back: RegisterRunRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn push_message_request_uses_camel_case_and_roundtrips() {
        let msg = Msg::assistant("hello");
        let req = PushMessageRequest::builder()
            .run_id("r1")
            .reply_id("rep1")
            .name("Assistant")
            .role("assistant")
            .msg(msg)
            .build();
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["runId"], "r1");
        assert_eq!(value["replyId"], "rep1");
        let back: PushMessageRequest = serde_json::from_value(value).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn push_message_name_falls_back_to_reply_id() {
        let req = PushMessageRequest::builder().reply_id("rep9").build();
        assert_eq!(req.name, "rep9");
    }

    #[test]
    fn request_user_input_omits_absent_structured_input() {
        let req = RequestUserInputRequest::builder()
            .request_id("q1")
            .run_id("r1")
            .agent_id("agent-1")
            .agent_name("Agent")
            .build();
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["requestId"], "q1");
        assert!(value.get("structuredInput").is_none());

        let with_schema = RequestUserInputRequest::builder()
            .request_id("q2")
            .structured_input(serde_json::json!({ "type": "object" }))
            .build();
        let value2 = serde_json::to_value(&with_schema).unwrap();
        assert_eq!(value2["structuredInput"]["type"], "object");
    }

    #[test]
    fn user_input_metadata_roundtrip_and_to_map() {
        let meta = UserInputMetadata::builder()
            .source("studio")
            .request_id("req-1")
            .build();
        let json = serde_json::to_string(&meta).unwrap();
        let back: UserInputMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(meta, back);

        let map = meta.to_map();
        assert_eq!(map["source"], "studio");
        assert_eq!(map["requestId"], "req-1");
        assert_eq!(UserInputMetadata::default().source, "studio");
    }
}
