//! Message conversion and the ReMe-backed long-term memory adapter.
//!
//! Mirrors `ReMeLongTermMemory` (`record` / `retrieve`).

use std::sync::Arc;

use agentscope_core::message::msg::ContentBlockKind;
use agentscope_core::{Msg, MsgRole};

use crate::error::{MemError, Result};
use crate::mem0::memory::COMPRESSED_HISTORY_MARKER;
use crate::reme::client::ReMeClient;
use crate::reme::dto::{
    ReMeAddRequest, ReMeMessage, ReMeSearchRequest, ReMeSearchResponse, ReMeTrajectory,
};

/// Default `top_k` used by `ReMeLongTermMemory.retrieve`.
pub const DEFAULT_SEARCH_TOP_K: u32 = 5;

/// Maps a core [`MsgRole`] to a ReMe role, mirroring `convertToReMeMessage`.
///
/// Returns `None` for unsupported roles (`System` / `Tool`).
pub fn role_to_reme_role(role: MsgRole) -> Option<&'static str> {
    match role {
        MsgRole::User => Some("user"),
        MsgRole::Assistant => Some("assistant"),
        MsgRole::System | MsgRole::Tool => None,
    }
}

/// Whether a message should be recorded, mirroring `ReMeLongTermMemory.record`.
pub fn is_recordable(msg: &Msg) -> bool {
    match msg.role() {
        MsgRole::User => true,
        MsgRole::Assistant => !msg.has_block(ContentBlockKind::ToolUse),
        MsgRole::System | MsgRole::Tool => false,
    }
}

/// Converts a single [`Msg`] to a [`ReMeMessage`], or `None` when unsupported.
pub fn convert_to_reme_message(msg: &Msg) -> Option<ReMeMessage> {
    let role = role_to_reme_role(msg.role())?;
    Some(ReMeMessage {
        role: role.to_string(),
        content: msg.text_content(),
    })
}

/// Converts a slice of messages into ReMe messages, applying the Java filters.
pub fn convert_messages(msgs: &[Msg]) -> Vec<ReMeMessage> {
    msgs.iter()
        .filter(|msg| {
            if !is_recordable(msg) {
                return false;
            }
            let text = msg.text_content();
            !text.is_empty() && !text.contains(COMPRESSED_HISTORY_MARKER)
        })
        .filter_map(convert_to_reme_message)
        .collect()
}

/// Renders a search response, mirroring `ReMeLongTermMemory.retrieve`: prefer the
/// `answer` field, otherwise join the memory-list contents with newlines.
pub fn render_search_response(response: &ReMeSearchResponse) -> String {
    if let Some(answer) = response.answer.as_ref().filter(|a| !a.is_empty()) {
        return answer.clone();
    }
    response.memories().join("\n")
}

/// Long-term memory backed by ReMe, mirroring `ReMeLongTermMemory`.
///
/// Note: the Java adapter uses `userId` as the ReMe `workspace_id`.
#[derive(Debug, Clone)]
pub struct ReMeLongTermMemory {
    client: Arc<ReMeClient>,
    user_id: String,
}

impl ReMeLongTermMemory {
    /// Creates an adapter with the workspace identifier (Java `userId`).
    pub fn new(client: Arc<ReMeClient>, user_id: impl Into<String>) -> Result<Self> {
        let user_id = user_id.into();
        if user_id.trim().is_empty() {
            return Err(MemError::Config("userId cannot be null or blank".into()));
        }
        Ok(Self { client, user_id })
    }

    /// The underlying client.
    pub fn client(&self) -> &ReMeClient {
        &self.client
    }

    /// The workspace identifier.
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    /// Builds the add request used by [`Self::record`] (one trajectory).
    pub fn build_add_request(&self, messages: Vec<ReMeMessage>) -> ReMeAddRequest {
        ReMeAddRequest::new(self.user_id.clone(), vec![ReMeTrajectory::new(messages)])
    }

    /// Builds the search request used by [`Self::retrieve`].
    pub fn build_search_request(&self, query: impl Into<String>) -> ReMeSearchRequest {
        ReMeSearchRequest::with_parts(self.user_id.clone(), query, DEFAULT_SEARCH_TOP_K)
    }

