//! The two built-in deterministic verifiers, mirroring `ShellExitCodeVerifier` and
//! `JsonResultSchemaVerifier`.
//!
//! Both are *selected* by the caller: they check one already-settled action against an explicit,
//! machine-checkable expectation and never infer an assertion from natural language. Every
//! precondition that cannot be established yields [`VerificationOutcome::Unknown`], so a missing
//! or contradictory evidence set can never read back as a pass.
use agentscope_core::message::{ContentBlock, ToolExecutionOutcome, ToolResultState};
use agentscope_core::observation::ActionStatus;
use agentscope_core::state::VerificationOutcome;
use agentscope_core::tool::ToolValidator;

use super::{DeterministicVerifier, SettledAction, Verdict};
use crate::{Error, Result};

/// Largest schema JSON [`JsonResultSchemaVerifier`] accepts, matching Java.
const MAX_SCHEMA_BYTES: usize = 65_536;
/// Largest output [`JsonResultSchemaVerifier`] inspects, matching Java.
const MAX_OUTPUT_CHARS: usize = 262_144;

/// The `kind` a shell producer reports in its execution details.
const SHELL_KIND: &str = "shell";

fn unknown(reason: &str) -> Verdict {
    Verdict::new(VerificationOutcome::Unknown, reason)
}

fn failed(reason: &str) -> Verdict {
    Verdict::new(VerificationOutcome::Failed, reason)
}

fn passed(reason: &str) -> Verdict {
    Verdict::new(VerificationOutcome::Passed, reason)
}

/// Checks a caller-selected shell attempt, not arbitrary task semantics.
///
/// Mirrors Java `ShellExitCodeVerifier`: the evidence must be one typed `shell` execution whose
/// observation and tool result agree, and only a complete, untruncated process result is judged.
pub struct ShellExitCodeVerifier {
    tool_name: String,
    expected_exit_code: i32,
    id: String,
}

impl ShellExitCodeVerifier {
    /// Creates a verifier for `tool_name` expecting `expected_exit_code`.
    ///
    /// # Panics
    /// When the name is blank or longer than 128 characters (Java throws the same
    /// `IllegalArgumentException`).
    pub fn new(tool_name: impl Into<String>, expected_exit_code: i32) -> Self {
        let tool_name = tool_name.into();
        assert!(
            !tool_name.trim().is_empty() && tool_name.chars().count() <= 128,
            "Bounded tool name required"
        );
        let id = format!("shell-exit-v1:{tool_name}:{expected_exit_code}");
        Self {
            tool_name,
            expected_exit_code,
            id,
        }
    }

    /// The expected process exit code.
    pub fn expected_exit_code(&self) -> i32 {
        self.expected_exit_code
    }
}

impl DeterministicVerifier for ShellExitCodeVerifier {
    fn id(&self) -> &str {
        &self.id
    }

    fn verify(&self, evidence: &SettledAction) -> Verdict {
        let observation = &evidence.observation;
        let Some(result) = evidence.result.as_ref() else {
            return unknown("Missing or inconsistent typed shell evidence");
        };
        let Some(details) = observation.execution_details.as_ref() else {
            return unknown("Missing or inconsistent typed shell evidence");
        };
        // The observation and the result must describe the same call, and the execution
        // details the observation settled with must be the ones the producer attached.
        if observation.tool_name != self.tool_name
            || result.is_suspended()
            || matches!(
                result.state,
                ToolResultState::Denied | ToolResultState::Interrupted
            )
            || details.kind != SHELL_KIND
            || Some(&observation.tool_call_id) != result.id.as_ref()
            || Some(&observation.tool_name) != result.name.as_ref()
            || result.execution_details().as_ref() != Some(details)
        {
            return unknown("Missing or inconsistent typed shell evidence");
        }
        if !matches!(
            observation.status,
            ActionStatus::Returned | ActionStatus::Failed
        ) {
            return unknown("Action did not settle with a process result");
        }
        let Some(exit_code) = details.exit_code else {
            return unknown("Process outcome is incomplete");
        };
        if matches!(
            details.outcome,
            ToolExecutionOutcome::Unknown | ToolExecutionOutcome::Partial
        ) {
            return unknown("Process outcome is incomplete");
        }
        if (exit_code == 0) != (details.outcome == ToolExecutionOutcome::Succeeded) {
            return unknown("Inconsistent exit code and producer outcome");
        }
        if exit_code == 0
            && (observation.status == ActionStatus::Failed
                || result.state == ToolResultState::Error)
        {
            return unknown("Tool failure contradicts the reported successful process");
        }
        if exit_code != self.expected_exit_code {
            return failed("Exit code did not match the configured expectation");
        }
        if details.output_truncated {
            return unknown("Captured output is truncated");
        }
        passed(
            "Selected shell attempt returned the configured exit code; no broader semantic assertion",
        )
    }
}

