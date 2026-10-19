//! Ollama model options, mirroring the option surface of
//! `io.agentscope.extensions.model.ollama.options.OllamaOptions`.
//!
//! Only the sampling/modelling knobs that AgentScope exposes through
//! `GenerateOptions` plus the transport-level `format`/`keep_alive`/`think`
//! fields are ported here; the long tail of llama.cpp loader flags is tracked in
//! `MODEL__GAP_TRACKER.md` §3 as P2.

use std::collections::BTreeMap;

use agentscope_core::GenerateOptions;

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
            format: native_format(options).or_else(|| options.additional_body_params.get("format").cloned()),
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
        }
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
        arguments: if arguments.is_empty() { None } else { Some(arguments) },
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
}
