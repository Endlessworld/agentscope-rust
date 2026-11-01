//! Session configuration option construction for the ACP CodingAgent.
//!
//! Mirrors `SessionConfigOptionsFactory` from the Kotlin agent: builds the
//! `model` and `mode` select options advertised to the client.

use agent_client_protocol::schema::v1::{
    SessionConfigGroupId, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelect, SessionConfigSelectGroup, SessionConfigSelectOption,
    SessionConfigSelectOptions, SessionConfigValueId,
};
use agentscope_core::model::catalog::{ModelCatalog, ModelDescriptor, ResolveRequest};
use agentscope_core::{PermissionBehavior, PermissionContextState, PermissionMode, PermissionRule};

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
#[derive(Debug, Clone, Default)]
pub struct ModelChoice {
    pub id: String,
    pub name: String,
    /// Local provider key (`Go`, `Zen`, ...), used for grouping and catalog lookup.
    pub provider: Option<String>,
    /// Resolved catalog key, when known.
    pub catalog_key: Option<String>,
    /// Provider base URL, used to disambiguate shared gateway hosts.
    pub base_url: Option<String>,
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
/// (the model/provider default applies). Explicit levels are mapped to
/// `reasoning_effort` in `session::thought_level_params`.
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
    .category(SessionConfigOptionCategory::ThoughtLevel)
}

/// Derives thought-level ids from a catalog descriptor.
///
/// Unknown models (no descriptor) keep the historical choices so existing clients do
/// not lose the selector before the first catalog refresh.
pub fn thought_level_ids(descriptor: Option<&ModelDescriptor>) -> Vec<String> {
    let Some(descriptor) = descriptor else {
        return thought_level_choices()
            .into_iter()
            .map(|(id, _)| id.to_string())
            .collect();
    };
    if !descriptor.reasoning {
        return Vec::new();
    }
    let mut ids = vec!["auto".to_string()];
    for option in &descriptor.reasoning_options {
        match option {
            agentscope_core::model::catalog::ReasoningOption::Toggle => {
                // A toggle cannot be mapped to a portable `reasoning_effort`
                // value (some providers use a separate `reasoning.enabled`
                // field), so do not advertise an unsupported `off` value.
            }
            agentscope_core::model::catalog::ReasoningOption::Effort { values } => {
                for value in values {
                    if !ids.iter().any(|id| id == value) {
                        ids.push(value.clone());
                    }
                }
            }
            agentscope_core::model::catalog::ReasoningOption::BudgetTokens => {}
        }
    }
    ids
}

/// Derives thought-level ids for a model choice list.
pub fn thought_level_ids_for(model: &str, models: &[ModelChoice]) -> Vec<String> {
    let descriptor = models
        .iter()
        .find(|choice| choice.id == model)
        .and_then(resolve_choice);
    thought_level_ids(descriptor.as_deref())
}

/// Builds a thought-level option from a descriptor.
///
/// The option is always emitted so clients keep a visible selector: a model the
/// catalog reports as non-reasoning still advertises `auto` (which sends no
/// reasoning parameter) instead of losing the control entirely.
pub fn thought_level_option_for(
    current: &str,
    descriptor: Option<&ModelDescriptor>,
) -> SessionConfigOption {
    let mut ids = thought_level_ids(descriptor);
    if ids.is_empty() {
        ids.push(DEFAULT_THOUGHT_LEVEL.to_string());
    }
    let current = if ids.iter().any(|id| id == current) {
        current
    } else {
        DEFAULT_THOUGHT_LEVEL
    };
    let options: Vec<SessionConfigSelectOption> = ids
        .into_iter()
        .map(|id| {
            let label = thought_level_label(&id);
            SessionConfigSelectOption::new(id, label)
        })
        .collect();
    SessionConfigOption::select(
        CONFIG_THOUGHT_LEVEL,
        "Thought",
        SessionConfigValueId::new(current),
        SessionConfigSelectOptions::Ungrouped(options),
    )
    .category(SessionConfigOptionCategory::ThoughtLevel)
    .description("Reasoning effort applied to the next prompt")
}

