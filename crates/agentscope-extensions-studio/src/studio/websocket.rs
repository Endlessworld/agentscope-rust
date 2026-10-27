//! WebSocket client for real-time communication with Studio.
//!
//! Java uses Socket.IO against the `/python` namespace. Rust has no Socket.IO
//! client in this workspace, so the wire transport is abstracted behind
//! [`StudioTransport`] and the event handling / pending-request bookkeeping is
//! implemented here as pure, testable logic.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{oneshot, Mutex};

use agentscope_core::message::{ContentBlock, JsonMap, ToolResultBlock, ToolResultState};

use crate::error::{StudioError, StudioResult};
use crate::studio::config::StudioConfig;

/// The Studio event name carrying user input over the socket.
pub const EVENT_FORWARD_USER_INPUT: &str = "forwardUserInput";

/// User input delivered by Studio, containing content blocks plus optional
/// structured form data.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UserInputData {
    /// Content blocks provided by the user.
    pub blocks_input: Vec<ContentBlock>,
    /// Optional structured data (e.g. form fields).
    pub structured_input: Option<JsonMap>,
}

impl UserInputData {
    /// Creates new user input data.
    pub fn new(blocks_input: Vec<ContentBlock>, structured_input: Option<JsonMap>) -> Self {
        Self {
            blocks_input,
            structured_input,
        }
    }
}

/// Abstraction over the real-time socket carrying Studio events.
///
/// A Socket.IO implementation (or a plain WebSocket adapter) can implement this
/// trait without the client above needing to know the wire protocol.
#[async_trait::async_trait]
pub trait StudioTransport: Send + Sync {
    /// Establishes the connection (with authentication via `run_id`).
    async fn connect(&self) -> StudioResult<()>;

    /// Closes the connection, releasing resources.
    async fn close(&self) -> StudioResult<()>;

    /// Whether the transport currently holds an open connection.
    fn is_connected(&self) -> bool;
}

/// A no-op transport used when Studio is unavailable or in tests.
#[derive(Debug, Default)]
pub struct NoopStudioTransport;

#[async_trait::async_trait]
impl StudioTransport for NoopStudioTransport {
    async fn connect(&self) -> StudioResult<()> {
        Ok(())
    }
    async fn close(&self) -> StudioResult<()> {
        Ok(())
    }
    fn is_connected(&self) -> bool {
        false
    }
}

/// Tracks in-flight `waitForInput` requests by id.
#[derive(Debug, Default)]
pub struct PendingInputRegistry {
    pending: Mutex<HashMap<String, oneshot::Sender<UserInputData>>>,
}

impl PendingInputRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a pending request, returning the receiver to await on.
    ///
    /// If an entry for `request_id` already exists it is replaced (the old
    /// sender is dropped, causing its receiver to observe a closed channel).
    pub async fn register(
        &self,
        request_id: impl Into<String>,
    ) -> oneshot::Receiver<UserInputData> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(request_id.into(), tx);
        rx
    }

    /// Resolves a pending request with the given input, returning `true` if a
    /// matching request existed.
    pub async fn resolve(&self, request_id: &str, data: UserInputData) -> bool {
        match self.pending.lock().await.remove(request_id) {
            Some(tx) => tx.send(data).is_ok(),
            None => false,
        }
    }

    /// Removes a pending request without resolving it.
    pub async fn remove(&self, request_id: &str) {
        self.pending.lock().await.remove(request_id);
    }

    /// Number of pending requests currently registered.
    pub async fn len(&self) -> usize {
        self.pending.lock().await.len()
    }

    /// Whether there are no pending requests.
    pub async fn is_empty(&self) -> bool {
        self.pending.lock().await.is_empty()
    }
}

/// WebSocket client for Studio, exposing `wait_for_input` semantics on top of a
/// pluggable [`StudioTransport`].
#[derive(Clone)]
pub struct StudioWebSocketClient {
    config: StudioConfig,
    transport: Arc<dyn StudioTransport>,
    registry: Arc<PendingInputRegistry>,
}

impl std::fmt::Debug for StudioWebSocketClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The transport is a trait object without `Debug`; report it opaquely.
        f.debug_struct("StudioWebSocketClient")
            .field("config", &self.config)
            .field("connected", &self.transport.is_connected())
            .finish_non_exhaustive()
    }
}

impl StudioWebSocketClient {
    /// Creates a client using the given transport.
    pub fn with_transport(config: StudioConfig, transport: Arc<dyn StudioTransport>) -> Self {
        Self::with_transport_and_registry(config, transport, Arc::new(PendingInputRegistry::new()))
    }

