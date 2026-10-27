//! Per-model capability resolution.
//!
//! OpenAI-compatible endpoints are not uniform: reasoning models (`o1`/`o3`/
//! `o4`/`gpt-5`) reject `temperature`, `top_p`, `frequency_penalty`,
//! `presence_penalty` and `seed`, while many third-party gateways (DeepSeek,
//! GLM, Kimi, Qwen, llama.cpp, ...) only accept the legacy `max_tokens` field
//! and not `max_completion_tokens`.
//!
//! Sending a parameter a model does not support is a hard HTTP 400, so the
//! request builder consults [`resolve_capabilities`] before serialising
//! (mirrors new-api's `GetOpenAIChatCapabilities`).

/// Capabilities that decide which request fields may be sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCapabilities {
    /// Whether `temperature` / `top_p` are accepted.
    pub supports_sampling: bool,
    /// Whether `frequency_penalty` / `presence_penalty` are accepted.
    pub supports_penalties: bool,
    /// Whether `seed` is accepted.
    pub supports_seed: bool,
    /// Whether the model wants `max_completion_tokens` instead of `max_tokens`.
    pub use_max_completion_tokens: bool,
    /// Whether `parallel_tool_calls` is accepted.
    pub supports_parallel_tool_calls: bool,
    /// Whether `stream_options` is accepted.
    pub supports_stream_options: bool,
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            supports_sampling: true,
            supports_penalties: true,
            supports_seed: true,
            use_max_completion_tokens: true,
            supports_parallel_tool_calls: true,
            supports_stream_options: true,
        }
    }
}

/// Partial overrides layered on top of [`ModelCapabilities::default`].
#[derive(Default, Clone, Copy)]
struct Overrides {
    supports_sampling: Option<bool>,
    supports_penalties: Option<bool>,
    supports_seed: Option<bool>,
    use_max_completion_tokens: Option<bool>,
    supports_parallel_tool_calls: Option<bool>,
    supports_stream_options: Option<bool>,
}

impl Overrides {
    fn apply(self, caps: &mut ModelCapabilities) {
        if let Some(v) = self.supports_sampling {
            caps.supports_sampling = v;
        }
        if let Some(v) = self.supports_penalties {
            caps.supports_penalties = v;
        }
        if let Some(v) = self.supports_seed {
            caps.supports_seed = v;
        }
        if let Some(v) = self.use_max_completion_tokens {
            caps.use_max_completion_tokens = v;
        }
        if let Some(v) = self.supports_parallel_tool_calls {
            caps.supports_parallel_tool_calls = v;
        }
        if let Some(v) = self.supports_stream_options {
            caps.supports_stream_options = v;
        }
    }
}

/// Prefix → overrides table. All matching entries apply, shortest prefix first,
/// so a more specific entry (`o3-mini`) layers on top of a broader one (`o3`).
const OVERRIDE_TABLE: &[(&str, Overrides)] = &[
    // OpenAI reasoning models: reject sampling / penalty / seed parameters.
    (
        "o1",
        Overrides {
            supports_sampling: Some(false),
            supports_penalties: Some(false),
            supports_seed: Some(false),
            use_max_completion_tokens: Some(true),
            ..Overrides::NONE
        },
    ),
    (
        "o3",
        Overrides {
            supports_sampling: Some(false),
            supports_penalties: Some(false),
            supports_seed: Some(false),
            use_max_completion_tokens: Some(true),
            ..Overrides::NONE
        },
    ),
    (
        "o4",
        Overrides {
            supports_sampling: Some(false),
            supports_penalties: Some(false),
            supports_seed: Some(false),
            use_max_completion_tokens: Some(true),
            ..Overrides::NONE
        },
    ),
    (
        "gpt-5",
        Overrides {
            supports_sampling: Some(false),
            supports_penalties: Some(false),
            supports_seed: Some(false),
            use_max_completion_tokens: Some(true),
            ..Overrides::NONE
        },
    ),
    // Third-party OpenAI-compatible gateways that only accept `max_tokens`.
    ("deepseek", max_tokens_only()),
    ("glm", max_tokens_only()),
    ("zhipu", max_tokens_only()),
    ("kimi", max_tokens_only()),
    ("moonshot", max_tokens_only()),
    ("qwen", max_tokens_only()),
    ("llama", max_tokens_only()),
    ("mistral", max_tokens_only()),
    ("grok", max_tokens_only()),
    ("gemma", max_tokens_only()),
    ("phi", max_tokens_only()),
    ("ollama", max_tokens_only()),
];

