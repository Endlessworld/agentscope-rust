//! Typed System One request/response DTOs.
//!
//! Mirrors `io.agentscope.extensions.judge.jev.{Question, Answer, SystemOneRequest,
//! SystemOneResult, Usage}`. The Java bindings use Jackson `@JsonTypeInfo(property = "type")`
//! plus a global `SNAKE_CASE` naming strategy; here the `type` tag is serde's internal tag, and
//! the only affected field pair is [`Usage`] (`input_tokens` / `output_tokens`) whose Rust names
//! already match.
//!
//! Fields the Java records allow to be `null` stay `Option`, so that the response contract
//! checks in [`crate::jev::validation`] can report the same messages instead of failing during
//! deserialization. Unknown JSON properties are ignored, matching Jackson's
//! `FAIL_ON_UNKNOWN_PROPERTIES = false`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON object map for arbitrary rubric / instruction payloads (Java `Map<String, Object>`).
pub type JsonMap = serde_json::Map<String, Value>;

/// What `true` and `false` mean for a [`NoulQuestion`] (Java `NoulQuestion.NoulCriteria`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NoulCriteria {
    /// Description of the affirmative outcome.
    #[serde(rename = "true", skip_serializing_if = "Option::is_none")]
    pub true_value: Option<Value>,
    /// Description of the negative outcome.
    #[serde(rename = "false", skip_serializing_if = "Option::is_none")]
    pub false_value: Option<Value>,
}

impl NoulCriteria {
    /// Creates criteria with both sides described.
    pub fn new(true_value: impl Into<Value>, false_value: impl Into<Value>) -> Self {
        Self {
            true_value: Some(true_value.into()),
            false_value: Some(false_value.into()),
        }
    }
}

/// A yes/no question whose answer is the probability that the answer is yes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoulQuestion {
    /// Free-form instructions (Java `Object`).
    ///
    /// Java annotates the record with `@JsonInclude(NON_NULL)`, so an absent `instructions` is
    /// omitted rather than serialized as `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Value>,
    /// Optional descriptions of the two outcomes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub criteria: Option<NoulCriteria>,
}

impl NoulQuestion {
    /// Creates a question with optional yes/no criteria.
    pub fn new(instructions: impl Into<Value>, criteria: Option<NoulCriteria>) -> Self {
        Self {
            instructions: Some(instructions.into()),
            criteria,
        }
    }
}

/// A closed-set selection question.
///
/// The criteria map defines the legal options and their rubrics; the model answers with one
/// chosen option plus a probability for every declared option.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChoiceQuestion {
    /// Free-form instructions (Java `Object`).
    pub instructions: Option<Value>,
    /// Legal options keyed by option id (Java `Map<String, Object>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub criteria: Option<JsonMap>,
}

impl ChoiceQuestion {
    /// Creates a choice question from option id -> rubric entries.
    pub fn new(instructions: impl Into<Value>, criteria: JsonMap) -> Self {
        Self {
            instructions: Some(instructions.into()),
            criteria: Some(criteria),
        }
    }

    /// Creates a choice question from `(id, rubric)` pairs, preserving the given order in the
    /// resulting criteria (JSON object ordering is not semantically meaningful, but tests and
    /// request bodies read better when it is deterministic).
    pub fn from_pairs<I, K>(instructions: impl Into<Value>, options: I) -> Self
    where
        I: IntoIterator<Item = (K, Value)>,
        K: Into<String>,
    {
        let mut criteria = JsonMap::new();
        for (id, rubric) in options {
            criteria.insert(id.into(), rubric);
        }
        Self::new(instructions, criteria)
    }
}

/// An ordered rubric question.
///
/// Each criteria entry describes one level; the answer carries a probability distribution over
/// those levels plus a probability-weighted score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreQuestion {
    /// Free-form instructions (Java `Object`).
    pub instructions: Option<Value>,
    /// Ordered levels (Java `List<Object>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub criteria: Option<Vec<Value>>,
}

impl ScoreQuestion {
    /// Creates a score question from ordered level descriptions.
    pub fn new(instructions: impl Into<Value>, criteria: Vec<Value>) -> Self {
        Self {
            instructions: Some(instructions.into()),
            criteria: Some(criteria),
        }
    }

