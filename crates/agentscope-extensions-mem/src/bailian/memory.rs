//! Message conversion and the Bailian-backed long-term memory adapter.
//!
//! Mirrors `BailianLongTermMemory` (`record` / `retrieve`).

use std::sync::Arc;

use agentscope_core::message::msg::ContentBlockKind;
use agentscope_core::{JsonMap, Msg, MsgRole};

use crate::bailian::client::{BailianMemoryClient, DEFAULT_BASE_URL};
use crate::bailian::dto::{
    BailianAddRequest, BailianMemoryNode, BailianMessage, BailianSearchRequest,
};
use crate::error::{MemError, Result};
use crate::mem0::memory::COMPRESSED_HISTORY_MARKER;

/// Default `top_k` used by `BailianLongTermMemory`.
pub const DEFAULT_TOP_K: u32 = 10;
/// Default `min_score` used by `BailianLongTermMemory`.
pub const DEFAULT_MIN_SCORE: f64 = 0.3;

/// Maps a core [`MsgRole`] to a Bailian role, mirroring `convertToBailianMessage`.
///
/// Returns `None` for unsupported roles (`System` / `Tool`), which callers filter out.
pub fn role_to_bailian_role(role: MsgRole) -> Option<&'static str> {
    match role {
        MsgRole::User => Some("user"),
        MsgRole::Assistant => Some("assistant"),
        MsgRole::System | MsgRole::Tool => None,
    }
}

/// Whether a message should be recorded, mirroring `BailianLongTermMemory.record`:
/// only `User` / pure `Assistant` (no tool-use block) with non-blank text and no
/// compressed-history marker.
pub fn is_recordable(msg: &Msg) -> bool {
    match msg.role() {
        MsgRole::User => true,
        MsgRole::Assistant => !msg.has_block(ContentBlockKind::ToolUse),
        MsgRole::System | MsgRole::Tool => false,
    }
}

/// Converts a single [`Msg`] to a [`BailianMessage`], or `None` when unsupported.
pub fn convert_to_bailian_message(msg: &Msg) -> Option<BailianMessage> {
    let role = role_to_bailian_role(msg.role())?;
    Some(BailianMessage {
        role: role.to_string(),
        content: msg.text_content(),
    })
}

/// Converts a slice of messages into Bailian messages, applying the Java filters.
pub fn convert_messages(msgs: &[Msg]) -> Vec<BailianMessage> {
    msgs.iter()
        .filter(|msg| {
            if !is_recordable(msg) {
                return false;
            }
            let text = msg.text_content();
            !text.trim().is_empty() && !text.contains(COMPRESSED_HISTORY_MARKER)
        })
        .filter_map(convert_to_bailian_message)
        .collect()
}

/// Joins memory-node contents into a newline-separated string, mirroring `retrieve`.
pub fn join_memory_nodes(nodes: &[BailianMemoryNode]) -> String {
    nodes
        .iter()
        .filter_map(|n| n.content.as_deref())
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Configuration for [`BailianLongTermMemory`].
#[derive(Debug, Clone)]
pub struct BailianLongTermMemoryConfig {
    /// API key (required).
    pub api_key: String,
    /// Base URL (defaults to [`DEFAULT_BASE_URL`]).
    pub api_base_url: Option<String>,
    /// User identifier (required).
    pub user_id: String,
    /// Memory library identifier.
    pub memory_library_id: Option<String>,
    /// Project identifier.
    pub project_id: Option<String>,
    /// Profile schema identifier.
    pub profile_schema: Option<String>,
    /// `top_k` (defaults to [`DEFAULT_TOP_K`]).
    pub top_k: Option<u32>,
    /// `min_score` (defaults to [`DEFAULT_MIN_SCORE`]).
    pub min_score: Option<f64>,
    /// Enable rerank (defaults to `false`).
    pub enable_rerank: Option<bool>,
    /// Enable judge (defaults to `false`).
    pub enable_judge: Option<bool>,
    /// Enable rewrite (defaults to `false`).
    pub enable_rewrite: Option<bool>,
    /// Metadata stored with added memories.
    pub metadata: Option<JsonMap>,
}

impl BailianLongTermMemoryConfig {
    /// Creates a config with the required key/user identifiers.
    pub fn new(api_key: impl Into<String>, user_id: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            api_base_url: None,
            user_id: user_id.into(),
            memory_library_id: None,
            project_id: None,
            profile_schema: None,
            top_k: None,
            min_score: None,
            enable_rerank: None,
            enable_judge: None,
            enable_rewrite: None,
            metadata: None,
        }
    }

    /// The effective base URL.
    pub fn effective_base_url(&self) -> &str {
        self.api_base_url.as_deref().unwrap_or(DEFAULT_BASE_URL)
    }

    /// The effective `top_k`.
    pub fn effective_top_k(&self) -> u32 {
        self.top_k.unwrap_or(DEFAULT_TOP_K)
    }

    /// The effective `min_score`.
    pub fn effective_min_score(&self) -> f64 {
        self.min_score.unwrap_or(DEFAULT_MIN_SCORE)
    }
}