/// Explicit output-contract check; never infers a schema from a natural-language criterion.
///
/// Mirrors Java `JsonResultSchemaVerifier`: the evidence must be one successful textual result
/// whose single text block is a valid JSON value conforming to the configured schema.
pub struct JsonResultSchemaVerifier {
    schema: serde_json::Map<String, serde_json::Value>,
    id: String,
}

impl JsonResultSchemaVerifier {
    /// Creates a verifier for `schema` (a JSON Schema object).
    ///
    /// # Panics
    /// When the schema is absent or empty, exceeds the size bound, or references another schema
    /// (`$ref` / `$dynamicRef` / `$recursiveRef` / `$schema` / `$id`). Java rejects exactly these
    /// in the constructor so a verifier can never reach for a remote document at check time.
    pub fn new(schema: serde_json::Value) -> Self {
        let serde_json::Value::Object(schema) = schema else {
            panic!("Explicit schema required");
        };
        assert!(!schema.is_empty(), "Explicit schema required");
        let json = serde_json::Value::Object(schema.clone()).to_string();
        assert!(json.len() <= MAX_SCHEMA_BYTES, "Schema too large");
        reject_references(&serde_json::Value::Object(schema.clone()));
        Self {
            schema,
            id: format!("json-schema-v1:{}", hex_sha256(json.as_bytes())),
        }
    }
}

/// Rejects schemas that would need another document or a dialect override to interpret.
fn reject_references(value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if matches!(
                    key.as_str(),
                    "$ref" | "$dynamicRef" | "$recursiveRef" | "$schema" | "$id"
                ) {
                    panic!(
                        "Referenced schemas and dialect overrides are not supported by this bounded offline verifier"
                    );
                }
                reject_references(child);
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(reject_references),
        _ => {}
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl DeterministicVerifier for JsonResultSchemaVerifier {
    fn id(&self) -> &str {
        &self.id
    }

    fn verify(&self, evidence: &SettledAction) -> Verdict {
        let action = &evidence.observation;
        let Some(result) = evidence.result.as_ref() else {
            return unknown("Expected one successful textual JSON result");
        };
        if action.status != ActionStatus::Returned
            || !matches!(
                result.state,
                ToolResultState::Success | ToolResultState::Running
            )
            || result.is_suspended()
            || Some(&action.tool_call_id) != result.id.as_ref()
            || Some(&action.tool_name) != result.name.as_ref()
            || result.output.len() != 1
        {
            return unknown("Expected one successful textual JSON result");
        }
        let ContentBlock::Text(text) = &result.output[0] else {
            return unknown("Expected one successful textual JSON result");
        };
        if let Some(details) = result.execution_details() {
            if details.output_truncated || details.outcome != ToolExecutionOutcome::Succeeded {
                return unknown("Producer outcome is incomplete or unsuccessful");
            }
        }
        if text.text.chars().count() > MAX_OUTPUT_CHARS {
            return unknown("Output is absent or exceeds the bounded schema-check size");
        }
        if text.text.trim().is_empty() {
            return failed("JSON output is empty");
        }
        // `from_str` consumes exactly one value, so trailing tokens are a failure.
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text.text) else {
            return failed("Output is not a single valid JSON value");
        };
        match ToolValidator::validate_json_output(&value, &self.schema) {
            None => passed("Selected JSON output conforms to the configured schema"),
            Some(_) => failed("Selected output does not conform to the configured JSON schema"),
        }
    }
}

/// Name of the built-in shell exit-code verifier for [`load`].
pub const SHELL_EXIT_CODE_VERIFIER: &str = "shell_exit_code";
/// Name of the built-in JSON result schema verifier for [`load`].
pub const JSON_RESULT_SCHEMA_VERIFIER: &str = "json_result_schema";
/// Tool name [`load`] assumes when `params.tool_name` is absent.
const DEFAULT_SHELL_TOOL_NAME: &str = "execute";

