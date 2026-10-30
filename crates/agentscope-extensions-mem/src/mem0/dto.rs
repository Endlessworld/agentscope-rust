//! Mem0 API data transfer objects.
//!
//! Field names and `NON_NULL` semantics mirror the Java
//! `io.agentscope.core.memory.mem0` DTOs (Jackson `@JsonProperty` / `@JsonInclude`).

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};

use agentscope_core::{ContentBlock, JsonMap, Msg, MsgRole, TextBlock};

/// Delegation flavour of the Mem0 service (Java `Mem0ApiType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mem0ApiType {
    /// Official cloud service (`/v1/memories/`, `/v2/memories/search/`).
    #[default]
    Platform,
    /// Self-deployed service (`/memories`, `/search`).
    SelfHosted,
}

impl Mem0ApiType {
    /// Parses a string case-insensitively; mirrors Java `Mem0ApiType.fromString`
    /// (unknown / empty values fall back to [`Mem0ApiType::Platform`]).
    pub fn from_string(value: Option<&str>) -> Self {
        let Some(value) = value else {
            return Self::Platform;
        };
        if value.is_empty() {
            return Self::Platform;
        }
        let normalized = value.to_lowercase().replace('_', "-");
        if normalized == "self-hosted" || normalized == "selfhosted" {
            Self::SelfHosted
        } else {
            Self::Platform
        }
    }
}

/// A message in the Mem0 API format.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Mem0Message {
    /// Role of the sender, typically `"user"` or `"assistant"`.
    pub role: String,
    /// Text content.
    pub content: String,
    /// Optional sender name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Mem0Message {
    /// Creates a message from a role and content.
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            name: None,
        }
    }

    /// Creates a message with an explicit sender name.
    pub fn with_name(
        role: impl Into<String>,
        content: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            name: Some(name.into()),
        }
    }
}

/// Request body for `POST /v1/memories/` (platform) or `POST /memories` (self-hosted).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Mem0AddRequest {
    /// Messages to process for memory extraction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<Mem0Message>>,
    /// Agent identifier.
    #[serde(rename = "agent_id", skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// User identifier.
    #[serde(rename = "user_id", skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Application identifier.
    #[serde(rename = "app_id", skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// Run / session identifier.
    #[serde(rename = "run_id", skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Additional metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
    /// String to include specific preferences in the memory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub includes: Option<String>,
    /// String to exclude specific preferences from the memory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excludes: Option<String>,
    /// Whether to use LLM inference (Java default `true`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub infer: Option<bool>,
    /// Output format version (Java default `"v1.1"`).
    #[serde(rename = "output_format", skip_serializing_if = "Option::is_none")]
    pub output_format: Option<String>,
    /// Custom categories (name -> description).
    #[serde(rename = "custom_categories", skip_serializing_if = "Option::is_none")]
    pub custom_categories: Option<JsonMap>,
    /// Custom instructions for organising memories.
    #[serde(
        rename = "custom_instructions",
        skip_serializing_if = "Option::is_none"
    )]
    pub custom_instructions: Option<String>,
    /// Whether the memory is immutable (Java default `false`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub immutable: Option<bool>,
    /// Whether to add asynchronously (Java default `true`).
    #[serde(rename = "async_mode", skip_serializing_if = "Option::is_none")]
    pub async_mode: Option<bool>,
    /// Unix timestamp of the memory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
    /// Expiration date (`YYYY-MM-DD`).
    #[serde(rename = "expiration_date", skip_serializing_if = "Option::is_none")]
    pub expiration_date: Option<String>,
    /// Organisation ID.
    #[serde(rename = "org_id", skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
    /// Project ID.
    #[serde(rename = "project_id", skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// API version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Memory type.
    ///
    /// Deprecated in the Java DTO ("not in official API spec"); kept for JSON parity.
    #[serde(rename = "memory_type", skip_serializing_if = "Option::is_none")]
    pub memory_type: Option<String>,
}

impl Mem0AddRequest {
    /// Creates an add request populated with the Java no-arg-constructor defaults
    /// (`infer=true`, `output_format="v1.1"`, `async_mode=true`, `immutable=false`).
    pub fn new() -> Self {
        Self {
            infer: Some(true),
            output_format: Some("v1.1".to_string()),
            async_mode: Some(true),
            immutable: Some(false),
            ..Default::default()
        }
    }

