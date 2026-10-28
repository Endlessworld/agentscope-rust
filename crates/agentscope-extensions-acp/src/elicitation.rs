//! `ask_user` — the LLM-facing tool that pushes an ACP elicitation form.
//!
//! ACP elicitation is agent→client: the agent sends `elicitation/create` with a
//! restricted, flat JSON Schema (`ElicitationSchema`) and the client renders a
//! form. This module exposes that round-trip to the model as an ordinary tool:
//! the model calls `ask_user`, the session's live client connection is used to
//! push the form, and the user's answer is fed back as the tool result.
//!
//! Wire-level facts this module relies on (schema 1.9.1 + `protocol/v1/elicitation`):
//! - Only five property kinds exist: string (single-select via `enum`/`oneOf`),
//!   number, integer, boolean, array (multi-select). Anything else is refused
//!   here rather than smuggled through as an unknown control.
//! - Form mode MUST NOT be used for secrets/credentials.
//! - The agent MUST NOT request a mode the client did not advertise, so the
//!   tool is only registered when the client advertised `elicitation.form`
//!   (see `AcpCodingAgent::create_session`).
//!
//! The tool talks to the client through [`ElicitationClient`], not through the
//! concrete ACP connection type, so the schema mapping and the response mapping
//! are plain functions that unit-test offline with a mock client.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    BooleanPropertySchema, CreateElicitationRequest, CreateElicitationResponse, ElicitationAction,
    ElicitationContentValue, ElicitationFormMode, ElicitationPropertySchema, ElicitationSchema,
    ElicitationSessionScope, EnumOption, IntegerPropertySchema, MultiSelectPropertySchema,
    NumberPropertySchema, SessionId, StringFormat, StringPropertySchema, ToolCallId,
};
use agentscope_core::error::Result;
use agentscope_core::message::{JsonMap, ToolResultBlock};
use agentscope_core::tool::{Tool, ToolExecutionContext};
use async_trait::async_trait;

/// The ACP round-trip the tool needs, abstracted so tests can inject a mock.
#[async_trait]
pub trait ElicitationClient: Send + Sync {
    /// Sends `elicitation/create` and awaits the user's action.
    async fn elicit(
        &self,
        request: CreateElicitationRequest,
    ) -> std::result::Result<CreateElicitationResponse, String>;
}

/// Per-session handle to the live client connection.
///
/// Shared (cloned into the tool) and filled in by the session at the start of
/// every prompt, because the ACP connection only exists once a prompt arrives —
/// long after the agent (and therefore the tool) was built.
pub type ElicitationSlot = Arc<Mutex<Option<Arc<dyn ElicitationClient>>>>;

/// LLM tool that asks the user for structured input through an ACP form.
pub struct AskUserTool {
    slot: ElicitationSlot,
    session_id: SessionId,
}

impl AskUserTool {
    /// Creates the tool bound to a session's elicitation slot.
    pub fn new(slot: ElicitationSlot, session_id: SessionId) -> Self {
        Self { slot, session_id }
    }
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the user for structured input through a form and wait for their answer. Use it 
         when you need a decision (pick one of several options) or a piece of information 
         only the user can provide. Never use it for secrets, credentials, API keys or 
         passwords."
    }

    /// Waiting for user input has no side effect on the workspace, and marking
    /// it read-only keeps it out of the permission-approval path (`Ask`), which
    /// would otherwise pop a second, unrelated prompt.
    fn is_read_only(&self) -> bool {
        true
    }

    /// Form round-trips must not overlap.
    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "Human-readable explanation of what input is needed and why."
                },
                "title": {
                    "type": "string",
                    "description": "Optional form title."
                },
                "description": {
                    "type": "string",
                    "description": "Optional form description."
                },
                "properties": {
                    "type": "object",
                    "description": "Form fields: a flat JSON Schema object. Each property must
                                    be one of: {'type':'string'} (add 'enum' or 'oneOf' of 
                                    {const,title} for a single-select), {'type':'number'} / 
                                    {'type':'integer'} (optional minimum/maximum/default), 
                                    {'type':'boolean'} (optional default), or {'type':'array'} 
                                    with 'items' = {'type':'string','enum':[...]} or 
                                    'items' = {'type':'string','oneOf':[{const,title},...]}.",
                    "additionalProperties": { "type": "object" }
                },
                "required": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Names of the properties the user must fill in."
                }
            },
            "required": ["message", "properties"]
        })
    }

    async fn call(&self, args: JsonMap, ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        let client = { self.slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone() };
        let Some(client) = client else {
            return Ok(ToolResultBlock::error(
                "ask_user is unavailable: no ACP client connection is bound to this session. 
                 Ask the user in plain text instead.",
            ));
        };
        let tool_call_id = ctx.tool_call.as_ref().map(|c| c.id.clone());
        let request = match build_form_request(&args, &self.session_id, tool_call_id) {
            Ok(request) => request,
            Err(error) => {
                return Ok(ToolResultBlock::error(format!(
                    "ask_user: invalid form description: {error}"
                )))
            }
        };
        match client.elicit(request).await {
            Ok(response) => Ok(response_to_tool_result(response)),
            Err(error) => Ok(ToolResultBlock::error(format!(
                "ask_user failed: {error}. Ask the user in plain text instead."
            ))),
        }
    }
}