/// Loads a built-in deterministic verifier by name, mirroring Java `VerificationService#load`.
///
/// Rust call sites name the *check* they want, where Java resolves a previously stored report by
/// id; the two share the "load by identifier, fail loudly on an unknown one" contract. `params`
/// carries the constructor arguments:
///
/// - `shell_exit_code`: `{ "tool_name"?: string, "expected_exit_code"?: integer }`, defaulting to
///   the harness shell tool and exit code 0.
/// - `json_result_schema`: `{ "schema": object }`, required.
pub fn load(name: &str, params: &serde_json::Value) -> Result<Box<dyn DeterministicVerifier>> {
    match name {
        SHELL_EXIT_CODE_VERIFIER => {
            let tool_name = params
                .get("tool_name")
                .and_then(|value| value.as_str())
                .unwrap_or(DEFAULT_SHELL_TOOL_NAME)
                .to_string();
            let expected_exit_code = params
                .get("expected_exit_code")
                .and_then(|value| value.as_i64())
                .unwrap_or(0) as i32;
            Ok(Box::new(ShellExitCodeVerifier::new(
                tool_name,
                expected_exit_code,
            )))
        }
        JSON_RESULT_SCHEMA_VERIFIER => {
            let schema = params.get("schema").cloned().ok_or_else(|| {
                Error::Other("json_result_schema requires a `schema` object".to_string())
            })?;
            Ok(Box::new(JsonResultSchemaVerifier::new(schema)))
        }
        other => Err(Error::Other(format!("Unknown verifier: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use agentscope_core::message::{ToolExecutionDetails, ToolResultBlock};
    use agentscope_core::observation::ActionObservation;

    use super::*;

    fn shell_details(exit_code: i32, truncated: bool) -> ToolExecutionDetails {
        let outcome = if exit_code == 0 {
            ToolExecutionOutcome::Succeeded
        } else {
            ToolExecutionOutcome::Failed
        };
        ToolExecutionDetails::new("shell", outcome)
            .with_exit_code(exit_code)
            .with_output_truncated(truncated)
    }

    fn shell_evidence(exit_code: i32) -> SettledAction {
        let details = shell_details(exit_code, false);
        let result = ToolResultBlock::text(format!("Exit code: {exit_code}"))
            .with_id_and_name("call-1", "execute")
            .with_execution_details(details.clone())
            .with_state(if exit_code == 0 {
                ToolResultState::Success
            } else {
                ToolResultState::Error
            });
        SettledAction {
            observation: ActionObservation {
                action_id: "action-1".into(),
                tool_call_id: "call-1".into(),
                tool_name: "execute".into(),
                agent_id: Some("agent".into()),
                user_id: None,
                session_id: None,
                started_at: 1,
                ended_at: Some(2),
                status: ActionStatus::Returned,
                error_type: None,
                execution_details: Some(details),
            },
            result: Some(result),
            evidence_binding: None,
        }
    }

    #[test]
    fn the_shell_verifier_id_matches_the_java_shape() {
        let verifier = ShellExitCodeVerifier::new("execute", 0);
        assert_eq!(verifier.id(), "shell-exit-v1:execute:0");
        assert_eq!(verifier.expected_exit_code(), 0);
    }

    #[test]
    fn a_matching_exit_code_passes() {
        let verdict = ShellExitCodeVerifier::new("execute", 0).verify(&shell_evidence(0));
        assert_eq!(verdict.outcome, VerificationOutcome::Passed);
        assert!(verdict.reason.contains("no broader semantic assertion"));
    }

    #[test]
    fn a_different_exit_code_fails() {
        let verdict = ShellExitCodeVerifier::new("execute", 0).verify(&shell_evidence(2));
        assert_eq!(verdict.outcome, VerificationOutcome::Failed);
        assert_eq!(
            verdict.reason,
            "Exit code did not match the configured expectation"
        );

        // The same evidence passes when the expectation matches it.
        let verdict = ShellExitCodeVerifier::new("execute", 2).verify(&shell_evidence(2));
        assert_eq!(verdict.outcome, VerificationOutcome::Passed);
    }

    #[test]
    fn evidence_for_another_tool_is_unknown_not_a_pass() {
        let verdict = ShellExitCodeVerifier::new("other", 0).verify(&shell_evidence(0));
        assert_eq!(verdict.outcome, VerificationOutcome::Unknown);
    }

    #[test]
    fn truncated_output_is_unknown() {
        let mut evidence = shell_evidence(0);
        let details = shell_details(0, true);
        evidence.observation.execution_details = Some(details.clone());
        evidence.result = evidence
            .result
            .map(|result| result.with_execution_details(details));
        let verdict = ShellExitCodeVerifier::new("execute", 0).verify(&evidence);
        assert_eq!(verdict.outcome, VerificationOutcome::Unknown);
        assert_eq!(verdict.reason, "Captured output is truncated");
    }

    fn json_evidence(text: &str) -> SettledAction {
        let result = ToolResultBlock::text(text)
            .with_id_and_name("call-1", "report")
            .with_state(ToolResultState::Success);
        SettledAction {
            observation: ActionObservation {
                action_id: "action-1".into(),
                tool_call_id: "call-1".into(),
                tool_name: "report".into(),
                agent_id: None,
                user_id: None,
                session_id: None,
                started_at: 1,
                ended_at: Some(2),
                status: ActionStatus::Returned,
                error_type: None,
                execution_details: None,
            },
            result: Some(result),
            evidence_binding: None,
        }
    }

    fn report_schema() -> JsonResultSchemaVerifier {
        JsonResultSchemaVerifier::new(serde_json::json!({
            "type": "object",
            "required": ["status"],
            "properties": {"status": {"type": "string"}}
        }))
    }

    #[test]
    fn conforming_json_passes() {
        let verdict = report_schema().verify(&json_evidence("{\"status\":\"ok\"}"));
        assert_eq!(verdict.outcome, VerificationOutcome::Passed);
        assert!(verdict.reason.contains("conforms to the configured schema"));
    }

    #[test]
    fn non_conforming_json_fails() {
        let verdict = report_schema().verify(&json_evidence("{\"status\":42}"));
        assert_eq!(verdict.outcome, VerificationOutcome::Failed);
        assert_eq!(
            verdict.reason,
            "Selected output does not conform to the configured JSON schema"
        );
    }

    #[test]
    fn non_json_output_fails_with_the_java_reason() {
        let verdict = report_schema().verify(&json_evidence("not json"));
        assert_eq!(verdict.outcome, VerificationOutcome::Failed);
        assert_eq!(verdict.reason, "Output is not a single valid JSON value");
    }

    #[test]
    fn the_schema_verifier_id_is_derived_from_the_schema_bytes() {
        let id = report_schema().id().to_string();
        assert!(id.starts_with("json-schema-v1:"), "{id}");
        assert_eq!(id.len(), "json-schema-v1:".len() + 64, "{id}");
        // A different schema yields a different id.
        let other = JsonResultSchemaVerifier::new(serde_json::json!({"type": "array"}));
        assert_ne!(other.id(), id);
    }

    #[test]
    #[should_panic(expected = "Explicit schema required")]
    fn an_empty_schema_is_rejected() {
        JsonResultSchemaVerifier::new(serde_json::json!({}));
    }

    #[test]
    #[should_panic(expected = "Referenced schemas and dialect overrides are not supported")]
    fn a_referencing_schema_is_rejected() {
        JsonResultSchemaVerifier::new(serde_json::json!({"$ref": "https://example.com/s.json"}));
    }

    #[test]
    fn load_resolves_the_built_in_verifiers_and_rejects_unknown_names() {
        let verifier = load(SHELL_EXIT_CODE_VERIFIER, &serde_json::Value::Null).unwrap();
        assert_eq!(verifier.id(), "shell-exit-v1:execute:0");

        let verifier = load(
            SHELL_EXIT_CODE_VERIFIER,
            &serde_json::json!({"tool_name": "sh", "expected_exit_code": 3}),
        )
        .unwrap();
        assert_eq!(verifier.id(), "shell-exit-v1:sh:3");

        let verifier = load(
            JSON_RESULT_SCHEMA_VERIFIER,
            &serde_json::json!({"schema": {"type": "object"}}),
        )
        .unwrap();
        assert!(verifier.id().starts_with("json-schema-v1:"));

        assert!(load("no_such_verifier", &serde_json::Value::Null).is_err());
        // The schema verifier cannot default: the contract must be explicit.
        assert!(load(JSON_RESULT_SCHEMA_VERIFIER, &serde_json::Value::Null).is_err());
    }
}