    /// Sets the messages.
    pub fn with_messages(mut self, messages: Vec<Mem0Message>) -> Self {
        self.messages = Some(messages);
        self
    }

    /// Sets the agent identifier.
    pub fn with_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }

    /// Sets the user identifier.
    pub fn with_user_id(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    /// Sets the run identifier.
    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }

    /// Sets the metadata map.
    pub fn with_metadata(mut self, metadata: JsonMap) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// Sets the `infer` flag.
    pub fn with_infer(mut self, infer: bool) -> Self {
        self.infer = Some(infer);
        self
    }

    /// Sets the application identifier.
    pub fn with_app_id(mut self, app_id: impl Into<String>) -> Self {
        self.app_id = Some(app_id.into());
        self
    }

    /// Sets the `includes` prompt fragment.
    pub fn with_includes(mut self, includes: impl Into<String>) -> Self {
        self.includes = Some(includes.into());
        self
    }

    /// Sets the `excludes` prompt fragment.
    pub fn with_excludes(mut self, excludes: impl Into<String>) -> Self {
        self.excludes = Some(excludes.into());
        self
    }

    /// Sets the output format version.
    pub fn with_output_format(mut self, output_format: impl Into<String>) -> Self {
        self.output_format = Some(output_format.into());
        self
    }

    /// Sets the custom categories map.
    pub fn with_custom_categories(mut self, custom_categories: JsonMap) -> Self {
        self.custom_categories = Some(custom_categories);
        self
    }

    /// Sets the custom instructions.
    pub fn with_custom_instructions(mut self, custom_instructions: impl Into<String>) -> Self {
        self.custom_instructions = Some(custom_instructions.into());
        self
    }

    /// Sets the `immutable` flag.
    pub fn with_immutable(mut self, immutable: bool) -> Self {
        self.immutable = Some(immutable);
        self
    }

    /// Sets the `async_mode` flag.
    pub fn with_async_mode(mut self, async_mode: bool) -> Self {
        self.async_mode = Some(async_mode);
        self
    }

    /// Sets the Unix timestamp.
    pub fn with_timestamp(mut self, timestamp: i64) -> Self {
        self.timestamp = Some(timestamp);
        self
    }

    /// Sets the expiration date (`YYYY-MM-DD`).
    pub fn with_expiration_date(mut self, expiration_date: impl Into<String>) -> Self {
        self.expiration_date = Some(expiration_date.into());
        self
    }

    /// Sets the organisation identifier.
    pub fn with_org_id(mut self, org_id: impl Into<String>) -> Self {
        self.org_id = Some(org_id.into());
        self
    }

    /// Sets the project identifier.
    pub fn with_project_id(mut self, project_id: impl Into<String>) -> Self {
        self.project_id = Some(project_id.into());
        self
    }

    /// Sets the API version.
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// Sets the deprecated `memory_type` field (JSON parity only).
    pub fn with_memory_type(mut self, memory_type: impl Into<String>) -> Self {
        self.memory_type = Some(memory_type.into());
        self
    }
}

/// Response of the Mem0 add API.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Mem0AddResponse {
    /// Extracted memories; entries stay generic because Mem0 returns heterogeneous objects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results: Option<Vec<JsonMap>>,
    /// Optional operation message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Request body for the Mem0 search API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mem0SearchRequest {
    /// Semantic search query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// API version (Java default `"v2"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Filters (`user_id`, `agent_id`, `run_id`, `app_id`, ...).
    ///
    /// Always serialized, even when empty (`@JsonInclude(ALWAYS)` in Java). Defaults
    /// to an empty map on deserialization, mirroring Jackson's field initializer in
    /// `Mem0SearchRequest` (a payload without `filters` must not be rejected).
    #[serde(default)]
    pub filters: JsonMap,
    /// Maximum number of results (Java default `10`).
    #[serde(rename = "top_k", skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    /// Fields to include in the response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<String>>,
    /// Whether to rerank the memories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerank: Option<bool>,
    /// Whether to use keyword search.
    #[serde(rename = "keyword_search", skip_serializing_if = "Option::is_none")]
    pub keyword_search: Option<bool>,
    /// Whether to filter memories.
    #[serde(rename = "filter_memories", skip_serializing_if = "Option::is_none")]
    pub filter_memories: Option<bool>,
    /// Minimum similarity threshold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f64>,
    /// Organisation ID.
    #[serde(rename = "org_id", skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
    /// Project ID.
    #[serde(rename = "project_id", skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// User identifier (also mirrored into `filters`).
    ///
    /// Private so it cannot be assigned without updating `filters`: Java
    /// `Mem0SearchRequest.setUserId` always synchronises the two, and a filter-less
    /// search would silently leak across users. Use [`Self::with_user_id`] /
    /// [`Self::set_user_id`] (or [`Self::sync_filters`] after deserialization).
    #[serde(rename = "user_id", skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
}

