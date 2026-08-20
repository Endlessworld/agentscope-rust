//! Session configuration option construction for the ACP CodingAgent.
//!
//! Mirrors `SessionConfigOptionsFactory` from the Kotlin agent: builds the
//! `model` and `mode` select options advertised to the client.

use agent_client_protocol::schema::v1::{
    SessionConfigKind, SessionConfigOption, SessionConfigSelect, SessionConfigSelectOption,
    SessionConfigSelectOptions, SessionConfigValueId,
};

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

/// Reads the current value of a select config option (or returns `None`).
pub fn select_current_value(option: &SessionConfigOption) -> Option<String> {
    match &option.kind {
        SessionConfigKind::Select(SessionConfigSelect { current_value, .. }) => {
            Some(current_value.to_string())
        }
        _ => None,
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
}
