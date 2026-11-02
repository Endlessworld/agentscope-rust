//! Ollama model options, mirroring the option surface of
//! `io.agentscope.extensions.model.ollama.options.OllamaOptions`.
//!
//! The typed fields cover everything AgentScope exposes through
//! `GenerateOptions` plus the transport-level `format`/`keep_alive`/`think`
//! fields. Java additionally models ~1300 lines of llama.cpp loader flags
//! (`num_batch`, `rope_frequency_base`, `mirostat`, `tfs_z`, `typical_p`, ...);
//! instead of mirroring each one, [`OllamaOptions::extra`] forwards an arbitrary
//! key/value map into the same `options` object Ollama accepts, so any of them
//! (present or future) can be sent without touching this crate.

use std::collections::BTreeMap;

use agentscope_core::{ExecutionConfig, GenerateOptions};

use super::dto::OllamaFunction;

/// The `think` capability: either a boolean toggle or a named level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThinkOption {
    /// Enable/disable chain-of-thought.
    Boolean(bool),
    /// Granular level (e.g. `low`, `medium`, `high`).
    Level(String),
}

impl ThinkOption {
    /// Converts to the JSON primitive Ollama expects (bool or string).
    pub fn to_json_value(&self) -> serde_json::Value {
        match self {
            ThinkOption::Boolean(b) => serde_json::Value::Bool(*b),
            ThinkOption::Level(l) => serde_json::Value::String(l.clone()),
        }
    }
}

/// Ollama-specific generation options.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OllamaOptions {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    /// Maps to the Ollama `num_predict` option (a.k.a. max tokens).
    pub num_predict: Option<u32>,
    pub seed: Option<i64>,
    pub frequency_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub repeat_penalty: Option<f64>,
    pub stop: Option<Vec<String>>,
    pub num_ctx: Option<u32>,
    pub num_gpu: Option<u32>,
    pub num_thread: Option<u32>,
    pub model: Option<String>,
    pub format: Option<serde_json::Value>,
    pub keep_alive: Option<String>,
    pub think: Option<ThinkOption>,
    /// Execution budget (timeout / retry) carried alongside the wire options.
    ///
    /// Mirrors Java's `OllamaOptions.executionConfig`: it is *not* a wire field
    /// (see [`Self::to_options_map`]) but must survive the conversion from
    /// [`GenerateOptions`] and the merge with the model defaults, otherwise a
    /// per-call timeout / retry limit is silently dropped.
    pub execution_config: Option<ExecutionConfig>,
    /// Pass-through for llama.cpp / Ollama options without a typed field
    /// (`num_batch`, `tfs_z`, `mirostat`, `rope_frequency_base`, ...).
    ///
    /// Entries are written into the request's `options` object; a typed field
    /// with the same key always wins.
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl OllamaOptions {
    /// Returns an all-`None` option set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Extracts the Ollama-relevant subset of [`GenerateOptions`].
    pub fn from_generate_options(options: &GenerateOptions) -> Self {
        Self {
            temperature: options.temperature,
            top_p: options.top_p,
            top_k: options.top_k,
            num_predict: options.max_tokens,
            seed: options.seed,
            frequency_penalty: options.frequency_penalty,
            presence_penalty: options.presence_penalty,
            model: options.model_name.clone(),
            format: native_format(options)
                .or_else(|| options.additional_body_params.get("format").cloned()),
            execution_config: options.execution_config,
            ..Default::default()
        }
    }

    /// Merges `other` over `self`, with `other` winning on every set field.
    pub fn merge(&self, other: &OllamaOptions) -> OllamaOptions {
        OllamaOptions {
            temperature: other.temperature.or(self.temperature),
            top_p: other.top_p.or(self.top_p),
            top_k: other.top_k.or(self.top_k),
            num_predict: other.num_predict.or(self.num_predict),
            seed: other.seed.or(self.seed),
            frequency_penalty: other.frequency_penalty.or(self.frequency_penalty),
            presence_penalty: other.presence_penalty.or(self.presence_penalty),
            repeat_penalty: other.repeat_penalty.or(self.repeat_penalty),
            stop: other.stop.clone().or_else(|| self.stop.clone()),
            num_ctx: other.num_ctx.or(self.num_ctx),
            num_gpu: other.num_gpu.or(self.num_gpu),
            num_thread: other.num_thread.or(self.num_thread),
            model: other.model.clone().or_else(|| self.model.clone()),
            format: other.format.clone().or_else(|| self.format.clone()),
            keep_alive: other.keep_alive.clone().or_else(|| self.keep_alive.clone()),
            think: other.think.clone().or_else(|| self.think.clone()),
            // Java's `OllamaOptions.merge` lets the incoming (per-call) execution
            // config replace the default one wholesale.
            execution_config: other.execution_config.or(self.execution_config),
            extra: {
                let mut extra = self.extra.clone();
                extra.extend(other.extra.clone());
                extra
            },
        }
    }

    /// Adds a free-form `options` entry (llama.cpp loader flag or similar).
    pub fn with_extra(
        mut self,
        key: impl Into<String>,
        value: impl Into<serde_json::Value>,
    ) -> Self {
        self.extra.insert(key.into(), value.into());
        self
    }

    /// Builds the `options` map sent inside the request body.
    ///
    /// `format`, `keep_alive` and `think` are top-level request fields and are
    /// therefore excluded from this map (matching `OllamaToolsHelper`). The
    /// execution config is an execution-layer concern and is excluded as well
    /// (Java `OllamaChatModelExecutionOptionsTest` asserts the same).
    pub fn resolved_execution_config(&self, per_call: Option<ExecutionConfig>) -> ExecutionConfig {
        let defaults = ExecutionConfig::model_defaults();
        ExecutionConfig::merge_configs(per_call.as_ref(), self.execution_config.as_ref())
            .and_then(|merged| ExecutionConfig::merge_configs(Some(&merged), Some(&defaults)))
            .unwrap_or(defaults)
    }

    /// Builds the `options` map sent inside the request body.
    ///
    /// `format`, `keep_alive` and `think` are top-level request fields and are
    /// therefore excluded from this map (matching `OllamaToolsHelper`).
    pub fn to_options_map(&self) -> BTreeMap<String, serde_json::Value> {
        let mut map = BTreeMap::new();
        if let Some(v) = self.temperature {
            map.insert("temperature".into(), serde_json::json!(v));
        }
        if let Some(v) = self.top_p {
            map.insert("top_p".into(), serde_json::json!(v));
        }
        if let Some(v) = self.top_k {
            map.insert("top_k".into(), serde_json::json!(v));
        }
        if let Some(v) = self.num_predict {
            map.insert("num_predict".into(), serde_json::json!(v));
        }
        if let Some(v) = self.seed {
            map.insert("seed".into(), serde_json::json!(v));
        }
        if let Some(v) = self.frequency_penalty {
            map.insert("frequency_penalty".into(), serde_json::json!(v));
        }
        if let Some(v) = self.presence_penalty {
            map.insert("presence_penalty".into(), serde_json::json!(v));
        }
        if let Some(v) = self.repeat_penalty {
            map.insert("repeat_penalty".into(), serde_json::json!(v));
        }
        if let Some(v) = self.num_ctx {
            map.insert("num_ctx".into(), serde_json::json!(v));
        }
        if let Some(v) = self.num_gpu {
            map.insert("num_gpu".into(), serde_json::json!(v));
        }
        if let Some(v) = self.num_thread {
            map.insert("num_thread".into(), serde_json::json!(v));
        }
        if let Some(v) = &self.stop {
            map.insert("stop".into(), serde_json::json!(v));
        }
        // Long-tail passthrough last: typed fields win on a key collision.
        for (key, value) in &self.extra {
            map.entry(key.clone()).or_insert_with(|| value.clone());
        }
        map
    }
}