impl Default for Mem0SearchRequest {
    fn default() -> Self {
        Self {
            query: None,
            version: Some("v2".to_string()),
            filters: JsonMap::new(),
            top_k: Some(10),
            fields: None,
            rerank: None,
            keyword_search: None,
            filter_memories: None,
            threshold: None,
            org_id: None,
            project_id: None,
            user_id: None,
        }
    }
}

impl Mem0SearchRequest {
    /// Creates an empty search request with Java defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the query string.
    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.query = Some(query.into());
        self
    }

    /// Sets `top_k`.
    pub fn with_top_k(mut self, top_k: u32) -> Self {
        self.top_k = Some(top_k);
        self
    }

    /// Sets `user_id` and mirrors it into `filters`, matching Java
    /// `Builder.userId` / `setUserId`.
    pub fn with_user_id(mut self, user_id: impl Into<String>) -> Self {
        self.set_user_id(user_id);
        self
    }

    /// Sets `user_id` in place and mirrors it into `filters`, matching Java
    /// `Mem0SearchRequest.setUserId`.
    pub fn set_user_id(&mut self, user_id: impl Into<String>) -> &mut Self {
        let user_id = user_id.into();
        self.filters.insert(
            "user_id".to_string(),
            serde_json::Value::String(user_id.clone()),
        );
        self.user_id = Some(user_id);
        self
    }

    /// The configured user identifier.
    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }

    /// Re-applies the `user_id` → `filters` mirror.
    ///
    /// Needed after deserializing a request whose `filters` object omitted
    /// `user_id` (Serde cannot run Java's setter, and the Mem0 API requires the
    /// filter to isolate a tenant). [`crate::mem0::client::Mem0Client::search`] calls
    /// it before every request, so the wire payload can never lose the filter.
    pub fn sync_filters(&mut self) -> &mut Self {
        if let Some(user_id) = self.user_id.clone() {
            self.filters
                .insert("user_id".to_string(), serde_json::Value::String(user_id));
        }
        self
    }

    /// Sets the API version.
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// Sets the response `fields` projection.
    pub fn with_fields(mut self, fields: Vec<String>) -> Self {
        self.fields = Some(fields);
        self
    }

    /// Sets the `rerank` flag.
    pub fn with_rerank(mut self, rerank: bool) -> Self {
        self.rerank = Some(rerank);
        self
    }

    /// Sets the `keyword_search` flag.
    pub fn with_keyword_search(mut self, keyword_search: bool) -> Self {
        self.keyword_search = Some(keyword_search);
        self
    }

    /// Sets the `filter_memories` flag.
    pub fn with_filter_memories(mut self, filter_memories: bool) -> Self {
        self.filter_memories = Some(filter_memories);
        self
    }

    /// Sets the similarity `threshold`.
    pub fn with_threshold(mut self, threshold: f64) -> Self {
        self.threshold = Some(threshold);
        self
    }

    /// Sets the organisation identifier.
    pub fn with_org_id(mut self, org_id: impl Into<String>) -> Self {
        self.org_id = Some(org_id.into());
        self
    }

    /// Sets the project identifier.
    pub fn with_project_id(mut self, project_id: impl Into<String>) -> Self {
        self.project_id = Some(project_id.into());
        self
    }

    /// Adds `agent_id` to `filters`, matching Java `Builder.agentId`.
    pub fn with_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.filters.insert(
            "agent_id".to_string(),
            serde_json::Value::String(agent_id.into()),
        );
        self
    }

    /// Adds `run_id` to `filters`, matching Java `Builder.runId`.
    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.filters.insert(
            "run_id".to_string(),
            serde_json::Value::String(run_id.into()),
        );
        self
    }

    /// Adds `app_id` to `filters`, matching Java `Builder.appId`.
    pub fn with_app_id(mut self, app_id: impl Into<String>) -> Self {
        self.filters.insert(
            "app_id".to_string(),
            serde_json::Value::String(app_id.into()),
        );
        self
    }
}