/// Long-term memory backed by Bailian, mirroring `BailianLongTermMemory`.
#[derive(Debug, Clone)]
pub struct BailianLongTermMemory {
    client: Arc<BailianMemoryClient>,
    config: BailianLongTermMemoryConfig,
}

impl BailianLongTermMemory {
    /// Creates an adapter, validating the key and user identifier like Java.
    ///
    /// Java `BailianLongTermMemory.Builder.build()` checks `apiKey` (`"apiKey cannot be
    /// null or blank"`) *before* building the client. The Rust adapter takes an already
    /// built client, so the check is repeated here: otherwise `config.api_key` would be
    /// a dead field and an empty key paired with a valid client would slip through.
    pub fn new(
        client: Arc<BailianMemoryClient>,
        config: BailianLongTermMemoryConfig,
    ) -> Result<Self> {
        if config.api_key.trim().is_empty() {
            return Err(MemError::Config("apiKey cannot be null or blank".into()));
        }
        if config.user_id.trim().is_empty() {
            return Err(MemError::Config("userId cannot be null or blank".into()));
        }
        Ok(Self { client, config })
    }

    /// The underlying client.
    pub fn client(&self) -> &BailianMemoryClient {
        &self.client
    }

    /// The adapter configuration.
    pub fn config(&self) -> &BailianLongTermMemoryConfig {
        &self.config
    }

    /// Builds the add request used by [`Self::record`].
    pub fn build_add_request(&self, messages: Vec<BailianMessage>) -> BailianAddRequest {
        let mut request = BailianAddRequest::new()
            .with_user_id(self.config.user_id.clone())
            .with_messages(messages);
        if let Some(id) = non_blank(&self.config.memory_library_id) {
            request = request.with_memory_library_id(id.to_string());
        }
        if let Some(id) = non_blank(&self.config.project_id) {
            request = request.with_project_id(id.to_string());
        }
        if let Some(schema) = non_blank(&self.config.profile_schema) {
            request = request.with_profile_schema(schema.to_string());
        }
        if let Some(metadata) = &self.config.metadata {
            if !metadata.is_empty() {
                request = request.with_metadata(metadata.clone());
            }
        }
        request
    }

    /// Builds the search request used by [`Self::retrieve`].
    pub fn build_search_request(&self, query: impl Into<String>) -> BailianSearchRequest {
        let search_message = BailianMessage::new("user", query);
        let mut request = BailianSearchRequest::new()
            .with_user_id(self.config.user_id.clone())
            .with_messages(vec![search_message])
            .with_top_k(self.config.effective_top_k())
            .with_min_score(self.config.effective_min_score())
            .with_enable_rerank(self.config.enable_rerank.unwrap_or(false))
            .with_enable_judge(self.config.enable_judge.unwrap_or(false))
            .with_enable_rewrite(self.config.enable_rewrite.unwrap_or(false));
        if let Some(id) = non_blank(&self.config.memory_library_id) {
            request = request.with_memory_library_id(id.to_string());
        }
        if let Some(id) = non_blank(&self.config.project_id) {
            request = request.with_project_ids(vec![id.to_string()]);
        }
        request
    }

    /// Records messages to Bailian, mirroring `BailianLongTermMemory.record`.
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