/// Maps a core [`ResponseFormat`] onto Ollama's native top-level `format` field.
///
/// Ollama expects the bare JSON Schema document (not the OpenAI
/// `response_format` wrapper) for structured output, and the literal `"json"`
/// for a JSON-object response. Mirrors `OllamaOptions.fromGenerateOptions`.
fn native_format(options: &GenerateOptions) -> Option<serde_json::Value> {
    let rf = options.response_format.as_ref()?;
    match rf.format_type.as_str() {
        "json_schema" => rf
            .json_schema
            .as_ref()
            .map(|js| serde_json::Value::Object(js.schema.clone())),
        "json_object" => Some(serde_json::json!("json")),
        _ => None,
    }
}

/// Converts a tool-call argument map into the Ollama function payload.
pub fn function_from_parts(
    name: impl Into<String>,
    arguments: BTreeMap<String, serde_json::Value>,
) -> OllamaFunction {
    OllamaFunction {
        name: name.into(),
        arguments: if arguments.is_empty() {
            None
        } else {
            Some(arguments)
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_generate_options_maps_sampling_fields() {
        let mut gen = GenerateOptions::default();
        gen.temperature = Some(0.7);
        gen.top_p = Some(0.9);
        gen.top_k = Some(40);
        gen.max_tokens = Some(512);
        gen.seed = Some(42);

        let opts = OllamaOptions::from_generate_options(&gen);
        assert_eq!(opts.temperature, Some(0.7));
        assert_eq!(opts.num_predict, Some(512));

        let map = opts.to_options_map();
        assert_eq!(map.get("num_predict"), Some(&serde_json::json!(512)));
        assert_eq!(map.get("top_k"), Some(&serde_json::json!(40)));
        assert!(!map.contains_key("format"));
        assert!(!map.contains_key("think"));
    }

    #[test]
    fn from_generate_options_maps_json_schema_to_native_format() {
        use agentscope_core::formatter::{JsonSchema, ResponseFormat};
        let mut schema = serde_json::Map::new();
        schema.insert("type".into(), serde_json::json!("object"));
        schema.insert(
            "properties".into(),
            serde_json::json!({"city": {"type": "string"}}),
        );
        let mut gen = GenerateOptions::default();
        gen.response_format = Some(ResponseFormat::json_schema(JsonSchema::new(
            "Weather",
            schema.clone(),
        )));

        let opts = OllamaOptions::from_generate_options(&gen);
        // Ollama wants the bare schema, not the OpenAI response_format wrapper.
        assert_eq!(opts.format, Some(serde_json::Value::Object(schema)));
    }

    #[test]
    fn from_generate_options_maps_json_object_to_literal_json() {
        use agentscope_core::formatter::ResponseFormat;
        let mut gen = GenerateOptions::default();
        gen.response_format = Some(ResponseFormat::json_object());
        assert_eq!(
            OllamaOptions::from_generate_options(&gen).format,
            Some(serde_json::json!("json"))
        );
    }

    #[test]
    fn from_generate_options_ignores_text_response_format() {
        use agentscope_core::formatter::ResponseFormat;
        let mut gen = GenerateOptions::default();
        gen.response_format = Some(ResponseFormat::text());
        assert!(OllamaOptions::from_generate_options(&gen).format.is_none());
    }

    #[test]
    fn merge_prefers_other() {
        let base = OllamaOptions {
            temperature: Some(0.1),
            top_k: Some(10),
            ..Default::default()
        };
        let runtime = OllamaOptions {
            temperature: Some(0.9),
            ..Default::default()
        };
        let merged = base.merge(&runtime);
        assert_eq!(merged.temperature, Some(0.9));
        assert_eq!(merged.top_k, Some(10));
    }

    #[test]
    fn from_generate_options_preserves_execution_config() {
        let config = ExecutionConfig {
            timeout: Some(std::time::Duration::from_millis(250)),
            max_attempts: Some(3),
            ..Default::default()
        };
        let gen = GenerateOptions {
            execution_config: Some(config),
            temperature: Some(0.3),
            ..Default::default()
        };

        let opts = OllamaOptions::from_generate_options(&gen);
        assert_eq!(opts.execution_config, Some(config));
        // The execution budget must never leak into the wire body.
        assert!(!opts.to_options_map().contains_key("timeout"));
        assert!(!opts.to_options_map().contains_key("max_attempts"));
    }

    #[test]
    fn merge_prefers_per_call_execution_config() {
        let default = ExecutionConfig {
            timeout: Some(std::time::Duration::from_secs(60)),
            max_attempts: Some(7),
            ..Default::default()
        };
        let per_call = ExecutionConfig {
            timeout: Some(std::time::Duration::from_millis(200)),
            ..Default::default()
        };
        let base = OllamaOptions {
            execution_config: Some(default),
            ..Default::default()
        };
        let runtime = OllamaOptions {
            execution_config: Some(per_call),
            ..Default::default()
        };

        let merged = base.merge(&runtime);
        assert_eq!(merged.execution_config, Some(per_call));
        // Unset fields fall back to the model defaults, not to the other config.
        assert_eq!(merged.resolved_execution_config(None).max_attempts, Some(7));
    }

    #[test]
    fn resolved_execution_config_falls_back_to_model_defaults() {
        let opts = OllamaOptions::default();
        let resolved = opts.resolved_execution_config(None);
        assert_eq!(resolved.timeout, ExecutionConfig::model_defaults().timeout);
        assert_eq!(
            resolved.max_attempts,
            ExecutionConfig::model_defaults().max_attempts
        );
    }

    #[test]
    fn think_option_serializes_to_primitive() {
        assert_eq!(
            ThinkOption::Boolean(true).to_json_value(),
            serde_json::json!(true)
        );
        assert_eq!(
            ThinkOption::Level("high".into()).to_json_value(),
            serde_json::json!("high")
        );
    }

    #[test]
    fn extra_options_reach_the_wire_map_and_lose_to_typed_fields() {
        let options = OllamaOptions::new()
            .with_extra("num_batch", 512)
            .with_extra("tfs_z", 1.0)
            .with_extra("top_k", 999);
        let map = options.to_options_map();
        assert_eq!(map.get("num_batch"), Some(&serde_json::json!(512)));
        assert_eq!(map.get("tfs_z"), Some(&serde_json::json!(1.0)));
        // No typed value set, so the passthrough is used as-is.
        assert_eq!(map.get("top_k"), Some(&serde_json::json!(999)));

        let typed = OllamaOptions {
            top_k: Some(40),
            ..Default::default()
        }
        .with_extra("top_k", 999);
        assert_eq!(
            typed.to_options_map().get("top_k"),
            Some(&serde_json::json!(40))
        );
    }

    #[test]
    fn merge_unions_extra_options_with_the_per_call_side_winning() {
        let defaults = OllamaOptions::new()
            .with_extra("num_batch", 128)
            .with_extra("mirostat", 1);
        let per_call = OllamaOptions::new().with_extra("num_batch", 512);
        let merged = defaults.merge(&per_call);
        assert_eq!(merged.extra.get("num_batch"), Some(&serde_json::json!(512)));
        assert_eq!(merged.extra.get("mirostat"), Some(&serde_json::json!(1)));
    }
}
