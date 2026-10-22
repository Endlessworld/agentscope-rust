//! Parsing and merging of the two models.dev payloads.
//!
//! `models.json?type=all` is the small, flat source; `api.json` is the large source with
//! reasoning/cost/status detail. Both are normalized into the same descriptor shape and
//! merged by catalog key, with the API source filling in detail rather than replacing the
//! flat source's identity.

use std::collections::{BTreeSet, HashMap};

use serde_json::Value;

use super::types::{
    Cost, ModelDescriptor, ModelKind, ModelStatus, ReasoningOption,
};
use crate::Result;

/// One parsed source payload.
#[derive(Debug, Default)]
pub(crate) struct ParsedSource {
    pub entries: Vec<ModelDescriptor>,
    pub provider_api: HashMap<String, String>,
}

/// Parses `https://models.dev/models.json?type=all`.
pub(crate) fn parse_models_json(bytes: &[u8]) -> Result<ParsedSource> {
    let root: Value = serde_json::from_slice(bytes)?;
    let object = root
        .as_object()
        .ok_or_else(|| crate::Error::Model("catalog: models.json root is not an object".into()))?;
    let mut parsed = ParsedSource::default();
    for (key, value) in object {
        let (provider, id) = split_catalog_key(key, value);
        if provider.is_empty() || id.is_empty() {
            continue;
        }
        let mut descriptor = descriptor_from_value(&provider, &id, value);
        descriptor.recompute_fingerprint();
        parsed.entries.push(descriptor);
    }
    Ok(parsed)
}

/// Parses `https://models.dev/api.json`.
pub(crate) fn parse_api_json(bytes: &[u8]) -> Result<ParsedSource> {
    let root: Value = serde_json::from_slice(bytes)?;
    let object = root
        .as_object()
        .ok_or_else(|| crate::Error::Model("catalog: api.json root is not an object".into()))?;
    let mut parsed = ParsedSource::default();
    for (provider, provider_value) in object {
        let api = provider_value
            .get("api")
            .and_then(Value::as_str)
            .or_else(|| {
                provider_value
                    .get("provider")
                    .and_then(|p| p.get("api"))
                    .and_then(Value::as_str)
            })
            .filter(|value| !value.trim().is_empty());
        if let Some(api) = api {
            parsed
                .provider_api
                .insert(provider.clone(), api.to_string());
        }
        let Some(models) = provider_value.get("models").and_then(Value::as_object) else {
            continue;
        };
        for (id, value) in models {
            if id.is_empty() {
                continue;
            }
            let mut descriptor = descriptor_from_value(provider, id, value);
            descriptor.recompute_fingerprint();
            parsed.entries.push(descriptor);
        }
    }
    Ok(parsed)
}

/// Merges the flat source with the detailed source.
///
/// Identity fields always come from `base` (which is keyed by `provider/model`); detail
/// fields prefer `enhanced` but never lose a known value from `base`.
pub(crate) fn merge_descriptors(base: ModelDescriptor, enhanced: ModelDescriptor) -> ModelDescriptor {
    let mut merged = base;
    if !enhanced.name.is_empty() && enhanced.name != enhanced.id {
        merged.name = enhanced.name;
    }
    if enhanced.description.is_some() {
        merged.description = enhanced.description;
    }
    if enhanced.family.is_some() {
        merged.family = enhanced.family;
    }
    if enhanced.kind != ModelKind::Chat || merged.kind == ModelKind::Chat {
        merged.kind = enhanced.kind;
    }
    if enhanced.context_window > 0 {
        merged.context_window = enhanced.context_window;
    }
    if enhanced.max_output > 0 {
        merged.max_output = enhanced.max_output;
    }
    if !enhanced.input_modalities.is_empty() {
        merged.input_modalities = enhanced.input_modalities;
    }
    if !enhanced.output_modalities.is_empty() {
        merged.output_modalities = enhanced.output_modalities;
    }
    merged.attachment |= enhanced.attachment;
    merged.reasoning |= enhanced.reasoning;
    if !enhanced.reasoning_options.is_empty() {
        merged.reasoning_options = enhanced.reasoning_options;
    }
    if enhanced.interleaved_field.is_some() {
        merged.interleaved_field = enhanced.interleaved_field;
    }
    merged.tool_call |= enhanced.tool_call;
    if enhanced.structured_output.is_some() {
        merged.structured_output = enhanced.structured_output;
    }
    merged.temperature |= enhanced.temperature;
    if enhanced.status != ModelStatus::Active {
        merged.status = enhanced.status;
    }
    merged.open_weights |= enhanced.open_weights;
    if enhanced.cost.is_some() {
        merged.cost = enhanced.cost;
    }
    if enhanced.canonical.is_some() {
        merged.canonical = enhanced.canonical;
    }
    finalize(&mut merged);
    merged
}

