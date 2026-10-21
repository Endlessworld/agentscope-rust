//! Pure, transient projection of [`TaskContextState`] onto the model's message list.
//! Mirrors `io.agentscope.core.middleware.TaskContextProjection`.
//!
//! Nothing here mutates or persists: the projection is recomputed for every model call.
//! It (a) demotes superseded `todo_write` full receipts to short receipts so the model
//! cannot mistake a historical status for the live plan, and (b) appends one synthetic
//! `<TASK_STATE>` message carrying the task scope, requirements and bounded verification
//! results.
//!
//! The synthetic message is *never* appended to durable conversation history: it is added
//! to the per-call projection only and carries
//! [`Msg::METADATA_SYNTHETIC`]/[`Msg::METADATA_REMINDER_KIND`] so a replay can drop it.

use crate::message::{ContentBlock, JsonMap, Msg, MsgRole, TextBlock, ToolResultBlock};
use crate::state::context::{TaskContextState, TaskScope};
use crate::state::requirement::TaskRequirement;
use crate::tool::todo::{
    render, METADATA_REPRESENTATION, METADATA_STATE_KEY, METADATA_STATE_REVISION,
    REPRESENTATION_FULL, REPRESENTATION_RECEIPT, STATE_KEY_TASKS_CONTEXT,
};

/// Message id of the synthetic task-state reminder (stable so replays can dedupe it).
pub const TASK_STATE_MESSAGE_ID: &str = "harness:todo_state";

/// [`Msg::METADATA_REMINDER_KIND`] value of the synthetic task-state reminder.
pub const TASK_STATE_REMINDER_KIND: &str = "todo_state";

/// Runtime-context key carrying the committed task snapshot for one model call.
///
/// The context compiler runs inside the middleware chain and holds no agent state, so the
/// snapshot travels with the call context (Java resolves the live `AgentState` from its
/// runtime context instead). A value stored here is read-only for the duration of the call.
pub const TASK_CONTEXT_STATE_CONTEXT_KEY: &str = "agentscope.task_context_state";

/// Projects the full task state (requirements *and* verification results).
pub fn project_all(input: Vec<Msg>, state: &TaskContextState) -> Vec<Msg> {
    project(input, state, true, true)
}

/// Projects the task state, selecting the requirement and verification sections
/// independently.
///
/// Requirement/evidence projections require an explicitly enabled capability; callers that
/// only want todo hygiene pass `false` for both flags.
pub fn project(
    input: Vec<Msg>,
    state: &TaskContextState,
    requirements: bool,
    verification_results: bool,
) -> Vec<Msg> {
    // Project from a detached snapshot: a concurrent mutation must not tear the output.
    let state = state.snapshot();
    let rendered = render(state.tasks());

    // The last *current* full receipt is the only todo rendering that may be trusted as the
    // agent's live plan. `position` counts blocks across all messages (Java parity).
    let mut last_full: Option<usize> = None;
    let mut position = 0usize;
    for message in &input {
        for block in message.content() {
            if let ContentBlock::ToolResult(result) = block {
                if current(result, &state, &rendered) {
                    last_full = Some(position);
                }
            }
            position += 1;
        }
    }
    let visible = last_full.is_some();

    let mut messages = Vec::with_capacity(input.len() + 1);
    position = 0;
    for message in input {
        if is_task_state_reminder(&message) {
            position += message.content().len();
            continue;
        }
        let mut blocks: Vec<ContentBlock> = Vec::with_capacity(message.content().len());
        let mut changed = false;
        for block in message.content() {
            match block {
                ContentBlock::ToolResult(result)
                    if tracked(result)
                        && representation(result) == Some(REPRESENTATION_FULL)
                        && Some(position) != last_full =>
                {
                    blocks.push(ContentBlock::ToolResult(demote(result)));
                    changed = true;
                }
                other => blocks.push(other.clone()),
            }
            position += 1;
        }
        messages.push(if changed { message.with_content(blocks) } else { message });
    }

    let render_scope_section = requirements || verification_results;
    let needed = (!visible && (!state.tasks().is_empty() || state.revision() > 0))
        || (requirements && !state.requirements().is_empty())
        || (render_scope_section && state.scope().is_some())
        || (verification_results && !state.verifications().is_empty());
    if !needed {
        return messages;
    }

    let mut text = format!("<TASK_STATE revision=\"{}\">\n", state.revision());
    if render_scope_section {
        text.push_str(&render_scope(state.scope()));
    }
    if visible {
        text.push_str("Todo progress: use the current full tool receipt.\n");
    } else {
        text.push_str("Current todo status (agent-maintained, not independent verification):\n");
        text.push_str(&escape(&rendered));
    }
    if requirements {
        text.push_str(&render_requirements(state.requirements()));
    }
    if verification_results {
        text.push_str(&render_verifications(&state));
    }
    text.push_str("\n</TASK_STATE>");

    let mut metadata = JsonMap::new();
    metadata.insert(
        Msg::METADATA_SYNTHETIC.to_string(),
        serde_json::Value::Bool(true),
    );
    metadata.insert(
        Msg::METADATA_REMINDER_KIND.to_string(),
        serde_json::Value::String(TASK_STATE_REMINDER_KIND.to_string()),
    );
    messages.push(
        Msg::builder()
            .id(TASK_STATE_MESSAGE_ID)
            .role(MsgRole::User)
            .name("system")
            .text_content(text)
            .metadata(metadata)
            .build(),
    );
    messages
}

