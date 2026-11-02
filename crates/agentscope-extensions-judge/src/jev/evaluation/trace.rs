//! Invocation-scoped traces (`evaluation/JevTrace`).
//!
//! Java folds `List<Msg>` into a JSON state map. Message metadata and thinking are deliberately
//! **not** evaluation evidence; only roles, text, tool uses and tool results survive.
//!
//! Deviation: this crate does not depend on `agentscope-core`, so [`TraceMessage`] is a neutral,
//! owned projection of the fields Java reads from `Msg`. Hosts adapt their own message type into
//! it (see `docs/extensions/JUDGE__GAP_TRACKER.md` §4.2). `role` carries the Java
//! `MsgRole.name()` spelling (`SYSTEM`, `USER`, `ASSISTANT`, `TOOL`, ...).

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::error::JevError;

/// The role spelling Java compares against (`MsgRole.name()`).
pub const ROLE_SYSTEM: &str = "SYSTEM";
/// The user role spelling.
pub const ROLE_USER: &str = "USER";
/// The assistant role spelling.
pub const ROLE_ASSISTANT: &str = "ASSISTANT";
/// The tool role spelling.
pub const ROLE_TOOL: &str = "TOOL";

/// One content block of a [`TraceMessage`].
#[derive(Debug, Clone, PartialEq)]
pub enum TraceBlock {
    /// A text block; contributes to the message text.
    Text(String),
    /// A thinking block; marks the message as pinned/reasoned and is otherwise ignored.
    Thinking(String),
    /// A tool invocation.
    ToolUse(TraceToolUse),
    /// A tool result.
    ToolResult(TraceToolResult),
    /// Any other block type (images, audio, ...): flagged as unsupported content.
    Other,
}

/// A tool invocation block.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceToolUse {
    /// The call id; must be non-blank and unique for the trace to be usable.
    pub id: Option<String>,
    /// The tool name.
    pub name: String,
    /// The JSON arguments the model produced.
    pub arguments: Value,
}

/// One part of a tool result output.
#[derive(Debug, Clone, PartialEq)]
pub enum TraceResultPart {
    /// A text part; the only supported part type.
    Text(String),
    /// A non-text part: flagged as unsupported tool content.
    Other,
}

/// A tool result block.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceToolResult {
    /// The call id this result answers.
    pub id: Option<String>,
    /// The tool name; must match the call's name.
    pub name: String,
    /// The output parts.
    pub parts: Vec<TraceResultPart>,
    /// The Java `ToolResultState` name, or `None` when the result carries no state.
    pub state: Option<String>,
    /// Metadata keys (unused by the trace fold, kept for adapters that need the suspended flag).
    pub metadata: BTreeMap<String, Value>,
}

/// A tool schema made available for the invocation (`ToolSchema`).
#[derive(Debug, Clone, PartialEq)]
pub struct TraceToolSchema {
    /// The tool name.
    pub name: String,
    /// The tool description.
    pub description: String,
    /// The JSON parameter schema.
    pub parameters: Value,
}

/// A neutral, owned projection of the message fields a trace keeps.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceMessage {
    /// The message id, used by the trace-evaluation middleware to find the invocation boundary.
    pub id: Option<String>,
    /// The role spelling (see [`ROLE_USER`]).
    pub role: String,
    /// The concatenated text content (`Msg.getTextContent`).
    pub text: String,
    /// The content blocks, in order.
    pub blocks: Vec<TraceBlock>,
}

impl TraceMessage {
    /// Creates a message with no blocks.
    pub fn new(role: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: None,
            role: role.into(),
            text: text.into(),
            blocks: Vec::new(),
        }
    }

    /// Creates a user message whose text is also its single text block.
    pub fn user(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            id: None,
            role: ROLE_USER.to_string(),
            text: text.clone(),
            blocks: vec![TraceBlock::Text(text)],
        }
    }

    /// Creates an assistant message whose text is also its single text block.
    pub fn assistant(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            id: None,
            role: ROLE_ASSISTANT.to_string(),
            text: text.clone(),
            blocks: vec![TraceBlock::Text(text)],
        }
    }

    /// Sets the message id.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Whether any block is a tool use.
    fn has_tool_use(&self) -> bool {
        self.blocks
            .iter()
            .any(|block| matches!(block, TraceBlock::ToolUse(_)))
    }
}