/// Builds the `elicitation/create` request from the tool arguments.
///
/// Pure (no I/O) so the JSON-Schema → ACP-schema mapping is unit-testable.
/// Every rejection is a plain `String` explaining what the model got wrong.
pub fn build_form_request(
    args: &JsonMap,
    session_id: &SessionId,
    tool_call_id: Option<String>,
) -> std::result::Result<CreateElicitationRequest, String> {
    let message = args
        .get("message")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or("missing non-empty string argument: message")?
        .to_string();

    let properties = args
        .get("properties")
        .and_then(|v| v.as_object())
        .ok_or("missing object argument: properties")?;
    if properties.is_empty() {
        return Err("`properties` must declare at least one field".to_string());
    }

    let required: Vec<String> = match args.get("required") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(value) => value
            .as_array()
            .ok_or("`required` must be an array of strings")?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "`required` must be an array of strings".to_string())
            })
            .collect::<std::result::Result<Vec<_>, _>>()?,
    };
    for name in &required {
        if !properties.contains_key(name) {
            return Err(format!("`required` names unknown property `{name}`"));
        }
    }

    let mut schema = ElicitationSchema::new();
    if let Some(title) = args.get("title").and_then(|v| v.as_str()) {
        schema = schema.title(title);
    }
    if let Some(description) = args.get("description").and_then(|v| v.as_str()) {
        schema = schema.description(description);
    }
    for (name, definition) in properties {
        let property = to_property_schema(name, definition)?;
        schema = schema.property(name.clone(), property, required.iter().any(|r| r == name));
    }

    let mut scope = ElicitationSessionScope::new(session_id.clone());
    if let Some(id) = tool_call_id.filter(|id| !id.is_empty()) {
        scope = scope.tool_call_id(ToolCallId::new(id));
    }
    Ok(CreateElicitationRequest::new(
        ElicitationFormMode::new(scope, schema),
        message,
    ))
}

/// Maps one JSON-Schema property onto the restricted ACP property kinds.
fn to_property_schema(
    name: &str,
    definition: &serde_json::Value,
) -> std::result::Result<ElicitationPropertySchema, String> {
    let object = definition
        .as_object()
        .ok_or_else(|| format!("property `{name}` must be an object"))?;
    let kind = object
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("property `{name}` is missing `type`"))?;

    let title = object.get("title").and_then(|v| v.as_str());
    let description = object.get("description").and_then(|v| v.as_str());

    match kind {
        "string" => Ok(string_schema(name, object, title, description)?.into()),
        "number" => {
            let mut schema = NumberPropertySchema::new()
                .minimum(object.get("minimum").and_then(|v| v.as_f64()))
                .maximum(object.get("maximum").and_then(|v| v.as_f64()))
                .default_value(object.get("default").and_then(|v| v.as_f64()));
            if let Some(title) = title {
                schema = schema.title(title);
            }
            if let Some(description) = description {
                schema = schema.description(description);
            }
            Ok(schema.into())
        }
        "integer" => {
            let mut schema = IntegerPropertySchema::new()
                .minimum(object.get("minimum").and_then(|v| v.as_i64()))
                .maximum(object.get("maximum").and_then(|v| v.as_i64()))
                .default_value(object.get("default").and_then(|v| v.as_i64()));
            if let Some(title) = title {
                schema = schema.title(title);
            }
            if let Some(description) = description {
                schema = schema.description(description);
            }
            Ok(schema.into())
        }
        "boolean" => {
            let mut schema = BooleanPropertySchema::new()
                .default_value(object.get("default").and_then(|v| v.as_bool()));
            if let Some(title) = title {
                schema = schema.title(title);
            }
            if let Some(description) = description {
                schema = schema.description(description);
            }
            Ok(schema.into())
        }
        "array" => Ok(multi_select_schema(name, object, title, description)?.into()),
        other => Err(format!(
            "property `{name}` has unsupported type `{other}` (allowed: string, number, 
             integer, boolean, array)"
        )),
    }
}

