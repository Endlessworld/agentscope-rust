//! Vendor-specific request sanitising for OpenAI-compatible endpoints.
//!
//! Mirrors Java's `openai/compat/{deepseek,glm,kimi,minimax}/*Formatter`: those
//! vendors speak the Chat Completions protocol but reject or ignore a handful of
//! fields, so the request body is adjusted after the generic mapping (and after
//! `additional_body_params`, matching Java's "extra params win" ordering).
//!
//! Only the rules that the generic capability table
//! ([`crate::common::capabilities`]) cannot express live here: the token-field
//! choice stays in `capabilities`, this module handles *rejection* semantics
//! (`strict`, penalties, forced tool choice), value ranges, message shape and the
//! vendor-only fields.

use serde_json::{json, Value};

/// The vendor family whose request rules apply to a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    DeepSeek,
    Glm,
    Kimi,
    MiniMax,
}

/// Resolves the vendor family from the model name (longest-prefix style).
pub fn vendor_for_model(model: &str) -> Option<Vendor> {
    let model = model.trim().to_ascii_lowercase();
    if model.starts_with("deepseek") {
        return Some(Vendor::DeepSeek);
    }
    if model.starts_with("glm") || model.starts_with("chatglm") || model.starts_with("zhipu") {
        return Some(Vendor::Glm);
    }
    if model.starts_with("kimi") || model.starts_with("moonshot") {
        return Some(Vendor::Kimi);
    }
    if model.starts_with("minimax") || model.starts_with("abab") {
        return Some(Vendor::MiniMax);
    }
    None
}

/// Applies the vendor rules for `model` to a serialized request body.
///
/// A model without a vendor family is left untouched.
pub fn apply_vendor_rules(model: &str, body: &mut Value) {
    let Some(vendor) = vendor_for_model(model) else {
        return;
    };
    // None of the four vendors documents OpenAI's `strict` tool flag; sending it
    // is at best ignored and at worst rejected (Java `supportsStrict() == false`).
    strip_strict_tools(body);
    match vendor {
        Vendor::DeepSeek => backfill_reasoning_content(body),
        Vendor::Glm => {
            strip_penalties(body);
            clamp_temperature(body, 0.0, 1.0);
            clamp_top_p(body, 0.01, 1.0);
            degrade_tool_choice(body, /* required_is_supported */ false);
            strip_message_names(body);
            ensure_user_message(body);
        }
        Vendor::Kimi => {
            let model = model.to_ascii_lowercase();
            if model.starts_with("kimi-") {
                strip_sampling_params(body);
            }
            if !model.starts_with("kimi-k3") {
                remove_key(body, "reasoning_effort");
            }
            degrade_tool_choice(
                body,
                /* required_is_supported */ !model.starts_with("kimi-k2"),
            );
        }
        Vendor::MiniMax => {
            minimax_tool_choice(body);
            if let Some(map) = body.as_object_mut() {
                // Split thinking into OpenAI-compatible reasoning fields so the
                // shared parser produces a ThinkingBlock (Java `applyReasoningSplit`).
                map.entry("reasoning_split".to_string())
                    .or_insert_with(|| json!(true));
            }
        }
    }
}

fn remove_key(body: &mut Value, key: &str) {
    if let Some(map) = body.as_object_mut() {
        map.remove(key);
    }
}

fn strip_strict_tools(body: &mut Value) {
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if let Some(map) = tool.as_object_mut() {
                map.remove("strict");
                if let Some(function) = map.get_mut("function").and_then(Value::as_object_mut) {
                    function.remove("strict");
                }
            }
        }
    }
}

fn strip_penalties(body: &mut Value) {
    remove_key(body, "frequency_penalty");
    remove_key(body, "presence_penalty");
}

fn strip_sampling_params(body: &mut Value) {
    for key in [
        "temperature",
        "top_p",
        "n",
        "frequency_penalty",
        "presence_penalty",
    ] {
        remove_key(body, key);
    }
}

fn clamp_temperature(body: &mut Value, min: f64, max: f64) {
    if let Some(value) = body.get("temperature").and_then(Value::as_f64) {
        let clamped = value.clamp(min, max);
        if clamped != value {
            body["temperature"] = json!(clamped);
        }
    }
}

fn clamp_top_p(body: &mut Value, min: f64, max: f64) {
    if let Some(value) = body.get("top_p").and_then(Value::as_f64) {
        let clamped = value.clamp(min, max);
        if clamped != value {
            body["top_p"] = json!(clamped);
        }
    }
}

/// Degrades a forced tool choice to `auto`, which is all these vendors accept.
///
/// `required_is_supported == false` degrades `required` as well; an explicit
/// function choice is always degraded. `none` stays untouched.
fn degrade_tool_choice(body: &mut Value, required_is_supported: bool) {
    let Some(choice) = body.get("tool_choice").cloned() else {
        return;
    };
    let degraded = match &choice {
        Value::String(name) if name == "required" => {
            if required_is_supported {
                return;
            }
            Some(json!("auto"))
        }
        Value::String(name) if name == "none" || name == "auto" => None,
        // A `{"type":"function","function":{"name":..}}` object.
        Value::Object(_) => Some(json!("auto")),
        _ => None,
    };
    if let Some(value) = degraded {
        body["tool_choice"] = value;
    }
}

/// MiniMax does not accept `tool_choice` at all; `none` is enforced by dropping
/// the definitions so the API default (`auto`) cannot call a tool.
fn minimax_tool_choice(body: &mut Value) {
    let is_none = body.get("tool_choice").and_then(Value::as_str) == Some("none");
    remove_key(body, "tool_choice");
    if is_none {
        remove_key(body, "tools");
    }
}

