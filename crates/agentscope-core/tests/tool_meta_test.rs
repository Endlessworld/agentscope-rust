//! Integration tests for the core tool meta module: ToolCallParam, ExtendedModel
//! schema merging, and the `reset_equipped_tools` meta tool (MetaToolFactory).
use std::sync::Arc;

use agentscope_core::{
    ExtendedModel, FunctionTool, JsonMap, SimpleExtendedModel, ToolCallParam,
    ToolExecutionContext, ToolGroupScope, ToolResultBlock, ToolUseBlock, Toolkit,
};

#[test]
fn tool_call_param_builder_and_accessors() {
    let block = ToolUseBlock::new("call-1", "web_search", JsonMap::new());
    let mut input = JsonMap::new();
    input.insert("query".to_string(), serde_json::json!("rust"));
    let param = ToolCallParam::builder()
        .tool_use_block(block.clone())
        .input(input.clone())
        .agent_id("agent-42")
        .build();
    assert_eq!(param.tool_use_block().unwrap().id, "call-1");
    assert_eq!(param.input().get("query").unwrap(), &serde_json::json!("rust"));
    assert_eq!(param.agent_id(), Some("agent-42"));
    assert!(param.emitter().is_none());
    // Convenience constructor.
    let p2 = ToolCallParam::from(block.clone());
    assert_eq!(p2.tool_use_block().unwrap().name, "web_search");
}

#[test]
fn simple_extended_model_merges_properties_and_required() {
    let mut props = JsonMap::new();
    props.insert("timeout".to_string(), serde_json::json!({"type": "integer"}));
    let model = SimpleExtendedModel::new(props, vec!["timeout".to_string()]);
    let mut base = JsonMap::new();
    let mut base_props = JsonMap::new();
    base_props.insert("cmd".to_string(), serde_json::json!({"type": "string"}));
    base.insert("properties".to_string(), serde_json::Value::Object(base_props));
    let merged = model.merge_with_base_schema(base).expect("merge succeeds");
    let props = merged["properties"].as_object().unwrap();
    assert!(props.contains_key("cmd"));
    assert!(props.contains_key("timeout"));
    let required = merged["required"].as_array().unwrap();
    assert!(required.iter().any(|r| r == "timeout"));
}

#[test]
fn simple_extended_model_detects_conflicts() {
    let mut props = JsonMap::new();
    props.insert("cmd".to_string(), serde_json::json!({"type": "string"}));
    let model = SimpleExtendedModel::new(props, vec![]);
    let mut base = JsonMap::new();
    let mut base_props = JsonMap::new();
    base_props.insert("cmd".to_string(), serde_json::json!({"type": "string"}));
    base.insert("properties".to_string(), serde_json::Value::Object(base_props));
    let err = model.merge_with_base_schema(base).unwrap_err();
    assert!(err.to_string().contains("conflicting"));
}

#[tokio::test]
async fn reset_equipped_tools_replaces_meta_active_groups() {
    let tk = Toolkit::new();
    tk.create_tool_group_with_scope("coding", "code tools", true, ToolGroupScope::Meta);
    tk.create_tool_group_with_scope("web", "web tools", false, ToolGroupScope::Meta);
    tk.create_tool_group_with_scope(
        "ext",
        "external",
        true,
        ToolGroupScope::External,
    );
    // A grouped tool to observe activation through schema filtering.
    tk.register_tool(Arc::new(FunctionTool::new(
        "fmt",
        "format code",
        serde_json::json!({"type": "object"}),
        Box::new(|_, _| Ok(ToolResultBlock::text("ok"))),
    )));
    tk.add_tool_to_group("coding", "fmt");

    tk.register_meta_tools();
    assert!(tk.tool_names().contains(&"reset_equipped_tools".to_string()));

    let mut input = JsonMap::new();
    input.insert("to_activate".to_string(), serde_json::json!(["coding"]));
    let call = ToolUseBlock::new("m1", "reset_equipped_tools", input);
    let rb = tk
        .call_tool(&call, ToolExecutionContext::default())
        .await
        .expect("reset succeeds");
    assert!(rb.text_content().contains("coding"));

    let active = tk.get_active_groups();
    assert!(active.contains(&"coding".to_string()), "coding activated: {active:?}");
    assert!(!active.contains(&"web".to_string()), "web deactivated: {active:?}");
    assert!(active.contains(&"ext".to_string()), "external unaffected: {active:?}");
    // The grouped tool becomes active again once its group is on.
    assert!(tk.is_tool_active("fmt"));
}

#[tokio::test]
async fn reset_equipped_tools_rejects_external_group() {
    let tk = Toolkit::new();
    tk.create_tool_group_with_scope(
        "ext",
        "external",
        true,
        ToolGroupScope::External,
    );
    tk.register_meta_tools();
    let mut input = JsonMap::new();
    input.insert("to_activate".to_string(), serde_json::json!(["ext"]));
    let call = ToolUseBlock::new("m1", "reset_equipped_tools", input);
    let rb = tk
        .call_tool(&call, ToolExecutionContext::default())
        .await
        .expect("returns a result block");
    assert!(rb.text_content().contains("not manageable"));
}

#[test]
fn reset_tool_schema_lists_meta_groups() {
    let tk = Toolkit::new();
    tk.create_tool_group_with_scope("coding", "code", false, ToolGroupScope::Meta);
    tk.create_tool_group_with_scope(
        "ext",
        "external",
        false,
        ToolGroupScope::External,
    );
    tk.register_meta_tools();
    let schema = tk
        .get_tool("reset_equipped_tools")
        .expect("meta tool registered")
        .schema();
    let items = &schema.parameters["properties"]["to_activate"]["items"];
    let enum_list = items["enum"].as_array().expect("meta groups in enum");
    assert!(enum_list.iter().any(|v| v == "coding"));
    assert!(!enum_list.iter().any(|v| v == "ext"), "external groups not in enum");
}