/// Builds a string property (plain text box, or a single-select when `enum` /
/// `oneOf` is present).
fn string_schema(
    name: &str,
    object: &serde_json::Map<String, serde_json::Value>,
    title: Option<&str>,
    description: Option<&str>,
) -> std::result::Result<StringPropertySchema, String> {
    let enum_values = match object.get("enum") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(
            value
                .as_array()
                .ok_or_else(|| format!("property `{name}`: `enum` must be an array"))?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| format!("property `{name}`: `enum` must contain strings"))
                })
                .collect::<std::result::Result<Vec<_>, _>>()?,
        ),
    };
    let one_of = match object.get("oneOf") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(parse_titled_options(name, value)?),
    };
    if enum_values.is_some() && one_of.is_some() {
        return Err(format!(
            "property `{name}`: `enum` and `oneOf` are mutually exclusive"
        ));
    }

    let format = match object.get("format").and_then(|v| v.as_str()) {
        None => None,
        Some("email") => Some(StringFormat::Email),
        Some("uri") => Some(StringFormat::Uri),
        Some("date") => Some(StringFormat::Date),
        Some("date-time") => Some(StringFormat::DateTime),
        Some(other) => {
            return Err(format!(
                "property `{name}`: unsupported string format `{other}` 
                 (allowed: email, uri, date, date-time)"
            ))
        }
    };

    let mut schema = StringPropertySchema::new()
        .min_length(
            object
                .get("minLength")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32),
        )
        .max_length(
            object
                .get("maxLength")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32),
        )
        .pattern(
            object
                .get("pattern")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        )
        .format(format)
        .default_value(
            object
                .get("default")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        )
        .enum_values(enum_values)
        .one_of(one_of);
    if let Some(title) = title {
        schema = schema.title(title);
    }
    if let Some(description) = description {
        schema = schema.description(description);
    }
    Ok(schema)
}

/// Builds a multi-select property; `items` must be a string enumeration.
fn multi_select_schema(
    name: &str,
    object: &serde_json::Map<String, serde_json::Value>,
    title: Option<&str>,
    description: Option<&str>,
) -> std::result::Result<MultiSelectPropertySchema, String> {
    let items = object
        .get("items")
        .and_then(|v| v.as_object())
        .ok_or_else(|| format!("property `{name}`: array requires an object `items`"))?;
    if items.get("type").and_then(|v| v.as_str()) != Some("string") {
        return Err(format!(
            "property `{name}`: array `items` must be a string enumeration"
        ));
    }
    // Serialized as `anyOf` by the schema, but accept `oneOf` too so the model
    // can use the same shape as the single-select form fields.
    let titled = items.get("oneOf").or_else(|| items.get("anyOf"));
    let mut schema = match (items.get("enum"), titled) {
        (Some(value), None) => MultiSelectPropertySchema::new(
            value
                .as_array()
                .ok_or_else(|| format!("property `{name}`: `items.enum` must be an array"))?
                .iter()
                .map(|v| {
                    v.as_str().map(str::to_string).ok_or_else(|| {
                        format!("property `{name}`: `items.enum` must contain strings")
                    })
                })
                .collect::<std::result::Result<Vec<_>, _>>()?,
        ),
        (None, Some(value)) => {
            MultiSelectPropertySchema::titled(parse_titled_options(name, value)?)
        }
        _ => {
            return Err(format!(
                "property `{name}`: array `items` must set exactly one of `enum` or `oneOf`"
            ))
        }
    };
    schema = schema
        .min_items(object.get("minItems").and_then(|v| v.as_u64()))
        .max_items(object.get("maxItems").and_then(|v| v.as_u64()))
        .default_value(
            object
                .get("default")
                .and_then(|v| v.as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                }),
        );
    if let Some(title) = title {
        schema = schema.title(title);
    }
    if let Some(description) = description {
        schema = schema.description(description);
    }
    Ok(schema)
}

