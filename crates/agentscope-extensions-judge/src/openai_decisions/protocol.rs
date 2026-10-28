//! Wire encoding for the Decisions endpoint: request body building and response
//! parsing/validation.
//!
//! The Java client delegates both directions to the official OpenAI SDK
//! (`DecisionCreateParams` / `Decision`), so the JSON shape here is the one that SDK produces.
//! `OpenAIDecisionsClientTest` pins it exactly:
//!
//! ```json
//! {
//!   "model": "gpt-6-luna",
//!   "prompt_cache_key": "tenant-42",
//!   "input": "text" | [{ "role": "user", "content": [
//!     { "type": "input_text", "text": "..." },
//!     { "type": "input_image", "image_url": "..." } ] }],
//!   "questions": [
//!     { "type": "predicate", "name": "...", "instructions": "..." },
//!     { "type": "choice", "name": "...", "instructions": "...",
//!       "choices": [{ "value": "true" }, { "value": true, "description": "..." }] },
//!     { "type": "score", "name": "...", "instructions": "...",
//!       "levels": [{ "label": "minor" }, { "label": "blocked", "description": "..." }] }
//!   ]
//! }
//! ```
//!
//! Answers come back as a `type`-tagged array (`predicate`/`choice`/`score`/`refusal`), one per
//! question, and every value is re-validated against the question before it reaches the caller
//! (`OpenAIDecisionsClient.parseAnswer` and friends). Unknown properties are ignored, matching
//! the SDK.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use super::dto::{
    ChoiceProbability, DecisionAnswer, DecisionLevel, DecisionOption, DecisionQuestion,
    DecisionRequest, DecisionResult, DecisionUsage, DecisionValue, ScoreProbability,
};
use super::error::DecisionsError;
use super::PROBABILITY_TOLERANCE;

/// A response whose JSON could not be parsed (Java "Unable to parse Decisions response").
pub const PARSE_MESSAGE: &str = "Unable to parse Decisions response";
/// The provider must answer every question (`OpenAIDecisionsClient.execute`).
pub const ANSWER_COUNT_MESSAGE: &str = "Decisions response must answer every question";
/// An answer name that is unknown or repeated.
pub const ANSWER_NAME_MESSAGE: &str = "Unknown or duplicate answer name";
/// A `null` element in the answers array.
pub const ANSWER_NULL_MESSAGE: &str = "Answer must not be null";
/// A `null` element in a choice distribution.
pub const CHOICE_PROBABILITY_NULL_MESSAGE: &str = "Choice probability must not be null";
/// A `null` element in a score distribution.
pub const SCORE_PROBABILITY_NULL_MESSAGE: &str = "Score probability must not be null";
/// An answer whose `type` does not match its question.
pub const ANSWER_TYPE_MESSAGE: &str = "Answer type does not match question type";
/// A choice distribution that does not cover every option.
pub const OPTION_COUNT_MESSAGE: &str = "Probability distribution must contain every option";
/// A score distribution that does not cover every level.
pub const LEVEL_COUNT_MESSAGE: &str = "Probability distribution must contain every level";
/// A choice probability for an unknown or repeated option.
pub const UNKNOWN_OPTION_MESSAGE: &str = "Unknown or duplicate choice probability";
/// A selected choice that is not one of the supplied options.
pub const SELECTED_OPTION_MESSAGE: &str = "Selected choice must be one of the supplied options";
/// A score probability with an out-of-range, repeated or mislabelled level.
pub const LEVEL_MISMATCH_MESSAGE: &str = "Unknown, duplicate or mismatched score level";
/// A distribution that does not sum to one.
pub const SUM_MESSAGE: &str = "Probabilities must sum to one";
/// A score that is not the probability-weighted level expectation.
pub const SCORE_MESSAGE: &str = "Score must match the probability-weighted level indices";
/// A probability or confidence outside `[0, 1]`.
pub const UNIT_INTERVAL_MESSAGE: &str = "Probability or confidence outside [0, 1]";
/// A negative (or non-integer) token count.
pub const USAGE_MESSAGE: &str = "Token usage must be a nonnegative integer";