    /// Retrieves relevant memories as a newline-separated string, mirroring `retrieve`.
    pub async fn retrieve(&self, msg: &Msg) -> Result<String> {
        let query = msg.text_content();
        if query.trim().is_empty() {
            return Ok(String::new());
        }
        let response = self
            .client
            .search(&self.build_search_request(query))
            .await?;
        Ok(match response.memory_nodes {
            Some(nodes) => join_memory_nodes(&nodes),
            None => String::new(),
        })
    }

    /// Java-parity variant of [`Self::record`].
    ///
    /// `BailianLongTermMemory.record` ends with
    /// `.doOnError(e -> log.warn(...)).onErrorComplete()`, so a failed add is logged
    /// and swallowed — the returned `Mono<Void>` always completes. This method
    /// reproduces that contract; use [`Self::record`] when the error must stay visible.
    pub async fn record_lossy(&self, msgs: &[Msg]) {
        if let Err(err) = self.record(msgs).await {
            tracing::warn!(error = %err, "Failed to record Bailian memory");
        }
    }

    /// Java-parity variant of [`Self::retrieve`].
    ///
    /// `BailianLongTermMemory.retrieve` terminates its pipeline with
    /// `.doOnError(e -> log.warn(...)).onErrorReturn("")`, so failures surface as an
    /// empty string. Use [`Self::retrieve`] when the error must stay visible.
    pub async fn retrieve_lossy(&self, msg: &Msg) -> String {
        match self.retrieve(msg).await {
            Ok(text) => text,
            Err(err) => {
                tracing::warn!(error = %err, "Failed to retrieve Bailian memory");
                String::new()
            }
        }
    }
}