/// Fills defaults and derives labels/fingerprints after parsing/merging.
pub(crate) fn finalize(descriptor: &mut ModelDescriptor) {
    if descriptor.context_window > 0 {
        if descriptor.max_output > descriptor.context_window {
            descriptor.max_output = descriptor.context_window;
        }
    } else {
        descriptor.max_output = 0;
    }
    if descriptor.input_modalities.is_empty() {
        descriptor.input_modalities.insert("text".into());
    }
    if descriptor.output_modalities.is_empty() {
        descriptor.output_modalities.insert("text".into());
    }
    descriptor.use_cases = derive_use_cases(descriptor);
    descriptor.recompute_fingerprint();
}

fn derive_use_cases(descriptor: &ModelDescriptor) -> BTreeSet<String> {
    let mut labels = BTreeSet::new();
    if descriptor.reasoning {
        labels.insert("reasoning".into());
    }
    if descriptor.tool_call {
        labels.insert("agentic".into());
    }
    if descriptor.context_window >= 500_000 {
        labels.insert("long-context".into());
    }
    if descriptor.input_modalities.contains("image") {
        labels.insert("vision".into());
    }
    if descriptor.input_modalities.contains("pdf") {
        labels.insert("document".into());
    }
    if descriptor.input_modalities.contains("audio") {
        labels.insert("audio".into());
    }
    if let Some(cost) = descriptor.cost {
        if cost.input == 0.0 && cost.output == 0.0 && cost.cache_read == 0.0 {
            labels.insert("free".into());
        }
    }
    if descriptor.status == ModelStatus::Deprecated {
        labels.insert("deprecated".into());
    } else if descriptor.status == ModelStatus::Beta {
        labels.insert("beta".into());
    } else if descriptor.status == ModelStatus::Alpha {
        labels.insert("alpha".into());
    }
    let text = format!(
        "{} {}",
        descriptor.family.as_deref().unwrap_or_default(),
        descriptor.description.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase();
    if text.contains("coder") || text.contains("coding") || text.contains("agent") {
        labels.insert("coding".into());
    }
    labels
}

fn descriptor_from_value(provider: &str, id: &str, value: &Value) -> ModelDescriptor {
    let context_window = positive_usize(value.pointer("/limit/context")).unwrap_or(0);
    let context_window = if (1_024..=10_000_000).contains(&context_window) {
        context_window
    } else {
        0
    };
    let max_output = positive_usize(value.pointer("/limit/output"))
        .map(|output| {
            if context_window > 0 {
                output.min(context_window)
            } else {
                output
            }
        })
        .unwrap_or(0);

    let name = value
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or(id)
        .to_string();
    let description = string_field(value, "description");
    let family = string_field(value, "family");
    let kind = ModelKind::from_type(value.get("type").and_then(Value::as_str));
    let input_modalities = modalities(value, "input");
    let output_modalities = modalities(value, "output");
    let interleaved_field = value
        .pointer("/interleaved/field")
        .and_then(Value::as_str)
        .or_else(|| value.get("interleaved").and_then(Value::as_str))
        .map(ToString::to_string);
    let canonical = string_field(value, "canonical_model_id");
    let cost = value.get("cost").and_then(parse_cost);
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .map(parse_status)
        .unwrap_or_default();

    let mut descriptor = ModelDescriptor {
        key: format!("{provider}/{id}"),
        provider: provider.to_string(),
        id: id.to_string(),
        name,
        description,
        family,
        kind,
        context_window,
        max_output,
        input_modalities,
        output_modalities,
        attachment: bool_field(value, "attachment", false),
        reasoning: bool_field(value, "reasoning", false),
        reasoning_options: parse_reasoning_options(value.get("reasoning_options")),
        interleaved_field,
        tool_call: bool_field(value, "tool_call", false),
        structured_output: value.get("structured_output").and_then(Value::as_bool),
        temperature: bool_field(value, "temperature", true),
        status,
        open_weights: bool_field(value, "open_weights", false),
        cost,
        canonical,
        use_cases: BTreeSet::new(),
        fingerprint: 0,
    };
    finalize(&mut descriptor);
    descriptor
}

fn split_catalog_key(key: &str, value: &Value) -> (String, String) {
    if let Some((provider, id)) = key.split_once('/') {
        return (provider.to_string(), id.to_string());
    }
    let provider = value
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(key)
        .to_string();
    (provider, id)
}

fn modalities(value: &Value, direction: &str) -> BTreeSet<String> {
    let mut result = BTreeSet::new();
    if let Some(values) = value
        .pointer(&format!("/modalities/{direction}"))
        .and_then(Value::as_array)
    {
        for value in values {
            if let Some(value) = value.as_str() {
                let value = value.trim().to_ascii_lowercase();
                if !value.is_empty() {
                    result.insert(value);
                }
            }
        }
    }
    result
}

fn parse_reasoning_options(value: Option<&Value>) -> Vec<ReasoningOption> {
    let Some(value) = value else {
        return Vec::new();
    };
    let mut options = Vec::new();
    match value {
        Value::Array(values) => {
            for value in values {
                if let Some(option) = reasoning_option(value) {
                    options.push(option);
                }
            }
        }
        Value::Object(_) => {
            if let Some(option) = reasoning_option(value) {
                options.push(option);
            }
        }
        _ => {}
    }
    options
}

fn reasoning_option(value: &Value) -> Option<ReasoningOption> {
    match value.get("type").and_then(Value::as_str)? {
        "toggle" => Some(ReasoningOption::Toggle),
        "effort" => {
            let mut values: Vec<String> = value
                .get("values")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(Value::as_str)
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty())
                .collect();
            values.sort_by(|a, b| {
                effort_rank(a)
                    .cmp(&effort_rank(b))
                    .then_with(|| a.cmp(b))
            });
            values.dedup();
            (!values.is_empty()).then_some(ReasoningOption::Effort { values })
        }
        "budget_tokens" => Some(ReasoningOption::BudgetTokens),
        _ => None,
    }
}

