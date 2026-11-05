//! `#[tool]` 宏的集成测试：schema 生成、参数绑定、返回值与错误路径。
use agentscope_core::message::JsonMap;
use agentscope_core::tool::{FunctionTool, Tool, ToolExecutionContext};
use agentscope_macros::tool;
use serde_json::{json, Value};

/// 把两个整数相加。
#[tool]
fn add(a: i64, b: i64) -> String {
    format!("{}", a + b)
}

/// 打招呼。
#[tool(
    name = "greet",
    description = "按名字打招呼",
    read_only,
    strict,
    concurrency_safe = false
)]
fn greet(
    #[tool(description = "要打招呼的人")] name: String,
    excited: Option<bool>,
    tags: Vec<String>,
) -> String {
    let bang = if excited.unwrap_or(false) { "!" } else { "" };
    format!("hi {name}{bang} [{}]", tags.join(","))
}

#[tool]
fn divide(a: i64, b: i64) -> Result<String, String> {
    if b == 0 {
        Err("division by zero".to_string())
    } else {
        Ok(format!("{}", a / b))
    }
}

#[tool]
fn echo_any(payload: Value) -> String {
    payload.to_string()
}

/// 借用参数：`&str` 与 `String` 的 schema 相同。
#[tool]
fn shout(text: &str) -> String {
    text.to_uppercase()
}

fn args(value: Value) -> JsonMap {
    value.as_object().expect("object arguments").clone()
}

async fn call(tool: &FunctionTool, input: Value) -> String {
    let result = tool
        .call(args(input), ToolExecutionContext::default())
        .await
        .expect("tool call returns a result");
    result
        .output
        .iter()
        .filter_map(|block| match block {
            agentscope_core::message::ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn schema_lists_declared_parameters_and_required_marks() {
    let schema = add_tool().schema();
    assert_eq!(schema.name, "add");
    assert_eq!(schema.parameters["type"], json!("object"));
    assert_eq!(schema.parameters["properties"]["a"]["type"], json!("integer"));
    assert_eq!(schema.parameters["properties"]["b"]["type"], json!("integer"));
    assert_eq!(schema.parameters["required"], json!(["a", "b"]));
    // 非 strict 工具不注入 additionalProperties。
    assert!(schema.parameters.get("additionalProperties").is_none());
    assert_eq!(schema.strict, None);
}

#[test]
fn doc_comment_becomes_the_default_description() {
    assert_eq!(add_tool().description(), "把两个整数相加。");
}

#[test]
fn option_parameters_are_not_required_and_attributes_reach_the_schema() {
    let tool = greet_tool();
    let schema = tool.schema();

    assert_eq!(schema.name, "greet");
    assert_eq!(schema.description, "按名字打招呼");
    assert_eq!(schema.strict, Some(true));
    assert_eq!(
        schema.parameters["properties"]["name"]["description"],
        json!("要打招呼的人")
    );
    assert_eq!(
        schema.parameters["properties"]["tags"]["items"]["type"],
        json!("string")
    );
    // `Option<bool>` 不是必填参数。
    assert_eq!(schema.parameters["required"], json!(["name", "tags"]));

    // 工具级开关映射到 FunctionTool。
    assert!(tool.is_read_only());
    assert!(!tool.is_concurrency_safe());
    assert!(tool.is_strict());
    // strict 模式递归注入 additionalProperties:false。
    assert_eq!(schema.parameters["additionalProperties"], json!(false));
}

#[test]
fn any_value_parameter_has_an_empty_schema() {
    let schema = Value::Object(echo_any_tool().schema().parameters);
    assert_eq!(schema["properties"]["payload"], json!({}));
    assert_eq!(schema["required"], json!(["payload"]));
}

#[test]
fn borrowed_str_parameter_maps_to_the_string_schema() {
    let schema = shout_tool().schema();
    assert_eq!(schema.parameters["properties"]["text"]["type"], json!("string"));
    assert_eq!(schema.parameters["required"], json!(["text"]));
}

#[tokio::test]
async fn generated_tool_binds_arguments_in_order() {
    assert_eq!(call(&add_tool(), json!({"a": 2, "b": 3})).await, "5");

    // 缺省的可选参数 → None；数组参数正常绑定。
    assert_eq!(
        call(&greet_tool(), json!({"name": "Ada", "tags": ["x", "y"]})).await,
        "hi Ada [x,y]"
    );
    assert_eq!(
        call(
            &greet_tool(),
            json!({"name": "Ada", "excited": true, "tags": []})
        )
        .await,
        "hi Ada! []"
    );

    assert_eq!(
        call(&echo_any_tool(), json!({"payload": {"k": [1, 2]}})).await,
        "{\"k\":[1,2]}"
    );

    // 借用参数同样能绑定。
    assert_eq!(call(&shout_tool(), json!({"text": "hi"})).await, "HI");
}

#[tokio::test]
async fn missing_or_mistyped_arguments_return_an_error_result_instead_of_panicking() {
    let missing = add_tool()
        .call(args(json!({"a": 1})), ToolExecutionContext::default())
        .await
        .expect("a missing argument is a tool error, not a panic");
    assert_eq!(
        missing.state,
        agentscope_core::message::ToolResultState::Error
    );
    assert!(
        text_of(&missing).contains("missing required parameter `b`"),
        "{missing:?}"
    );

    let mistyped = add_tool()
        .call(
            args(json!({"a": "one", "b": 2})),
            ToolExecutionContext::default(),
        )
        .await
        .expect("a mistyped argument is a tool error, not a panic");
    assert_eq!(
        mistyped.state,
        agentscope_core::message::ToolResultState::Error
    );
    assert!(text_of(&mistyped).contains("parameter `a`"), "{mistyped:?}");
}

#[tokio::test]
async fn result_functions_map_err_to_an_error_result() {
    assert_eq!(call(&divide_tool(), json!({"a": 6, "b": 3})).await, "2");

    let failure = divide_tool()
        .call(args(json!({"a": 1, "b": 0})), ToolExecutionContext::default())
        .await
        .expect("Err is a tool error result");
    assert_eq!(
        failure.state,
        agentscope_core::message::ToolResultState::Error
    );
    assert!(
        text_of(&failure).contains("division by zero"),
        "{failure:?}"
    );
}

fn text_of(result: &agentscope_core::message::ToolResultBlock) -> String {
    result
        .output
        .iter()
        .filter_map(|block| match block {
            agentscope_core::message::ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect()
}
