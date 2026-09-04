//! Session configuration option construction for the ACP CodingAgent.
//!
//! Mirrors `SessionConfigOptionsFactory` from the Kotlin agent: builds the
//! `model` and `mode` select options advertised to the client.

use agent_client_protocol::schema::v1::{
    SessionConfigKind, SessionConfigOption, SessionConfigSelect, SessionConfigSelectOption,
    SessionConfigSelectOptions, SessionConfigValueId,
};
use agentscope_core::{
    PermissionBehavior, PermissionContextState, PermissionMode, PermissionRule,
};

/// Mode id for the read-only exploration/planning mode.
pub const MODE_PLAN: &str = "plan";
/// Mode id for the auto-approve file edits / ask shell mode.
pub const MODE_ACCEPT_EDITS: &str = "accept_edits";
/// Mode id for the fully automatic mode (no permission prompts).
pub const MODE_YOLO: &str = "yolo";

/// Tools auto-approved in `accept_edits` mode (file writes + the internal
/// `todo_write` bookkeeping tool). Everything else that is not read-only
/// (e.g. the `execute` shell tool) falls through to `Ask` and requires human
/// confirmation.
const ACCEPT_EDITS_AUTO_ALLOW_TOOLS: &[&str] = &["write_file", "edit_file", "todo_write"];

/// Tools allowed in `accept_edits` mode whose rule_content matches any input.
const ACCEPT_EDITS_RULE_SOURCE: &str = "accept_edits-default";

/// An available model descriptor (id + display name).
#[derive(Debug, Clone)]
pub struct ModelChoice {
    pub id: String,
    pub name: String,
}

/// An available mode descriptor (id + display name).
#[derive(Debug, Clone)]
pub struct ModeChoice {
    pub id: String,
    pub name: String,
}

/// The thought-level (reasoning effort) selector id advertised to clients.
pub const CONFIG_THOUGHT_LEVEL: &str = "thought_level";

/// Default thought level: `auto` sends no reasoning parameter at all
/// (the model/provider default applies). Explicit levels are mapped
/// per-model-capability in `session::thought_level_params`.
pub const DEFAULT_THOUGHT_LEVEL: &str = "auto";

/// The available thought-level choices, mirroring Java `ThoughtLevel`:
/// `disabled` (no thinking) / `low` / `medium` / `high` / `max`.
pub fn thought_level_choices() -> Vec<(&'static str, &'static str)> {
    vec![
        ("auto", "Auto"),
        ("disabled", "Disabled"),
        ("low", "Low"),
        ("medium", "Medium"),
        ("high", "High"),
        ("max", "Max"),
    ]
}

/// Builds the `thought_level` select config option.
pub fn thought_level_option(current: &str) -> SessionConfigOption {
    let options: Vec<SessionConfigSelectOption> = thought_level_choices()
        .into_iter()
        .map(|(id, label)| SessionConfigSelectOption::new(id, label))
        .collect();
    SessionConfigOption::select(
        CONFIG_THOUGHT_LEVEL,
        "Thought",
        SessionConfigValueId::new(current),
        SessionConfigSelectOptions::Ungrouped(options),
    )
}

/// Builds the `model` select config option.
pub fn model_option(current: &str, models: &[ModelChoice]) -> SessionConfigOption {
    let options: Vec<SessionConfigSelectOption> = models
        .iter()
        .map(|m| SessionConfigSelectOption::new(m.id.clone(), m.name.clone()))
        .collect();
    SessionConfigOption::select(
        "model",
        "Model",
        SessionConfigValueId::new(current),
        SessionConfigSelectOptions::Ungrouped(options),
    )
}

/// Builds the `mode` select config option.
pub fn mode_option(current: &str, modes: &[ModeChoice]) -> SessionConfigOption {
    let options: Vec<SessionConfigSelectOption> = modes
        .iter()
        .map(|m| SessionConfigSelectOption::new(m.id.clone(), m.name.clone()))
        .collect();
    SessionConfigOption::select(
        "mode",
        "Mode",
        SessionConfigValueId::new(current),
        SessionConfigSelectOptions::Ungrouped(options),
    )
}

/// Returns the default set of config options (model + mode).
pub fn default_config_options(
    current_model: &str,
    models: &[ModelChoice],
    current_mode: &str,
    modes: &[ModeChoice],
) -> Vec<SessionConfigOption> {
    vec![
        model_option(current_model, models),
        mode_option(current_mode, modes),
    ]
}

/// Returns the default set of config options (model + mode + thought level).
pub fn default_config_options_with_thought(
    current_model: &str,
    models: &[ModelChoice],
    current_mode: &str,
    modes: &[ModeChoice],
    current_thought_level: &str,
) -> Vec<SessionConfigOption> {
    let mut opts = default_config_options(current_model, models, current_mode, modes);
    opts.push(thought_level_option(current_thought_level));
    opts
}

/// Reads the current value of a select config option (or returns `None`).
pub fn select_current_value(option: &SessionConfigOption) -> Option<String> {
    match &option.kind {
        SessionConfigKind::Select(SessionConfigSelect { current_value, .. }) => {
            Some(current_value.to_string())
        }
        _ => None,
    }
}