    /// Creates a client over a transport that resolves its own pending requests.
    ///
    /// Use this when the transport already owns a [`PendingInputRegistry`] it resolves incoming
    /// `forwardUserInput` events into (e.g. `SocketIoTransport`).
    pub fn with_transport_and_registry(
        config: StudioConfig,
        transport: Arc<dyn StudioTransport>,
        registry: Arc<PendingInputRegistry>,
    ) -> Self {
        Self {
            config,
            transport,
            registry,
        }
    }

    /// Creates a client with a no-op transport (useful when Studio is not wired).
    pub fn disconnected(config: StudioConfig) -> Self {
        Self::with_transport(config, Arc::new(NoopStudioTransport))
    }

    /// Returns the configuration backing this client.
    pub fn config(&self) -> &StudioConfig {
        &self.config
    }

    /// Connects to Studio's WebSocket endpoint with a 10s timeout.
    pub async fn connect(&self) -> StudioResult<()> {
        match tokio::time::timeout(Duration::from_secs(10), self.transport.connect()).await {
            Ok(result) => result,
            Err(_) => Err(StudioError::WebSocket("connection timed out".to_string())),
        }
    }

    /// Waits for user input matching `request_id`.
    ///
    /// Call this after `StudioClient::request_user_input`. The corresponding
    /// `forwardUserInput` event resolves the returned future.
    pub async fn wait_for_input(
        &self,
        request_id: &str,
        timeout: Duration,
    ) -> StudioResult<UserInputData> {
        let receiver = self.registry.register(request_id).await;
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(_)) => {
                self.registry.remove(request_id).await;
                Err(StudioError::WebSocket(
                    "pending request dropped".to_string(),
                ))
            }
            Err(_) => {
                self.registry.remove(request_id).await;
                Err(StudioError::InputTimeout(request_id.to_string()))
            }
        }
    }

    /// Handles a raw `forwardUserInput` event: `[requestId, blocksInput, structuredInput]`.
    ///
    /// Parses the payload and resolves the matching pending request, returning
    /// whether a request was waiting.
    pub async fn handle_user_input(
        &self,
        request_id: &str,
        blocks_input: &serde_json::Value,
        structured_input: &serde_json::Value,
    ) -> bool {
        let data = parse_user_input(blocks_input, structured_input);
        self.registry.resolve(request_id, data).await
    }

    /// Closes the underlying transport.
    pub async fn close(&self) -> StudioResult<()> {
        self.transport.close().await
    }

    /// Whether the underlying transport is connected.
    pub fn is_connected(&self) -> bool {
        self.transport.is_connected()
    }
}

/// Parses a `forwardUserInput` payload into a [`UserInputData`].
///
/// `blocks_input` must be a JSON array of content blocks (as produced by the
/// Studio TypeScript UI); `structured_input` may be a JSON object or `null`.
pub fn parse_user_input(
    blocks_input: &serde_json::Value,
    structured_input: &serde_json::Value,
) -> UserInputData {
    let blocks = parse_content_blocks(blocks_input);
    let structured = match structured_input {
        serde_json::Value::Object(map) => Some(map.clone()),
        _ => None,
    };
    UserInputData::new(blocks, structured)
}

/// Parses a JSON array of content blocks, applying the tool-result output
/// normalization (string outputs are wrapped into a text block).
pub fn parse_content_blocks(value: &serde_json::Value) -> Vec<ContentBlock> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    let mut blocks = Vec::with_capacity(items.len());
    for item in items {
        let normalized = preprocess_tool_result_block(item.clone());
        match serde_json::from_value::<ContentBlock>(normalized) {
            Ok(block) => blocks.push(block),
            Err(err) => {
                tracing::error!(error = %err, "failed to parse ContentBlock from studio");
            }
        }
    }
    blocks
}

/// Normalizes a tool-result block so its `output` is always an array of content
/// blocks. Mirrors `StudioWebSocketClient#preprocessToolResultBlock`.
pub fn preprocess_tool_result_block(mut block: serde_json::Value) -> serde_json::Value {
    let is_tool_result = block
        .get("type")
        .and_then(|t| t.as_str())
        .map(|t| t == "tool_result")
        .unwrap_or(false);
    if !is_tool_result {
        return block;
    }
    if let Some(output) = block.get("output") {
        if let Some(text) = output.as_str() {
            let text_block = serde_json::json!({ "type": "text", "text": text });
            if let Some(obj) = block.as_object_mut() {
                obj.insert(
                    "output".to_string(),
                    serde_json::Value::Array(vec![text_block]),
                );
            }
        }
    }
    block
}

