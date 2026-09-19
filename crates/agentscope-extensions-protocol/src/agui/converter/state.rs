//! State snapshot/delta conversion, mirroring `AguiStateConverter`.

use serde_json::{Map, Value};

use crate::agui::event::{AguiEvent, JsonPatchOperation};

/// Builds AG-UI state events (snapshots and RFC 6902 deltas) from AgentScope state maps.
///
/// Delta keys are emitted in sorted order. Java iterates a `HashSet`, whose order is
/// unspecified; sorting is a deterministic refinement of the same operation set.
#[derive(Debug, Clone, Copy, Default)]
pub struct AguiStateConverter;

impl AguiStateConverter {
    /// Creates a converter.
    pub fn new() -> Self {
        Self
    }

    /// Creates a `STATE_SNAPSHOT` event from a state map.
    pub fn create_snapshot(
        &self,
        state: &Map<String, Value>,
        thread_id: impl Into<String>,
        run_id: impl Into<String>,
    ) -> AguiEvent {
        AguiEvent::StateSnapshot {
            thread_id: thread_id.into(),
            run_id: run_id.into(),
            snapshot: state.clone(),
            timestamp: None,
            raw_event: None,
        }
    }

    /// Creates a `STATE_DELTA` event, or `None` when the states are equivalent.
    pub fn create_delta(
        &self,
        before: Option<&Map<String, Value>>,
        after: Option<&Map<String, Value>>,
        thread_id: impl Into<String>,
        run_id: impl Into<String>,
    ) -> Option<AguiEvent> {
        let operations = compute_delta(before, after, "");
        if operations.is_empty() {
            return None;
        }
        Some(AguiEvent::StateDelta {
            thread_id: thread_id.into(),
            run_id: run_id.into(),
            delta: operations,
            timestamp: None,
            raw_event: None,
        })
    }

    /// Whether the two states differ.
    pub fn has_changes(
        &self,
        before: Option<&Map<String, Value>>,
        after: Option<&Map<String, Value>>,
    ) -> bool {
        !compute_delta(before, after, "").is_empty()
    }
}

/// Computes the JSON Patch operations transforming `before` into `after`.
fn compute_delta(
    before: Option<&Map<String, Value>>,
    after: Option<&Map<String, Value>>,
    base_path: &str,
) -> Vec<JsonPatchOperation> {
    static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    let empty = EMPTY.get_or_init(Map::new);
    let before = before.unwrap_or(empty);
    let after = after.unwrap_or(empty);

    let mut keys: Vec<&String> = before.keys().chain(after.keys()).collect();
    keys.sort_unstable();
    keys.dedup();

    let mut operations = Vec::new();
    for key in keys {
        let path = format!("{base_path}/{}", escape_json_pointer(key));
        match (before.get(key), after.get(key)) {
            (None, Some(after_value)) => {
                operations.push(JsonPatchOperation::add(path, Some(after_value.clone())))
            }
            (Some(_), None) => operations.push(JsonPatchOperation::remove(path)),
            (Some(before_value), Some(after_value)) => {
                if before_value != after_value {
                    if let (Some(before_map), Some(after_map)) =
                        (before_value.as_object(), after_value.as_object())
                    {
                        operations.extend(compute_delta(Some(before_map), Some(after_map), &path));
                    } else {
                        operations
                            .push(JsonPatchOperation::replace(path, Some(after_value.clone())));
                    }
                }
            }
            (None, None) => {}
        }
    }
    operations
}