/// Whether the block is a receipt of the session task context.
fn tracked(result: &ToolResultBlock) -> bool {
    result.name.as_deref() == Some("todo_write")
        && result.metadata.get(METADATA_STATE_KEY).and_then(|v| v.as_str())
            == Some(STATE_KEY_TASKS_CONTEXT)
}

fn representation(result: &ToolResultBlock) -> Option<&str> {
    result
        .metadata
        .get(METADATA_REPRESENTATION)
        .and_then(|v| v.as_str())
}

fn receipt_revision(result: &ToolResultBlock) -> Option<u64> {
    result
        .metadata
        .get(METADATA_STATE_REVISION)
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
        })
}

/// Whether the receipt is the complete current state of *this* revision.
fn current(result: &ToolResultBlock, state: &TaskContextState, rendered: &str) -> bool {
    tracked(result)
        && receipt_revision(result) == Some(state.revision())
        && representation(result) == Some(REPRESENTATION_FULL)
        && result.output.len() == 1
        && match &result.output[0] {
            ContentBlock::Text(text) => text.text == rendered,
            _ => false,
        }
}

fn is_task_state_reminder(message: &Msg) -> bool {
    message
        .metadata
        .get(Msg::METADATA_SYNTHETIC)
        .and_then(|v| v.as_bool())
        == Some(true)
        && message
            .metadata
            .get(Msg::METADATA_REMINDER_KIND)
            .and_then(|v| v.as_str())
            == Some(TASK_STATE_REMINDER_KIND)
}

/// Rewrites a superseded full receipt into a short receipt pointing at the live state.
fn demote(result: &ToolResultBlock) -> ToolResultBlock {
    let mut metadata = result.metadata.clone();
    metadata.insert(
        METADATA_REPRESENTATION.to_string(),
        serde_json::Value::String(REPRESENTATION_RECEIPT.to_string()),
    );
    let revision = result
        .metadata
        .get(METADATA_STATE_REVISION)
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_string());
    ToolResultBlock {
        id: result.id.clone(),
        name: result.name.clone(),
        output: vec![ContentBlock::Text(TextBlock::new(format!(
            "Todo update recorded at revision {revision}; use the current task state, not this \
             historical status."
        )))],
        metadata,
        state: result.state,
    }
}

