//! Private ACP extension methods (`_acp_extends/...`).
//!
//! ACP has no standard way for a client to **answer** an outstanding interaction or to
//! continue an unfinished turn: `session/request_permission` and `elicitation/create` only run
//! agent → client. A suspended tool therefore had no protocol entry at all — the answer path
//! existed as an API (`AcpAgentSession::respond`) but nothing on the wire could reach it.
//!
//! Extension method names must start with `_` (ACP extensibility rules), so this is registered
//! as a narrow, explicitly-namespaced request instead of widening the standard surface.

use std::collections::BTreeMap;
use std::sync::Arc;

use agent_client_protocol::{JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, UntypedMessage};
use serde::{Deserialize, Serialize};

/// Implements [`JsonRpcResponse`] for a result type through serde.
///
/// The SDK implements the trait for `serde_json::Value` only, so a typed result has to say how
/// it crosses the wire. Every result here is a plain serializable struct, which is the same
/// shape the derive macro would generate.
macro_rules! wire_response {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl JsonRpcResponse for $ty {
                fn into_json(
                    self,
                    _method: &str,
                ) -> Result<serde_json::Value, agent_client_protocol::Error> {
                    serde_json::to_value(self).map_err(|error| {
                        agent_client_protocol::Error::internal_error().data(error.to_string())
                    })
                }

                fn from_json(
                    _method: &str,
                    value: serde_json::Value,
                ) -> Result<Self, agent_client_protocol::Error> {
                    serde_json::from_value(value).map_err(|error| {
                        agent_client_protocol::Error::invalid_params().data(error.to_string())
                    })
                }
            }
        )+
    };
}

/// Answers one or more pending interactions of a session and continues its turn.
///
/// Params: `{ "sessionId": "...", "answers": { "<requestId>": { ... } } }` where each answer
/// carries exactly one of:
/// - `output` — free-form text for an `external_execution` request;
/// - `approved` (+ optional `reason`) — an approve/deny answer for a `confirmation` request.
///
/// The response is an empty object; a rejected answer comes back as a JSON-RPC error.
pub const RESPOND_METHOD: &str = "_acp_extends/session/respond";

/// One answer as it appears on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExtAnswer {
    /// Free-form output for an `external_execution` request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Approve/deny answer for a `confirmation` request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved: Option<bool>,
    /// Reason shown to the model when the confirmation is denied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ExtAnswer {
    /// Folds the wire answer into the text the model receives, rejecting an empty one.
    ///
    /// A durable signal is delivered to the model as text, so an answer is folded the same
    /// way: what the user said, or the decision they made.
    pub fn to_answer_text(self) -> Result<String, String> {
        match (self.output, self.approved) {
            (Some(output), None) => Ok(output),
            (None, Some(true)) => Ok("approved".to_string()),
            (None, Some(false)) => Ok(self.reason.unwrap_or_else(|| "rejected".to_string())),
            (Some(_), Some(_)) => {
                Err("an answer must carry either `output` or `approved`, not both".to_string())
            }
            (None, None) => Err("an answer must carry either `output` or `approved`".to_string()),
        }
    }
}

/// Parameters of [`RESPOND_METHOD`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExtRespondParams {
    /// Session the answers belong to.
    pub session_id: String,
    /// One answer per pending request id.
    pub answers: BTreeMap<String, ExtAnswer>,
}

impl ExtRespondParams {
    /// Converts every answer, failing on the first invalid one.
    pub fn to_answers(&self) -> Result<BTreeMap<String, String>, String> {
        if self.answers.is_empty() {
            return Err("an answer is required".to_string());
        }
        let mut converted = BTreeMap::new();
        for (request_id, answer) in &self.answers {
            converted.insert(
                request_id.clone(),
                answer.clone().to_answer_text().map_err(|error| {
                    format!("invalid answer for request '{request_id}': {error}")
                })?,
            );
        }
        Ok(converted)
    }
}

