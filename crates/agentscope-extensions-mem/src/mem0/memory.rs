//! Message conversion and the Mem0-backed long-term memory adapter.
//!
//! Mirrors `Mem0LongTermMemory` (`record` / `retrieve`).

use std::sync::Arc;

use serde_json::Value;

use agentscope_core::{JsonMap, Msg, MsgRole};

use crate::error::{MemError, Result};
use crate::mem0::client::Mem0Client;
use crate::mem0::dto::{Mem0AddRequest, Mem0Message, Mem0SearchRequest, Mem0SearchResult};

/// Marker inserted into compressed history segments (Java literal).
pub const COMPRESSED_HISTORY_MARKER: &str = "<compressed_history>";

/// Default `top_k` used by `Mem0LongTermMemory.buildSearchRequest`.
pub const DEFAULT_SEARCH_TOP_K: u32 = 5;

/// Maps a core [`MsgRole`] to a Mem0 role, mirroring `convertToMem0Message`:
/// `User`/`System` become `"user"`, `Assistant`/`Tool` become `"assistant"`.
pub fn role_to_mem0_role(role: MsgRole) -> &'static str {
    match role {
        MsgRole::User | MsgRole::System => "user",
        MsgRole::Assistant | MsgRole::Tool => "assistant",
    }
}

/// Converts a single [`Msg`] to a [`Mem0Message`].
pub fn convert_to_mem0_message(msg: &Msg) -> Mem0Message {
    Mem0Message {
        role: role_to_mem0_role(msg.role()).to_string(),
        content: msg.text_content(),
        name: msg.name().map(str::to_string),
    }
}

/// Converts a slice of messages into Mem0 messages, dropping null / empty /
/// compressed-history entries exactly like `Mem0LongTermMemory.record`.
pub fn convert_messages(msgs: &[Msg]) -> Vec<Mem0Message> {
    msgs.iter()
        .filter(|msg| {
            let text = msg.text_content();
            !text.is_empty() && !text.contains(COMPRESSED_HISTORY_MARKER)
        })
        .map(convert_to_mem0_message)
        .collect()
}