/// Builds the JSON body for `POST {base}/decisions` (`DecisionCreateParams`).
///
/// `model` is the already-resolved model (request override or client default).
#[must_use]
pub fn decision_body(request: &DecisionRequest, model: &str) -> Value {
    let mut body = Map::new();
    body.insert("model".to_string(), Value::String(model.to_string()));
    if let Some(prompt_cache_key) = request.prompt_cache_key() {
        body.insert(
            "prompt_cache_key".to_string(),
            Value::String(prompt_cache_key.to_string()),
        );
    }
    body.insert("input".to_string(), input_value(request));
    body.insert(
        "questions".to_string(),
        Value::Array(
            request
                .questions()
                .iter()
                .map(|(name, question)| question_value(name, question))
                .collect(),
        ),
    );
    Value::Object(body)
}

/// Text-only input is sent as a string; any image switches to a single user message.
fn input_value(request: &DecisionRequest) -> Value {
    let input = request.input();
    if input.image_urls().is_empty() {
        return Value::String(input.text_value().unwrap_or_default().to_string());
    }
    let mut content = Vec::with_capacity(input.image_urls().len() + 1);
    if let Some(text) = input.text_value() {
        content.push(serde_json::json!({ "type": "input_text", "text": text }));
    }
    for image in input.image_urls() {
        content.push(serde_json::json!({ "type": "input_image", "image_url": image }));
    }
    serde_json::json!([{ "role": "user", "content": content }])
}

fn question_value(name: &str, question: &DecisionQuestion) -> Value {
    let mut value = Map::new();
    value.insert("name".to_string(), Value::String(name.to_string()));
    value.insert(
        "instructions".to_string(),
        Value::String(question.instructions().to_string()),
    );
    match question {
        DecisionQuestion::Predicate { .. } => {
            value.insert("type".to_string(), Value::String("predicate".to_string()));
        }
        DecisionQuestion::Choice { choices, .. } => {
            value.insert("type".to_string(), Value::String("choice".to_string()));
            value.insert(
                "choices".to_string(),
                Value::Array(choices.iter().map(option_value).collect()),
            );
        }
        DecisionQuestion::Score { levels, .. } => {
            value.insert("type".to_string(), Value::String("score".to_string()));
            value.insert(
                "levels".to_string(),
                Value::Array(levels.iter().map(level_value).collect()),
            );
        }
    }
    Value::Object(value)
}

fn option_value(option: &DecisionOption) -> Value {
    let mut value = Map::new();
    value.insert("value".to_string(), decision_value_json(&option.value));
    if let Some(description) = &option.description {
        value.insert(
            "description".to_string(),
            Value::String(description.clone()),
        );
    }
    Value::Object(value)
}

fn level_value(level: &DecisionLevel) -> Value {
    let mut value = Map::new();
    value.insert("label".to_string(), Value::String(level.label.clone()));
    if let Some(description) = &level.description {
        value.insert(
            "description".to_string(),
            Value::String(description.clone()),
        );
    }
    Value::Object(value)
}

fn decision_value_json(value: &DecisionValue) -> Value {
    match value {
        DecisionValue::Text(text) => Value::String(text.clone()),
        DecisionValue::Bool(boolean) => Value::Bool(*boolean),
    }
}

/// Parses and validates a 2xx response against the request that produced it.
///
/// Mirrors `OpenAIDecisionsClient.execute`: a missing/blank model, a missing answers array, a
/// wrong answer count, unknown or duplicated names, type mismatches and any out-of-contract
/// value all become [`DecisionsError::Api`] with a sanitized message.
pub fn parse_decision_response(
    request: &DecisionRequest,
    body: &str,
) -> Result<DecisionResult, DecisionsError> {
    let wire: WireResponse =
        serde_json::from_str(body).map_err(|_| DecisionsError::api(PARSE_MESSAGE, None, false))?;
    let model = wire
        .model
        .filter(|model| !model.trim().is_empty())
        .ok_or_else(|| DecisionsError::api(PARSE_MESSAGE, None, false))?;
    let answers = wire
        .answers
        .ok_or_else(|| DecisionsError::api(PARSE_MESSAGE, None, false))?;
    if answers.len() != request.questions().len() {
        return Err(invalid(ANSWER_COUNT_MESSAGE));
    }

    let mut parsed = BTreeMap::new();
    for answer in answers {
        let Some(answer) = answer else {
            return Err(invalid(ANSWER_NULL_MESSAGE));
        };
        let name = answer.name().to_string();
        let Some(question) = request.questions().get(&name) else {
            return Err(invalid(ANSWER_NAME_MESSAGE));
        };
        if parsed.contains_key(&name) {
            return Err(invalid(ANSWER_NAME_MESSAGE));
        }
        parsed.insert(name, parse_answer(question, answer)?);
    }

    let usage = wire.usage.map(validate_usage).transpose()?;
    Ok(DecisionResult {
        model,
        answers: parsed,
        usage,
    })
}

