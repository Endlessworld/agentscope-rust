//! Private ACP extension methods (`_agentscope/...`).
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

use agent_client_protocol::{JsonRpcMessage, JsonRpcRequest, UntypedMessage};
use agentscope_harness::agent::SessionAnswer;
use serde::{Deserialize, Serialize};

/// Answers one or more pending interactions of a session and continues its turn.
///
/// Params: `{ "sessionId": "...", "answers": { "<requestId>": { ... } } }` where each answer
/// carries exactly one of:
/// - `output` — free-form text for an `external_execution` request;
/// - `approved` (+ optional `reason`) — an approve/deny answer for a `confirmation` request.
///
/// The response is an empty object; a rejected answer comes back as a JSON-RPC error.
pub const RESPOND_METHOD: &str = "_agentscope/session/respond";

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
    /// Converts the wire answer into the harness answer, rejecting an empty one.
    pub fn to_session_answer(self) -> Result<SessionAnswer, String> {
        match (self.output, self.approved) {
            (Some(output), None) => Ok(SessionAnswer::text(output)),
            (None, Some(true)) => Ok(SessionAnswer::approve()),
            (None, Some(false)) => Ok(SessionAnswer::reject(
                self.reason.unwrap_or_else(|| "rejected".to_string()),
            )),
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
    pub fn to_answers(&self) -> Result<BTreeMap<String, SessionAnswer>, String> {
        if self.answers.is_empty() {
            return Err("an answer is required".to_string());
        }
        let mut converted = BTreeMap::new();
        for (request_id, answer) in &self.answers {
            converted.insert(
                request_id.clone(),
                answer.clone().to_session_answer().map_err(|error| {
                    format!("invalid answer for request '{request_id}': {error}")
                })?,
            );
        }
        Ok(converted)
    }
}

/// An incoming `_agentscope/session/respond` request.
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
        params: &impl Serialize,
    ) -> Result<Self, agent_client_protocol::Error> {
        if method != RESPOND_METHOD {
            return Err(agent_client_protocol::Error::method_not_found());
        }
        let params: ExtRespondParams =
            serde_json::from_value(serde_json::to_value(params).map_err(|error| {
                agent_client_protocol::Error::invalid_params().data(error.to_string())
            })?)
            .map_err(|error| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_method_is_namespaced_as_an_extension() {
        // ACP routes `_`-prefixed methods to extensions; anything else would collide with the
        // standard surface (and could never be delivered by the SDK).
        assert!(RESPOND_METHOD.starts_with('_'));
        assert!(ExtRespondRequest::matches_method(RESPOND_METHOD));
        assert!(!ExtRespondRequest::matches_method("session/prompt"));
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
        assert_eq!(answers.get("req-1"), Some(&SessionAnswer::approve()));
    }

    #[test]
    fn a_denial_without_a_reason_still_answers() {
        let answer = ExtAnswer {
            output: None,
            approved: Some(false),
            reason: None,
        };
        assert_eq!(
            answer.to_session_answer().unwrap(),
            SessionAnswer::reject("rejected")
        );
    }

    #[test]
    fn an_ambiguous_answer_is_rejected() {
        let answer = ExtAnswer {
            output: Some("text".into()),
            approved: Some(true),
            reason: None,
        };
        assert!(answer.to_session_answer().is_err());
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
}
