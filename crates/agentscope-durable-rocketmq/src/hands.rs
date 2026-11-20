//! The Brain–Hands wire envelope: one execution, one kind, optional request/result.

use agentscope_durable::types::{ExecutionRequest, ExecutionResult};
use serde::{Deserialize, Serialize};

/// What a hands message asks for or reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HandsKind {
    /// Brain → Hands: run this execution.
    Request,
    /// Hands → Brain: this is what happened.
    Reply,
    /// Brain → Hands: stop trying to run this execution.
    Cancel,
}

/// One message on a hands channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandsEnvelope {
    pub execution_id: String,
    pub kind: HandsKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<ExecutionRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ExecutionResult>,
    #[serde(default)]
    pub attempt: u32,
}

impl HandsEnvelope {
    pub fn request(request: &ExecutionRequest, attempt: u32) -> Self {
        Self {
            execution_id: request.execution_id.clone(),
            kind: HandsKind::Request,
            request: Some(request.clone()),
            result: None,
            attempt,
        }
    }

    pub fn reply(execution_id: &str, result: &ExecutionResult, attempt: u32) -> Self {
        Self {
            execution_id: execution_id.to_string(),
            kind: HandsKind::Reply,
            request: None,
            result: Some(result.clone()),
            attempt,
        }
    }

    pub fn cancel(execution_id: &str, request: Option<&ExecutionRequest>) -> Self {
        Self {
            execution_id: execution_id.to_string(),
            kind: HandsKind::Cancel,
            request: request.cloned(),
            result: None,
            attempt: 0,
        }
    }

    /// The JSON bytes a [`Message`](crate::Message) carries.
    pub fn encode(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Decode and validate a message body.
    pub fn decode(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        let envelope: Self = serde_json::from_slice(bytes)?;
        envelope.validate().map_err(serde::de::Error::custom)?;
        Ok(envelope)
    }

    fn validate(&self) -> Result<(), String> {
        if self.execution_id.trim().is_empty() {
            return Err("executionId must not be empty".to_string());
        }
        match self.kind {
            HandsKind::Request if self.request.is_none() => {
                Err("a request message must carry a request".to_string())
            }
            HandsKind::Reply if self.result.is_none() => {
                Err("a reply message must carry a result".to_string())
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use agentscope_durable::types::{ExecutionState, SessionKey, ThreadKey, ToolCall};
    use serde_json::json;

    use super::*;

    fn request() -> ExecutionRequest {
        ExecutionRequest::new(
            "exec-1",
            ThreadKey::new(SessionKey::new("ns", "s1").expect("session"), "main").expect("thread"),
            "turn-1",
            ToolCall::new("call-1", "local", "echo", json!({})).expect("call"),
        )
        .expect("request")
    }

    #[test]
    fn a_request_round_trips_through_the_wire() {
        let envelope = HandsEnvelope::request(&request(), 2);
        let bytes = envelope.encode().expect("encode");
        let restored = HandsEnvelope::decode(&bytes).expect("decode");
        assert_eq!(restored, envelope);
        assert_eq!(restored.kind, HandsKind::Request);
        assert_eq!(restored.attempt, 2);
    }

    #[test]
    fn a_reply_round_trips_and_carries_the_result() {
        let envelope = HandsEnvelope::reply(
            "exec-1",
            &ExecutionResult::succeeded(json!({"ok": true})),
            1,
        );
        let restored = HandsEnvelope::decode(&envelope.encode().expect("encode")).expect("decode");
        assert_eq!(restored.kind, HandsKind::Reply);
        assert_eq!(
            restored.result.map(|result| result.state),
            Some(ExecutionState::Succeeded)
        );
    }

    #[test]
    fn a_reply_without_a_result_is_rejected_at_decode() {
        let body = serde_json::to_vec(&json!({
            "executionId": "exec-1",
            "kind": "REPLY"
        }))
        .expect("json");
        assert!(HandsEnvelope::decode(&body).is_err());
    }

    #[test]
    fn an_empty_execution_id_is_rejected() {
        let body = serde_json::to_vec(&json!({
            "executionId": "",
            "kind": "CANCEL"
        }))
        .expect("json");
        assert!(HandsEnvelope::decode(&body).is_err());
    }
}