fn non_blank(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bailian::client::{ADD_MEMORY_ENDPOINT, SEARCH_MEMORY_ENDPOINT};
    use crate::http::mock::MockTransport;
    use agentscope_core::{ContentBlock, ToolUseBlock};

    fn adapter(config: BailianLongTermMemoryConfig) -> (MockTransport, BailianLongTermMemory) {
        let mock = MockTransport::new();
        let client = BailianMemoryClient::with_transport(
            Arc::new(mock.clone()),
            config.api_key.clone(),
            config.effective_base_url(),
        )
        .unwrap();
        let adapter = BailianLongTermMemory::new(Arc::new(client), config).unwrap();
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
        assert_eq!(role_to_bailian_role(MsgRole::User), Some("user"));
        assert_eq!(role_to_bailian_role(MsgRole::Assistant), Some("assistant"));
        assert_eq!(role_to_bailian_role(MsgRole::System), None);
        assert_eq!(role_to_bailian_role(MsgRole::Tool), None);
    }

    #[test]
    fn is_recordable_rejects_tool_use_assistant_and_system() {
        assert!(is_recordable(&Msg::user("hi")));
        assert!(is_recordable(&Msg::assistant("plain")));
        assert!(!is_recordable(&tool_use_assistant()));
        assert!(!is_recordable(&Msg::system("sys")));
    }

    #[test]
    fn convert_messages_applies_bailian_filters() {
        let msgs = vec![
            Msg::user("kept"),
            Msg::system("dropped"),
            tool_use_assistant(),
            Msg::user(format!("{COMPRESSED_HISTORY_MARKER} x")),
            Msg::user("   "),
            Msg::assistant("also kept"),
        ];
        let converted = convert_messages(&msgs);
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0], BailianMessage::new("user", "kept"));
        assert_eq!(converted[1], BailianMessage::new("assistant", "also kept"));
    }

    #[test]
    fn join_memory_nodes_skips_blank_and_none() {
        let nodes = vec![
            BailianMemoryNode {
                content: Some("a".into()),
                ..Default::default()
            },
            BailianMemoryNode {
                content: None,
                ..Default::default()
            },
            BailianMemoryNode {
                content: Some("   ".into()),
                ..Default::default()
            },
            BailianMemoryNode {
                content: Some("b".into()),
                ..Default::default()
            },
        ];
        assert_eq!(join_memory_nodes(&nodes), "a\nb");
    }

    #[test]
    fn config_effective_defaults() {
        let config = BailianLongTermMemoryConfig::new("key", "user");
        assert_eq!(config.effective_base_url(), DEFAULT_BASE_URL);
        assert_eq!(config.effective_top_k(), DEFAULT_TOP_K);
        assert!((config.effective_min_score() - DEFAULT_MIN_SCORE).abs() < 1e-9);
    }

    #[test]
    fn new_rejects_blank_user_id() {
        let config = BailianLongTermMemoryConfig::new("key", "  ");
        let client = BailianMemoryClient::with_transport(
            Arc::new(MockTransport::new()),
            "key",
            DEFAULT_BASE_URL,
        )
        .unwrap();
        assert!(matches!(
            BailianLongTermMemory::new(Arc::new(client), config),
            Err(MemError::Config(_))
        ));
    }

    /// Java `BailianLongTermMemoryTest.testBuilderRequiresApiKey` /
    /// `testBuilderWithBlankApiKey`: the adapter itself rejects an empty key, even
    /// when a valid client is injected.
    #[test]
    fn new_rejects_blank_api_key() {
        let client = Arc::new(
            BailianMemoryClient::with_transport(
                Arc::new(MockTransport::new()),
                "key",
                DEFAULT_BASE_URL,
            )
            .unwrap(),
        );
        for blank in ["", "   "] {
            let config = BailianLongTermMemoryConfig::new(blank, "user");
            let err = BailianLongTermMemory::new(Arc::clone(&client), config).unwrap_err();
            assert!(matches!(err, MemError::Config(_)));
            assert!(err.to_string().contains("apiKey cannot be null or blank"));
        }
    }

    #[test]
    fn client_rejects_blank_base_url() {
        // Mirrors Java `BailianMemoryClient` builder validation.
        let err = BailianMemoryClient::with_transport(Arc::new(MockTransport::new()), "key", "   ")
            .unwrap_err();
        assert!(matches!(err, MemError::Config(_)));
    }

    #[test]
    fn client_rejects_blank_api_key() {
        let err = BailianMemoryClient::with_transport(
            Arc::new(MockTransport::new()),
            "   ",
            DEFAULT_BASE_URL,
        )
        .unwrap_err();
        assert!(matches!(err, MemError::Config(_)));
    }

    #[test]
    fn build_search_request_uses_config_values() {
        let mut config = BailianLongTermMemoryConfig::new("key", "user");
        config.top_k = Some(3);
        config.min_score = Some(0.5);
        config.enable_rerank = Some(true);
        config.enable_judge = Some(true);
        config.enable_rewrite = Some(true);
        config.memory_library_id = Some("lib".into());
        config.project_id = Some("proj".into());
        let (_mock, adapter) = adapter(config);
        let req = adapter.build_search_request("query");
        assert_eq!(req.user_id.as_deref(), Some("user"));
        assert_eq!(req.top_k, Some(3));
        assert_eq!(req.min_score, Some(0.5));
        assert_eq!(req.enable_rerank, Some(true));
        assert_eq!(req.memory_library_id.as_deref(), Some("lib"));
        assert_eq!(req.project_ids.as_deref(), Some(&["proj".to_string()][..]));
        assert_eq!(req.messages.as_ref().unwrap()[0].content, "query");
    }

    #[test]
    fn build_add_request_skips_blank_optional_fields() {
        let mut config = BailianLongTermMemoryConfig::new("key", "user");
        config.memory_library_id = Some("   ".into());
        config.metadata = Some(JsonMap::new());
        let (_mock, adapter) = adapter(config);
        let req = adapter.build_add_request(vec![BailianMessage::new("user", "hi")]);
        assert_eq!(req.user_id.as_deref(), Some("user"));
        assert!(req.memory_library_id.is_none());
        assert!(req.metadata.is_none());
    }

    /// Java `testRecordWithMemoryLibraryId` / `testRecordWithProfileSchema` /
    /// `testRecordWithProjectId` / `testRecordWithMetadata`: the optional identifiers
    /// travel on the add body when configured.
    #[tokio::test]
    async fn record_posts_optional_identifiers_and_metadata() {
        let mut config = BailianLongTermMemoryConfig::new("key", "user");
        config.memory_library_id = Some("lib".into());
        config.project_id = Some("proj".into());
        config.profile_schema = Some("schema".into());
        let mut metadata = JsonMap::new();
        metadata.insert("tier".into(), serde_json::json!("gold"));
        config.metadata = Some(metadata);
        let (mock, adapter) = adapter(config);
        mock.push_body(r#"{"memory_nodes":[]}"#);
        adapter.record(&[Msg::user("hi")]).await.unwrap();
        let body: serde_json::Value = serde_json::from_str(&mock.requests()[0].2).unwrap();
        assert_eq!(body["user_id"], "user");
        assert_eq!(body["memory_library_id"], "lib");
        assert_eq!(body["project_id"], "proj");
        assert_eq!(body["profile_schema"], "schema");
        assert_eq!(body["meta_data"]["tier"], "gold");
    }

    #[tokio::test]
    async fn record_skips_network_when_nothing_recordable() {
        let (mock, adapter) = adapter(BailianLongTermMemoryConfig::new("key", "user"));
        adapter.record(&[]).await.unwrap();
        adapter.record(&[Msg::system("sys")]).await.unwrap();
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn record_posts_to_add_endpoint_with_bearer_auth() {
        let (mock, adapter) = adapter(BailianLongTermMemoryConfig::new("key", "user"));
        mock.push_body(r#"{"memory_nodes":[]}"#);
        adapter.record(&[Msg::user("hello")]).await.unwrap();
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].0.ends_with(ADD_MEMORY_ENDPOINT));
        assert!(requests[0]
            .1
            .contains(&("Authorization".to_string(), "Bearer key".to_string())));
        let body: serde_json::Value = serde_json::from_str(&requests[0].2).unwrap();
        assert_eq!(body["user_id"], "user");
    }

    #[tokio::test]
    async fn retrieve_returns_empty_for_blank_query_without_network() {
        let (mock, adapter) = adapter(BailianLongTermMemoryConfig::new("key", "user"));
        assert_eq!(adapter.retrieve(&Msg::user("  ")).await.unwrap(), "");
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn retrieve_joins_memory_node_contents() {
        let (mock, adapter) = adapter(BailianLongTermMemoryConfig::new("key", "user"));
        mock.push_body(r#"{"memory_nodes":[{"content":"one"},{"content":"two"},{}]}"#);
        assert_eq!(adapter.retrieve(&Msg::user("q")).await.unwrap(), "one\ntwo");
        assert!(mock.requests()[0].0.ends_with(SEARCH_MEMORY_ENDPOINT));
    }

    #[tokio::test]
    async fn record_lossy_swallows_http_errors() {
        // Mirrors Java `testRecordWithHttpError`: `onErrorComplete()`.
        let (mock, adapter) = adapter(BailianLongTermMemoryConfig::new("key", "user"));
        mock.push_response(Err(MemError::HttpStatus {
            status: 500,
            body: "{\"error\":\"Internal error\"}".into(),
        }));
        // The strict API keeps the error visible...
        assert!(adapter.record(&[Msg::user("hi")]).await.is_err());
        // ...while the Java-parity variant swallows it.
        mock.push_response(Err(MemError::HttpStatus {
            status: 500,
            body: "{\"error\":\"Internal error\"}".into(),
        }));
        adapter.record_lossy(&[Msg::user("hi")]).await;
    }

    #[tokio::test]
    async fn record_lossy_swallows_transport_errors_without_panicking() {
        // Mirrors Java `testRecordWithNetworkError`.
        let (mock, adapter) = adapter(BailianLongTermMemoryConfig::new("key", "user"));
        mock.push_response(Err(MemError::Transport("connection refused".into())));
        adapter.record_lossy(&[Msg::user("hi")]).await;
        assert_eq!(mock.requests().len(), 1);
    }

    #[tokio::test]
    async fn retrieve_lossy_returns_empty_on_error_and_passes_through_success() {
        // Mirrors Java `testRetrieveWithHttpError` / `testRetrieveWithNetworkError`.
        let (mock, adapter) = adapter(BailianLongTermMemoryConfig::new("key", "user"));
        mock.push_response(Err(MemError::Transport("down".into())));
        assert_eq!(adapter.retrieve_lossy(&Msg::user("q")).await, "");
        mock.push_body(r#"{"memory_nodes":[{"content":"one"}]}"#);
        assert_eq!(adapter.retrieve_lossy(&Msg::user("q")).await, "one");
    }
}