/// Declares a request type whose response is a plain serializable struct.
macro_rules! impl_ext_request {
    ($request:ty, $params:ty, $method:path, $response:ty) => {
        impl JsonRpcMessage for $request {
            fn matches_method(method: &str) -> bool {
                method == $method
            }

            fn method(&self) -> &str {
                &self.method
            }

            fn to_untyped_message(&self) -> Result<UntypedMessage, agent_client_protocol::Error> {
                UntypedMessage::new(self.method(), &self.params)
            }

            fn parse_message(
                method: &str,
                params: &serde_json::Value,
            ) -> Result<Self, agent_client_protocol::Error> {
                if method != $method {
                    return Err(agent_client_protocol::Error::method_not_found());
                }
                let params: $params = serde_json::from_value(params.clone()).map_err(|error| {
                    agent_client_protocol::Error::invalid_params().data(error.to_string())
                })?;
                Ok(Self {
                    method: Arc::from(method),
                    params,
                })
            }
        }

        impl JsonRpcRequest for $request {
            type Response = $response;
        }
    };
}

/// An incoming `_acp_extends/session/respond` request.
#[derive(Debug, Clone)]
pub struct ExtRespondRequest {
    method: Arc<str>,
    pub params: ExtRespondParams,
}

impl JsonRpcMessage for ExtRespondRequest {
    fn matches_method(method: &str) -> bool {
        method == RESPOND_METHOD
    }

    fn method(&self) -> &str {
        &self.method
    }

    fn to_untyped_message(&self) -> Result<UntypedMessage, agent_client_protocol::Error> {
        UntypedMessage::new(self.method(), &self.params)
    }

    fn parse_message(
        method: &str,
        params: &serde_json::Value,
    ) -> Result<Self, agent_client_protocol::Error> {
        if method != RESPOND_METHOD {
            return Err(agent_client_protocol::Error::method_not_found());
        }
        let params: ExtRespondParams = serde_json::from_value(params.clone()).map_err(|error| {
            agent_client_protocol::Error::invalid_params().data(error.to_string())
        })?;
        Ok(Self {
            method: Arc::from(method),
            params,
        })
    }
}

impl JsonRpcRequest for ExtRespondRequest {
    type Response = serde_json::Value;
}

/// Folds one message into the turn a session is executing.
///
/// Params: `{ "sessionId": "...", "text": "...", "clientRequestId": "..." }`. The answer names
/// the turn the message was folded into, so a client that already rendered it can bind the
/// output that follows.
pub const STEER_METHOD: &str = "_acp_extends/session/steer";

/// Error token: the session has no turn a message could be folded into.
pub const NO_ACTIVE_TURN: &str = "no_active_turn";

/// Parameters of [`STEER_METHOD`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExtSteerParams {
    /// Session whose active turn receives the message.
    pub session_id: String,
    /// What the user wants adjusted; folded in as one user message at the next boundary.
    pub text: String,
    /// Idempotency key: a retried steer reaches the same signal, never the model twice.
    pub client_request_id: String,
}

impl ExtSteerParams {
    /// Rejects an empty adjustment before it reaches the durable layer.
    pub fn validate(&self) -> Result<(), String> {
        if self.text.trim().is_empty() {
            return Err("a steer needs text".to_string());
        }
        if self.client_request_id.trim().is_empty() {
            return Err("a steer needs a clientRequestId".to_string());
        }
        Ok(())
    }
}

/// Response of [`STEER_METHOD`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExtSteerResult {
    /// The turn the message was folded into.
    pub turn_id: String,
}

wire_response!(ExtSteerResult);

/// An incoming `_acp_extends/session/steer` request.
#[derive(Debug, Clone)]
pub struct ExtSteerRequest {
    method: Arc<str>,
    pub params: ExtSteerParams,
}

impl JsonRpcMessage for ExtSteerRequest {
    fn matches_method(method: &str) -> bool {
        method == STEER_METHOD
    }

    fn method(&self) -> &str {
        &self.method
    }

    fn to_untyped_message(&self) -> Result<UntypedMessage, agent_client_protocol::Error> {
        UntypedMessage::new(self.method(), &self.params)
    }