    /// The number of rubric levels, or `None` when criteria are missing.
    pub fn level_count(&self) -> Option<usize> {
        self.criteria.as_ref().map(|criteria| criteria.len())
    }
}

/// A typed question sent to a TypeSafe System One model.
///
/// Questions are intentionally closed-world: the caller declares the legal options or levels and
/// the model returns a probability distribution over them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    /// Yes/no question.
    Noul(NoulQuestion),
    /// Closed-set selection question.
    Choice(ChoiceQuestion),
    /// Ordered rubric question.
    Score(ScoreQuestion),
}

impl Question {
    /// The JSON `type` discriminator used on the wire.
    pub fn type_name(&self) -> &'static str {
        match self {
            Question::Noul(_) => "noul",
            Question::Choice(_) => "choice",
            Question::Score(_) => "score",
        }
    }
}

/// The probability that the corresponding yes/no question is true.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoulAnswer {
    /// Probability in `[0, 1]`.
    pub noul: Option<f64>,
}

/// A selected option and its probability distribution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChoiceAnswer {
    /// The selected option id.
    pub choice: Option<String>,
    /// Probability per declared option.
    pub probabilities: Option<BTreeMap<String, f64>>,
    /// Model confidence in `[0, 1]`.
    pub confidence: Option<f64>,
}

/// A probability-weighted score and its level distribution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreAnswer {
    /// Probability-weighted score, within `[0, levels - 1]`.
    pub score: Option<f64>,
    /// Human readable label per level, keyed by level index.
    pub legend: Option<BTreeMap<String, String>>,
    /// Probability per level, keyed by level index.
    pub probabilities: Option<BTreeMap<String, f64>>,
    /// Model confidence in `[0, 1]`.
    pub confidence: Option<f64>,
}

/// A typed answer returned for a corresponding [`Question`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    /// Yes/no answer.
    Noul(NoulAnswer),
    /// Closed-set selection answer.
    Choice(ChoiceAnswer),
    /// Ordered rubric answer.
    Score(ScoreAnswer),
}

impl Answer {
    /// The JSON `type` discriminator used on the wire.
    pub fn type_name(&self) -> &'static str {
        match self {
            Answer::Noul(_) => "noul",
            Answer::Choice(_) => "choice",
            Answer::Score(_) => "score",
        }
    }
}

/// Token usage reported by the System One API.
///
/// Java declares both counters as primitive `long`, so Jackson turns a *missing* field into
/// `0` instead of failing; `#[serde(default)]` reproduces that. A `null` value still fails to
/// deserialize, exactly like Jackson binding `null` to a primitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens billed for this call.
    #[serde(default)]
    pub input_tokens: i64,
    /// Output tokens billed for this call.
    #[serde(default)]
    pub output_tokens: i64,
}

impl Usage {
    /// Creates a usage record.
    pub fn new(input_tokens: i64, output_tokens: i64) -> Self {
        Self {
            input_tokens,
            output_tokens,
        }
    }

    /// Whether both counters are non-negative (the extra check performed by
    /// `JevClient.validateResponse`).
    pub fn is_non_negative(&self) -> bool {
        self.input_tokens >= 0 && self.output_tokens >= 0
    }
}

/// A System One request.
///
/// The model may be omitted; the client then applies its configured default model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemOneRequest {
    /// Arbitrary state handed to the model (Java `Object`).
    pub state: Option<Value>,
    /// Optional model override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Questions keyed by question id.
    pub questions: BTreeMap<String, Question>,
}

impl SystemOneRequest {
    /// Creates a request.
    pub fn new(
        state: Option<Value>,
        model: Option<String>,
        questions: BTreeMap<String, Question>,
    ) -> Self {
        Self {
            state,
            model,
            questions,
        }
    }

    /// Starts building a request.
    pub fn builder() -> SystemOneRequestBuilder {
        SystemOneRequestBuilder::default()
    }
}

/// Incremental builder for [`SystemOneRequest`], mirroring Java's `SystemOneRequest.Builder`.
#[derive(Debug, Default)]
pub struct SystemOneRequestBuilder {
    state: Option<Value>,
    model: Option<String>,
    questions: BTreeMap<String, Question>,
}