fn invalid(message: impl Into<String>) -> DecisionsError {
    DecisionsError::api(message, None, false)
}

fn parse_answer(
    question: &DecisionQuestion,
    answer: WireAnswer,
) -> Result<DecisionAnswer, DecisionsError> {
    // A refusal is legal for every question type and never parsed further.
    if matches!(answer, WireAnswer::Refusal { .. }) {
        return Ok(DecisionAnswer::Refusal);
    }
    match (question, answer) {
        (DecisionQuestion::Predicate { .. }, WireAnswer::Predicate { probability, .. }) => {
            Ok(DecisionAnswer::Predicate {
                probability: unit_interval(probability)?,
            })
        }
        (
            DecisionQuestion::Choice { choices, .. },
            WireAnswer::Choice {
                choice,
                confidence,
                probabilities,
                ..
            },
        ) => {
            let values: Vec<DecisionValue> =
                choices.iter().map(|option| option.value.clone()).collect();
            parse_choice(choice, confidence, probabilities, &values)
        }
        (
            DecisionQuestion::Score { levels, .. },
            WireAnswer::Score {
                score,
                confidence,
                probabilities,
                ..
            },
        ) => parse_score(score, confidence, probabilities, levels),
        _ => Err(invalid(ANSWER_TYPE_MESSAGE)),
    }
}

fn parse_choice(
    choice: DecisionValue,
    confidence: f64,
    probabilities: Vec<Option<WireChoiceProbability>>,
    expected: &[DecisionValue],
) -> Result<DecisionAnswer, DecisionsError> {
    if probabilities.len() != expected.len() {
        return Err(invalid(OPTION_COUNT_MESSAGE));
    }
    let mut seen: Vec<DecisionValue> = Vec::with_capacity(probabilities.len());
    let mut entries = Vec::with_capacity(probabilities.len());
    let mut sum = 0.0;
    for entry in probabilities {
        let Some(entry) = entry else {
            return Err(invalid(CHOICE_PROBABILITY_NULL_MESSAGE));
        };
        let probability = unit_interval(entry.probability)?;
        if !expected.contains(&entry.value) || seen.contains(&entry.value) {
            return Err(invalid(UNKNOWN_OPTION_MESSAGE));
        }
        sum += probability;
        seen.push(entry.value.clone());
        entries.push(ChoiceProbability {
            value: entry.value,
            probability,
        });
    }
    validate_sum(sum, expected.len())?;
    if !seen.contains(&choice) {
        return Err(invalid(SELECTED_OPTION_MESSAGE));
    }
    Ok(DecisionAnswer::Choice {
        choice,
        confidence: unit_interval(confidence)?,
        probabilities: entries,
    })
}

fn parse_score(
    score: f64,
    confidence: f64,
    probabilities: Vec<Option<WireScoreProbability>>,
    levels: &[DecisionLevel],
) -> Result<DecisionAnswer, DecisionsError> {
    if probabilities.len() != levels.len() {
        return Err(invalid(LEVEL_COUNT_MESSAGE));
    }
    let mut seen: Vec<i64> = Vec::with_capacity(probabilities.len());
    let mut entries = Vec::with_capacity(probabilities.len());
    let mut sum = 0.0;
    let mut expectation = 0.0;
    for entry in probabilities {
        let Some(entry) = entry else {
            return Err(invalid(SCORE_PROBABILITY_NULL_MESSAGE));
        };
        let index = entry.value;
        if index < 0
            || index as usize >= levels.len()
            || seen.contains(&index)
            || levels[index as usize].label != entry.label
        {
            return Err(invalid(LEVEL_MISMATCH_MESSAGE));
        }
        let probability = unit_interval(entry.probability)?;
        sum += probability;
        expectation += index as f64 * probability;
        seen.push(index);
        entries.push(ScoreProbability {
            value: index as usize,
            label: entry.label,
            probability,
        });
    }
    validate_sum(sum, levels.len())?;

    let highest = levels.len().saturating_sub(1) as f64;
    if !score.is_finite()
        || score < 0.0
        || score > highest
        || (score - expectation).abs() > PROBABILITY_TOLERANCE * levels.len() as f64
    {
        return Err(invalid(SCORE_MESSAGE));
    }

    Ok(DecisionAnswer::Score {
        score,
        confidence: unit_interval(confidence)?,
        probabilities: entries,
    })
}