/// The canonical ACP mode list exposed to clients, ordered by escalating
/// autonomy: `plan` (read-only) → `accept_edits` (auto file edits, shell asks)
/// → `yolo` (everything auto-approved).
pub fn default_modes() -> Vec<ModeChoice> {
    vec![
        ModeChoice { id: MODE_PLAN.to_string(), name: "Plan".to_string() },
        ModeChoice { id: MODE_ACCEPT_EDITS.to_string(), name: "Accept Edits".to_string() },
        ModeChoice { id: MODE_YOLO.to_string(), name: "YOLO".to_string() },
    ]
}

/// The mode selected by default for a fresh session.
pub fn default_mode() -> &'static str {
    MODE_ACCEPT_EDITS
}

/// Maps an ACP session mode id onto a framework permission context ("default
/// rules"), mirroring the Java `AgentMode` semantics:
///
/// - `plan` → `Explore`: read-only tools are auto-allowed, everything else is
///   denied (a design/planning phase never mutates the workspace).
/// - `accept_edits` → `AcceptEdits` + auto-allow rules for file-edit and
///   `todo_write` tools; the `execute` shell tool has no allow rule and is
///   therefore `Ask`-ed to the user for confirmation.
/// - `yolo` → `Bypass`: all permission checks are skipped.
///
/// Unknown modes fall back to the default (trivial) context which allows
/// everything — matching pre-existing behaviour for unconfigured agents.
pub fn permission_context_for_mode(mode: &str) -> PermissionContextState {
    match mode {
        MODE_PLAN => PermissionContextState::builder()
            .mode(PermissionMode::Explore)
            .build(),
        MODE_ACCEPT_EDITS => {
            let mut builder = PermissionContextState::builder()
                .mode(PermissionMode::AcceptEdits);
            for tool in ACCEPT_EDITS_AUTO_ALLOW_TOOLS {
                builder = builder.add_allow_rule(
                    *tool,
                    PermissionRule::new(
                        *tool,
                        None,
                        PermissionBehavior::Allow,
                        ACCEPT_EDITS_RULE_SOURCE,
                    ),
                );
            }
            builder.build()
        }
        MODE_YOLO => PermissionContextState::builder()
            .mode(PermissionMode::Bypass)
            .build(),
        _ => PermissionContextState::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_option_builds_select() {
        let models = vec![
            ModelChoice { id: "openai:gpt-4o".into(), name: "GPT-4o".into() },
            ModelChoice { id: "deepseek:chat".into(), name: "DeepSeek".into() },
        ];
        let opt = model_option("openai:gpt-4o", &models);
        assert_eq!(opt.id.to_string(), "model");
        assert_eq!(select_current_value(&opt).as_deref(), Some("openai:gpt-4o"));
    }

    #[test]
    fn mode_option_builds_select() {
        let modes = vec![ModeChoice { id: "yolo".into(), name: "YOLO".into() }];
        let opt = mode_option("yolo", &modes);
        assert_eq!(opt.id.to_string(), "mode");
        assert_eq!(select_current_value(&opt).as_deref(), Some("yolo"));
    }

    #[test]
    fn default_config_options_contains_model_and_mode() {
        let opts = default_config_options("m", &[], "yolo", &[]);
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].id.to_string(), "model");
        assert_eq!(opts[1].id.to_string(), "mode");
    }

    #[test]
    fn default_modes_are_the_three_supported_modes() {
        let ids: Vec<String> = default_modes().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![MODE_PLAN, MODE_ACCEPT_EDITS, MODE_YOLO]);
    }

    #[test]
    fn plan_maps_to_explore_read_only() {
        let ctx = permission_context_for_mode(MODE_PLAN);
        assert_eq!(ctx.mode, PermissionMode::Explore);
        assert!(!ctx.is_trivial());
    }

    #[test]
    fn accept_edits_maps_to_accept_edits_with_auto_allow_rules() {
        let ctx = permission_context_for_mode(MODE_ACCEPT_EDITS);
        assert_eq!(ctx.mode, PermissionMode::AcceptEdits);
        // Every file-edit tool must carry an allow rule so no Ask interrupts.
        for tool in ACCEPT_EDITS_AUTO_ALLOW_TOOLS {
            let rules = ctx.allow_rules.get(*tool).expect("auto-allow rule present");
            assert!(rules.iter().any(|r| r.behavior == PermissionBehavior::Allow));
        }
        // The shell tool must NOT be allow-listed: it should ask the user.
        assert!(ctx.allow_rules.get("execute").is_none());
    }

    #[test]
    fn yolo_maps_to_bypass() {
        let ctx = permission_context_for_mode(MODE_YOLO);
        assert_eq!(ctx.mode, PermissionMode::Bypass);
    }

    #[test]
    fn unknown_mode_falls_back_to_trivial_allow_all() {
        let ctx = permission_context_for_mode("whatever");
        assert!(ctx.is_trivial());
    }
}