impl SystemOneRequestBuilder {
    /// Sets the request state.
    pub fn state(mut self, state: impl Into<Value>) -> Self {
        self.state = Some(state.into());
        self
    }

    /// Sets the model override.
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Replaces the whole question map.
    pub fn questions(mut self, questions: BTreeMap<String, Question>) -> Self {
        self.questions = questions;
        self
    }

    /// Adds (or replaces) one question.
    pub fn question(mut self, id: impl Into<String>, question: Question) -> Self {
        self.questions.insert(id.into(), question);
        self
    }

    /// Builds the request.
    pub fn build(self) -> SystemOneRequest {
        SystemOneRequest::new(self.state, self.model, self.questions)
    }
}

/// A System One response (validated by [`crate::jev::validation::validate_response`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SystemOneResult {
    /// The model that produced the answers.
    pub model: Option<String>,
    /// Answers keyed by question id.
    pub answers: Option<BTreeMap<String, Answer>>,
    /// Token usage.
    pub usage: Option<Usage>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn option_criteria(options: &[(&str, &str)]) -> JsonMap {
        let mut criteria = JsonMap::new();
        for (id, rubric) in options {
            criteria.insert((*id).to_string(), Value::String((*rubric).to_string()));
        }
        criteria
    }

    fn all_types_request() -> SystemOneRequest {
        SystemOneRequest::builder()
            .state(json!({ "message": "My payouts are failing" }))
            .model("jev-latest")
            .question(
                "urgent",
                Question::Noul(NoulQuestion::new(
                    "Does this message express urgency?",
                    Some(NoulCriteria::new("Time sensitive", "No urgency")),
                )),
            )
            .question(
                "team",
                Question::Choice(ChoiceQuestion::new(
                    "Which team should handle this?",
                    option_criteria(&[("billing", "Payments"), ("technical", "Bugs")]),
                )),
            )
            .question(
                "frustration",
                Question::Score(ScoreQuestion::new(
                    "How frustrated is the customer?",
                    vec![json!("Calm"), json!("Frustrated"), json!("Very angry")],
                )),
            )
            .build()
    }

    #[test]
    fn serializes_and_deserializes_all_question_types() {
        let request = all_types_request();
        let json = serde_json::to_value(&request).unwrap();

        assert_eq!(json["state"]["message"], json!("My payouts are failing"));
        assert_eq!(json["model"], json!("jev-latest"));
        assert_eq!(json["questions"]["urgent"]["type"], json!("noul"));
        assert_eq!(json["questions"]["team"]["type"], json!("choice"));
        assert_eq!(json["questions"]["frustration"]["type"], json!("score"));
        assert_eq!(
            json["questions"]["urgent"]["criteria"]["true"],
            json!("Time sensitive")
        );
        assert_eq!(
            json["questions"]["team"]["criteria"]["billing"],
            json!("Payments")
        );
        assert_eq!(
            json["questions"]["frustration"]["criteria"][2],
            json!("Very angry")
        );

        let copied: SystemOneRequest = serde_json::from_value(json).unwrap();
        assert_eq!(request, copied);
    }

    #[test]
    fn omits_null_noul_criteria_and_ignores_unknown_properties() {
        let question = Question::Noul(NoulQuestion::new("Is it urgent?", None));
        let json = serde_json::to_value(&question).unwrap();
        assert_eq!(
            json,
            json!({ "type": "noul", "instructions": "Is it urgent?" })
        );

        // Jackson is configured with FAIL_ON_UNKNOWN_PROPERTIES=false, so extra fields the API
        // adds later must not break parsing.
        let parsed: Question =
            serde_json::from_value(json!({ "type": "noul", "instructions": "x", "future": 1 }))
                .unwrap();
        assert_eq!(parsed.type_name(), "noul");

        let empty: NoulCriteria = serde_json::from_value(json!({})).unwrap();
        assert_eq!(empty, NoulCriteria::default());
        assert_eq!(
            serde_json::to_value(NoulCriteria::default()).unwrap(),
            json!({})
        );
    }

    #[test]
    fn deserializes_typed_answers_and_snake_case_usage() {
        let json = serde_json::json!({
            "model": "jev-1.13.0",
            "answers": {
                "urgent": { "type": "noul", "noul": 0.91 },
                "team": {
                    "type": "choice",
                    "choice": "technical",
                    "probabilities": { "billing": 0.15, "technical": 0.85 },
                    "confidence": 0.78
                },
                "frustration": {
                    "type": "score",
                    "score": 1.05,
                    "legend": { "0": "Calm", "1": "Frustrated", "2": "Very angry" },
                    "probabilities": { "0": 0.05, "1": 0.85, "2": 0.10 },
                    "confidence": 0.92
                }
            },
            "usage": { "input_tokens": 392, "output_tokens": 65 }
        });

        let result: SystemOneResult = serde_json::from_value(json).unwrap();
        assert_eq!(result.model.as_deref(), Some("jev-1.13.0"));
        let answers = result.answers.as_ref().unwrap();
        assert_eq!(
            answers["urgent"],
            Answer::Noul(NoulAnswer { noul: Some(0.91) })
        );
        match &answers["team"] {
            Answer::Choice(choice) => {
                assert_eq!(choice.choice.as_deref(), Some("technical"));
                assert_eq!(choice.probabilities.as_ref().unwrap()["technical"], 0.85);
                assert_eq!(choice.confidence, Some(0.78));
            }
            other => panic!("expected a choice answer, got {other:?}"),
        }
        match &answers["frustration"] {
            Answer::Score(score) => {
                assert_eq!(score.score, Some(1.05));
                assert_eq!(score.legend.as_ref().unwrap()["1"], "Frustrated");
            }
            other => panic!("expected a score answer, got {other:?}"),
        }
        assert_eq!(result.usage, Some(Usage::new(392, 65)));

        let serialized = serde_json::to_value(result).unwrap();
        assert_eq!(serialized["usage"]["input_tokens"], json!(392));
        assert_eq!(serialized["usage"]["output_tokens"], json!(65));
    }

    #[test]
    fn rejects_unknown_type_discriminator() {
        let error = serde_json::from_value::<Answer>(json!({ "type": "ranking" })).unwrap_err();
        assert!(error.to_string().contains("unknown variant"), "{error}");
    }

    #[test]
    fn builder_replaces_questions_and_omits_absent_model() {
        let request = SystemOneRequest::builder()
            .state("plain state")
            .question("a", Question::Noul(NoulQuestion::new("a?", None)))
            .question("b", Question::Noul(NoulQuestion::new("b?", None)))
            .build();
        assert_eq!(request.questions.len(), 2);
        let json = serde_json::to_value(&request).unwrap();
        assert!(json.get("model").is_none());
        assert_eq!(json["state"], json!("plain state"));

        let replaced = SystemOneRequest::builder()
            .questions(BTreeMap::new())
            .build();
        assert!(replaced.questions.is_empty());
    }

    #[test]
    fn omits_null_noul_instructions_like_jackson_non_null() {
        let question = Question::Noul(NoulQuestion {
            instructions: None,
            criteria: None,
        });
        assert_eq!(
            serde_json::to_value(&question).unwrap(),
            json!({ "type": "noul" }),
            "Java's @JsonInclude(NON_NULL) omits absent instructions"
        );

        // An explicit JSON `null` still maps to `None`, so responses stay parseable.
        let parsed: Question =
            serde_json::from_value(json!({ "type": "noul", "instructions": null })).unwrap();
        assert_eq!(
            parsed,
            Question::Noul(NoulQuestion {
                instructions: None,
                criteria: None
            })
        );
    }

    #[test]
    fn usage_defaults_missing_counters_to_zero_like_a_java_primitive() {
        let usage: Usage = serde_json::from_value(json!({})).unwrap();
        assert_eq!(usage, Usage::new(0, 0));
        let partial: Usage = serde_json::from_value(json!({ "output_tokens": 7 })).unwrap();
        assert_eq!(partial, Usage::new(0, 7));

        // Jackson fails to bind `null` onto a primitive long, and so does the port.
        assert!(serde_json::from_value::<Usage>(json!({ "input_tokens": null })).is_err());
    }
}
