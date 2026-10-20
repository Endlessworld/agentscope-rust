//! Integration tests for the core `tool` and `event` modules.
use std::sync::Arc;

use agentscope_core::{
    AgentEvent, AgentEventType, FunctionTool, JsonMap, Tool, ToolExecutionContext, Toolkit,
    ToolResultBlock, ToolSchema, ToolUseBlock,
};

fn parse_args(json: &str) -> JsonMap {
    serde_json::from_str(json).unwrap()
}

#[tokio::test]
async fn function_tool_call() {
    let tool = Arc::new(FunctionTool::new(
        "add",
        "Add two numbers",
        serde_json::json!({"type": "object", "properties": {"a": {"type": "number"}}}),
        Box::new(|args, _ctx| {
            let a = args.get("a").and_then(|v| v.as_i64()).unwrap_or(0);
            Ok(ToolResultBlock::text(format!("result={}", a + 1)))
        }),
    ));
    assert_eq!(tool.name(), "add");

    let result = tool
        .call(parse_args(r#"{"a": 41}"#), ToolExecutionContext::default())
        .await
        .unwrap();
    let text = result.text_content();
    assert_eq!(text, "result=42");
}

#[test]
fn tool_schema_generation() {
    let tool = FunctionTool::new(
        "add",
        "Add two numbers",
        serde_json::json!({"type": "object", "properties": {"a": {"type": "number"}}}),
        Box::new(|_, _| Ok(ToolResultBlock::text("ok"))),
    );
    let schema = tool.schema();
    assert_eq!(schema.name, "add");
    assert_eq!(schema.parameters.get("type").unwrap(), "object");
}

#[tokio::test]
async fn toolkit_register_and_call() {
    let tk = Toolkit::new();
    let tool = Arc::new(FunctionTool::new(
        "greet",
        "Greet a user",
        serde_json::json!({"type": "object", "properties": {"name": {"type": "string"}}}),
        Box::new(|args, _| {
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("world");
            Ok(ToolResultBlock::text(format!("Hello, {name}")))
        }),
    ));
    tk.register_tool(tool);
    assert!(tk.tool_names().contains(&"greet".to_string()));
    assert_eq!(tk.tool_schemas().len(), 1);

    let call = ToolUseBlock::new("call-1", "greet", parse_args(r#"{"name": "Alice"}"#));
    let result = tk
        .call_tool(&call, ToolExecutionContext::default())
        .await
        .unwrap();
    assert_eq!(result.text_content(), "Hello, Alice");
}

#[tokio::test]
async fn toolkit_external_schema_tool() {
    let tk = Toolkit::new();
    let schema = ToolSchema::new("query_db", "Query external DB")
        .parameters(parse_args(r#"{"type": "object", "properties": {"sql": {"type": "string"}}}"#));
    tk.register_schema(schema);
    assert!(tk.is_external_tool("query_db"));
    let call = ToolUseBlock::new("call-1", "query_db", parse_args(r#"{"sql": "select 1"}"#));
    let err = tk.call_tool(&call, ToolExecutionContext::default()).await.unwrap_err();
    assert!(err.to_string().contains("external"));
}

#[test]
fn event_type_wire_names() {
    assert_eq!(AgentEventType::AgentStart.wire_name(), "AGENT_START");
    assert_eq!(AgentEventType::ToolResultDataDelta.wire_name(), "TOOL_RESULT_DATA_DELTA");
    assert_eq!(AgentEventType::from_wire_name("RUN_STARTED").unwrap(), AgentEventType::AgentStart);
    assert_eq!(
        AgentEventType::from_wire_name("BINARY_BLOCK_START").unwrap(),
        AgentEventType::DataBlockStart
    );
    assert!(AgentEventType::from_wire_name("UNKNOWN").is_none());
}

#[test]
fn tool_progress_wire_name_round_trips() {
    // Mirror of Java `AgentEventType.TOOL_PROGRESS`: a distinct type now that progress
    // no longer borrows `TOOL_RESULT_DATA_DELTA`.
    assert_eq!(AgentEventType::ToolProgress.wire_name(), "TOOL_PROGRESS");
    assert_eq!(
        AgentEventType::from_wire_name("TOOL_PROGRESS"),
        Some(AgentEventType::ToolProgress)
    );
    // The legacy alias still resolves to the data delta, i.e. the two are distinct.
    assert_eq!(
        AgentEventType::from_wire_name("TOOL_RESULT_DATA_DELTA"),
        Some(AgentEventType::ToolResultDataDelta)
    );

    let ev = AgentEvent::with_fields(AgentEventType::ToolProgress, JsonMap::new());
    let json = serde_json::to_string(&ev).unwrap();
    assert!(
        json.contains("\"type\":\"TOOL_PROGRESS\""),
        "unexpected serialization: {json}"
    );
    let back: AgentEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(back.event_type(), AgentEventType::ToolProgress);
    assert!(matches!(back, AgentEvent::ToolProgress(_)));
}

#[test]
fn agent_event_creation_and_metadata() {
    let ev = AgentEvent::with_fields(AgentEventType::TextBlockDelta, parse_args(r#"{"text": "hi"}"#))
        .with_source("main/researcher")
        .with_metadata("k", serde_json::Value::from(1));
    assert_eq!(ev.event_type(), AgentEventType::TextBlockDelta);
    assert_eq!(ev.source(), Some("main/researcher"));
    assert_eq!(ev.data().fields.get("text").unwrap(), "hi");
    assert_eq!(ev.metadata().get("k").unwrap(), 1);
}

#[test]
fn agent_event_serialize_roundtrip() {
    let ev = AgentEvent::with_fields(AgentEventType::AgentStart, JsonMap::new());
    let json = serde_json::to_string(&ev).unwrap();
    assert!(json.contains("\"type\":\"AGENT_START\""));
    let back: AgentEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(back.event_type(), AgentEventType::AgentStart);
}