fn strip_message_names(body: &mut Value) {
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            if let Some(map) = message.as_object_mut() {
                map.remove("name");
            }
        }
    }
}

/// GLM requires at least one `user` message (error 1214 otherwise).
fn ensure_user_message(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let has_user = messages
        .iter()
        .any(|message| message.get("role").and_then(Value::as_str) == Some("user"));
    if !has_user {
        messages.push(json!({"role": "user", "content": ""}));
    }
}

/// DeepSeek's thinking mode requires `reasoning_content` on **every** assistant
/// message when tools are present; a missing field is a hard HTTP 400.
fn backfill_reasoning_content(body: &mut Value) {
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            let is_assistant = message.get("role").and_then(Value::as_str) == Some("assistant");
            if is_assistant && message.get("reasoning_content").is_none() {
                if let Some(map) = message.as_object_mut() {
                    map.insert("reasoning_content".to_string(), json!(""));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire_body() -> Value {
        json!({
            "model": "x",
            "messages": [{"role": "assistant", "content": "hi", "name": "bot"}],
            "tools": [{"type": "function", "function": {"name": "f", "strict": true}}],
            "tool_choice": "required",
            "temperature": 1.7,
            "top_p": 0.0,
            "frequency_penalty": 0.5,
            "presence_penalty": 0.5,
            "reasoning_effort": "high"
        })
    }

    #[test]
    fn every_vendor_drops_the_strict_flag() {
        for model in ["deepseek-chat", "glm-5.2", "kimi-k2.6", "minimax-m2"] {
            let mut body = wire_body();
            apply_vendor_rules(model, &mut body);
            assert!(
                body["tools"][0]["function"].get("strict").is_none(),
                "{model} kept strict"
            );
        }
    }

    #[test]
    fn unknown_models_are_left_untouched() {
        let mut body = wire_body();
        let expected = body.clone();
        apply_vendor_rules("gpt-4.1", &mut body);
        assert_eq!(body, expected);
    }

    #[test]
    fn glm_strips_penalties_clamps_ranges_and_degrades_forced_choice() {
        let mut body = wire_body();
        apply_vendor_rules("glm-4.7", &mut body);
        assert!(body.get("frequency_penalty").is_none());
        assert!(body.get("presence_penalty").is_none());
        assert_eq!(body["temperature"], json!(1.0));
        assert_eq!(body["top_p"], json!(0.01));
        assert_eq!(body["tool_choice"], json!("auto"));
        assert!(body["messages"][0].get("name").is_none());
        // A placeholder user message satisfies GLM's requirement.
        assert_eq!(body["messages"][1]["role"], json!("user"));
    }

    #[test]
    fn glm_keeps_none_and_does_not_add_a_second_user_message() {
        let mut body = wire_body();
        body["tool_choice"] = json!("none");
        body["messages"] = json!([{"role": "user", "content": "hi"}]);
        apply_vendor_rules("glm-5", &mut body);
        assert_eq!(body["tool_choice"], json!("none"));
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn kimi_strips_fixed_sampling_params_and_model_specific_reasoning_effort() {
        let mut body = wire_body();
        apply_vendor_rules("kimi-k2.6", &mut body);
        for key in [
            "temperature",
            "top_p",
            "frequency_penalty",
            "presence_penalty",
        ] {
            assert!(body.get(key).is_none(), "kimi kept {key}");
        }
        // `reasoning_effort` is only documented for kimi-k3.
        assert!(body.get("reasoning_effort").is_none());
        // K2.x degrades `required` to `auto`.
        assert_eq!(body["tool_choice"], json!("auto"));

        let mut body = wire_body();
        apply_vendor_rules("kimi-k3", &mut body);
        assert_eq!(body["reasoning_effort"], json!("high"));
        assert_eq!(body["tool_choice"], json!("required"));
    }

    #[test]
    fn minimax_drops_tool_choice_and_enables_reasoning_split() {
        let mut body = wire_body();
        apply_vendor_rules("minimax-m2", &mut body);
        assert!(body.get("tool_choice").is_none());
        assert_eq!(body["reasoning_split"], json!(true));
        // Tools stay available; only `none` removes them.
        assert!(body.get("tools").is_some());

        let mut body = wire_body();
        body["tool_choice"] = json!("none");
        apply_vendor_rules("MiniMax-M1", &mut body);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn deepseek_backfills_missing_reasoning_content() {
        let mut body = wire_body();
        body["messages"] = json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "a"},
            {"role": "assistant", "content": "b", "reasoning_content": "why"}
        ]);
        apply_vendor_rules("deepseek-v4-pro", &mut body);
        assert!(body["messages"][0].get("reasoning_content").is_none());
        assert_eq!(body["messages"][1]["reasoning_content"], json!(""));
        assert_eq!(body["messages"][2]["reasoning_content"], json!("why"));
    }

    #[test]
    fn vendor_resolution_uses_model_prefixes() {
        assert_eq!(vendor_for_model("DeepSeek-V4"), Some(Vendor::DeepSeek));
        assert_eq!(vendor_for_model("chatglm-4"), Some(Vendor::Glm));
        assert_eq!(vendor_for_model("moonshot-v1-8k"), Some(Vendor::Kimi));
        assert_eq!(vendor_for_model("abab6.5"), Some(Vendor::MiniMax));
        assert_eq!(vendor_for_model("qwen-max"), None);
    }
}
