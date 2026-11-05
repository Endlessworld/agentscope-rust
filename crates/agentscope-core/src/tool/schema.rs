//! JSON Schema 辅助函数：strict 模式下的 `additionalProperties` 注入。
//!
//! 对齐 Java `io.agentscope.core.tool.ToolSchemaGenerator` 的
//! `addAdditionalPropertiesFalseRecursively`：当工具启用 strict 模式时，
//! 递归地把 `additionalProperties: false` 注入 schema 中所有 object 节点，
//! 使得工具入参校验可以拒绝 schema 未声明的键，同时满足 OpenAI strict 模式
//! 对 schema 的要求。
//!
//! 注入规则（与 Java 一致）：
//! - 仅当节点的 `type` 为 `"object"` 且节点自身尚未声明 `additionalProperties`
//!   时才注入 `false`；
//! - 递归进入 `properties` / `items` / `oneOf` / `anyOf` / `allOf` /
//!   `$defs` / `definitions`，与当前节点自身的类型无关。
use serde_json::Value;

/// 递归地为 schema 中所有 object 节点注入 `additionalProperties: false`。
///
/// 见[模块文档](self)对注入规则的说明。已经是 `false`（或显式声明了其它值）
/// 的节点不会被覆盖。
///
/// # 示例
///
/// ```
/// use agentscope_core::tool::schema::apply_additional_properties_false;
/// use serde_json::json;
///
/// let mut schema = json!({
///     "type": "object",
///     "properties": {
///         "nested": { "type": "object", "properties": { "x": { "type": "string" } } },
///         "list": { "type": "array", "items": { "type": "object" } }
///     }
/// });
/// apply_additional_properties_false(&mut schema);
/// assert_eq!(schema["additionalProperties"], json!(false));
/// assert_eq!(schema["properties"]["nested"]["additionalProperties"], json!(false));
/// assert_eq!(schema["properties"]["list"]["items"]["additionalProperties"], json!(false));
/// ```
pub fn apply_additional_properties_false(schema: &mut Value) {
    let Value::Object(map) = schema else {
        return;
    };

    // 仅 object 类型的节点才注入；已显式声明 additionalProperties 的节点保持原样。
    let is_object = map.get("type").and_then(|v| v.as_str()) == Some("object");
    if is_object && !map.contains_key("additionalProperties") {
        map.insert("additionalProperties".into(), Value::Bool(false));
    }

    // properties: { name: schema }
    if let Some(Value::Object(properties)) = map.get_mut("properties") {
        for (_, child) in properties.iter_mut() {
            apply_additional_properties_false(child);
        }
    }

    // items: schema（也兼容数组形式的元组 schema）
    if let Some(items) = map.get_mut("items") {
        match items {
            Value::Array(list) => {
                for child in list.iter_mut() {
                    apply_additional_properties_false(child);
                }
            }
            other => apply_additional_properties_false(other),
        }
    }

    // oneOf / anyOf / allOf: [schema]
    for key in ["oneOf", "anyOf", "allOf"] {
        if let Some(Value::Array(list)) = map.get_mut(key) {
            for child in list.iter_mut() {
                apply_additional_properties_false(child);
            }
        }
    }

    // $defs / definitions: { name: schema }
    for key in ["$defs", "definitions"] {
        if let Some(Value::Object(defs)) = map.get_mut(key) {
            for (_, child) in defs.iter_mut() {
                apply_additional_properties_false(child);
            }
        }
    }
}

/// 生成工具对外暴露 / 用于入参校验的参数 schema。
///
/// - 根节点统一为 `object`（工具入参总是 JSON 对象）；
/// - `strict` 为真时递归注入 `additionalProperties: false`（见
///   [`apply_additional_properties_false`]），从而拒绝 schema 未声明的键。
///
/// [`crate::tool::Tool::schema`] 与 [`ToolExecutor`](super::executor::ToolExecutor) 共用本函数，
/// 保证「发给模型的 schema」与「校验入参用的 schema」始终一致。
pub fn effective_parameters(schema: &Value, strict: bool) -> serde_json::Map<String, Value> {
    let mut parameters = match schema {
        Value::Object(map) => map.clone(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("type".into(), Value::String("object".into()));
            map.insert("properties".into(), other.clone());
            map
        }
    };
    if !parameters.contains_key("type") {
        parameters.insert("type".into(), Value::String("object".into()));
    }
    if strict {
        let mut value = Value::Object(std::mem::take(&mut parameters));
        apply_additional_properties_false(&mut value);
        if let Value::Object(map) = value {
            parameters = map;
        }
    }
    parameters
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn injects_into_nested_object_nodes() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "nested": {
                    "type": "object",
                    "properties": { "inner": { "type": "object", "properties": {} } }
                }
            }
        });
        apply_additional_properties_false(&mut schema);

        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(
            schema["properties"]["nested"]["additionalProperties"],
            json!(false)
        );
        assert_eq!(
            schema["properties"]["nested"]["properties"]["inner"]["additionalProperties"],
            json!(false)
        );
    }

    #[test]
    fn injects_into_array_items_object() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "list": { "type": "array", "items": { "type": "object", "properties": {} } }
            }
        });
        apply_additional_properties_false(&mut schema);

        assert_eq!(
            schema["properties"]["list"]["items"]["additionalProperties"],
            json!(false)
        );
    }

    #[test]
    fn keeps_existing_additional_properties() {
        let mut schema = json!({
            "type": "object",
            "additionalProperties": true,
            "properties": {
                "kept": { "type": "object", "additionalProperties": { "type": "string" } }
            }
        });
        apply_additional_properties_false(&mut schema);

        assert_eq!(schema["additionalProperties"], json!(true));
        assert_eq!(
            schema["properties"]["kept"]["additionalProperties"],
            json!({ "type": "string" })
        );
    }

    #[test]
    fn skips_non_object_nodes() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "text": { "type": "string" },
                "number": { "type": "number" },
                "flag": { "type": "boolean" }
            }
        });
        apply_additional_properties_false(&mut schema);

        assert!(schema["properties"]["text"]
            .get("additionalProperties")
            .is_none());
        assert!(schema["properties"]["number"]
            .get("additionalProperties")
            .is_none());
        assert!(schema["properties"]["flag"]
            .get("additionalProperties")
            .is_none());
    }

    #[test]
    fn injects_into_defs_and_combinators() {
        let mut schema = json!({
            "type": "object",
            "$defs": { "Point": { "type": "object", "properties": {} } },
            "definitions": { "Line": { "type": "object", "properties": {} } },
            "properties": {
                "choice": { "oneOf": [{ "type": "object" }, { "type": "string" }] },
                "any": { "anyOf": [{ "type": "object" }] },
                "all": { "allOf": [{ "type": "object" }] }
            }
        });
        apply_additional_properties_false(&mut schema);

        assert_eq!(
            schema["$defs"]["Point"]["additionalProperties"],
            json!(false)
        );
        assert_eq!(
            schema["definitions"]["Line"]["additionalProperties"],
            json!(false)
        );
        assert_eq!(
            schema["properties"]["choice"]["oneOf"][0]["additionalProperties"],
            json!(false)
        );
        assert!(schema["properties"]["choice"]["oneOf"][1]
            .get("additionalProperties")
            .is_none());
        assert_eq!(
            schema["properties"]["any"]["anyOf"][0]["additionalProperties"],
            json!(false)
        );
        assert_eq!(
            schema["properties"]["all"]["allOf"][0]["additionalProperties"],
            json!(false)
        );
    }
}