    fn parse_message(
        method: &str,
        params: &serde_json::Value,
    ) -> Result<Self, agent_client_protocol::Error> {
        if method != STEER_METHOD {
            return Err(agent_client_protocol::Error::method_not_found());
        }
        let params: ExtSteerParams = serde_json::from_value(params.clone()).map_err(|error| {
            agent_client_protocol::Error::invalid_params().data(error.to_string())
        })?;
        Ok(Self {
            method: Arc::from(method),
            params,
        })
    }
}

impl JsonRpcRequest for ExtSteerRequest {
    type Response = ExtSteerResult;
}

/// Queues a message as its own turn and answers before it runs.
///
/// Params: `{ "sessionId": "...", "content": [ ... ], "clientRequestId": "..." }`. The answer
/// names the admitted turn and its place among the session's unfinished turns, so a client can
/// render the bubble as "queued #2" without waiting for the turn to run.
pub const QUEUE_METHOD: &str = "_acp_extends/session/queue";

/// Lists the session's unfinished turns in the order the runtime will run them.
///
/// Params: `{ "sessionId": "..." }`; the answer is `{ "items": [ { "turnId", "position",
/// "status" } ] }`. The turn's input is not echoed: the client queued it and still holds it,
/// and the durable store keeps the input for the run, not for listing.
pub const QUEUE_LIST_METHOD: &str = "_acp_extends/session/queue/list";

/// Removes a queued turn before it runs.
///
/// Params: `{ "sessionId": "...", "turnId": "...", "clientRequestId": "..." }`. A turn that
/// already started is not removed — stopping the running turn is `session/cancel`.
pub const QUEUE_REMOVE_METHOD: &str = "_acp_extends/session/queue/remove";

/// Parameters of [`QUEUE_METHOD`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExtQueueParams {
    /// Session the queued turn belongs to.
    pub session_id: String,
    /// The message, in the same block form `session/prompt` accepts.
    pub content: Vec<agent_client_protocol::v1::schema::ContentBlock>,
    /// Idempotency key: a retried queue reaches the same turn, never a second one.
    pub client_request_id: String,
}

impl ExtQueueParams {
    /// Rejects an unusable message before it reaches the durable layer.
    pub fn validate(&self) -> Result<(), String> {
        if self.content.is_empty() {
            return Err("a queued message needs content".to_string());
        }
        if self.client_request_id.trim().is_empty() {
            return Err("a queued message needs a clientRequestId".to_string());
        }
        Ok(())
    }
}

/// Response of [`QUEUE_METHOD`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExtQueueResult {
    /// The turn the message was admitted as.
    pub turn_id: String,
    /// Its place among the session's unfinished turns; `1` is the next to run.
    pub position: u32,
}

/// An incoming `_acp_extends/session/queue` request.
#[derive(Debug, Clone)]
pub struct ExtQueueRequest {
    method: Arc<str>,
    pub params: ExtQueueParams,
}

impl JsonRpcMessage for ExtQueueRequest {
    fn matches_method(method: &str) -> bool {
        method == QUEUE_METHOD
    }

    fn method(&self) -> &str {
        &self.method
    }

    fn to_untyped_message(&self) -> Result<UntypedMessage, agent_client_protocol::Error> {
        UntypedMessage::new(self.method(), &self.params)
    }

    fn parse_message(
        method: &str,
        params: &serde_json::Value,
    ) -> Result<Self, agent_client_protocol::Error> {
        if method != QUEUE_METHOD {
            return Err(agent_client_protocol::Error::method_not_found());
        }
        let params: ExtQueueParams = serde_json::from_value(params.clone()).map_err(|error| {
            agent_client_protocol::Error::invalid_params().data(error.to_string())
        })?;
        Ok(Self {
            method: Arc::from(method),
            params,
        })
    }
}

impl JsonRpcRequest for ExtQueueRequest {
    type Response = ExtQueueResult;
}

/// Parameters of [`QUEUE_LIST_METHOD`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExtQueueListParams {
    /// Session whose queue is listed.
    pub session_id: String,
}