/// A detached, invocation-scoped trace.
#[derive(Debug, Clone, PartialEq)]
pub struct JevTrace {
    /// The trace id (non-blank).
    pub id: String,
    /// The evidence fields, in deterministic key order.
    pub fields: BTreeMap<String, Value>,
    /// Explicit trace problems; a non-empty list means the trace is not usable as evidence.
    pub issues: Vec<String>,
}

impl JevTrace {
    /// Creates a trace (`JevTrace` compact constructor).
    pub fn new(
        id: impl Into<String>,
        fields: BTreeMap<String, Value>,
        issues: Vec<String>,
    ) -> Result<Self, JevError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(JevError::invalid("trace id required"));
        }
        Ok(Self { id, fields, issues })
    }

    /// Creates a trace with no issues.
    pub fn without_issues(
        id: impl Into<String>,
        fields: BTreeMap<String, Value>,
    ) -> Result<Self, JevError> {
        Self::new(id, fields, Vec::new())
    }

    /// Whether `key` holds a non-null, non-blank value.
    pub fn has(&self, key: &str) -> bool {
        match self.fields.get(key) {
            None | Some(Value::Null) => false,
            Some(Value::String(text)) => !text.trim().is_empty(),
            Some(_) => true,
        }
    }

    /// The string value at `key`, or an empty string.
    pub fn text(&self, key: &str) -> &str {
        match self.fields.get(key) {
            Some(Value::String(text)) => text,
            _ => "",
        }
    }

    /// The array value at `key`, or an empty slice.
    pub fn list(&self, key: &str) -> &[Value] {
        match self.fields.get(key) {
            Some(Value::Array(items)) => items,
            _ => &[],
        }
    }

    /// A copy of this trace with one field replaced.
    pub fn with(&self, key: impl Into<String>, value: Value) -> Self {
        let mut fields = self.fields.clone();
        fields.insert(key.into(), value);
        Self {
            id: self.id.clone(),
            fields,
            issues: self.issues.clone(),
        }
    }

    /// Folds messages plus available tools into a trace (`JevTrace.fromMessages`).
    ///
    /// Tool results must follow a uniquely identified call; partial traces carry explicit issues
    /// instead of guessing.
    pub fn from_messages(
        id: impl Into<String>,
        messages: &[TraceMessage],
        tools: Option<&[TraceToolSchema]>,
    ) -> Result<Self, JevError> {
        let mut turns: Vec<Value> = Vec::new();
        let mut calls: Vec<Value> = Vec::new();
        // Java keeps one mutable map shared by the call list and the id index; Rust needs the
        // index into the list so a result can mutate its own call in place.
        let mut index_by_id: BTreeMap<String, usize> = BTreeMap::new();
        let mut issues: Vec<String> = Vec::new();
        let mut request = String::new();
        let mut answer = String::new();

        for message in messages {
            let text = message.text.as_str();
            turns.push(json!({"role": message.role, "text": text}));
            let turn_index = turns.len() - 1;
            if message.role == ROLE_USER && !text.trim().is_empty() {
                request = text.to_string();
                answer.clear();
            }
            let has_calls = message.has_tool_use();
            if message.role == ROLE_ASSISTANT {
                answer = if has_calls {
                    String::new()
                } else {
                    text.to_string()
                };
            }
            for block in &message.blocks {
                match block {
                    TraceBlock::ToolUse(use_block) => {
                        let call_id = use_block.id.as_deref().unwrap_or("").to_string();
                        if call_id.trim().is_empty() || index_by_id.contains_key(&call_id) {
                            issues.push("AMBIGUOUS_CALL_ID".to_string());
                            continue;
                        }
                        let call = json!({
                            "id": call_id,
                            "name": use_block.name,
                            "arguments": use_block.arguments,
                            "message_index": turn_index,
                        });
                        index_by_id.insert(call_id, calls.len());
                        calls.push(call);
                    }
                    TraceBlock::ToolResult(result) => {
                        let result_id = result.id.as_deref().unwrap_or("");
                        let Some(&index) = index_by_id.get(result_id) else {
                            issues.push("UNPAIRED_TOOL_RESULT".to_string());
                            continue;
                        };
                        let Some(object) = calls[index].as_object_mut() else {
                            issues.push("UNPAIRED_TOOL_RESULT".to_string());
                            continue;
                        };
                        if object.contains_key("result")
                            || object.get("name").and_then(Value::as_str)
                                != Some(result.name.as_str())
                        {
                            issues.push("UNPAIRED_TOOL_RESULT".to_string());
                            continue;
                        }
                        let mut texts = Vec::new();
                        for part in &result.parts {
                            match part {
                                TraceResultPart::Text(text) => texts.push(text.clone()),
                                TraceResultPart::Other => {
                                    issues.push("UNSUPPORTED_TOOL_CONTENT".to_string());
                                }
                            }
                        }
                        object.insert("result".to_string(), json!(texts.join("\n")));
                        object.insert(
                            "result_state".to_string(),
                            json!(result.state.as_deref().unwrap_or("UNKNOWN")),
                        );
                        object.insert("result_message_index".to_string(), json!(turn_index));
                    }
                    TraceBlock::Text(_) | TraceBlock::Thinking(_) => {}
                    TraceBlock::Other => issues.push("UNSUPPORTED_MESSAGE_CONTENT".to_string()),
                }
            }
        }

        let has_pending = calls.iter().any(|call| {
            call.as_object()
                .map(|object| !object.contains_key("result"))
                .unwrap_or(false)
        });
        if has_pending {
            issues.push("PENDING_TOOL_CALL".to_string());
        }

        let tool_results: Vec<Value> = calls
            .iter()
            .filter(|call| {
                call.as_object()
                    .map(|object| object.contains_key("result"))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();

        let mut fields = BTreeMap::new();
        fields.insert("request".to_string(), json!(request));
        fields.insert("final_answer".to_string(), json!(answer));
        fields.insert("messages".to_string(), Value::Array(turns));
        fields.insert("tool_calls".to_string(), Value::Array(calls));
        fields.insert("tool_results".to_string(), Value::Array(tool_results));
        if let Some(tools) = tools {
            fields.insert(
                "available_tools".to_string(),
                Value::Array(
                    tools
                        .iter()
                        .map(|tool| {
                            json!({
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.parameters,
                            })
                        })
                        .collect(),
                ),
            );
        }
        Self::new(id, fields, issues)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_use(id: &str, name: &str) -> TraceBlock {
        TraceBlock::ToolUse(TraceToolUse {
            id: Some(id.to_string()),
            name: name.to_string(),
            arguments: json!({"path": "a.rs"}),
        })
    }

    fn tool_result(id: &str, name: &str, text: &str) -> TraceBlock {
        TraceBlock::ToolResult(TraceToolResult {
            id: Some(id.to_string()),
            name: name.to_string(),
            parts: vec![TraceResultPart::Text(text.to_string())],
            state: Some("SUCCESS".to_string()),
            metadata: BTreeMap::new(),
        })
    }

    #[test]
    fn accessors_distinguish_absent_blank_and_present_fields() {
        let mut fields = BTreeMap::new();
        fields.insert("blank".to_string(), json!("  "));
        fields.insert("null".to_string(), Value::Null);
        fields.insert("text".to_string(), json!("hello"));
        fields.insert("list".to_string(), json!([1, 2]));
        let trace = JevTrace::without_issues("t", fields).unwrap();
        assert!(!trace.has("blank"));
        assert!(!trace.has("null"));
        assert!(!trace.has("missing"));
        assert!(trace.has("text"));
        assert!(trace.has("list"));
        assert_eq!(trace.text("text"), "hello");
        assert_eq!(trace.text("list"), "");
        assert_eq!(trace.list("list"), &[json!(1), json!(2)]);
        assert!(trace.list("text").is_empty());
        assert!(trace.with("added", json!(1)).list("added").is_empty());
        assert_eq!(trace.with("added", json!(7)).fields["added"], json!(7));
        assert!(trace.issues.is_empty());
        assert_eq!(
            JevTrace::without_issues(" ", BTreeMap::new())
                .unwrap_err()
                .message(),
            "trace id required"
        );
    }

    #[test]
    fn from_messages_pairs_calls_with_results() {
        let mut assistant = TraceMessage::new(ROLE_ASSISTANT, "");
        assistant.blocks = vec![
            tool_use("c1", "read_file"),
            tool_result("c1", "read_file", "body"),
        ];
        let messages = vec![TraceMessage::user("read a.rs"), assistant];
        let trace = JevTrace::from_messages(
            "t1",
            &messages,
            Some(&[TraceToolSchema {
                name: "read_file".to_string(),
                description: "reads".to_string(),
                parameters: json!({"type": "object"}),
            }]),
        )
        .unwrap();

        assert!(trace.issues.is_empty(), "{:?}", trace.issues);
        assert_eq!(trace.text("request"), "read a.rs");
        assert_eq!(trace.text("final_answer"), "");
        assert_eq!(trace.list("messages").len(), 2);
        assert_eq!(trace.list("tool_calls").len(), 1);
        assert_eq!(trace.list("tool_results").len(), 1);
        let call = &trace.list("tool_calls")[0];
        assert_eq!(call["id"], json!("c1"));
        assert_eq!(call["result"], json!("body"));
        assert_eq!(call["result_state"], json!("SUCCESS"));
        assert_eq!(call["message_index"], json!(1));
        assert_eq!(trace.list("available_tools")[0]["name"], json!("read_file"));
    }

    #[test]
    fn assistant_text_is_the_final_answer_only_without_tool_calls() {
        let messages = vec![
            TraceMessage::user("q"),
            TraceMessage::assistant("plain answer"),
        ];
        let trace = JevTrace::from_messages("t", &messages, None).unwrap();
        assert_eq!(trace.text("final_answer"), "plain answer");
        assert!(!trace.fields.contains_key("available_tools"));

        let mut with_call = TraceMessage::new(ROLE_ASSISTANT, "thinking out loud");
        with_call.blocks = vec![tool_use("c1", "t")];
        let trace =
            JevTrace::from_messages("t", &[TraceMessage::user("q"), with_call], None).unwrap();
        assert_eq!(trace.text("final_answer"), "");
        assert_eq!(trace.issues, vec!["PENDING_TOOL_CALL".to_string()]);
    }

    #[test]
    fn ambiguous_unpaired_and_unsupported_content_are_reported() {
        let mut first = TraceMessage::new(ROLE_ASSISTANT, "");
        first.blocks = vec![tool_use("c1", "a"), tool_use("c1", "b")];
        let mut second = TraceMessage::new(ROLE_ASSISTANT, "");
        second.blocks = vec![
            tool_use("c2", "reader"),
            tool_result("c2", "writer", "x"),
            tool_result("missing", "reader", "y"),
            TraceBlock::Other,
        ];
        let mut third = TraceMessage::new(ROLE_TOOL, "");
        third.blocks = vec![TraceBlock::Thinking("hmm".to_string())];
        let trace = JevTrace::from_messages("t", &[first, second, third], None).unwrap();
        assert_eq!(
            trace.issues,
            vec![
                "AMBIGUOUS_CALL_ID".to_string(),
                "UNPAIRED_TOOL_RESULT".to_string(),
                "UNPAIRED_TOOL_RESULT".to_string(),
                "UNSUPPORTED_MESSAGE_CONTENT".to_string(),
                "PENDING_TOOL_CALL".to_string(),
            ]
        );
    }

    #[test]
    fn non_text_tool_parts_are_flagged_but_text_is_joined() {
        let mut assistant = TraceMessage::new(ROLE_ASSISTANT, "");
        assistant.blocks = vec![
            tool_use("c1", "read"),
            TraceBlock::ToolResult(TraceToolResult {
                id: Some("c1".to_string()),
                name: "read".to_string(),
                parts: vec![
                    TraceResultPart::Text("a".to_string()),
                    TraceResultPart::Other,
                    TraceResultPart::Text("b".to_string()),
                ],
                state: None,
                metadata: BTreeMap::new(),
            }),
        ];
        let trace = JevTrace::from_messages("t", &[assistant], None).unwrap();
        assert_eq!(trace.issues, vec!["UNSUPPORTED_TOOL_CONTENT".to_string()]);
        assert_eq!(trace.list("tool_calls")[0]["result"], json!("a\nb"));
        assert_eq!(
            trace.list("tool_calls")[0]["result_state"],
            json!("UNKNOWN")
        );
    }

    #[test]
    fn duplicate_results_report_unpaired() {
        let mut assistant = TraceMessage::new(ROLE_ASSISTANT, "");
        assistant.blocks = vec![
            tool_use("c1", "read"),
            tool_result("c1", "read", "first"),
            tool_result("c1", "read", "second"),
        ];
        let trace = JevTrace::from_messages("t", &[assistant], None).unwrap();
        assert_eq!(trace.list("tool_calls")[0]["result"], json!("first"));
        assert_eq!(trace.issues, vec!["UNPAIRED_TOOL_RESULT".to_string()]);
    }
}