/// Response of the Mem0 search API.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Mem0SearchResponse {
    /// Results ordered by relevance (highest score first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results: Option<Vec<Mem0SearchResult>>,
}

/// A single Mem0 memory search result (v2 shape).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Mem0SearchResult {
    /// Unique memory identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The memory text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    /// Owning user identifier.
    #[serde(rename = "user_id", default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Associated metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
    /// Categories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub categories: Option<Vec<String>>,
    /// Immutability flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub immutable: Option<bool>,
    /// Expiration timestamp (Java `OffsetDateTime`).
    #[serde(
        rename = "expiration_date",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub expiration_date: Option<DateTime<FixedOffset>>,
    /// Creation timestamp (Java `OffsetDateTime`).
    #[serde(
        rename = "created_at",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub created_at: Option<DateTime<FixedOffset>>,
    /// Update timestamp (Java `OffsetDateTime`).
    #[serde(
        rename = "updated_at",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub updated_at: Option<DateTime<FixedOffset>>,
    /// Relevance score returned by the v2 search API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// Structured temporal attributes extracted from the memory.
    #[serde(
        rename = "structured_attributes",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub structured_attributes: Option<JsonMap>,
}

impl Mem0SearchResult {
    /// Maps this result to a core [`Msg`] carrying the memory text.
    ///
    /// Retrieval output is fed back to the agent as an assistant-side memory note;
    /// the originating memory id / score / categories are preserved in metadata so
    /// callers can post-filter without a second round trip.
    pub fn to_msg(&self) -> Msg {
        let text = self.memory.clone().unwrap_or_default();
        let mut metadata = JsonMap::new();
        if let Some(id) = &self.id {
            metadata.insert(
                "memory_id".to_string(),
                serde_json::Value::String(id.clone()),
            );
        }
        if let Some(score) = self.score {
            metadata.insert(
                "score".to_string(),
                serde_json::Number::from_f64(score)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null),
            );
        }
        if let Some(categories) = &self.categories {
            if let Ok(value) = serde_json::to_value(categories) {
                metadata.insert("categories".to_string(), value);
            }
        }
        metadata.insert(
            "memory_source".to_string(),
            serde_json::Value::String("mem0".into()),
        );
        Msg::builder()
            .role(MsgRole::Assistant)
            .content(vec![ContentBlock::Text(TextBlock::new(text))])
            .metadata(metadata)
            .build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_serializes_name_only_when_present() {
        let without = Mem0Message::new("user", "hi");
        assert_eq!(
            serde_json::to_value(&without).unwrap(),
            serde_json::json!({"role": "user", "content": "hi"})
        );

        let with = Mem0Message::with_name("assistant", "yo", "bot");
        let json = serde_json::to_value(&with).unwrap();
        assert_eq!(json["name"], "bot");
        assert_eq!(serde_json::from_value::<Mem0Message>(json).unwrap(), with);
    }

