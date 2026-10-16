//! Integration tests for the core `model` and `formatter` modules.
use agentscope_core::{
    ChatResponse, ContentBlock, ExecutionConfig, GenerateOptions, JsonSchema, Msg, MsgRole,
    ResponseFormat, TextBlock, ToolChoice, ToolSchema,
};

#[test]
fn tool_choice_specific_validation() {
    let c = ToolChoice::specific("generate_response");
    assert_eq!(c, ToolChoice::Specific { tool_name: "generate_response".into() });
}

#[test]
fn tool_choice_serialize_roundtrip() {
    let c = ToolChoice::Specific { tool_name: "foo".into() };
    let json = serde_json::to_string(&c).unwrap();
    assert_eq!(json, r#"{"type":"specific","tool_name":"foo"}"#);
    let back: ToolChoice = serde_json::from_str(&json).unwrap();
    assert_eq!(back, c);
}

#[test]
fn tool_schema_builder() {
    let schema = ToolSchema::new("add", "Add two numbers")
        .parameters(serde_json::json!({"type": "object", "properties": {"a": {"type": "number"}}}).as_object().unwrap().clone())
        .strict(true);
    assert_eq!(schema.name, "add");
    assert_eq!(schema.strict, Some(true));
    assert!(schema.parameters.contains_key("type"));
}

#[test]
fn chat_response_auto_id() {
    let r = ChatResponse::builder()
        .content(vec![ContentBlock::Text(TextBlock::new("hi"))])
        .usage(Default::default())
        .build();
    assert!(!r.id.is_empty(), "id should be auto-generated");
    assert_eq!(r.content.len(), 1);
}

#[test]
fn chat_response_serialize_roundtrip() {
    let r = ChatResponse::builder()
        .id("resp-1")
        .content(vec![ContentBlock::Text(TextBlock::new("hello"))])
        .usage(Default::default())
        .finish_reason("stop")
        .build();
    let json = serde_json::to_string(&r).unwrap();
    assert!(json.contains("\"id\":\"resp-1\""));
    let back: ChatResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(back.id, "resp-1");
    assert_eq!(back.finish_reason.as_deref(), Some("stop"));
}

#[test]
fn generate_options_merge_precedence() {
    let primary = GenerateOptions::builder().temperature(0.1).build();
    let fallback = GenerateOptions::builder().temperature(0.7).top_p(0.9).build();
    let merged = GenerateOptions::merge_options(&primary, &fallback);
    assert_eq!(merged.temperature, Some(0.1));
    assert_eq!(merged.top_p, Some(0.9));
}

#[test]
fn execution_config_merge() {
    let primary = ExecutionConfig::builder().timeout(std::time::Duration::from_secs(30)).build();
    let fallback = ExecutionConfig::model_defaults();
    let merged = ExecutionConfig::merge_configs(Some(&primary), Some(&fallback)).unwrap();
    assert_eq!(merged.timeout, Some(std::time::Duration::from_secs(30)));
    assert_eq!(merged.max_attempts, Some(7));
}

#[test]
fn model_defaults_backoff_reaches_a_minute() {
    let cfg = ExecutionConfig::model_defaults();
    // Six waits (2s, 4s, 8s, 16s, 32s, then capped at 60s) before giving up.
    let waits: Vec<u64> = (0..6).map(|a| cfg.backoff_for(a).as_secs()).collect();
    assert_eq!(waits, vec![2, 4, 8, 16, 32, 60]);
    assert_eq!(cfg.max_attempts, Some(7));
}

#[test]
fn execution_config_backoff() {
    let cfg = ExecutionConfig::builder()
        .initial_backoff(std::time::Duration::from_secs(1))
        .max_backoff(std::time::Duration::from_secs(8))
        .backoff_multiplier(2.0)
        .build();
    assert_eq!(cfg.backoff_for(0), std::time::Duration::from_secs(1));
    assert_eq!(cfg.backoff_for(1), std::time::Duration::from_secs(2));
    // capped at max 8s
    assert_eq!(cfg.backoff_for(5), std::time::Duration::from_secs(8));
}

#[test]
fn response_format_serialize() {
    let schema = JsonSchema::builder().name("MathResponse").schema(Default::default()).strict(true).build();
    let fmt = ResponseFormat::json_schema(schema);
    let json = serde_json::to_string(&fmt).unwrap();
    assert!(json.contains("\"type\":\"json_schema\""));
    let back: ResponseFormat = serde_json::from_str(&json).unwrap();
    assert_eq!(back.format_type, "json_schema");
    assert!(back.json_schema.is_some());
}

#[test]
fn msg_json_roundtrip_with_tool_result() {
    let msg = Msg::assistant("Hello world");
    let json = serde_json::to_string(&msg).unwrap();
    let back: Msg = serde_json::from_str(&json).unwrap();
    assert_eq!(back.role(), MsgRole::Assistant);
    assert_eq!(back.text_content(), "Hello world");
}