fn validate_sum(sum: f64, size: usize) -> Result<(), DecisionsError> {
    if (sum - 1.0).abs() > PROBABILITY_TOLERANCE * size as f64 {
        return Err(invalid(SUM_MESSAGE));
    }
    Ok(())
}

fn unit_interval(value: f64) -> Result<f64, DecisionsError> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(invalid(UNIT_INTERVAL_MESSAGE));
    }
    Ok(value)
}

/// Maps the wire usage onto [`DecisionUsage`], rejecting negative counts.
fn validate_usage(usage: WireUsage) -> Result<DecisionUsage, DecisionsError> {
    let WireUsage {
        input_tokens,
        input_tokens_details,
        output_tokens,
        output_tokens_details,
        total_tokens,
    } = usage;
    let input_details = input_tokens_details.unwrap_or_default();
    let output_details = output_tokens_details.unwrap_or_default();
    Ok(DecisionUsage {
        input_tokens: count(input_tokens)?,
        output_tokens: count(output_tokens)?,
        total_tokens: count(total_tokens)?,
        cached_tokens: count(input_details.cached_tokens)?,
        cache_write_tokens: count(input_details.cache_write_tokens)?,
        reasoning_tokens: count(output_details.reasoning_tokens)?,
    })
}

fn count(value: Option<i64>) -> Result<Option<i64>, DecisionsError> {
    match value {
        Some(value) if value < 0 => Err(invalid(USAGE_MESSAGE)),
        other => Ok(other),
    }
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    model: Option<String>,
    answers: Option<Vec<Option<WireAnswer>>>,
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireAnswer {
    Predicate {
        name: String,
        probability: f64,
    },
    Choice {
        name: String,
        choice: DecisionValue,
        confidence: f64,
        probabilities: Vec<Option<WireChoiceProbability>>,
    },
    Score {
        name: String,
        score: f64,
        confidence: f64,
        probabilities: Vec<Option<WireScoreProbability>>,
    },
    Refusal {
        name: String,
    },
}

impl WireAnswer {
    fn name(&self) -> &str {
        match self {
            WireAnswer::Predicate { name, .. }
            | WireAnswer::Choice { name, .. }
            | WireAnswer::Score { name, .. }
            | WireAnswer::Refusal { name } => name,
        }
    }
}

#[derive(Debug, Deserialize)]
struct WireChoiceProbability {
    value: DecisionValue,
    probability: f64,
}

#[derive(Debug, Deserialize)]
struct WireScoreProbability {
    value: i64,
    label: String,
    probability: f64,
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    input_tokens: Option<i64>,
    input_tokens_details: Option<WireInputTokensDetails>,
    output_tokens: Option<i64>,
    output_tokens_details: Option<WireOutputTokensDetails>,
    total_tokens: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct WireInputTokensDetails {
    cached_tokens: Option<i64>,
    cache_write_tokens: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct WireOutputTokensDetails {
    reasoning_tokens: Option<i64>,
}

/// `DecisionValue` deserializes from either a JSON string or a JSON boolean, and from nothing
/// else (a number or `null` is a parse error, as in Java's `choiceValue`).
impl<'de> Deserialize<'de> for DecisionValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::String(text) => Ok(Self::Text(text)),
            Value::Bool(boolean) => Ok(Self::Bool(boolean)),
            other => Err(serde::de::Error::custom(format!(
                "choice value must be a string or boolean, got {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUCCESS: &str = r#"{"model":"fixture-model", "answers":[
      {"name":"severity", "type":"score", "score":0.8, "confidence":0.6,
       "probabilities":[{"value":1,"label":"blocked","probability":0.8},
                        {"value":0,"label":"minor","probability":0.2}]},
      {"name":"damaged", "type":"predicate", "probability":0},
      {"name":"route", "type":"choice", "choice":true, "confidence":0.7,
       "probabilities":[{"value":true,"probability":0.9},{"value":"true","probability":0.1}]}
    ], "usage":{
      "input_tokens":12,
      "input_tokens_details":{"cached_tokens":3,"cache_write_tokens":2},
      "output_tokens":0,
      "output_tokens_details":{"reasoning_tokens":1},
      "total_tokens":12
    },"future_field":42}"#;

    fn request() -> DecisionRequest {
        DecisionRequest::builder()
            .input_text("The screen is broken")
            .question(
                "damaged",
                DecisionQuestion::predicate("Is the item damaged?"),
            )
            .question(
                "route",
                DecisionQuestion::choice(
                    "Select",
                    vec![
                        DecisionOption::text("true"),
                        DecisionOption::new(true, Some("Needs repair".to_string())),
                    ],
                )
                .unwrap(),
            )
            .question(
                "severity",
                DecisionQuestion::score(
                    "Rate",
                    vec![
                        DecisionLevel::labeled("minor"),
                        DecisionLevel::labeled("blocked"),
                    ],
                )
                .unwrap(),
            )
            .build()
            .unwrap()
    }

    #[test]
    fn body_sends_text_only_input_as_a_string() {
        let body = decision_body(&request(), "gpt-6-luna");
        assert_eq!(
            body["input"],
            Value::String("The screen is broken".to_string())
        );
        assert_eq!(body["model"], "gpt-6-luna");
        assert!(body.get("prompt_cache_key").is_none());
    }

    #[test]
    fn body_sends_questions_as_a_typed_array() {
        let body = decision_body(&request(), "gpt-6-luna");
        let questions = body["questions"].as_array().unwrap();
        assert_eq!(
            questions.len(),
            3,
            "names are sorted: damaged, route, severity"
        );
        assert_eq!(questions[0]["name"], "damaged");
        assert_eq!(questions[0]["type"], "predicate");
        assert_eq!(questions[0]["instructions"], "Is the item damaged?");

        assert_eq!(questions[1]["type"], "choice");
        assert_eq!(questions[1]["choices"][0]["value"], "true");
        assert!(questions[1]["choices"][0].get("description").is_none());
        assert_eq!(questions[1]["choices"][1]["value"], Value::Bool(true));
        assert_eq!(questions[1]["choices"][1]["description"], "Needs repair");

        assert_eq!(questions[2]["type"], "score");
        assert_eq!(questions[2]["levels"][0]["label"], "minor");
        assert!(questions[2]["levels"][0].get("description").is_none());
    }

    #[test]
    fn body_sends_images_as_one_user_message() {
        let request = DecisionRequest::builder()
            .input(
                super::super::dto::DecisionInput::with_images(
                    "Inspect",
                    vec![
                        "data:image/png;base64,aW1hZ2U=".to_string(),
                        "https://images.example/product.png".to_string(),
                    ],
                )
                .unwrap(),
            )
            .prompt_cache_key("tenant-42")
            .question("a", DecisionQuestion::predicate("x"))
            .build()
            .unwrap();
        let body = decision_body(&request, "override-model");
        assert_eq!(body["model"], "override-model");
        assert_eq!(body["prompt_cache_key"], "tenant-42");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][0]["content"][0]["text"], "Inspect");
        assert_eq!(
            body["input"][0]["content"][1]["image_url"],
            "data:image/png;base64,aW1hZ2U="
        );
        assert_eq!(
            body["input"][0]["content"][2]["image_url"],
            "https://images.example/product.png"
        );
    }

    #[test]
    fn body_sends_image_only_input_without_a_text_part() {
        let request = DecisionRequest::builder()
            .input(
                super::super::dto::DecisionInput::image("data:image/png;base64,aW1hZ2U=").unwrap(),
            )
            .question("a", DecisionQuestion::predicate("x"))
            .build()
            .unwrap();
        let body = decision_body(&request, "gpt-6-luna");
        let content = body["input"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "input_image");
    }

    #[test]
    fn parses_every_answer_type_and_preserves_provider_values() {
        let result = parse_decision_response(&request(), SUCCESS).unwrap();
        assert_eq!(result.model, "fixture-model");
        assert_eq!(result.answers.len(), 3);
        assert_eq!(
            result.answers["damaged"],
            DecisionAnswer::Predicate { probability: 0.0 }
        );
        match &result.answers["route"] {
            DecisionAnswer::Choice {
                choice,
                confidence,
                probabilities,
            } => {
                assert_eq!(choice, &DecisionValue::Bool(true));
                assert_eq!(*confidence, 0.7);
                assert_eq!(probabilities[0].value, DecisionValue::Bool(true));
                assert_eq!(probabilities[1].value, DecisionValue::text("true"));
                assert_eq!(probabilities[1].probability, 0.1);
            }
            other => panic!("expected a choice answer, got {other:?}"),
        }
        match &result.answers["severity"] {
            DecisionAnswer::Score { score, .. } => assert_eq!(*score, 0.8),
            other => panic!("expected a score answer, got {other:?}"),
        }
        assert_eq!(
            result.usage,
            Some(DecisionUsage {
                input_tokens: Some(12),
                output_tokens: Some(0),
                total_tokens: Some(12),
                cached_tokens: Some(3),
                cache_write_tokens: Some(2),
                reasoning_tokens: Some(1),
            })
        );
    }

    #[test]
    fn preserves_refusals_alongside_other_answers() {
        let body = SUCCESS.replace(
            r#"{"name":"damaged", "type":"predicate", "probability":0}"#,
            r#"{"name":"damaged", "type":"refusal"}"#,
        );
        let result = parse_decision_response(&request(), &body).unwrap();
        assert_eq!(result.answers.len(), 3);
        assert!(result.answers["damaged"].is_refusal());
        assert_eq!(
            result
                .answers
                .values()
                .filter(|answer| !answer.is_refusal())
                .count(),
            2
        );
    }

    #[test]
    fn keeps_unknown_usage_counters_absent() {
        let mut body: Value = serde_json::from_str(SUCCESS).unwrap();
        body.as_object_mut().unwrap().remove("usage");
        let result = parse_decision_response(&request(), &body.to_string()).unwrap();
        assert_eq!(result.usage, None);

        body.as_object_mut().unwrap().insert(
            "usage".to_string(),
            serde_json::json!({ "output_tokens": 0 }),
        );
        let usage = parse_decision_response(&request(), &body.to_string())
            .unwrap()
            .usage
            .unwrap();
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.output_tokens, Some(0));
        assert_eq!(usage.cached_tokens, None);
        assert_eq!(usage.cache_write_tokens, None);
        assert_eq!(usage.reasoning_tokens, None);
    }

    #[test]
    fn rejects_malformed_bodies() {
        for body in ["null", "[]", "{}", "{private", "\"text\""] {
            let error = parse_decision_response(&request(), body).unwrap_err();
            assert_eq!(error.message(), PARSE_MESSAGE, "body {body}");
            assert_eq!(error.status_code(), None);
            assert!(!error.is_retryable());
        }
    }

    /// Every invalid response the Java suite enumerates, one mutation at a time.
    fn invalid_bodies() -> Vec<(&'static str, String)> {
        fn mutate(mutate: impl Fn(&mut Value)) -> String {
            let mut body: Value = serde_json::from_str(SUCCESS).unwrap();
            mutate(&mut body);
            body.to_string()
        }
        vec![
            (
                PARSE_MESSAGE,
                mutate(|body| {
                    body.as_object_mut().unwrap().remove("answers");
                }),
            ),
            (
                ANSWER_COUNT_MESSAGE,
                mutate(|body| {
                    body["answers"].as_array_mut().unwrap().remove(0);
                }),
            ),
            (
                ANSWER_COUNT_MESSAGE,
                mutate(|body| {
                    let first = body["answers"][0].clone();
                    body["answers"].as_array_mut().unwrap().push(first);
                }),
            ),
            (
                ANSWER_NAME_MESSAGE,
                mutate(|body| {
                    body["answers"][1]["name"] = Value::String("unknown".to_string());
                }),
            ),
            // `route` has not been parsed yet, so this one fails on the type instead; the
            // duplicate branch needs a name that an earlier answer already claimed.
            (
                ANSWER_TYPE_MESSAGE,
                mutate(|body| {
                    body["answers"][1]["name"] = Value::String("route".to_string());
                }),
            ),
            (
                ANSWER_NAME_MESSAGE,
                mutate(|body| {
                    body["answers"][2]["name"] = Value::String("damaged".to_string());
                }),
            ),
            // A `type` swap without the target variant's fields fails deserialization; Java's
            // SDK tolerates it as far as `parseAnswer` and reports the type mismatch instead.
            // Both are non-retryable API errors that never echo the body.
            (
                PARSE_MESSAGE,
                mutate(|body| {
                    body["answers"][1]["type"] = Value::String("choice".to_string());
                }),
            ),
            // A fully shaped score answer aimed at a choice question: the type mismatch case.
            (
                ANSWER_TYPE_MESSAGE,
                mutate(|body| {
                    body["answers"][0]["name"] = Value::String("route".to_string());
                }),
            ),
            (
                PARSE_MESSAGE,
                mutate(|body| {
                    body["answers"][1]["type"] = Value::String("future-type".to_string());
                }),
            ),
            (
                PARSE_MESSAGE,
                mutate(|body| {
                    body["answers"][1]
                        .as_object_mut()
                        .unwrap()
                        .remove("probability");
                }),
            ),
            (
                PARSE_MESSAGE,
                mutate(|body| {
                    body["answers"][1]["probability"] = Value::Null;
                }),
            ),
            (
                PARSE_MESSAGE,
                mutate(|body| {
                    body["answers"][1]["probability"] = Value::String("0".to_string());
                }),
            ),
            (
                UNIT_INTERVAL_MESSAGE,
                mutate(|body| {
                    body["answers"][1]["probability"] = serde_json::json!(-0.1);
                }),
            ),
            (
                UNIT_INTERVAL_MESSAGE,
                mutate(|body| {
                    body["answers"][1]["probability"] = serde_json::json!(1.1);
                }),
            ),
            (
                PARSE_MESSAGE,
                mutate(|body| {
                    body["answers"][2]
                        .as_object_mut()
                        .unwrap()
                        .remove("confidence");
                }),
            ),
            (
                UNIT_INTERVAL_MESSAGE,
                mutate(|body| {
                    body["answers"][2]["confidence"] = serde_json::json!(2);
                }),
            ),
            (
                SELECTED_OPTION_MESSAGE,
                mutate(|body| {
                    body["answers"][2]["choice"] = Value::String("unknown".to_string());
                }),
            ),
            (
                PARSE_MESSAGE,
                mutate(|body| {
                    body["answers"][2]["choice"] = serde_json::json!(5);
                }),
            ),
            (
                UNKNOWN_OPTION_MESSAGE,
                mutate(|body| {
                    body["answers"][2]["probabilities"][0]["value"] =
                        Value::String("true".to_string());
                }),
            ),
            (
                UNKNOWN_OPTION_MESSAGE,
                mutate(|body| {
                    body["answers"][2]["probabilities"][0]["value"] =
                        Value::String("unknown".to_string());
                }),
            ),
            (
                SUM_MESSAGE,
                mutate(|body| {
                    body["answers"][2]["probabilities"][0]["probability"] = serde_json::json!(0.2);
                }),
            ),
            (
                OPTION_COUNT_MESSAGE,
                mutate(|body| {
                    body["answers"][2]["probabilities"]
                        .as_array_mut()
                        .unwrap()
                        .remove(1);
                }),
            ),
            (
                PARSE_MESSAGE,
                mutate(|body| {
                    body["answers"][0].as_object_mut().unwrap().remove("score");
                }),
            ),
            (
                SCORE_MESSAGE,
                mutate(|body| {
                    body["answers"][0]["score"] = serde_json::json!(0.1);
                }),
            ),
            (
                LEVEL_MISMATCH_MESSAGE,
                mutate(|body| {
                    body["answers"][0]["probabilities"][0]["label"] =
                        Value::String("wrong".to_string());
                }),
            ),
            (
                LEVEL_MISMATCH_MESSAGE,
                mutate(|body| {
                    body["answers"][0]["probabilities"][0]["value"] = serde_json::json!(3);
                }),
            ),
            (
                LEVEL_MISMATCH_MESSAGE,
                mutate(|body| {
                    body["answers"][0]["probabilities"][0]["value"] = serde_json::json!(0);
                }),
            ),
            (
                PARSE_MESSAGE,
                mutate(|body| {
                    body["answers"][0]["probabilities"][0]["value"] = serde_json::json!(0.5);
                }),
            ),
            (
                PARSE_MESSAGE,
                mutate(|body| {
                    body["answers"][0]["probabilities"][0]["probability"] = Value::Null;
                }),
            ),
            (
                PARSE_MESSAGE,
                mutate(|body| {
                    body["usage"] = Value::String("wrong".to_string());
                }),
            ),
            (
                USAGE_MESSAGE,
                mutate(|body| {
                    body["usage"]["input_tokens"] = serde_json::json!(-1);
                }),
            ),
            (
                PARSE_MESSAGE,
                mutate(|body| {
                    body["usage"]["input_tokens"] = serde_json::json!(1.5);
                }),
            ),
            (
                USAGE_MESSAGE,
                mutate(|body| {
                    body["usage"]["input_tokens_details"]["cached_tokens"] = serde_json::json!(-1);
                }),
            ),
            (
                PARSE_MESSAGE,
                mutate(|body| {
                    body["usage"]["input_tokens_details"]["cache_write_tokens"] =
                        serde_json::json!(1.5);
                }),
            ),
            (
                USAGE_MESSAGE,
                mutate(|body| {
                    body["usage"]["output_tokens_details"]["reasoning_tokens"] =
                        serde_json::json!(-1);
                }),
            ),
            (
                PARSE_MESSAGE,
                mutate(|body| {
                    body.as_object_mut().unwrap().remove("model");
                }),
            ),
            (
                ANSWER_NULL_MESSAGE,
                mutate(|body| {
                    body["answers"][1] = Value::Null;
                }),
            ),
            (
                CHOICE_PROBABILITY_NULL_MESSAGE,
                mutate(|body| {
                    body["answers"][2]["probabilities"][0] = Value::Null;
                }),
            ),
            (
                SCORE_PROBABILITY_NULL_MESSAGE,
                mutate(|body| {
                    body["answers"][0]["probabilities"][0] = Value::Null;
                }),
            ),
        ]
    }

    #[test]
    fn rejects_every_out_of_contract_response() {
        let request = request();
        for (expected, body) in invalid_bodies() {
            let error = parse_decision_response(&request, &body).unwrap_err();
            assert_eq!(error.message(), expected, "body {body}");
            assert!(!error.is_retryable());
            assert!(!error.message().contains("private"));
        }
    }

    #[test]
    fn rejects_a_blank_model() {
        let body = SUCCESS.replace("fixture-model", "   ");
        assert_eq!(
            parse_decision_response(&request(), &body)
                .unwrap_err()
                .message(),
            PARSE_MESSAGE
        );
    }

    #[test]
    fn accepts_probability_sums_within_the_rounding_bound() {
        // Tolerance is 1e-6 per level, so a two-level distribution may drift by 2e-6 in total.
        // Note the fixture orders the levels highest-first (value 1, then value 0).
        let mut body: Value = serde_json::from_str(SUCCESS).unwrap();
        body["answers"][0]["probabilities"][0]["probability"] = serde_json::json!(0.800_001);
        body["answers"][0]["probabilities"][1]["probability"] = serde_json::json!(0.200_000_5);
        body["answers"][0]["score"] = serde_json::json!(0.800_001);
        assert!(parse_decision_response(&request(), &body.to_string()).is_ok());

        // A 2.5e-6 drift exceeds 2e-6 and is rejected before the score is even considered.
        body["answers"][0]["probabilities"][0]["probability"] = serde_json::json!(0.800_001_5);
        body["answers"][0]["probabilities"][1]["probability"] = serde_json::json!(0.200_001);
        assert_eq!(
            parse_decision_response(&request(), &body.to_string())
                .unwrap_err()
                .message(),
            SUM_MESSAGE
        );
    }

    #[test]
    fn score_must_equal_the_probability_weighted_expectation() {
        let mut body: Value = serde_json::from_str(SUCCESS).unwrap();
        body["answers"][0]["score"] = serde_json::json!(0.800_001);
        assert!(parse_decision_response(&request(), &body.to_string()).is_ok());

        body["answers"][0]["score"] = serde_json::json!(0.800_01);
        assert_eq!(
            parse_decision_response(&request(), &body.to_string())
                .unwrap_err()
                .message(),
            SCORE_MESSAGE
        );

        // Out of the rubric range is rejected too, even when the distribution is consistent.
        body["answers"][0]["score"] = serde_json::json!(1.5);
        assert_eq!(
            parse_decision_response(&request(), &body.to_string())
                .unwrap_err()
                .message(),
            SCORE_MESSAGE
        );
    }

    #[test]
    fn rejects_a_choice_selection_outside_the_supplied_options() {
        let mut body: Value = serde_json::from_str(SUCCESS).unwrap();
        body["answers"][2]["choice"] = Value::Bool(false);
        assert_eq!(
            parse_decision_response(&request(), &body.to_string())
                .unwrap_err()
                .message(),
            SELECTED_OPTION_MESSAGE
        );
    }
}