/// Parses a `oneOf: [{const, title, description?}, ...]` single/multi-select list.
fn parse_titled_options(
    name: &str,
    value: &serde_json::Value,
) -> std::result::Result<Vec<EnumOption>, String> {
    let options = value
        .as_array()
        .ok_or_else(|| format!("property `{name}`: `oneOf` must be an array"))?;
    if options.is_empty() {
        return Err(format!("property `{name}`: `oneOf` must not be empty"));
    }
    options
        .iter()
        .map(|option| {
            let entry = option.as_object().ok_or_else(|| {
                format!("property `{name}`: each `oneOf` entry must be an object")
            })?;
            let value = entry.get("const").and_then(|v| v.as_str()).ok_or_else(|| {
                format!("property `{name}`: `oneOf` entry is missing string `const`")
            })?;
            let mut option = EnumOption::new(
                value,
                entry.get("title").and_then(|v| v.as_str()).unwrap_or(value),
            );
            if let Some(description) = entry.get("description").and_then(|v| v.as_str()) {
                option = option.description(description);
            }
            Ok(option)
        })
        .collect()
}

/// Turns the user's response into the tool result the model sees.
pub fn response_to_tool_result(response: CreateElicitationResponse) -> ToolResultBlock {
    match response.action {
        ElicitationAction::Accept(accept) => match accept.content {
            Some(content) if !content.is_empty() => {
                let object = content_to_json(&content);
                ToolResultBlock::text(
                    serde_json::to_string_pretty(&object).unwrap_or_else(|_| "{}".to_string()),
                )
            }
            _ => ToolResultBlock::text("The user submitted an empty form."),
        },
        ElicitationAction::Decline => ToolResultBlock::text(
            "The user declined to answer. Do not ask again; proceed with a reasonable default 
             or ask in plain text.",
        ),
        ElicitationAction::Cancel => {
            ToolResultBlock::text("The user dismissed the request without answering.")
        }
        _ => ToolResultBlock::text("The user's response was not understood."),
    }
}

/// Converts accepted form content into a JSON object.
pub fn content_to_json(content: &BTreeMap<String, ElicitationContentValue>) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    for (key, value) in content {
        object.insert(key.clone(), content_value_to_json(value));
    }
    serde_json::Value::Object(object)
}