/// Extracts the tool-result blocks from a set of parsed blocks and fills in the
/// given id/name, mirroring the Java normalisation of `ToolResultBlock`.
pub fn normalize_tool_result(id: &str, name: &str, output: Vec<ContentBlock>) -> ToolResultBlock {
    ToolResultBlock::new(id, name, output).with_state(ToolResultState::Success)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::{TextBlock, ThinkingBlock};

    #[test]
    fn parse_content_blocks_maps_text_and_thinking() {
        let value = serde_json::json!([
            { "type": "text", "text": "hi" },
            { "type": "thinking", "thinking": "hmm" }
        ]);
        let blocks = parse_content_blocks(&value);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], ContentBlock::Text(TextBlock::new("hi")));
        assert_eq!(blocks[1], ContentBlock::Thinking(ThinkingBlock::new("hmm")));
    }

    #[test]
    fn parse_content_blocks_ignores_non_array() {
        assert!(parse_content_blocks(&serde_json::json!({ "type": "text" })).is_empty());
    }

    #[test]
    fn preprocess_wraps_string_tool_result_output() {
        let block = serde_json::json!({
            "type": "tool_result",
            "id": "t1",
            "name": "calc",
            "output": "42"
        });
        let normalized = preprocess_tool_result_block(block);
        assert_eq!(
            normalized["output"],
            serde_json::json!([{ "type": "text", "text": "42" }])
        );
    }

    #[test]
    fn preprocess_leaves_non_tool_result_untouched() {
        let block = serde_json::json!({ "type": "text", "text": "42" });
        let same = block.clone();
        assert_eq!(preprocess_tool_result_block(block), same);
    }

    #[test]
    fn parse_user_input_splits_blocks_and_structured() {
        let blocks = serde_json::json!([{ "type": "text", "text": "answer" }]);
        let structured = serde_json::json!({ "field": "value" });
        let data = parse_user_input(&blocks, &structured);
        assert_eq!(data.blocks_input.len(), 1);
        assert_eq!(data.structured_input.as_ref().unwrap()["field"], "value");

        let empty = parse_user_input(&blocks, &serde_json::Value::Null);
        assert!(empty.structured_input.is_none());
    }

    #[test]
    fn normalize_tool_result_marks_success() {
        let out =
            normalize_tool_result("t1", "calc", vec![ContentBlock::Text(TextBlock::new("42"))]);
        assert_eq!(out.id.as_deref(), Some("t1"));
        assert_eq!(out.name.as_deref(), Some("calc"));
        assert_eq!(out.state, ToolResultState::Success);
        assert_eq!(out.output.len(), 1);
    }

    #[tokio::test]
    async fn pending_registry_resolves_waiter() {
        let reg = PendingInputRegistry::new();
        let rx = reg.register("req-1").await;
        let payload = UserInputData::new(vec![ContentBlock::Text(TextBlock::new("go"))], None);
        assert!(reg.resolve("req-1", payload.clone()).await);
        assert_eq!(rx.await.unwrap(), payload);
        assert!(reg.is_empty().await);
    }

    #[tokio::test]
    async fn pending_registry_unknown_request_returns_false() {
        let reg = PendingInputRegistry::new();
        assert!(!reg.resolve("nope", UserInputData::default()).await);
    }

    #[tokio::test]
    async fn websocket_client_with_noop_transport_is_disconnected() {
        let config = StudioConfig::builder().build();
        let client = StudioWebSocketClient::with_transport(config, Arc::new(NoopStudioTransport));
        assert!(!client.is_connected());
        client.connect().await.unwrap();
        client.close().await.unwrap();
    }

    #[tokio::test]
    async fn wait_for_input_times_out_without_event() {
        let config = StudioConfig::builder().build();
        let client = StudioWebSocketClient::with_transport(config, Arc::new(NoopStudioTransport));
        let err = client
            .wait_for_input("missing", Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(matches!(err, StudioError::InputTimeout(_)));
    }

    #[tokio::test]
    async fn handle_user_input_resolves_waiter() {
        let config = StudioConfig::builder().build();
        let client = Arc::new(StudioWebSocketClient::with_transport(
            config,
            Arc::new(NoopStudioTransport),
        ));
        let waiter = {
            let c = client.clone();
            tokio::spawn(async move { c.wait_for_input("req-7", Duration::from_secs(1)).await })
        };
        // Let the spawned task register its pending request.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let blocks = serde_json::json!([{ "type": "text", "text": "typed" }]);
        assert!(
            client
                .handle_user_input("req-7", &blocks, &serde_json::Value::Null)
                .await
        );
        let data = waiter.await.unwrap().unwrap();
        assert_eq!(
            data.blocks_input,
            vec![ContentBlock::Text(TextBlock::new("typed"))]
        );
    }
}