    #[test]
    fn add_request_carries_java_defaults() {
        let req = Mem0AddRequest::new();
        assert_eq!(req.infer, Some(true));
        assert_eq!(req.output_format.as_deref(), Some("v1.1"));
        assert_eq!(req.async_mode, Some(true));
        assert_eq!(req.immutable, Some(false));
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["output_format"], "v1.1");
        assert!(json.get("messages").is_none());
    }

    #[test]
    fn add_request_serializes_snake_case_identifiers() {
        let req = Mem0AddRequest::new()
            .with_agent_id("a")
            .with_user_id("u")
            .with_run_id("r")
            .with_messages(vec![Mem0Message::new("user", "hello")]);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["agent_id"], "a");
        assert_eq!(json["user_id"], "u");
        assert_eq!(json["run_id"], "r");
        assert_eq!(json["messages"][0]["role"], "user");
        let back: Mem0AddRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn add_response_deserializes_results_and_message() {
        let resp: Mem0AddResponse =
            serde_json::from_str(r#"{"results":[{"id":"m1"}],"message":"ok"}"#).unwrap();
        assert_eq!(resp.results.as_ref().unwrap().len(), 1);
        assert_eq!(resp.message.as_deref(), Some("ok"));
        assert!(serde_json::to_value(&Mem0AddResponse::default())
            .unwrap()
            .is_object());
    }

    #[test]
    fn search_request_has_java_defaults_and_always_emits_filters() {
        let req = Mem0SearchRequest::new();
        assert_eq!(req.top_k, Some(10));
        assert_eq!(req.version.as_deref(), Some("v2"));
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["filters"], serde_json::json!({}));
        assert_eq!(json["top_k"], 10);
    }

    #[test]
    fn search_request_mirrors_identifiers_into_filters() {
        let req = Mem0SearchRequest::new()
            .with_query("q")
            .with_top_k(3)
            .with_user_id("u")
            .with_agent_id("a")
            .with_run_id("r")
            .with_app_id("app");
        assert_eq!(req.user_id(), Some("u"));
        assert_eq!(req.filters["user_id"], "u");
        assert_eq!(req.filters["agent_id"], "a");
        assert_eq!(req.filters["run_id"], "r");
        assert_eq!(req.filters["app_id"], "app");
        assert_eq!(req.top_k, Some(3));
        let back: Mem0SearchRequest =
            serde_json::from_value(serde_json::to_value(&req).unwrap()).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn api_type_from_string_is_case_insensitive_and_defaults_to_platform() {
        assert_eq!(Mem0ApiType::from_string(None), Mem0ApiType::Platform);
        assert_eq!(Mem0ApiType::from_string(Some("")), Mem0ApiType::Platform);
        assert_eq!(
            Mem0ApiType::from_string(Some("SELF-HOSTED")),
            Mem0ApiType::SelfHosted
        );
        assert_eq!(
            Mem0ApiType::from_string(Some("self_hosted")),
            Mem0ApiType::SelfHosted
        );
        assert_eq!(
            Mem0ApiType::from_string(Some("selfhosted")),
            Mem0ApiType::SelfHosted
        );
        assert_eq!(
            Mem0ApiType::from_string(Some("platform")),
            Mem0ApiType::Platform
        );
        assert_eq!(
            Mem0ApiType::from_string(Some("unknown")),
            Mem0ApiType::Platform
        );
        assert_eq!(Mem0ApiType::default(), Mem0ApiType::Platform);
    }

    #[test]
    fn search_response_accepts_missing_results() {
        let resp: Mem0SearchResponse = serde_json::from_str("{}").unwrap();
        assert!(resp.results.is_none());
    }

    #[test]
    fn search_result_serde_roundtrips_timestamps() {
        let json = serde_json::json!({
            "id": "m1",
            "memory": "x",
            "user_id": "u",
            "created_at": "2024-01-02T03:04:05+00:00",
            "score": 0.5
        });
        let result: Mem0SearchResult = serde_json::from_value(json).unwrap();
        assert_eq!(result.user_id.as_deref(), Some("u"));
        assert!(result.created_at.is_some());
        let back = serde_json::to_value(&result).unwrap();
        // chrono normalizes an explicit +00:00 offset to the `Z` UTC form on serialization.
        assert_eq!(back["created_at"], "2024-01-02T03:04:05Z");
    }

    #[test]
    fn search_result_maps_to_core_msg_with_metadata() {
        let result = Mem0SearchResult {
            id: Some("m1".into()),
            memory: Some("remember this".into()),
            score: Some(0.75),
            categories: Some(vec!["pref".into()]),
            ..Default::default()
        };
        let msg = result.to_msg();
        assert_eq!(msg.role(), MsgRole::Assistant);
        assert_eq!(msg.text_content(), "remember this");
        assert_eq!(msg.metadata()["memory_id"], "m1");
        assert_eq!(msg.metadata()["memory_source"], "mem0");
        assert_eq!(msg.metadata()["categories"], serde_json::json!(["pref"]));
        assert!((msg.metadata()["score"].as_f64().unwrap() - 0.75).abs() < 1e-9);
    }

    #[test]
    fn search_result_without_memory_maps_to_empty_text() {
        let msg = Mem0SearchResult::default().to_msg();
        assert_eq!(msg.text_content(), "");
        assert_eq!(msg.metadata()["memory_source"], "mem0");
        assert!(msg.metadata().get("memory_id").is_none());
    }

    /// Covers Java `Mem0AddRequestTest` builder tests
    /// (`testBuilderWithCustomCategories` / `testBuilderWithTimestampAndExpiration` /
    /// `testBuilderWithImmutableAndAsyncMode` / `testBuilderWithOrgAndProject` /
    /// `testBuilderWithVersion` / `testBuilderWithDeprecatedMemoryType` /
    /// `testBuilderWithIncludesAndExcludes` / `testBuilderWithCustomInstructions`)
    /// as one field-by-field assertion over the serialized payload.
    #[test]
    fn add_request_builder_covers_every_java_setter() {
        let mut categories = JsonMap::new();
        categories.insert("food".into(), serde_json::json!("likes"));
        let req = Mem0AddRequest::new()
            .with_messages(vec![Mem0Message::new("user", "hi")])
            .with_agent_id("a")
            .with_user_id("u")
            .with_app_id("app")
            .with_run_id("r")
            .with_metadata(JsonMap::new())
            .with_includes("inc")
            .with_excludes("exc")
            .with_output_format("v1.0")
            .with_custom_categories(categories)
            .with_custom_instructions("be nice")
            .with_immutable(true)
            .with_async_mode(false)
            .with_timestamp(1_700_000_000)
            .with_expiration_date("2026-12-31")
            .with_org_id("org")
            .with_project_id("proj")
            .with_version("v1")
            .with_memory_type("factual");
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["agent_id"], "a");
        assert_eq!(json["user_id"], "u");
        assert_eq!(json["app_id"], "app");
        assert_eq!(json["run_id"], "r");
        assert_eq!(json["includes"], "inc");
        assert_eq!(json["excludes"], "exc");
        assert_eq!(json["output_format"], "v1.0");
        assert_eq!(json["custom_categories"]["food"], "likes");
        assert_eq!(json["custom_instructions"], "be nice");
        assert_eq!(json["immutable"], true);
        assert_eq!(json["async_mode"], false);
        assert_eq!(json["timestamp"], 1_700_000_000);
        assert_eq!(json["expiration_date"], "2026-12-31");
        assert_eq!(json["org_id"], "org");
        assert_eq!(json["project_id"], "proj");
        assert_eq!(json["version"], "v1");
        assert_eq!(json["memory_type"], "factual");
        // `infer` keeps the Java constructor default unless overridden.
        assert_eq!(json["infer"], true);
    }

    /// Covers Java `Mem0SearchRequestTest`
    /// (`testBuilderWithFields` / `testBuilderWithRerank` / `testBuilderWithKeywordSearch` /
    /// `testBuilderWithFilterMemories` / `testBuilderWithThreshold` /
    /// `testBuilderWithOrgAndProject` / `testBuilderWithVersion`) with one
    /// field-by-field assertion.
    #[test]
    fn search_request_builder_covers_every_java_setter() {
        let req = Mem0SearchRequest::new()
            .with_query("q")
            .with_version("v1")
            .with_fields(vec!["memory".into()])
            .with_rerank(true)
            .with_keyword_search(true)
            .with_filter_memories(false)
            .with_threshold(0.4)
            .with_org_id("org")
            .with_project_id("proj");
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["version"], "v1");
        assert_eq!(json["fields"], serde_json::json!(["memory"]));
        assert_eq!(json["rerank"], true);
        assert_eq!(json["keyword_search"], true);
        assert_eq!(json["filter_memories"], false);
        assert!((json["threshold"].as_f64().unwrap() - 0.4).abs() < 1e-9);
        assert_eq!(json["org_id"], "org");
        assert_eq!(json["project_id"], "proj");
    }

    /// Java `Mem0SearchRequestTest.testBuilderWithUserIdFilter`: the user id must
    /// never exist without the matching `filters.user_id`, including when the caller
    /// mutates the request in place (`set_user_id`) or deserializes a payload whose
    /// `filters` object is empty (`sync_filters`).
    #[test]
    fn search_request_keeps_user_id_mirrored_into_filters() {
        let mut req = Mem0SearchRequest::new();
        req.set_user_id("u");
        assert_eq!(req.user_id(), Some("u"));
        assert_eq!(req.filters["user_id"], "u");

        let mut deserialized: Mem0SearchRequest =
            serde_json::from_value(serde_json::json!({"user_id": "u", "filters": {}})).unwrap();
        assert!(deserialized.filters.get("user_id").is_none());
        deserialized.sync_filters();
        assert_eq!(deserialized.filters["user_id"], "u");
        assert_eq!(
            serde_json::to_value(&deserialized).unwrap()["filters"]["user_id"],
            "u"
        );
    }
}