/// Joins search results into the newline-separated string the Java adapter returns.
pub fn join_search_results(results: &[Mem0SearchResult]) -> String {
    results
        .iter()
        .filter_map(|r| r.memory.as_deref())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Configuration for [`Mem0LongTermMemory`].
#[derive(Debug, Clone, Default)]
pub struct Mem0LongTermMemoryConfig {
    /// Agent identifier (Java `agentName` -> `agent_id`).
    pub agent_id: Option<String>,
    /// User identifier.
    pub user_id: Option<String>,
    /// Run / session identifier (Java `runName` -> `run_id`).
    pub run_id: Option<String>,
    /// Custom metadata, stored on add and merged into search filters.
    pub metadata: Option<JsonMap>,
}

/// Long-term memory backed by Mem0, mirroring `Mem0LongTermMemory`.
#[derive(Debug, Clone)]
pub struct Mem0LongTermMemory {
    client: Arc<Mem0Client>,
    config: Mem0LongTermMemoryConfig,
}

impl Mem0LongTermMemory {
    /// Creates an adapter; fails when no identifier is configured (Java
    /// `IllegalArgumentException`).
    pub fn new(client: Arc<Mem0Client>, config: Mem0LongTermMemoryConfig) -> Result<Self> {
        if config.agent_id.is_none() && config.user_id.is_none() && config.run_id.is_none() {
            return Err(MemError::Config(
                "at least one of agentName, userName, or runName must be provided".into(),
            ));
        }
        Ok(Self { client, config })
    }

    /// The underlying client.
    pub fn client(&self) -> &Mem0Client {
        &self.client
    }

    /// The adapter configuration.
    pub fn config(&self) -> &Mem0LongTermMemoryConfig {
        &self.config
    }

    /// Builds the add request used by [`Self::record`].
    pub fn build_add_request(&self, messages: Vec<Mem0Message>) -> Mem0AddRequest {
        let mut request = Mem0AddRequest::new()
            .with_messages(messages)
            .with_infer(true);
        if let Some(agent_id) = &self.config.agent_id {
            request = request.with_agent_id(agent_id.clone());
        }
        if let Some(user_id) = &self.config.user_id {
            request = request.with_user_id(user_id.clone());
        }
        if let Some(run_id) = &self.config.run_id {
            request = request.with_run_id(run_id.clone());
        }
        if let Some(metadata) = &self.config.metadata {
            request = request.with_metadata(metadata.clone());
        }
        request
    }

    /// Builds the search request used by [`Self::retrieve`], mirroring
    /// `Mem0LongTermMemory.buildSearchRequest` (top_k = 5, metadata merged into filters).
    pub fn build_search_request(&self, query: impl Into<String>) -> Mem0SearchRequest {
        let mut request = Mem0SearchRequest::new()
            .with_query(query)
            .with_top_k(DEFAULT_SEARCH_TOP_K);
        if let Some(user_id) = &self.config.user_id {
            request = request.with_user_id(user_id.clone());
        }
        if let Some(agent_id) = &self.config.agent_id {
            request = request.with_agent_id(agent_id.clone());
        }
        if let Some(run_id) = &self.config.run_id {
            request = request.with_run_id(run_id.clone());
        }
        if let Some(Value::Object(metadata)) = self
            .config
            .metadata
            .as_ref()
            .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
        {
            for (key, value) in metadata {
                request.filters.insert(key, value);
            }
        }
        request
    }

    /// Records messages to Mem0, mirroring `Mem0LongTermMemory.record`.
    ///
    /// Returns without a request when nothing survives filtering.
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

    /// Retrieves relevant memories as a newline-separated string, mirroring
    /// `Mem0LongTermMemory.retrieve` (null / empty query yields an empty string).
    pub async fn retrieve(&self, msg: &Msg) -> Result<String> {
        let query = msg.text_content();
        if query.is_empty() {
            return Ok(String::new());
        }
        let response = self
            .client
            .search(&self.build_search_request(query))
            .await?;
        Ok(match response.results {
            Some(results) => join_search_results(&results),
            None => String::new(),
        })
    }

    /// Java-parity variant of [`Self::retrieve`].
    ///
    /// `Mem0LongTermMemory.retrieve` terminates its reactive pipeline with
    /// `onErrorReturn("")`, so transport / HTTP-status / JSON failures surface as an
    /// empty string instead of an error. This method reproduces that observable
    /// contract; use [`Self::retrieve`] when the error must stay visible.
    pub async fn retrieve_lossy(&self, msg: &Msg) -> String {
        match self.retrieve(msg).await {
            Ok(text) => text,
            Err(err) => {
                tracing::warn!(error = %err, "Failed to retrieve Mem0 memory");
                String::new()
            }
        }
    }

    /// Retrieves results as core [`Msg`]s (Rust-specific convenience, see tracker §4.2).
    pub async fn retrieve_msgs(&self, msg: &Msg) -> Result<Vec<Msg>> {
        let query = msg.text_content();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let response = self
            .client
            .search(&self.build_search_request(query))
            .await?;
        Ok(response
            .results
            .unwrap_or_default()
            .iter()
            .map(Mem0SearchResult::to_msg)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::mock::MockTransport;
    use agentscope_core::{ContentBlock, TextBlock};

    fn client(transport: Arc<MockTransport>) -> Arc<Mem0Client> {
        Arc::new(Mem0Client::with_transport(
            transport,
            "https://mem0.test",
            Some("key".into()),
            crate::mem0::dto::Mem0ApiType::Platform,
        ))
    }

    fn adapter(config: Mem0LongTermMemoryConfig) -> (MockTransport, Mem0LongTermMemory) {
        let transport = MockTransport::new();
        let transport_for_client: Arc<MockTransport> = Arc::new(transport.clone());
        let adapter = Mem0LongTermMemory::new(client(transport_for_client), config).unwrap();
        (transport, adapter)
    }

    #[test]
    fn role_mapping_matches_java() {
        assert_eq!(role_to_mem0_role(MsgRole::User), "user");
        assert_eq!(role_to_mem0_role(MsgRole::System), "user");
        assert_eq!(role_to_mem0_role(MsgRole::Assistant), "assistant");
        assert_eq!(role_to_mem0_role(MsgRole::Tool), "assistant");
    }

    #[test]
    fn convert_message_preserves_name() {
        let msg = Msg::builder()
            .role(MsgRole::User)
            .name("alice")
            .text_content("hi")
            .build();
        let converted = convert_to_mem0_message(&msg);
        assert_eq!(converted.role, "user");
        assert_eq!(converted.content, "hi");
        assert_eq!(converted.name.as_deref(), Some("alice"));
    }

    #[test]
    fn convert_messages_drops_empty_and_compressed_history() {
        let msgs = vec![
            Msg::user("keep"),
            Msg::user(""),
            Msg::user(format!("{COMPRESSED_HISTORY_MARKER} old")),
            Msg::assistant("also keep"),
        ];
        let converted = convert_messages(&msgs);
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0].content, "keep");
        assert_eq!(converted[1].role, "assistant");
    }

    #[test]
    fn join_search_results_filters_none_and_joins() {
        let results = vec![
            Mem0SearchResult {
                memory: Some("a".into()),
                ..Default::default()
            },
            Mem0SearchResult::default(),
            Mem0SearchResult {
                memory: Some("b".into()),
                ..Default::default()
            },
        ];
        assert_eq!(join_search_results(&results), "a\nb");
        assert_eq!(join_search_results(&[]), "");
    }

    #[test]
    fn new_requires_at_least_one_identifier() {
        let transport = Arc::new(MockTransport::new());
        let err = Mem0LongTermMemory::new(
            client(transport.clone()),
            Mem0LongTermMemoryConfig::default(),
        )
        .unwrap_err();
        assert!(matches!(err, MemError::Config(_)));
    }

    #[test]
    fn build_search_request_uses_top_k_5_and_merges_config() {
        let mut metadata = JsonMap::new();
        metadata.insert("tier".into(), serde_json::Value::String("gold".into()));
        let (_mock, adapter) = adapter(Mem0LongTermMemoryConfig {
            agent_id: Some("agent".into()),
            user_id: Some("user".into()),
            run_id: Some("run".into()),
            metadata: Some(metadata),
        });
        let req = adapter.build_search_request("query");
        assert_eq!(req.top_k, Some(DEFAULT_SEARCH_TOP_K));
        assert_eq!(req.query.as_deref(), Some("query"));
        assert_eq!(req.filters["user_id"], "user");
        assert_eq!(req.filters["agent_id"], "agent");
        assert_eq!(req.filters["run_id"], "run");
        assert_eq!(req.filters["tier"], "gold");
    }

    #[tokio::test]
    async fn record_skips_request_when_nothing_survives_filtering() {
        let (mock, adapter) = adapter(Mem0LongTermMemoryConfig {
            user_id: Some("u".into()),
            ..Default::default()
        });
        adapter.record(&[]).await.unwrap();
        adapter
            .record(&[Msg::user(format!("{COMPRESSED_HISTORY_MARKER}"))])
            .await
            .unwrap();
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn record_posts_add_request_with_messages() {
        let (mock, adapter) = adapter(Mem0LongTermMemoryConfig {
            user_id: Some("u".into()),
            ..Default::default()
        });
        mock.push_body("{}");
        adapter.record(&[Msg::user("hello")]).await.unwrap();
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].0.ends_with("/v1/memories/"));
        let body: serde_json::Value = serde_json::from_str(&requests[0].2).unwrap();
        assert_eq!(body["user_id"], "u");
        assert_eq!(body["messages"][0]["content"], "hello");
    }

    #[tokio::test]
    async fn retrieve_returns_empty_for_blank_query_without_network() {
        let (mock, adapter) = adapter(Mem0LongTermMemoryConfig {
            user_id: Some("u".into()),
            ..Default::default()
        });
        assert_eq!(adapter.retrieve(&Msg::user("")).await.unwrap(), "");
        assert!(adapter
            .retrieve_msgs(&Msg::user(""))
            .await
            .unwrap()
            .is_empty());
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn retrieve_handles_array_and_object_search_shapes() {
        let (mock, adapter) = adapter(Mem0LongTermMemoryConfig {
            user_id: Some("u".into()),
            ..Default::default()
        });
        // v1.0 bare array shape.
        mock.push_body("[{\"memory\":\"one\"},{\"memory\":\"two\"}]");
        assert_eq!(adapter.retrieve(&Msg::user("q")).await.unwrap(), "one\ntwo");
        // v1.1 wrapped shape.
        mock.push_body(r#"{"results":[{"memory":"three"}]}"#);
        assert_eq!(adapter.retrieve(&Msg::user("q")).await.unwrap(), "three");
        assert_eq!(mock.requests().len(), 2);
    }

    #[tokio::test]
    async fn retrieve_msgs_maps_hits_to_assistant_msgs() {
        let (mock, adapter) = adapter(Mem0LongTermMemoryConfig {
            user_id: Some("u".into()),
            ..Default::default()
        });
        mock.push_body(r#"{"results":[{"id":"m1","memory":"fact"}]}"#);
        let msgs = adapter.retrieve_msgs(&Msg::user("q")).await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role(), MsgRole::Assistant);
        assert_eq!(msgs[0].text_content(), "fact");
    }

    #[test]
    fn compressed_history_marker_matches_java_literal() {
        assert_eq!(COMPRESSED_HISTORY_MARKER, "<compressed_history>");
        // Sanity check that a tool-use assistant message is still recordable in mem0
        // (mem0 keeps assistant/tool turns; only Bailian/ReMe drop tool-use turns).
        let msg = Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![ContentBlock::Text(TextBlock::new("final answer"))])
            .build();
        assert_eq!(convert_messages(std::slice::from_ref(&msg)).len(), 1);
    }

    #[tokio::test]
    async fn retrieve_lossy_returns_empty_on_http_status_error() {
        // Mirrors Java `testRetrieveWithHttpError`: `onErrorReturn("")`.
        let (mock, adapter) = adapter(Mem0LongTermMemoryConfig {
            user_id: Some("u".into()),
            ..Default::default()
        });
        mock.push_response(Err(MemError::HttpStatus {
            status: 404,
            body: "{\"error\":\"Not found\"}".into(),
        }));
        // The strict API keeps the error visible...
        assert!(adapter.retrieve(&Msg::user("q")).await.is_err());
        // ...while the Java-parity variant swallows it.
        mock.push_response(Err(MemError::HttpStatus {
            status: 404,
            body: "{\"error\":\"Not found\"}".into(),
        }));
        assert_eq!(adapter.retrieve_lossy(&Msg::user("q")).await, "");
    }

    #[tokio::test]
    async fn retrieve_lossy_returns_empty_on_transport_error() {
        let (mock, adapter) = adapter(Mem0LongTermMemoryConfig {
            user_id: Some("u".into()),
            ..Default::default()
        });
        mock.push_response(Err(MemError::Transport("connection refused".into())));
        assert_eq!(adapter.retrieve_lossy(&Msg::user("q")).await, "");
    }

    #[tokio::test]
    async fn retrieve_lossy_passes_through_successful_results() {
        let (mock, adapter) = adapter(Mem0LongTermMemoryConfig {
            user_id: Some("u".into()),
            ..Default::default()
        });
        mock.push_body(r#"{"results":[{"memory":"one"},{"memory":"two"}]}"#);
        assert_eq!(adapter.retrieve_lossy(&Msg::user("q")).await, "one\ntwo");
    }
}