fn render_scope(scope: Option<&TaskScope>) -> String {
    match scope {
        None => String::new(),
        Some(scope) => format!(
            "Task: {}\nObjective: {}\nObjective source: {}\n",
            escape(&scope.task_id),
            escape(&scope.objective),
            escape(&scope.source_ref)
        ),
    }
}

fn render_requirements(requirements: &[TaskRequirement]) -> String {
    if requirements.is_empty() {
        return String::new();
    }
    let mut text = String::from(
        "\n<REQUIREMENTS>\nCANDIDATE = unconfirmed proposal; CONFIRMED = authorized requirement, \
         not verified satisfaction; REJECTED = inactive. Proposed source references are \
         unverified.\n",
    );
    for requirement in requirements {
        text.push_str(&format!(
            "{} | {} | {} | {}\nProposed source: {}",
            escape(&requirement.id),
            requirement.kind,
            requirement.status,
            escape(&requirement.text),
            escape(&requirement.proposed_source_ref)
        ));
        if let Some(decision) = &requirement.decision {
            text.push_str(&format!(
                "\nDecision: {} | {}",
                decision.authority,
                escape(&decision.source_ref)
            ));
        }
        text.push('\n');
    }
    text.push_str("</REQUIREMENTS>");
    text
}

fn render_verifications(state: &TaskContextState) -> String {
    if state.verifications().is_empty() {
        return String::new();
    }
    let binding = state.evidence_binding();
    let mut text = String::from(
        "\n<VERIFICATION>\nThese are bounded checks, not an overall task completion verdict. STALE \
         evidence must not be used for current acceptance.\n",
    );
    for report in state.verifications() {
        let current = binding.as_ref() == Some(&report.binding);
        text.push_str(&format!(
            "{} | {} | verifier={} | evidence=verification_{} | action={}\nChecked version: \
             {}\nReason: {}\n",
            escape(&report.requirement_id),
            if current {
                report.outcome.as_str().to_string()
            } else {
                "STALE".to_string()
            },
            escape(&report.verifier_id),
            escape(&report.id),
            escape(&report.action_id),
            escape(&report.binding.subject_version),
            escape(&report.reason)
        ));
    }
    text.push_str("</VERIFICATION>");
    text
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::context::Task;
    use crate::state::requirement::{
        RequirementAuthority, RequirementDecision, RequirementKind, RequirementStatus,
    };
    use crate::state::verification::{TaskVerification, VerificationOutcome};
    use serde_json::json;

    fn task(subject: &str) -> Task {
        Task::builder().subject(subject).description(subject).build()
    }

    fn receipt(tool_call_id: &str, revision: u64, rendered: &str) -> ToolResultBlock {
        let mut metadata = JsonMap::new();
        metadata.insert(
            METADATA_STATE_KEY.to_string(),
            json!(STATE_KEY_TASKS_CONTEXT),
        );
        metadata.insert(METADATA_STATE_REVISION.to_string(), json!(revision));
        metadata.insert(
            METADATA_REPRESENTATION.to_string(),
            json!(REPRESENTATION_FULL),
        );
        ToolResultBlock {
            id: Some(tool_call_id.to_string()),
            name: Some("todo_write".to_string()),
            output: vec![ContentBlock::Text(TextBlock::new(rendered))],
            metadata,
            state: crate::message::ToolResultState::Success,
        }
    }

    fn tool_results(message: &Msg) -> Vec<&ToolResultBlock> {
        message
            .content()
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult(result) => Some(result),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn historical_full_receipts_are_demoted_and_the_live_receipt_is_kept() {
        let mut state = TaskContextState::new();
        state.replace_tasks(vec![task("step one")], 0).unwrap();
        // A requirement keeps the projection interesting: the synthetic message is needed
        // even though a live receipt is visible.
        state
            .propose(
                RequirementKind::Constraint,
                "keep tests offline",
                "user message 2",
            )
            .unwrap();
        let rendered = render(state.tasks());

        let input = vec![
            Msg::user("plan"),
            Msg::tool_blocks(vec![receipt(
                "t1",
                1,
                "1 open todo(s):\n- [ ] step two",
            )]),
            Msg::tool_blocks(vec![receipt("t2", state.revision(), &rendered)]),
        ];
        let out = project_all(input, &state);
        assert_eq!(out.len(), 4);

        let stale = tool_results(&out[1]);
        assert_eq!(stale.len(), 1);
        assert_eq!(
            stale[0].metadata.get(METADATA_REPRESENTATION).and_then(|v| v.as_str()),
            Some(REPRESENTATION_RECEIPT)
        );
        assert_eq!(
            stale[0].text_content(),
            "Todo update recorded at revision 1; use the current task state, not this historical status."
        );
        assert_eq!(stale[0].id.as_deref(), Some("t1"));
        assert_eq!(stale[0].name.as_deref(), Some("todo_write"));

        let live = tool_results(&out[2]);
        assert_eq!(
            live[0].metadata.get(METADATA_REPRESENTATION).and_then(|v| v.as_str()),
            Some(REPRESENTATION_FULL)
        );
        assert_eq!(live[0].text_content(), rendered);

        let reminder = &out[3];
        assert_eq!(reminder.id(), TASK_STATE_MESSAGE_ID);
        assert_eq!(reminder.name(), Some("system"));
        assert_eq!(reminder.role(), MsgRole::User);
        assert_eq!(
            reminder.metadata.get(Msg::METADATA_SYNTHETIC),
            Some(&json!(true))
        );
        assert_eq!(
            reminder
                .metadata
                .get(Msg::METADATA_REMINDER_KIND)
                .and_then(|v| v.as_str()),
            Some(TASK_STATE_REMINDER_KIND)
        );
        let text = reminder.text_content();
        assert!(
            text.starts_with(&format!(
                "<TASK_STATE revision=\"{}\">\nTodo progress: use the current full tool receipt.\n",
                state.revision()
            )),
            "{text}"
        );
        // The agent-maintained rendering is *not* repeated when a live receipt exists.
        assert!(!text.contains("Current todo status"), "{text}");
        assert!(text.contains("keep tests offline"), "{text}");
        assert!(text.ends_with("\n</TASK_STATE>"), "{text}");
    }

    #[test]
    fn task_state_renders_scope_requirements_and_verifications() {
        let mut state = TaskContextState::new();
        state
            .begin_task(
                TaskScope::new("task-1", "ship & verify <it>", "user message 1").unwrap(),
                0,
            )
            .unwrap();
        let revision = state.revision();
        state.set_subject_version("v1", revision).unwrap();
        let id = state
            .propose(
                RequirementKind::AcceptanceCriterion,
                "suite is green",
                "user message 2",
            )
            .unwrap();
        let revision = state.revision();
        state
            .decide(
                &id,
                RequirementStatus::Confirmed,
                RequirementDecision::new(RequirementAuthority::User, "user message 2").unwrap(),
                revision,
            )
            .unwrap();
        let revision = state.revision();
        let report = TaskVerification::new(
            "ver-1",
            id.clone(),
            "call-1",
            "cargo test",
            VerificationOutcome::Passed,
            "all green",
            state.evidence_binding().unwrap(),
            1,
        )
        .unwrap();
        state.record_verification(report, revision).unwrap();

        let out = project(Vec::new(), &state, true, true);
        assert_eq!(out.len(), 1);
        let text = out[0].text_content();
        assert!(
            text.starts_with(&format!("<TASK_STATE revision=\"{}\">\n", state.revision())),
            "{text}"
        );
        assert!(
            text.contains(
                "Task: task-1\nObjective: ship &amp; verify &lt;it&gt;\nObjective source: user \
                 message 1\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "\n<REQUIREMENTS>\nCANDIDATE = unconfirmed proposal; CONFIRMED = authorized \
                 requirement, not verified satisfaction; REJECTED = inactive. Proposed source \
                 references are unverified.\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "{id} | ACCEPTANCE_CRITERION | CONFIRMED | suite is green\nProposed source: user \
                 message 2\nDecision: USER | user message 2\n"
            )),
            "{text}"
        );
        assert!(
            text.contains(
                "\n<VERIFICATION>\nThese are bounded checks, not an overall task completion \
                 verdict. STALE evidence must not be used for current acceptance.\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "{id} | PASSED | verifier=cargo test | evidence=verification_ver-1 | \
                 action=call-1\nChecked version: v1\nReason: all green\n"
            )),
            "{text}"
        );
        assert!(text.ends_with("\n</TASK_STATE>"), "{text}");
    }

    #[test]
    fn evidence_from_another_subject_renders_as_stale() {
        let mut state = TaskContextState::new();
        state
            .begin_task(TaskScope::new("task-1", "ship it", "user message 1").unwrap(), 0)
            .unwrap();
        let revision = state.revision();
        state.set_subject_version("v1", revision).unwrap();
        let id = state
            .propose(
                RequirementKind::AcceptanceCriterion,
                "suite is green",
                "user message 2",
            )
            .unwrap();
        let revision = state.revision();
        state
            .decide(
                &id,
                RequirementStatus::Confirmed,
                RequirementDecision::new(RequirementAuthority::Caller, "ticket-7").unwrap(),
                revision,
            )
            .unwrap();
        let revision = state.revision();
        let report = TaskVerification::new(
            "ver-1",
            id.clone(),
            "call-1",
            "cargo test",
            VerificationOutcome::Passed,
            "all green",
            state.evidence_binding().unwrap(),
            1,
        )
        .unwrap();
        state.record_verification(report, revision).unwrap();
        // The workspace changed after the check: the evidence is no longer current.
        let revision = state.revision();
        state.invalidate_subject(revision).unwrap();

        let out = project(Vec::new(), &state, true, true);
        let text = out[0].text_content();
        assert!(
            text.contains(&format!(
                "{id} | STALE | verifier=cargo test | evidence=verification_ver-1 | \
                 action=call-1\n"
            )),
            "{text}"
        );
        assert!(!text.contains("| PASSED |"), "{text}");
    }

    #[test]
    fn an_existing_task_state_reminder_is_dropped_instead_of_duplicated() {
        let mut state = TaskContextState::new();
        state.replace_tasks(vec![task("step one")], 0).unwrap();
        let rendered = render(state.tasks());
        let mut metadata = JsonMap::new();
        metadata.insert(Msg::METADATA_SYNTHETIC.to_string(), json!(true));
        metadata.insert(
            Msg::METADATA_REMINDER_KIND.to_string(),
            json!(TASK_STATE_REMINDER_KIND),
        );
        let previous = Msg::builder()
            .id(TASK_STATE_MESSAGE_ID)
            .role(MsgRole::User)
            .name("system")
            .text_content("<TASK_STATE revision=\"0\">stale</TASK_STATE>")
            .metadata(metadata)
            .build();

        let input = vec![
            previous,
            Msg::tool_blocks(vec![receipt("t1", state.revision(), &rendered)]),
        ];
        let out = project_all(input, &state);
        assert_eq!(out.len(), 1);
        assert!(out.iter().all(|message| message.id() != TASK_STATE_MESSAGE_ID));
        assert_eq!(tool_results(&out[0]).len(), 1);
    }

    #[test]
    fn an_empty_state_leaves_the_history_untouched() {
        let state = TaskContextState::new();
        let input = vec![Msg::user("hi"), Msg::assistant("hello")];
        let out = project_all(input.clone(), &state);
        assert_eq!(out, input);
    }
}