const fn max_tokens_only() -> Overrides {
    Overrides {
        use_max_completion_tokens: Some(false),
        ..Overrides::NONE
    }
}

impl Overrides {
    const NONE: Overrides = Overrides {
        supports_sampling: None,
        supports_penalties: None,
        supports_seed: None,
        use_max_completion_tokens: None,
        supports_parallel_tool_calls: None,
        supports_stream_options: None,
    };
}

/// Clamps `max_tokens` / `max_completion_tokens` to the model's output limit.
pub fn clamp_max_tokens(
    mut options: agentscope_core::GenerateOptions,
    max_output: usize,
) -> agentscope_core::GenerateOptions {
    if max_output == 0 {
        return options;
    }
    let max_output = max_output.min(u32::MAX as usize) as u32;
    options.max_tokens = options.max_tokens.map(|value| value.min(max_output));
    options.max_completion_tokens = options
        .max_completion_tokens
        .map(|value| value.min(max_output));
    options
}

/// Resolves capabilities using catalog data when available, falling back to the
/// prefix table for fields the catalog does not model.
pub fn resolve_capabilities_with(
    model_id: &str,
    descriptor: Option<&agentscope_core::model::catalog::ModelDescriptor>,
) -> ModelCapabilities {
    let mut caps = resolve_capabilities(model_id);
    if let Some(descriptor) = descriptor {
        if !descriptor.temperature {
            // OpenAI reasoning endpoints reject the whole sampling/penalty group.
            caps.supports_sampling = false;
            caps.supports_penalties = false;
        }
        if !descriptor.tool_call {
            caps.supports_parallel_tool_calls = false;
        }
    }
    caps
}

/// Resolves the capabilities of a model id by layering every matching prefix
/// override, shortest first (longest prefix wins on conflicts).
pub fn resolve_capabilities(model_id: &str) -> ModelCapabilities {
    let mut caps = ModelCapabilities::default();
    let lower = model_id.to_ascii_lowercase();
    let mut matches: Vec<(&str, &Overrides)> = OVERRIDE_TABLE
        .iter()
        .filter(|(prefix, _)| lower.starts_with(prefix))
        .map(|(prefix, o)| (*prefix, o))
        .collect();
    matches.sort_by_key(|(prefix, _)| prefix.len());
    for (_, overrides) in matches {
        overrides.apply(&mut caps);
    }
    caps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_models_reject_sampling() {
        for model in ["o1-preview", "o3-mini", "o4-mini", "gpt-5"] {
            let caps = resolve_capabilities(model);
            assert!(!caps.supports_sampling, "{model}");
            assert!(!caps.supports_penalties, "{model}");
            assert!(!caps.supports_seed, "{model}");
            assert!(caps.use_max_completion_tokens, "{model}");
        }
    }

    #[test]
    fn third_party_gateways_use_max_tokens() {
        for model in [
            "deepseek-chat",
            "glm-4",
            "kimi-k2",
            "qwen-max",
            "grok-2",
            "llama3",
        ] {
            let caps = resolve_capabilities(model);
            assert!(!caps.use_max_completion_tokens, "{model}");
            assert!(caps.supports_sampling, "{model}");
        }
    }

    #[test]
    fn openai_flagship_keeps_defaults() {
        let caps = resolve_capabilities("gpt-4o-2024-05-13");
        assert_eq!(caps, ModelCapabilities::default());
    }

    #[test]
    fn longest_prefix_wins() {
        // `gpt-5` disables sampling; an unknown `gpt-5-turbo` still matches it.
        assert!(!resolve_capabilities("gpt-5-turbo").supports_sampling);
    }
}