/// Escapes a JSON Pointer segment (RFC 6901): `~` -> `~0`, `/` -> `~1`.
fn escape_json_pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn snapshot_carries_the_state_verbatim() {
        let converter = AguiStateConverter::new();
        let state = map(json!({"count": 1, "name": "x"}));
        let event = converter.create_snapshot(&state, "t", "r");
        match event {
            AguiEvent::StateSnapshot {
                thread_id,
                run_id,
                snapshot,
                ..
            } => {
                assert_eq!(thread_id, "t");
                assert_eq!(run_id, "r");
                assert_eq!(snapshot, state);
            }
            other => panic!("expected StateSnapshot, got {other:?}"),
        }
    }

    #[test]
    fn identical_states_produce_no_delta() {
        let converter = AguiStateConverter::new();
        let state = map(json!({"a": 1}));
        assert!(converter
            .create_delta(Some(&state), Some(&state), "t", "r")
            .is_none());
        assert!(!converter.has_changes(Some(&state), Some(&state)));
        assert!(converter.create_delta(None, None, "t", "r").is_none());
        assert!(!converter.has_changes(None, None));
    }

    #[test]
    fn add_remove_and_replace_operations_are_emitted() {
        let converter = AguiStateConverter::new();
        let before = map(json!({"keep": 1, "gone": 2, "changed": "a"}));
        let after = map(json!({"keep": 1, "changed": "b", "new": 3}));
        let event = converter
            .create_delta(Some(&before), Some(&after), "t", "r")
            .unwrap();
        let AguiEvent::StateDelta { delta, .. } = event else {
            panic!("expected StateDelta");
        };
        let rendered: Vec<(String, String, Option<Value>)> = delta
            .iter()
            .map(|op| (op.op.clone(), op.path.clone(), op.value.clone()))
            .collect();
        assert_eq!(
            rendered,
            vec![
                (
                    "replace".to_string(),
                    "/changed".to_string(),
                    Some(json!("b"))
                ),
                ("remove".to_string(), "/gone".to_string(), None),
                ("add".to_string(), "/new".to_string(), Some(json!(3))),
            ]
        );
        assert!(converter.has_changes(Some(&before), Some(&after)));
    }

    #[test]
    fn nested_maps_recurse_into_deeper_paths() {
        let converter = AguiStateConverter::new();
        let before = map(json!({"outer": {"inner": 1, "same": true}}));
        let after = map(json!({"outer": {"inner": 2, "same": true}}));
        let AguiEvent::StateDelta { delta, .. } = converter
            .create_delta(Some(&before), Some(&after), "t", "r")
            .unwrap()
        else {
            panic!("expected StateDelta");
        };
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].op, "replace");
        assert_eq!(delta[0].path, "/outer/inner");
        assert_eq!(delta[0].value, Some(json!(2)));
    }

    #[test]
    fn nested_addition_recurses_when_the_key_already_exists_as_an_object() {
        let converter = AguiStateConverter::new();
        let before = map(json!({"outer": {}}));
        let after = map(json!({"outer": {"inner": 1}}));
        let AguiEvent::StateDelta { delta, .. } = converter
            .create_delta(Some(&before), Some(&after), "t", "r")
            .unwrap()
        else {
            panic!("expected StateDelta");
        };
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].op, "add");
        assert_eq!(delta[0].path, "/outer/inner");
    }

    #[test]
    fn json_pointer_segments_are_escaped() {
        assert_eq!(escape_json_pointer("a~b"), "a~0b");
        assert_eq!(escape_json_pointer("a/b"), "a~1b");
        let converter = AguiStateConverter::new();
        let before = map(json!({}));
        let after = map(json!({"a/b": 1, "c~d": 2}));
        let AguiEvent::StateDelta { delta, .. } = converter
            .create_delta(Some(&before), Some(&after), "t", "r")
            .unwrap()
        else {
            panic!("expected StateDelta");
        };
        let paths: Vec<&str> = delta.iter().map(|op| op.path.as_str()).collect();
        assert_eq!(paths, vec!["/a~1b", "/c~0d"]);
    }

    #[test]
    fn null_and_missing_documents_are_treated_as_empty_objects() {
        let converter = AguiStateConverter::new();
        let after = map(json!({"a": 1}));
        let AguiEvent::StateDelta { delta, .. } = converter
            .create_delta(None, Some(&after), "t", "r")
            .unwrap()
        else {
            panic!("expected StateDelta");
        };
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].op, "add");
        let before = map(json!({"a": 1}));
        let event = converter.create_delta(Some(&before), None, "t", "r").unwrap();
        let AguiEvent::StateDelta { delta, .. } = event else {
            panic!("expected StateDelta");
        };
        assert_eq!(delta[0].op, "remove");
    }

    #[test]
    fn type_changes_replace_instead_of_recursing() {
        let converter = AguiStateConverter::new();
        let before = map(json!({"a": {"nested": 1}}));
        let after = map(json!({"a": "scalar"}));
        let AguiEvent::StateDelta { delta, .. } = converter
            .create_delta(Some(&before), Some(&after), "t", "r")
            .unwrap()
        else {
            panic!("expected StateDelta");
        };
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].op, "replace");
        assert_eq!(delta[0].path, "/a");
        assert_eq!(delta[0].value, Some(json!("scalar")));
    }
}