    /// Records messages to ReMe, mirroring `ReMeLongTermMemory.record`.
    pub async fn record(&self, msgs: &[Msg]) -> Result<()> {
        if msgs.is_empty() {
            return Ok(());
        }
        let messages = convert_messages(msgs);
        if messages.is_empty() {
            return Ok(());
        }
        self.client.add(&self.build_add_request(messages)).await?;
        Ok(())
    }

    /// Retrieves relevant memories, mirroring `ReMeLongTermMemory.retrieve`.
    pub async fn retrieve(&self, msg: &Msg) -> Result<String> {
        let query = msg.text_content();
        if query.is_empty() {
            return Ok(String::new());
        }
        let response = self
            .client
            .search(&self.build_search_request(query))
            .await?;
        Ok(render_search_response(&response))
    }

    /// Java-parity variant of [`Self::retrieve`].
    ///
    /// `ReMeLongTermMemory.retrieve` terminates its reactive pipeline with
    /// `onErrorReturn("")`, so transport / HTTP-status / JSON failures surface as an
    /// empty string instead of an error. This method reproduces that observable
    /// contract; use [`Self::retrieve`] when the error must stay visible.
    pub async fn retrieve_lossy(&self, msg: &Msg) -> String {
        match self.retrieve(msg).await {
            Ok(text) => text,
            Err(err) => {
                tracing::warn!(error = %err, "Failed to retrieve ReMe memory");
                String::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::mock::MockTransport;
    use crate::reme::client::{RETRIEVE_ENDPOINT, SUMMARY_ENDPOINT};
    use crate::reme::dto::{ReMeMemoryItem, ReMeSearchMetadata};
    use agentscope_core::{ContentBlock, JsonMap, ToolUseBlock};

    fn adapter(user_id: &str) -> (MockTransport, ReMeLongTermMemory) {
        let mock = MockTransport::new();
        let client = Arc::new(
            ReMeClient::with_transport(Arc::new(mock.clone()), "https://reme.test")
                .expect("valid base url"),
        );
        let adapter = ReMeLongTermMemory::new(client, user_id).unwrap();
        (mock, adapter)
    }

    fn tool_use_assistant() -> Msg {
        Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![ContentBlock::ToolUse(ToolUseBlock::new(
                "call-1",
                "search",
                JsonMap::new(),
            ))])
            .build()
    }

    #[test]
    fn role_mapping_drops_system_and_tool() {
        assert_eq!(role_to_reme_role(MsgRole::User), Some("user"));
        assert_eq!(role_to_reme_role(MsgRole::Assistant), Some("assistant"));
        assert_eq!(role_to_reme_role(MsgRole::System), None);
        assert_eq!(role_to_reme_role(MsgRole::Tool), None);
    }

    #[test]
    fn is_recordable_rejects_tool_use_assistant() {
        assert!(is_recordable(&Msg::user("hi")));
        assert!(is_recordable(&Msg::assistant("plain")));
        assert!(!is_recordable(&tool_use_assistant()));
        assert!(!is_recordable(&Msg::tool("id", "name", "out")));
    }

    #[test]
    fn convert_messages_applies_reme_filters() {
        let msgs = vec![
            Msg::user("kept"),
            Msg::system("dropped"),
            tool_use_assistant(),
            Msg::user(format!("{COMPRESSED_HISTORY_MARKER} x")),
            Msg::user(""),
        ];
        let converted = convert_messages(&msgs);
        assert_eq!(converted, vec![ReMeMessage::new("user", "kept")]);
    }

    #[test]
    fn render_prefers_answer_then_joins_memories() {
        let with_answer = ReMeSearchResponse {
            answer: Some("the answer".into()),
            ..Default::default()
        };
        assert_eq!(render_search_response(&with_answer), "the answer");

        let joined = ReMeSearchResponse {
            answer: Some(String::new()),
            metadata: Some(ReMeSearchMetadata {
                memory_list: Some(vec![
                    ReMeMemoryItem {
                        content: Some("m1".into()),
                        ..Default::default()
                    },
                    ReMeMemoryItem {
                        content: None,
                        ..Default::default()
                    },
                    ReMeMemoryItem {
                        content: Some("m2".into()),
                        ..Default::default()
                    },
                ]),
            }),
            ..Default::default()
        };
        assert_eq!(render_search_response(&joined), "m1\nm2");
        assert_eq!(render_search_response(&ReMeSearchResponse::default()), "");
    }

    #[test]
    fn new_rejects_blank_user_id() {
        let client = Arc::new(
            ReMeClient::with_transport(Arc::new(MockTransport::new()), "https://reme.test")
                .expect("valid base url"),
        );
        assert!(matches!(
            ReMeLongTermMemory::new(client, "   "),
            Err(MemError::Config(_))
        ));
    }

    #[test]
    fn build_requests_use_user_id_as_workspace() {
        let (_mock, adapter) = adapter("alice");
        let add = adapter.build_add_request(vec![ReMeMessage::new("user", "hi")]);
        assert_eq!(add.workspace_id.as_deref(), Some("alice"));
        assert_eq!(
            add.trajectories.as_ref().unwrap()[0]
                .messages
                .as_ref()
                .unwrap()[0]
                .content,
            "hi"
        );
        let search = adapter.build_search_request("q");
        assert_eq!(search.workspace_id.as_deref(), Some("alice"));
        assert_eq!(search.query.as_deref(), Some("q"));
        assert_eq!(search.top_k, Some(DEFAULT_SEARCH_TOP_K));
    }

    #[tokio::test]
    async fn record_skips_network_when_nothing_recordable() {
        let (mock, adapter) = adapter("alice");
        adapter.record(&[]).await.unwrap();
        adapter.record(&[Msg::system("sys")]).await.unwrap();
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn record_posts_trajectory_to_summary_endpoint() {
        let (mock, adapter) = adapter("alice");
        mock.push_body(r#"{"success":true}"#);
        adapter.record(&[Msg::user("hello")]).await.unwrap();
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].0.ends_with(SUMMARY_ENDPOINT));
        let body: serde_json::Value = serde_json::from_str(&requests[0].2).unwrap();
        assert_eq!(body["workspace_id"], "alice");
        assert_eq!(body["trajectories"][0]["messages"][0]["content"], "hello");
    }

    #[tokio::test]
    async fn retrieve_returns_empty_for_blank_query_without_network() {
        let (mock, adapter) = adapter("alice");
        assert_eq!(adapter.retrieve(&Msg::user("")).await.unwrap(), "");
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn retrieve_renders_answer_from_service() {
        let (mock, adapter) = adapter("alice");
        mock.push_body(r#"{"answer":"memory answer"}"#);
        assert_eq!(
            adapter.retrieve(&Msg::user("q")).await.unwrap(),
            "memory answer"
        );
        assert!(mock.requests()[0].0.ends_with(RETRIEVE_ENDPOINT));
    }

    #[tokio::test]
    async fn retrieve_lossy_returns_empty_on_http_error() {
        // Mirrors Java `testRetrieveWithHttpError`: `onErrorReturn("")`.
        let (mock, adapter) = adapter("alice");
        mock.push_response(Err(MemError::HttpStatus {
            status: 500,
            body: "{\"error\":\"boom\"}".into(),
        }));
        assert!(adapter.retrieve(&Msg::user("q")).await.is_err());
        mock.push_response(Err(MemError::HttpStatus {
            status: 500,
            body: "{\"error\":\"boom\"}".into(),
        }));
        assert_eq!(adapter.retrieve_lossy(&Msg::user("q")).await, "");
    }

    #[tokio::test]
    async fn retrieve_lossy_passes_through_memory_list() {
        let (mock, adapter) = adapter("alice");
        mock.push_body(r#"{"metadata":{"memory_list":[{"content":"m1"},{"content":"m2"}]}}"#);
        assert_eq!(adapter.retrieve_lossy(&Msg::user("q")).await, "m1\nm2");
    }
}
