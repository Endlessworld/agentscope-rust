//! `#[tool]` 宏的运行时胶水（`agentscope-macros` 生成代码调用的公开 API）。
//!
//! 宏展开后会引用这里的类型与函数，因此它们属于公开 API；但除了宏生成的代码之外，
//! 调用者不应直接依赖它们（语义对齐 Java `ToolSchemaGenerator` + `ReflectiveFunctionTool`）。
//!
//! 设计要点：生成的代码只引用 `::agentscope_core::...` 与 `::std::...`，不引用
//! `serde_json`，因此使用者只需要依赖 `agentscope-core`。
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::message::{JsonMap, ToolResultBlock};

/// 一个工具参数在 JSON Schema 中的类型。
#[derive(Debug, Clone)]
pub enum ParamKind {
    /// `{"type": "string"}`
    String,
    /// `{"type": "integer"}`
    Integer,
    /// `{"type": "number"}`
    Number,
    /// `{"type": "boolean"}`
    Boolean,
    /// `{"type": "array", "items": <inner>}`
    Array(Box<ParamKind>),
    /// 任意值（`{}`），对应 `serde_json::Value` 参数。
    Any,
}

impl ParamKind {
    /// 转换为 JSON Schema 片段。
    pub fn to_schema(&self) -> Value {
        match self {
            ParamKind::String => serde_json::json!({ "type": "string" }),
            ParamKind::Integer => serde_json::json!({ "type": "integer" }),
            ParamKind::Number => serde_json::json!({ "type": "number" }),
            ParamKind::Boolean => serde_json::json!({ "type": "boolean" }),
            ParamKind::Array(inner) => {
                serde_json::json!({ "type": "array", "items": inner.to_schema() })
            }
            ParamKind::Any => serde_json::json!({}),
        }
    }
}

/// 宏生成工具时对一个参数的描述。
#[derive(Debug, Clone)]
pub struct ParamSpec {
    /// 参数名（即 JSON 对象的键）。
    pub name: &'static str,
    /// 参数说明（写入 `description`）。
    pub description: Option<&'static str>,
    /// 是否必填（非 `Option<T>` 参数默认必填）。
    pub required: bool,
    /// JSON Schema 类型。
    pub kind: ParamKind,
}

impl ParamSpec {
    /// 创建一个必填参数。
    pub fn new(name: &'static str, kind: ParamKind) -> Self {
        Self {
            name,
            description: None,
            required: true,
            kind,
        }
    }

    /// 标记为可选（`Option<T>` 参数，不进 `required`）。
    pub fn optional(mut self) -> Self {
        self.required = false;
        self
    }

    /// 设置参数说明。
    pub fn describe(mut self, description: &'static str) -> Self {
        self.description = Some(description);
        self
    }
}

/// 构造工具参数 schema：`{"type":"object","properties":{...},"required":[...]}`。
///
/// 对齐 Java `ToolSchemaGenerator#generateParameterSchema`：只包含声明过的参数，
/// 非必填参数不进 `required`。
pub fn object_schema(params: Vec<ParamSpec>) -> Value {
    let mut properties = Map::new();
    let mut required: Vec<Value> = Vec::new();
    for param in params {
        let mut schema = param.kind.to_schema();
        if let (Some(description), Some(object)) = (param.description, schema.as_object_mut()) {
            object.insert(
                "description".to_string(),
                Value::String(description.to_string()),
            );
        }
        if param.required {
            required.push(Value::String(param.name.to_string()));
        }
        properties.insert(param.name.to_string(), schema);
    }

    let mut root = Map::new();
    root.insert("type".to_string(), Value::String("object".to_string()));
    root.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        root.insert("required".to_string(), Value::Array(required));
    }
    Value::Object(root)
}

/// 读取一个必填参数，把缺失 / 类型错误转成可返回给模型的错误文本。
pub fn required_arg<T: DeserializeOwned>(
    args: &JsonMap,
    name: &str,
) -> std::result::Result<T, String> {
    match args.get(name) {
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|error| format!("invalid value for parameter `{name}`: {error}")),
        None => Err(format!("missing required parameter `{name}`")),
    }
}

/// 读取一个可选参数：缺省或显式 `null` 都返回 `None`。
pub fn optional_arg<T: DeserializeOwned>(
    args: &JsonMap,
    name: &str,
) -> std::result::Result<Option<T>, String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|error| format!("invalid value for parameter `{name}`: {error}")),
    }
}

/// 把函数返回值包装成成功结果的文本块。
pub fn text_result(value: impl Into<String>) -> ToolResultBlock {
    ToolResultBlock::text(value)
}

/// 把参数错误 / 函数错误包装成错误结果（模型可据此重试）。
pub fn error_result(message: impl Into<String>) -> ToolResultBlock {
    ToolResultBlock::error(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_schema_lists_declared_parameters_and_required() {
        let schema = object_schema(vec![
            ParamSpec::new("name", ParamKind::String),
            ParamSpec::new("tags", ParamKind::Array(Box::new(ParamKind::String))),
            ParamSpec::new("note", ParamKind::Any).optional(),
        ]);

        assert_eq!(schema["type"], serde_json::json!("object"));
        assert_eq!(schema["properties"]["name"]["type"], serde_json::json!("string"));
        assert_eq!(
            schema["properties"]["tags"]["items"]["type"],
            serde_json::json!("string")
        );
        // `Any` 参数不声明类型（等价于「任意 JSON 值」）。
        assert!(schema["properties"]["note"].get("type").is_none());
        assert_eq!(
            schema["required"],
            serde_json::json!(["name", "tags"]),
            "可选参数不能进 required"
        );
    }

    #[test]
    fn description_reaches_the_property_schema() {
        let schema = object_schema(vec![
            ParamSpec::new("city", ParamKind::String).describe("城市名")
        ]);
        assert_eq!(
            schema["properties"]["city"]["description"],
            serde_json::json!("城市名")
        );
    }

    #[test]
    fn required_arg_reports_missing_and_type_mismatch() {
        let mut args = JsonMap::new();
        args.insert("count".to_string(), serde_json::json!("2"));

        let error: String = required_arg::<i64>(&args, "count").unwrap_err();
        assert!(
            error.contains("invalid value for parameter `count`"),
            "{error}"
        );

        let missing: std::result::Result<i64, String> = required_arg(&args, "other");
        assert_eq!(missing.unwrap_err(), "missing required parameter `other`");
    }

    #[test]
    fn optional_arg_treats_absent_and_null_as_none() {
        let mut args = JsonMap::new();
        let absent: Option<String> = optional_arg(&args, "flag").unwrap();
        assert_eq!(absent, None);

        args.insert("flag".to_string(), serde_json::Value::Null);
        let null: Option<String> = optional_arg(&args, "flag").unwrap();
        assert_eq!(null, None);

        args.insert("flag".to_string(), serde_json::json!("on"));
        let present: Option<String> = optional_arg(&args, "flag").unwrap();
        assert_eq!(present.as_deref(), Some("on"));
    }
}