fn thought_level_label(id: &str) -> String {
    match id {
        "auto" => "Auto".into(),
        "off" | "disabled" | "none" => "Off".into(),
        "low" => "Low".into(),
        "medium" => "Medium".into(),
        "high" => "High".into(),
        "xhigh" => "XHigh".into(),
        "max" => "Max".into(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

/// Builds the `model` select config option, enriched with catalog cards when the
/// catalog has a matching entry.
///
/// The list is emitted flat by default: grouped selects render as an empty
/// dropdown in several clients (IntelliJ IDEA 2026 among them). Callers that know
/// the client renders groups can opt in via [`model_option_grouped`].
pub fn model_option(current: &str, models: &[ModelChoice]) -> SessionConfigOption {
    model_option_with(current, models, false)
}

/// Builds the `model` select option, grouping choices by provider when
/// `grouped` is set and every choice carries a provider key.
pub fn model_option_grouped(current: &str, models: &[ModelChoice]) -> SessionConfigOption {
    model_option_with(current, models, true)
}

fn model_option_with(current: &str, models: &[ModelChoice], grouped: bool) -> SessionConfigOption {
    let mut by_provider: std::collections::BTreeMap<String, Vec<SessionConfigSelectOption>> =
        std::collections::BTreeMap::new();
    let mut ungrouped = Vec::new();
    let mut can_group = grouped;

    for choice in models {
        let option = model_select_option(choice);
        match choice.provider.as_deref() {
            Some(provider) if !provider.is_empty() => {
                by_provider
                    .entry(provider.to_string())
                    .or_default()
                    .push(option);
            }
            _ => {
                can_group = false;
                ungrouped.push(option);
            }
        }
    }

    let options = if models.is_empty() {
        // Never advertise a `currentValue` that is absent from the list: clients
        // render an empty dropdown (or drop the option) in that case.
        SessionConfigSelectOptions::Ungrouped(vec![SessionConfigSelectOption::new(
            current.to_string(),
            current.to_string(),
        )])
    } else if can_group && !by_provider.is_empty() {
        let mut groups = Vec::new();
        for (provider, options) in by_provider {
            groups.push(SessionConfigSelectGroup::new(
                SessionConfigGroupId::new(provider.clone()),
                provider,
                options,
            ));
        }
        if !ungrouped.is_empty() {
            groups.push(SessionConfigSelectGroup::new(
                SessionConfigGroupId::new("other"),
                "Other",
                ungrouped,
            ));
        }
        SessionConfigSelectOptions::Grouped(groups)
    } else {
        // Preserve the configured order when flattening.
        let flat: Vec<SessionConfigSelectOption> = models.iter().map(model_select_option).collect();
        SessionConfigSelectOptions::Ungrouped(flat)
    };

    SessionConfigOption::select(
        "model",
        "Model",
        SessionConfigValueId::new(current),
        options,
    )
    .category(SessionConfigOptionCategory::Model)
    .description("Model capabilities, context window and input types follow the selection")
}

/// Builds one model choice, attaching the catalog card as `_meta` when known.
fn model_select_option(choice: &ModelChoice) -> SessionConfigSelectOption {
    let mut option = SessionConfigSelectOption::new(choice.id.clone(), choice.name.clone());
    let descriptor = resolve_choice(choice);
    if let Some(descriptor) = &descriptor {
        option = option.description(model_description(descriptor));
        let card = serde_json::json!({
            "schema": 1,
            "catalogKey": descriptor.key,
            "confidence": "catalog",
            "provider": descriptor.provider,
            "model": descriptor.id,
            "contextWindow": descriptor.context_window,
            "maxInputTokens": descriptor.input_limit(
                agentscope_core::model::catalog::DEFAULT_RESERVED_OUTPUT
            ),
            "maxOutputTokens": descriptor.max_output,
            "inputModalities": descriptor.input_modalities,
            "outputModalities": descriptor.output_modalities,
            "attachment": descriptor.attachment,
            "capabilities": {
                "toolCall": descriptor.tool_call,
                "reasoning": descriptor.reasoning,
                "structuredOutput": descriptor.structured_output,
                "temperature": descriptor.temperature,
                "interleavedReasoning": descriptor.interleaved_field,
            },
            "reasoningLevels": thought_level_ids(Some(descriptor)),
            "useCases": descriptor.use_cases,
            "status": descriptor.status,
            "cost": descriptor.cost,
        });
        let meta = serde_json::json!({ "agentscope.dev/model": card })
            .as_object()
            .cloned()
            .unwrap_or_default();
        return option.meta(meta);
    }
    option = option.description(format!("{} · default 256k · text + text", choice.name));
    let card = serde_json::json!({
        "schema": 1,
        "catalogKey": null,
        "confidence": "default",
        "provider": choice.provider,
        "model": choice.id,
        "contextWindow": agentscope_core::model::catalog::DEFAULT_CONTEXT_WINDOW,
        "maxInputTokens": agentscope_core::model::catalog::DEFAULT_CONTEXT_WINDOW
            .saturating_sub(agentscope_core::model::catalog::DEFAULT_RESERVED_OUTPUT)
            .saturating_sub(5_120),
        "maxOutputTokens": agentscope_core::model::catalog::DEFAULT_RESERVED_OUTPUT,
        "inputModalities": ["text"],
        "outputModalities": ["text"],
        "attachment": false,
        "capabilities": {
            "toolCall": true,
            "reasoning": false,
            "structuredOutput": null,
            "temperature": true,
            "interleavedReasoning": null,
        },
        "reasoningLevels": ["auto"],
        "useCases": [],
        "status": "active",
        "cost": null,
    });
    let meta = serde_json::json!({ "agentscope.dev/model": card })
        .as_object()
        .cloned()
        .unwrap_or_default();
    option.meta(meta)
}

pub(crate) fn resolve_choice(choice: &ModelChoice) -> Option<std::sync::Arc<ModelDescriptor>> {
    let catalog = ModelCatalog::global();
    let bare = choice
        .provider
        .as_ref()
        .and_then(|provider| choice.id.strip_prefix(&format!("{provider}/")))
        .unwrap_or(&choice.id);
    let local_models = [bare.to_string()];
    let result = catalog.resolve(&ResolveRequest {
        provider_key: choice.provider.as_deref(),
        model_id: &choice.id,
        base_url: choice.base_url.as_deref(),
        local_models: &local_models,
    });
    result.descriptor
}

/// Whether any selectable model accepts image input, per the catalog.
///
/// `initialize` only needs this boolean, and answering it with [`resolve_choice`] forces the whole
/// ~15 MB model table to be parsed. Hosts therefore compute it while the table is already resident
/// (startup, each refresh) and cache it — see `AcpCodingAgent::image_support`.
pub(crate) fn image_support(models: &[ModelChoice]) -> bool {
    models
        .iter()
        .filter_map(resolve_choice)
        .any(|descriptor| descriptor.input_modalities.contains("image"))
}

fn model_description(descriptor: &ModelDescriptor) -> String {
    let mut parts = vec![
        format!("{} ctx", human_tokens(descriptor.context_window)),
        format!("{} out", human_tokens(descriptor.max_output)),
        descriptor
            .input_modalities
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("+"),
    ];
    if descriptor.reasoning {
        parts.push("reasoning".into());
    }
    if descriptor.status != agentscope_core::model::catalog::ModelStatus::Active {
        parts.push(format!("{:?}", descriptor.status).to_ascii_lowercase());
    }
    format!("{} · {}", descriptor.name, parts.join(" · "))
}

fn human_tokens(tokens: usize) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
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
    .category(SessionConfigOptionCategory::Mode)
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
    default_config_options_with_thought_grouped(
        current_model,
        models,
        current_mode,
        modes,
        current_thought_level,
        false,
    )
}

/// Same as [`default_config_options_with_thought`], with an explicit choice
/// between a grouped and a flat model list.
pub fn default_config_options_with_thought_grouped(
    current_model: &str,
    models: &[ModelChoice],
    current_mode: &str,
    modes: &[ModeChoice],
    current_thought_level: &str,
    group_models: bool,
) -> Vec<SessionConfigOption> {
    let model = if group_models {
        model_option_grouped(current_model, models)
    } else {
        model_option(current_model, models)
    };
    let descriptor = models
        .iter()
        .find(|choice| choice.id == current_model)
        .and_then(resolve_choice);
    vec![
        model,
        mode_option(current_mode, modes),
        thought_level_option_for(current_thought_level, descriptor.as_deref()),
    ]
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
        ModeChoice {
            id: MODE_PLAN.to_string(),
            name: "Plan".to_string(),
        },
        ModeChoice {
            id: MODE_ACCEPT_EDITS.to_string(),
            name: "Accept Edits".to_string(),
        },
        ModeChoice {
            id: MODE_YOLO.to_string(),
            name: "YOLO".to_string(),
        },
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
            let mut builder = PermissionContextState::builder().mode(PermissionMode::AcceptEdits);
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
            ModelChoice {
                id: "openai:gpt-4o".into(),
                name: "GPT-4o".into(),
                ..Default::default()
            },
            ModelChoice {
                id: "deepseek:chat".into(),
                name: "DeepSeek".into(),
                ..Default::default()
            },
        ];
        let opt = model_option("openai:gpt-4o", &models);
        assert_eq!(opt.id.to_string(), "model");
        assert_eq!(select_current_value(&opt).as_deref(), Some("openai:gpt-4o"));
        assert_eq!(opt.category, Some(SessionConfigOptionCategory::Model));
    }

    fn provider_models() -> Vec<ModelChoice> {
        vec![
            ModelChoice {
                id: "a".into(),
                name: "Go/a".into(),
                provider: Some("Go".into()),
                ..Default::default()
            },
            ModelChoice {
                id: "b".into(),
                name: "Go/b".into(),
                provider: Some("Go".into()),
                ..Default::default()
            },
            ModelChoice {
                id: "c".into(),
                name: "Zen/c".into(),
                provider: Some("Zen".into()),
                ..Default::default()
            },
        ]
    }

    #[test]
    fn model_option_is_flat_by_default() {
        // Grouped selects render as an empty dropdown in several clients, so the
        // default form must stay flat while preserving every choice.
        let opt = model_option("a", &provider_models());
        match &opt.kind {
            SessionConfigKind::Select(select) => match &select.options {
                SessionConfigSelectOptions::Ungrouped(items) => {
                    let ids: Vec<&str> = items.iter().map(|i| i.value.0.as_ref()).collect();
                    assert_eq!(ids, vec!["a", "b", "c"]);
                }
                other => panic!("expected ungrouped options, got {other:?}"),
            },
            other => panic!("expected a select option, got {other:?}"),
        }
    }

    #[test]
    fn model_option_grouped_keeps_provider_groups() {
        let opt = model_option_grouped("a", &provider_models());
        match &opt.kind {
            SessionConfigKind::Select(select) => match &select.options {
                SessionConfigSelectOptions::Grouped(groups) => {
                    let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
                    assert_eq!(names, vec!["Go", "Zen"]);
                }
                other => panic!("expected grouped options, got {other:?}"),
            },
            other => panic!("expected a select option, got {other:?}"),
        }
    }

    #[test]
    fn thought_level_option_is_always_emitted() {
        // An unknown model keeps the historical choices so clients still show the
        // selector before the first catalog refresh.
        let opts = default_config_options_with_thought("m", &[], "yolo", &[], "auto");
        let thought = opts
            .iter()
            .find(|opt| opt.id.to_string() == CONFIG_THOUGHT_LEVEL)
            .expect("thought level option present");
        assert_eq!(
            thought.category,
            Some(SessionConfigOptionCategory::ThoughtLevel)
        );
    }

    #[test]
    fn mode_option_builds_select() {
        let modes = vec![ModeChoice {
            id: "yolo".into(),
            name: "YOLO".into(),
        }];
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
            assert!(rules
                .iter()
                .any(|r| r.behavior == PermissionBehavior::Allow));
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