/// One unfinished turn as [`QUEUE_LIST_METHOD`] reports it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExtQueueItem {
    pub turn_id: String,
    /// Its place in the queue; `1` is the next to run.
    pub position: u32,
    /// The durable status: `queued`, `running`, `waiting`, `pausing`, `paused`, `cancelling`.
    pub status: String,
}

/// Response of [`QUEUE_LIST_METHOD`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExtQueueListResult {
    pub items: Vec<ExtQueueItem>,
}

/// An incoming `_acp_extends/session/queue/list` request.
#[derive(Debug, Clone)]
pub struct ExtQueueListRequest {
    method: Arc<str>,
    pub params: ExtQueueListParams,
}

/// Parameters of [`QUEUE_REMOVE_METHOD`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExtQueueRemoveParams {
    /// Session the queued turn belongs to.
    pub session_id: String,
    /// The turn to remove.
    pub turn_id: String,
    /// Idempotency key: a retried removal reaches the same command.
    pub client_request_id: String,
}

/// Response of [`QUEUE_REMOVE_METHOD`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExtQueueRemoveResult {
    pub turn_id: String,
    /// `false` when the turn had already started or settled, so nothing was removed.
    pub removed: bool,
}

/// An incoming `_acp_extends/session/queue/remove` request.
#[derive(Debug, Clone)]
pub struct ExtQueueRemoveRequest {
    method: Arc<str>,
    pub params: ExtQueueRemoveParams,
}

impl_ext_request!(
    ExtQueueListRequest,
    ExtQueueListParams,
    QUEUE_LIST_METHOD,
    ExtQueueListResult
);
impl_ext_request!(
    ExtQueueRemoveRequest,
    ExtQueueRemoveParams,
    QUEUE_REMOVE_METHOD,
    ExtQueueRemoveResult
);