fn effort_rank(value: &str) -> usize {
    const ORDER: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];
    ORDER
        .iter()
        .position(|candidate| *candidate == value)
        .unwrap_or(ORDER.len())
}

fn parse_cost(value: &Value) -> Option<Cost> {
    let object = value.as_object()?;
    let read = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value >= 0.0)
            .unwrap_or(0.0)
    };
    Some(Cost {
        input: read("input"),
        output: read("output"),
        cache_read: read("cache_read"),
    })
}

fn parse_status(value: &str) -> ModelStatus {
    match value.trim().to_ascii_lowercase().as_str() {
        "alpha" => ModelStatus::Alpha,
        "beta" => ModelStatus::Beta,
        "deprecated" => ModelStatus::Deprecated,
        _ => ModelStatus::Active,
    }
}

fn bool_field(value: &Value, key: &str, default: bool) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn positive_usize(value: Option<&Value>) -> Option<usize> {
    value
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_models_and_full_ids() {
        let payload = br#"{
            "opencode-go/deepseek-v4.1-flash": {
                "id": "opencode-go/deepseek-v4.1-flash",
                "name": "DeepSeek V4.1 Flash",
                "reasoning": true,
                "tool_call": true,
                "limit": {"context": 1000000, "output": 384000},
                "modalities": {"input": ["text", "image"], "output": ["text"]}
            }
        }"#;
        let parsed = parse_models_json(payload).unwrap();
        assert_eq!(parsed.entries.len(), 1);
        let entry = &parsed.entries[0];
        assert_eq!(entry.key, "opencode-go/deepseek-v4.1-flash");
        assert_eq!(entry.id, "deepseek-v4.1-flash");
        assert_eq!(entry.context_window, 1_000_000);
        assert!(!entry.use_cases.contains("coding"));
        assert!(entry.input_modalities.contains("image"));
    }

    #[test]
    fn parses_api_reasoning_object_or_array_and_status() {
        let payload = br#"{
            "openrouter": {
                "api": "https://openrouter.ai/api/v1",
                "models": {
                    "vendor/model-a": {
                        "name": "Model A",
                        "reasoning": true,
                        "reasoning_options": [
                            {"type": "effort", "values": ["xhigh", "none", "medium"]},
                            {"type": "budget_tokens"}
                        ],
                        "status": "beta",
                        "limit": {"context": 200000}
                    },
                    "vendor/model-b": {
                        "name": "Model B",
                        "reasoning_options": {"type": "toggle"},
                        "status": "deprecated"
                    }
                }
            }
        }"#;
        let parsed = parse_api_json(payload).unwrap();
        assert_eq!(parsed.provider_api["openrouter"], "https://openrouter.ai/api/v1");
        let a = parsed
            .entries
            .iter()
            .find(|entry| entry.id == "vendor/model-a")
            .unwrap();
        assert_eq!(a.key, "openrouter/vendor/model-a");
        assert_eq!(a.status, ModelStatus::Beta);
        assert!(matches!(
            &a.reasoning_options[0],
            ReasoningOption::Effort { values }
                if values == &["none", "medium", "xhigh"]
        ));
        assert!(matches!(a.reasoning_options[1], ReasoningOption::BudgetTokens));
        let b = parsed
            .entries
            .iter()
            .find(|entry| entry.id == "vendor/model-b")
            .unwrap();
        assert_eq!(b.status, ModelStatus::Deprecated);
        assert!(matches!(b.reasoning_options[0], ReasoningOption::Toggle));
    }

    #[test]
    fn merge_prefers_detail_without_losing_context() {
        let flat = parse_models_json(
            br#"{"p/m": {"name": "Pretty", "limit": {"context": 128000}, "reasoning": true}}"#,
        )
        .unwrap();
        let detailed = parse_api_json(
            br#"{"p": {"api": "https://example.com/v1", "models": {"m": {"name": "Detailed", "limit": {"context": 0}}}}}"#,
        )
        .unwrap();
        let merged = merge_descriptors(
            flat.entries.into_iter().next().unwrap(),
            detailed.entries.into_iter().next().unwrap(),
        );
        assert_eq!(merged.name, "Detailed");
        assert_eq!(merged.context_window, 128_000);
        assert!(merged.reasoning);
        assert!(merged.input_modalities.contains("text"));
    }

    #[test]
    #[ignore = "reads downloaded models.dev payloads from the OS temp directory"]
    fn parses_downloaded_models_dev_payloads() {
        let temp = std::env::temp_dir();
        let models_path = temp.join("models-all.json");
        let api_path = temp.join("models-api.json");
        let models = std::fs::read(&models_path).expect("models-all.json in temp");
        let api = std::fs::read(&api_path).expect("models-api.json in temp");

        let flat = parse_models_json(&models).unwrap();
        assert!(flat.entries.len() > 500, "flat entries: {}", flat.entries.len());
        let detailed = parse_api_json(&api).unwrap();
        assert!(
            detailed.entries.len() > 8_000,
            "api entries: {}",
            detailed.entries.len()
        );
        let flash = detailed
            .entries
            .iter()
            .find(|entry| entry.key == "opencode-go/deepseek-v4.1-flash")
            .expect("opencode-go/deepseek-v4.1-flash");
        assert_eq!(flash.context_window, 1_000_000);
        assert_eq!(flash.max_output, 384_000);
        assert!(flash.reasoning);
        assert!(flash.input_modalities.contains("image"));
        assert!(matches!(
            &flash.reasoning_options[0],
            ReasoningOption::Effort { values }
                if values == &["low", "high", "max"]
        ));
    }
}