fn content_value_to_json(value: &ElicitationContentValue) -> serde_json::Value {
    match value {
        ElicitationContentValue::String(v) => serde_json::Value::String(v.clone()),
        ElicitationContentValue::Integer(v) => serde_json::json!(v),
        ElicitationContentValue::Number(v) => serde_json::json!(v),
        ElicitationContentValue::Boolean(v) => serde_json::json!(v),
        ElicitationContentValue::StringArray(v) => serde_json::json!(v),
        _ => serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::ElicitationAcceptAction;

    fn map(value: serde_json::Value) -> JsonMap {
        match value {
            serde_json::Value::Object(map) => map,
            other => panic!("expected object, got {other}"),
        }
    }

    fn text_of(result: &ToolResultBlock) -> String {
        result
            .output
            .iter()
            .find_map(|b| match b {
                agentscope_core::message::ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    #[test]
    fn builds_a_single_select_form() {
        let args = map(serde_json::json!({
            "message": "How should I approach this refactoring?",
            "properties": {
                "strategy": {
                    "type": "string",
                    "title": "Strategy",
                    "enum": ["conservative", "balanced", "aggressive"]
                }
            },
            "required": ["strategy"]
        }));
        let request =
            build_form_request(&args, &SessionId::new("sess_1"), Some("call-1".into())).unwrap();

        let wire = serde_json::to_value(&request).unwrap();
        assert_eq!(wire["mode"], "form");
        assert_eq!(wire["sessionId"], "sess_1");
        assert_eq!(wire["toolCallId"], "call-1");
        assert_eq!(wire["message"], "How should I approach this refactoring?");
        assert_eq!(wire["requestedSchema"]["type"], "object");
        assert_eq!(wire["requestedSchema"]["required"][0], "strategy");
        assert_eq!(
            wire["requestedSchema"]["properties"]["strategy"]["enum"],
            serde_json::json!(["conservative", "balanced", "aggressive"])
        );
    }

    #[test]
    fn maps_every_supported_property_kind() {
        let args = map(serde_json::json!({
            "message": "fill me",
            "properties": {
                "note": {"type": "string", "default": "hi", "minLength": 1, "maxLength": 10},
                "email": {"type": "string", "format": "email"},
                "ratio": {"type": "number", "minimum": 0.0, "maximum": 1.0},
                "count": {"type": "integer", "minimum": 1, "maximum": 9},
                "agree": {"type": "boolean", "default": true},
                "tags": {"type": "array", "items": {"type": "string", "enum": ["a", "b"]}},
                "picks": {"type": "array", "items": {"type": "string", "oneOf": [
                    {"const": "x", "title": "Ex", "description": "the ex"}
                ]}},
                "titled": {"type": "string", "oneOf": [{"const": "y", "title": "Why"}]}
            }
        }));
        let request = build_form_request(&args, &SessionId::new("s"), None).unwrap();
        let props = &serde_json::to_value(&request).unwrap()["requestedSchema"]["properties"];

        assert_eq!(props["note"]["type"], "string");
        assert_eq!(props["note"]["default"], "hi");
        assert_eq!(props["note"]["minLength"], 1);
        assert_eq!(props["email"]["format"], "email");
        assert_eq!(props["ratio"]["type"], "number");
        assert_eq!(props["count"]["type"], "integer");
        assert_eq!(props["agree"]["default"], true);
        assert_eq!(
            props["tags"]["items"]["enum"],
            serde_json::json!(["a", "b"])
        );
        // Titled multi-select items serialize under `anyOf` (not `oneOf`).
        assert_eq!(props["picks"]["items"]["anyOf"][0]["const"], "x");
        assert_eq!(props["picks"]["items"]["anyOf"][0]["title"], "Ex");
        assert_eq!(props["titled"]["oneOf"][0]["title"], "Why");
    }

    #[test]
    fn no_tool_call_id_means_no_tool_call_scope() {
        let args = map(serde_json::json!({
            "message": "m",
            "properties": {"a": {"type": "string"}}
        }));
        let request = build_form_request(&args, &SessionId::new("s"), None).unwrap();
        let wire = serde_json::to_value(&request).unwrap();
        assert!(wire.get("toolCallId").is_none());
    }

    #[test]
    fn rejects_unsupported_and_malformed_fields() {
        let cases = [
            serde_json::json!({"properties": {"a": {"type": "string"}}}),
            serde_json::json!({"message": "  ", "properties": {"a": {"type": "string"}}}),
            serde_json::json!({"message": "m", "properties": {}}),
            serde_json::json!({"message": "m", "properties": {"a": {"type": "object"}}}),
            serde_json::json!({"message": "m", "properties": {"a": {}}}),
            serde_json::json!({"message": "m", "properties": {"a": {"type": "string", "enum": ["x"], "oneOf": [{"const": "y"}]}}}),
            serde_json::json!({"message": "m", "properties": {"a": {"type": "string", "format": "password"}}}),
            serde_json::json!({"message": "m", "properties": {"a": {"type": "array"}}}),
            serde_json::json!({"message": "m", "properties": {"a": {"type": "array", "items": {"type": "integer"}}}}),
            serde_json::json!({"message": "m", "properties": {"a": {"type": "string"}}, "required": ["b"]}),
        ];
        for (index, case) in cases.iter().enumerate() {
            let args = map(case.clone());
            assert!(
                build_form_request(&args, &SessionId::new("s"), None).is_err(),
                "case {index} should have been rejected: {case}"
            );
        }
    }

    #[test]
    fn accept_content_becomes_a_json_tool_result() {
        let mut content = BTreeMap::new();
        content.insert(
            "strategy".to_string(),
            ElicitationContentValue::String("balanced".to_string()),
        );
        content.insert("count".to_string(), ElicitationContentValue::Integer(3));
        content.insert("agree".to_string(), ElicitationContentValue::Boolean(true));
        content.insert(
            "tags".to_string(),
            ElicitationContentValue::StringArray(vec!["a".to_string()]),
        );
        let response = CreateElicitationResponse::new(ElicitationAction::Accept(
            ElicitationAcceptAction::new().content(content),
        ));
        let result = response_to_tool_result(response);
        let parsed: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(parsed["strategy"], "balanced");
        assert_eq!(parsed["count"], 3);
        assert_eq!(parsed["agree"], true);
        assert_eq!(parsed["tags"], serde_json::json!(["a"]));
    }

    #[test]
    fn decline_and_cancel_are_reported_as_text() {
        let declined =
            response_to_tool_result(CreateElicitationResponse::new(ElicitationAction::Decline));
        assert!(text_of(&declined).contains("declined"));
        let cancelled =
            response_to_tool_result(CreateElicitationResponse::new(ElicitationAction::Cancel));
        assert!(text_of(&cancelled).contains("dismissed"));
    }
}