wire_response!(ExtQueueResult, ExtQueueListResult, ExtQueueRemoveResult);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_method_is_namespaced_as_an_extension() {
        // ACP routes `_`-prefixed methods to extensions; anything else would collide with the
        // standard surface (and could never be delivered by the SDK).
        assert!(RESPOND_METHOD.starts_with("_acp_extends/"));
        assert!(ExtRespondRequest::matches_method(RESPOND_METHOD));
        assert!(!ExtRespondRequest::matches_method("session/prompt"));
    }

    #[test]
    fn the_steer_method_uses_our_namespace() {
        assert!(STEER_METHOD.starts_with("_acp_extends/"));
        assert!(ExtSteerRequest::matches_method(STEER_METHOD));
        assert!(!ExtSteerRequest::matches_method("session/prompt"));
    }

    #[test]
    fn steer_params_round_trip_through_the_wire_format() {
        let params = json!({
            "sessionId": "session-1",
            "text": "keep the old API",
            "clientRequestId": "steer-1"
        });
        let request = ExtSteerRequest::parse_message(STEER_METHOD, &params).unwrap();
        assert_eq!(request.params.session_id, "session-1");
        assert_eq!(request.params.client_request_id, "steer-1");
        request.params.validate().unwrap();
    }

    #[test]
    fn an_empty_steer_is_rejected_before_the_store_sees_it() {
        let params = ExtSteerParams {
            session_id: "session-1".into(),
            text: "   ".into(),
            client_request_id: "steer-1".into(),
        };
        assert!(params.validate().is_err());
    }

    #[test]
    fn a_steer_without_an_idempotency_key_is_rejected() {
        let params = ExtSteerParams {
            session_id: "session-1".into(),
            text: "keep the old API".into(),
            client_request_id: "  ".into(),
        };
        assert!(params.validate().is_err());
    }

    #[test]
    fn params_round_trip_through_the_wire_format() {
        let params = json!({
            "sessionId": "session-1",
            "answers": { "req-1": { "approved": true } }
        });
        let request = ExtRespondRequest::parse_message(RESPOND_METHOD, &params).unwrap();
        assert_eq!(request.method(), RESPOND_METHOD);
        assert_eq!(request.params.session_id, "session-1");
        let answers = request.params.to_answers().unwrap();
        assert_eq!(answers.get("req-1"), Some(&"approved".to_string()));
    }

    #[test]
    fn a_denial_without_a_reason_still_answers() {
        let answer = ExtAnswer {
            output: None,
            approved: Some(false),
            reason: None,
        };
        assert_eq!(answer.to_answer_text().unwrap(), "rejected");
    }

    #[test]
    fn an_ambiguous_answer_is_rejected() {
        let answer = ExtAnswer {
            output: Some("text".into()),
            approved: Some(true),
            reason: None,
        };
        assert!(answer.to_answer_text().is_err());
    }

    #[test]
    fn an_empty_answer_map_is_rejected() {
        let params = ExtRespondParams {
            session_id: "session-1".into(),
            answers: BTreeMap::new(),
        };
        assert!(params.to_answers().is_err());
    }

    #[test]
    fn another_method_is_not_parsed() {
        let params = json!({ "sessionId": "s", "answers": {} });
        assert!(ExtRespondRequest::parse_message("session/prompt", &params).is_err());
    }

    #[test]
    fn the_queue_methods_use_our_namespace() {
        assert!(QUEUE_METHOD.starts_with("_acp_extends/"));
        assert!(QUEUE_LIST_METHOD.starts_with("_acp_extends/"));
        assert!(QUEUE_REMOVE_METHOD.starts_with("_acp_extends/"));
        assert!(ExtQueueRequest::matches_method(QUEUE_METHOD));
        assert!(ExtQueueListRequest::matches_method(QUEUE_LIST_METHOD));
        assert!(ExtQueueRemoveRequest::matches_method(QUEUE_REMOVE_METHOD));
        assert!(!ExtQueueRequest::matches_method("session/prompt"));
    }

    #[test]
    fn queue_params_round_trip_through_the_wire_format() {
        let params = json!({
            "sessionId": "session-1",
            "content": [{ "type": "text", "text": "run the tests" }],
            "clientRequestId": "queue-1"
        });
        let request = ExtQueueRequest::parse_message(QUEUE_METHOD, &params).unwrap();
        assert_eq!(request.params.session_id, "session-1");
        assert_eq!(request.params.client_request_id, "queue-1");
        assert_eq!(request.params.content.len(), 1);
        request.params.validate().unwrap();
    }

    #[test]
    fn an_unusable_queued_message_is_rejected_before_the_store_sees_it() {
        let content: Vec<agent_client_protocol::v1::schema::ContentBlock> =
            serde_json::from_value(json!([{ "type": "text", "text": "run the tests" }]))
                .expect("one text block");
        let empty = ExtQueueParams {
            session_id: "session-1".into(),
            content: Vec::new(),
            client_request_id: "queue-1".into(),
        };
        assert!(empty.validate().is_err());
        let unnamed = ExtQueueParams {
            session_id: "session-1".into(),
            content,
            client_request_id: "  ".into(),
        };
        assert!(unnamed.validate().is_err());
    }

    #[test]
    fn a_queue_answer_names_the_turn_and_its_place() {
        let value = ExtQueueResult {
            turn_id: "turn_1".into(),
            position: 2,
        }
        .into_json(QUEUE_METHOD)
        .unwrap();
        assert_eq!(value, json!({ "turnId": "turn_1", "position": 2 }));
    }

    #[test]
    fn a_queue_list_answer_carries_the_items() {
        let value = ExtQueueListResult {
            items: vec![ExtQueueItem {
                turn_id: "turn_1".into(),
                position: 1,
                status: "queued".into(),
            }],
        }
        .into_json(QUEUE_LIST_METHOD)
        .unwrap();
        assert_eq!(
            value,
            json!({ "items": [{ "turnId": "turn_1", "position": 1, "status": "queued" }] })
        );
    }

    #[test]
    fn a_queue_removal_answer_says_what_happened() {
        let value = ExtQueueRemoveResult {
            turn_id: "turn_1".into(),
            removed: false,
        }
        .into_json(QUEUE_REMOVE_METHOD)
        .unwrap();
        assert_eq!(value, json!({ "turnId": "turn_1", "removed": false }));
    }
}
