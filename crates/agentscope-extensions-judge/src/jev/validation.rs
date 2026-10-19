//! Request/response contract validation, mirroring `JevClient.validateRequest`,
//! `validateQuestion`, `validateResult`, `validateAnswer`, `validateProbabilities` and
//! `validateResponse`.
//!
//! Error kinds match the Java split: request problems are argument errors
//! ([`JevError::InvalidArgument`], Java `IllegalArgumentException`) while response problems are
//! protocol errors ([`JevError::Protocol`], Java `JevException`).

use std::collections::BTreeSet;

use serde_json::Value;

use crate::error::JevError;
use crate::jev::dto::{
    Answer, ChoiceAnswer, ChoiceQuestion, NoulAnswer, NoulQuestion, Question, ScoreAnswer,
    ScoreQuestion, SystemOneRequest, SystemOneResult,
};
use crate::jev::probability::sum_within_tolerance;

/// Amount of legal options a [`ChoiceQuestion`] may declare (`JevClient.validateQuestion`).
pub const CHOICE_OPTION_RANGE: std::ops::RangeInclusive<usize> = 2..=255;

/// Amount of legal levels a [`ScoreQuestion`] may declare (`JevClient.validateQuestion`).
pub const SCORE_LEVEL_RANGE: std::ops::RangeInclusive<usize> = 2..=10;

fn is_null(value: &Option<Value>) -> bool {
    match value {
        None => true,
        Some(Value::Null) => true,
        Some(_) => false,
    }
}

/// Validates a request before it is serialized (`JevClient.validateRequest`).
///
/// Also exposed as the entry point for injected offline/online evaluation backends
/// (`JevClient.validateEvaluationRequest`).
pub fn validate_request(request: &SystemOneRequest) -> Result<(), JevError> {
    if is_null(&request.state) {
        return Err(JevError::invalid("state must not be null"));
    }
    if request.questions.is_empty() {
        return Err(JevError::invalid("questions must not be empty"));
    }
    for (id, question) in &request.questions {
        if id.trim().is_empty() {
            return Err(JevError::invalid("question ids must not be blank"));
        }
        validate_question(id, Some(question))?;
    }
    Ok(())
}

/// Validates one question (`JevClient.validateQuestion`).
///
/// `question` is `Option` so that a caller holding only an id can reproduce the
/// `question '<id>' must not be null` message.
pub fn validate_question(id: &str, question: Option<&Question>) -> Result<(), JevError> {
    let Some(question) = question else {
        return Err(JevError::invalid(format!(
            "question '{id}' must not be null"
        )));
    };
    match question {
        Question::Noul(noul) => validate_noul_question(id, noul),
        Question::Choice(choice) => validate_choice_question(id, choice),
        Question::Score(score) => validate_score_question(id, score),
    }
}

fn require_instructions(id: &str, instructions: &Option<Value>) -> Result<(), JevError> {
    if is_null(instructions) {
        return Err(JevError::invalid(format!(
            "question '{id}' instructions must not be null"
        )));
    }
    Ok(())
}

fn validate_noul_question(id: &str, question: &NoulQuestion) -> Result<(), JevError> {
    require_instructions(id, &question.instructions)
}

fn validate_choice_question(id: &str, question: &ChoiceQuestion) -> Result<(), JevError> {
    require_instructions(id, &question.instructions)?;
    let Some(criteria) = &question.criteria else {
        return Err(JevError::invalid(format!(
            "question '{id}' criteria must not be null"
        )));
    };
    if !CHOICE_OPTION_RANGE.contains(&criteria.len()) {
        return Err(JevError::invalid(format!(
            "question '{id}' criteria must contain 2 to 255 options"
        )));
    }
    Ok(())
}

fn validate_score_question(id: &str, question: &ScoreQuestion) -> Result<(), JevError> {
    require_instructions(id, &question.instructions)?;
    let Some(criteria) = &question.criteria else {
        return Err(JevError::invalid(format!(
            "question '{id}' criteria must not be null"
        )));
    };
    if !SCORE_LEVEL_RANGE.contains(&criteria.len()) {
        return Err(JevError::invalid(format!(
            "question '{id}' criteria must contain 2 to 10 levels"
        )));
    }
    Ok(())
}

/// Validates a response against the request that produced it (`JevClient.validateResult`).
///
/// Note the HTTP path calls exactly this function, while injected backends go through
/// [`validate_response`].
pub fn validate_result(
    request: &SystemOneRequest,
    result: &SystemOneResult,
) -> Result<(), JevError> {
    match result.model.as_deref() {
        Some(model) if !model.trim().is_empty() => {}
        _ => {
            return Err(JevError::protocol(
                "System One response model must not be blank",
            ))
        }
    }
    let Some(answers) = &result.answers else {
        return Err(JevError::protocol(
            "System One response answers must not be null",
        ));
    };
    if result.usage.is_none() {
        return Err(JevError::protocol(
            "System One response usage must not be null",
        ));
    }
    let request_ids: BTreeSet<&String> = request.questions.keys().collect();
    let answer_ids: BTreeSet<&String> = answers.keys().collect();
    if request_ids != answer_ids {
        return Err(JevError::protocol(
            "System One response answer keys do not match request keys",
        ));
    }
    for (id, question) in &request.questions {
        validate_answer(id, question, answers.get(id))?;
    }
    Ok(())
}

/// Validates a response on behalf of an injected evaluation backend
/// (`JevClient.validateResponse`), i.e. [`validate_result`] plus the usage sign check.
///
/// The usage check is deliberately *not* part of [`validate_result`]: Java applies it only here,
/// so a negative-count response from the HTTP path would pass it.
pub fn validate_response(
    request: &SystemOneRequest,
    result: &SystemOneResult,
) -> Result<(), JevError> {
    validate_request(request)?;
    validate_result(request, result)?;
    let usage = result
        .usage
        .expect("validate_result rejects a missing usage");
    if !usage.is_non_negative() {
        return Err(JevError::protocol("System One usage must not be negative"));
    }
    Ok(())
}

/// Validates one answer against its question (`JevClient.validateAnswer`).
pub fn validate_answer(
    id: &str,
    question: &Question,
    answer: Option<&Answer>,
) -> Result<(), JevError> {
    let Some(answer) = answer else {
        return Err(JevError::protocol(format!(
            "System One response missing answer for question '{id}'"
        )));
    };
    let matched = matches!(
        (question, answer),
        (Question::Noul(_), Answer::Noul(_))
            | (Question::Choice(_), Answer::Choice(_))
            | (Question::Score(_), Answer::Score(_))
    );
    if !matched {
        return Err(JevError::protocol(format!(
            "System One answer for question '{id}' must have type {}",
            question.type_name()
        )));
    }
    match (question, answer) {
        (Question::Noul(_), Answer::Noul(noul)) => validate_noul_answer(id, noul),
        (Question::Choice(choice), Answer::Choice(answer)) => {
            validate_choice_answer(id, choice, answer)
        }
        (Question::Score(score), Answer::Score(answer)) => validate_score_answer(id, score, answer),
        _ => unreachable!("type mismatch was rejected above"),
    }
}

fn validate_noul_answer(id: &str, answer: &NoulAnswer) -> Result<(), JevError> {
    require_probability(id, answer.noul)
}

fn validate_choice_answer(
    id: &str,
    question: &ChoiceQuestion,
    answer: &ChoiceAnswer,
) -> Result<(), JevError> {
    let criteria = question
        .criteria
        .as_ref()
        .map(|criteria| criteria.keys().map(String::as_str).collect::<BTreeSet<_>>());
    let known = criteria
        .as_ref()
        .is_some_and(|keys| answer.choice.as_deref().is_some_and(|c| keys.contains(c)));
    if !known {
        return Err(JevError::protocol(format!(
            "System One answer for question '{id}' selected an unknown option"
        )));
    }
    let expected: BTreeSet<String> = question
        .criteria
        .as_ref()
        .map(|c| c.keys().cloned().collect())
        .unwrap_or_default();
    validate_probabilities(id, answer.probabilities.as_ref(), &expected)?;
    require_confidence(id, answer.confidence)
}

fn validate_score_answer(
    id: &str,
    question: &ScoreQuestion,
    answer: &ScoreAnswer,
) -> Result<(), JevError> {
    let levels = question.criteria.as_ref().map(Vec::len).unwrap_or(0);
    let upper = levels.saturating_sub(1) as f64;
    let score_ok = answer
        .score
        .is_some_and(|score| score.is_finite() && score >= 0.0 && score <= upper);
    if !score_ok {
        return Err(JevError::protocol(format!(
            "System One answer for question '{id}' score must be finite and within the rubric range"
        )));
    }

    let legend_keys: BTreeSet<&String> = answer
        .legend
        .as_ref()
        .map(|legend| legend.keys().collect())
        .unwrap_or_default();
    let probability_keys: BTreeSet<&String> = answer
        .probabilities
        .as_ref()
        .map(|probabilities| probabilities.keys().collect())
        .unwrap_or_default();
    if answer.legend.is_none() || answer.probabilities.is_none() || legend_keys != probability_keys
    {
        return Err(JevError::protocol(format!(
            "System One answer for question '{id}' legend and probabilities must describe the same levels"
        )));
    }

    let expected = level_keys(levels);
    let actual: BTreeSet<String> = answer
        .probabilities
        .as_ref()
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default();
    if actual != expected {
        return Err(JevError::protocol(format!(
            "System One answer for question '{id}' probabilities must cover every rubric level"
        )));
    }
    validate_probabilities(id, answer.probabilities.as_ref(), &expected)?;
    require_confidence(id, answer.confidence)
}

/// The level keys `"0".."n-1"` a score answer must cover.
pub fn level_keys(levels: usize) -> BTreeSet<String> {
    (0..levels).map(|index| index.to_string()).collect()
}

/// Validates a probability distribution (`JevClient.validateProbabilities`): key set, per-value
/// range, and the rounding-aware sum rule from [`crate::jev::probability`].
pub fn validate_probabilities(
    id: &str,
    probabilities: Option<&std::collections::BTreeMap<String, f64>>,
    expected_keys: &BTreeSet<String>,
) -> Result<(), JevError> {
    let matches = probabilities.is_some_and(|probabilities| {
        probabilities.keys().cloned().collect::<BTreeSet<String>>() == *expected_keys
    });
    if !matches {
        return Err(JevError::protocol(format!(
            "System One answer for question '{id}' probabilities must match the question criteria"
        )));
    }
    let probabilities = probabilities.expect("checked above");
    let mut sum = 0.0;
    for value in probabilities.values() {
        require_probability(id, Some(*value))?;
        sum += *value;
    }
    if !sum_within_tolerance(sum, probabilities.len()) {
        return Err(JevError::protocol(format!(
            "System One answer for question '{id}' probabilities must sum to 1"
        )));
    }
    Ok(())
}

/// Requires a finite probability in `[0, 1]` (`JevClient.requireProbability`).
pub fn require_probability(id: &str, value: Option<f64>) -> Result<(), JevError> {
    match value {
        Some(value) if value.is_finite() && (0.0..=1.0).contains(&value) => Ok(()),
        _ => Err(JevError::protocol(format!(
            "System One answer for question '{id}' contains an invalid probability"
        ))),
    }
}

/// Requires a finite confidence in `[0, 1]` (`JevClient.requireConfidence`).
pub fn require_confidence(id: &str, confidence: Option<f64>) -> Result<(), JevError> {
    match confidence {
        Some(confidence) if confidence.is_finite() && (0.0..=1.0).contains(&confidence) => Ok(()),
        _ => Err(JevError::protocol(format!(
            "System One answer for question '{id}' contains invalid confidence"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::dto::{JsonMap, Usage};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn criteria(options: &[(&str, &str)]) -> JsonMap {
        let mut map = JsonMap::new();
        for (id, rubric) in options {
            map.insert((*id).to_string(), json!(rubric));
        }
        map
    }

    fn choice_request(option_count: usize) -> SystemOneRequest {
        let mut options = Vec::new();
        for index in 0..option_count {
            options.push((format!("option_{index}"), json!(format!("Option {index}"))));
        }
        SystemOneRequest::builder()
            .state("Pick one")
            .question(
                "choice",
                Question::Choice(ChoiceQuestion::from_pairs("Pick one", options)),
            )
            .build()
    }

    fn team_request() -> SystemOneRequest {
        SystemOneRequest::builder()
            .state("My payouts are failing")
            .question(
                "team",
                Question::Choice(ChoiceQuestion::new(
                    "Which team should handle this?",
                    criteria(&[("billing", "Payments"), ("technical", "Bugs")]),
                )),
            )
            .build()
    }

    fn success_body() -> serde_json::Value {
        json!({
            "model": "jev-1.13.0",
            "answers": {
                "team": {
                    "type": "choice",
                    "choice": "technical",
                    "probabilities": { "billing": 0.2, "technical": 0.8 },
                    "confidence": 0.77
                }
            },
            "usage": { "input_tokens": 100, "output_tokens": 20 }
        })
    }

    fn parse(value: serde_json::Value) -> SystemOneResult {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn accepts_a_valid_request_and_response() {
        let request = team_request();
        validate_request(&request).unwrap();
        let result = parse(success_body());
        validate_result(&request, &result).unwrap();
        validate_response(&request, &result).unwrap();
        assert_eq!(result.usage, Some(Usage::new(100, 20)));
    }

    #[test]
    fn rejects_invalid_request_input() {
        let missing_state = validate_request(&SystemOneRequest::builder().build()).unwrap_err();
        assert!(missing_state.is_invalid_argument());
        assert!(missing_state.message().contains("state"), "{missing_state}");

        let no_questions =
            validate_request(&SystemOneRequest::builder().state("x").build()).unwrap_err();
        assert!(
            no_questions.message().contains("questions"),
            "{no_questions}"
        );

        let single_option = SystemOneRequest::builder()
            .state("x")
            .question(
                "one",
                Question::Choice(ChoiceQuestion::new("Pick", criteria(&[("a", "A")]))),
            )
            .build();
        let error = validate_request(&single_option).unwrap_err();
        assert!(error.message().contains("2 to 255"), "{error}");

        let too_many_levels = SystemOneRequest::builder()
            .state("x")
            .question(
                "many",
                Question::Score(ScoreQuestion::new(
                    "Rate",
                    (0..11).map(|i| json!(format!("level {i}"))).collect(),
                )),
            )
            .build();
        let error = validate_request(&too_many_levels).unwrap_err();
        assert!(error.message().contains("2 to 10 levels"), "{error}");

        let null_instructions = SystemOneRequest::builder()
            .state("x")
            .question(
                "n",
                Question::Noul(NoulQuestion {
                    instructions: None,
                    criteria: None,
                }),
            )
            .build();
        let error = validate_request(&null_instructions).unwrap_err();
        assert!(
            error.message().contains("instructions must not be null"),
            "{error}"
        );

        let blank_id = SystemOneRequest::builder()
            .state("x")
            .question("  ", Question::Noul(NoulQuestion::new("q?", None)))
            .build();
        let error = validate_request(&blank_id).unwrap_err();
        assert!(error.message().contains("ids must not be blank"), "{error}");
    }

    #[test]
    fn rejects_null_question_and_reports_its_id() {
        let error = validate_question("team", None).unwrap_err();
        assert_eq!(error.message(), "question 'team' must not be null");
    }

    #[test]
    fn rejects_response_with_unknown_answer_key() {
        let mut body = success_body();
        body["answers"] = json!({
            "other": {
                "type": "choice",
                "choice": "technical",
                "probabilities": { "billing": 0.2, "technical": 0.8 },
                "confidence": 0.77
            }
        });
        let error = validate_result(&team_request(), &parse(body)).unwrap_err();
        assert!(
            error.message().contains("answer keys do not match"),
            "{error}"
        );
        assert!(!error.is_retryable());
    }

    #[test]
    fn rejects_invalid_probability_and_sum() {
        // Java `rejectsInvalidProbability`: both values are in range, so the diagnostic is the
        // sum check (0.8 + 0.8 = 1.6).
        let mut doubled = success_body();
        doubled["answers"]["team"]["probabilities"] = json!({ "billing": 0.8, "technical": 0.8 });
        let error = validate_result(&team_request(), &parse(doubled)).unwrap_err();
        assert!(
            error.message().contains("probabilities must sum to 1"),
            "{error}"
        );

        // An individual value outside [0, 1] is reported as an invalid probability.
        let mut out_of_range = success_body();
        out_of_range["answers"]["team"]["probabilities"] =
            json!({ "billing": -0.2, "technical": 1.2 });
        let error = validate_result(&team_request(), &parse(out_of_range)).unwrap_err();
        assert!(error.message().contains("invalid probability"), "{error}");

        let mut drifted = success_body();
        drifted["answers"]["team"]["probabilities"] = json!({ "billing": 0.2, "technical": 0.6 });
        let error = validate_result(&team_request(), &parse(drifted)).unwrap_err();
        assert!(
            error.message().contains("probabilities must sum to 1"),
            "{error}"
        );

        let mut extra = success_body();
        extra["answers"]["team"]["probabilities"] =
            json!({ "billing": 0.2, "technical": 0.8, "sales": 0.0 });
        let error = validate_result(&team_request(), &parse(extra)).unwrap_err();
        assert!(
            error.message().contains("must match the question criteria"),
            "{error}"
        );
    }

    #[test]
    fn rejects_answer_question_type_mismatch() {
        let mut body = success_body();
        body["answers"] = json!({ "team": { "type": "noul", "noul": 1.0 } });
        let error = validate_result(&team_request(), &parse(body)).unwrap_err();
        assert!(error.message().contains("must have type choice"), "{error}");
    }

    #[test]
    fn rejects_unknown_selected_option() {
        let mut body = success_body();
        body["answers"]["team"]["choice"] = json!("sales");
        let error = validate_result(&team_request(), &parse(body)).unwrap_err();
        assert!(error.message().contains("unknown option"), "{error}");
    }

    #[test]
    fn validates_score_legend_and_levels() {
        let request = SystemOneRequest::builder()
            .state("message")
            .question(
                "score",
                Question::Score(ScoreQuestion::new(
                    "How urgent is this?",
                    vec![json!("Low"), json!("High")],
                )),
            )
            .build();

        let incomplete = json!({
            "model": "jev-1.13.0",
            "answers": {
                "score": {
                    "type": "score",
                    "score": 1.0,
                    "legend": { "0": "Low" },
                    "probabilities": { "0": 1.0 },
                    "confidence": 1.0
                }
            },
            "usage": { "input_tokens": 100, "output_tokens": 20 }
        });
        let error = validate_result(&request, &parse(incomplete)).unwrap_err();
        assert!(
            error
                .message()
                .contains("probabilities must cover every rubric level"),
            "{error}"
        );

        let mismatched_legend = json!({
            "model": "jev-1.13.0",
            "answers": {
                "score": {
                    "type": "score",
                    "score": 1.0,
                    "legend": { "0": "Low", "1": "High", "2": "Extra" },
                    "probabilities": { "0": 0.1, "1": 0.9 },
                    "confidence": 0.9
                }
            },
            "usage": { "input_tokens": 100, "output_tokens": 20 }
        });
        let error = validate_result(&request, &parse(mismatched_legend)).unwrap_err();
        assert!(error.message().contains("same levels"), "{error}");

        let out_of_range = json!({
            "model": "jev-1.13.0",
            "answers": {
                "score": {
                    "type": "score",
                    "score": 1.5,
                    "legend": { "0": "Low", "1": "High" },
                    "probabilities": { "0": 0.1, "1": 0.9 },
                    "confidence": 0.9
                }
            },
            "usage": { "input_tokens": 100, "output_tokens": 20 }
        });
        let error = validate_result(&request, &parse(out_of_range)).unwrap_err();
        assert!(
            error.message().contains("within the rubric range"),
            "{error}"
        );
    }

    #[test]
    fn rejects_missing_model_answers_and_usage() {
        let request = team_request();

        let mut blank_model = success_body();
        blank_model["model"] = json!("");
        let error = validate_result(&request, &parse(blank_model)).unwrap_err();
        assert!(
            error.message().contains("model must not be blank"),
            "{error}"
        );

        let missing_answers: SystemOneResult = serde_json::from_value(json!({
            "model": "jev-1.13.0",
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        }))
        .unwrap();
        let error = validate_result(&request, &missing_answers).unwrap_err();
        assert!(
            error.message().contains("answers must not be null"),
            "{error}"
        );

        let missing_usage: SystemOneResult = serde_json::from_value(json!({
            "model": "jev-1.13.0",
            "answers": {
                "team": {
                    "type": "choice",
                    "choice": "technical",
                    "probabilities": { "billing": 0.2, "technical": 0.8 },
                    "confidence": 0.77
                }
            }
        }))
        .unwrap();
        let error = validate_result(&request, &missing_usage).unwrap_err();
        assert!(
            error.message().contains("usage must not be null"),
            "{error}"
        );
    }

    #[test]
    fn reports_missing_answer_for_requested_question() {
        let request = SystemOneRequest::builder()
            .state("x")
            .question("a", Question::Noul(NoulQuestion::new("a?", None)))
            .question("b", Question::Noul(NoulQuestion::new("b?", None)))
            .build();
        // Only "a" is answered: the key sets differ, so the key-set check fires first.
        let partial = json!({
            "model": "jev-1.13.0",
            "answers": { "a": { "type": "noul", "noul": 0.5 } },
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        });
        let result: SystemOneResult = serde_json::from_value(partial).unwrap();
        let error = validate_result(&request, &result).unwrap_err();
        assert!(
            error.message().contains("answer keys do not match"),
            "{error}"
        );

        // A question with no answer at all reports its id (the same path, exercised directly).
        let error =
            validate_answer("b", &Question::Noul(NoulQuestion::new("b?", None)), None).unwrap_err();
        assert_eq!(
            error.message(),
            "System One response missing answer for question 'b'"
        );

        // A JSON `null` answer cannot even be deserialized into the tagged enum, so the
        // diagnostic is a parse error rather than a validation error — matching the fact that
        // the API never sends one.
        let null_answer = serde_json::from_value::<SystemOneResult>(json!({
            "model": "jev-1.13.0",
            "answers": { "b": null },
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        }));
        assert!(null_answer.is_err());
    }

    #[test]
    fn accepts_probabilities_rounded_to_api_precision() {
        // Java `acceptsProbabilitiesRoundedToApiPrecision`: 34 options summing to 1.0001.
        let captured: [f64; 34] = [
            0.0064, 0.0165, 0.0061, 0.0232, 0.0411, 0.007, 0.0357, 0.0227, 0.022, 0.0074, 0.0187,
            0.0224, 0.0051, 0.0081, 0.0105, 0.0402, 0.0076, 0.0081, 0.02, 0.256, 0.0152, 0.1152,
            0.0562, 0.0145, 0.0038, 0.0036, 0.0232, 0.0427, 0.0079, 0.0116, 0.01, 0.0072, 0.0223,
            0.0819,
        ];
        let mut probabilities = BTreeMap::new();
        for (index, value) in captured.iter().enumerate() {
            probabilities.insert(format!("option_{index}"), *value);
        }
        let request = choice_request(34);
        let body = json!({
            "model": "jev-1.13.0",
            "answers": {
                "choice": {
                    "type": "choice",
                    "choice": "option_19",
                    "probabilities": probabilities,
                    "confidence": 0.9
                }
            },
            "usage": { "input_tokens": 100, "output_tokens": 20 }
        });

        validate_result(&request, &parse(body)).unwrap();
    }

    #[test]
    fn rejects_probability_sum_drift_beyond_rounding_error() {
        // Java `rejectsProbabilitySumDriftBeyondRoundingError`: sum 1.0101 > 34 * 5e-5.
        let mut probabilities = BTreeMap::new();
        for index in 0..34 {
            probabilities.insert(format!("option_{index}"), 1.0 / 34.0);
        }
        probabilities.insert("option_0".to_string(), 1.0 / 34.0 + 0.01);
        let request = choice_request(34);
        let body = json!({
            "model": "jev-1.13.0",
            "answers": {
                "choice": {
                    "type": "choice",
                    "choice": "option_0",
                    "probabilities": probabilities,
                    "confidence": 0.9
                }
            },
            "usage": { "input_tokens": 100, "output_tokens": 20 }
        });

        let error = validate_result(&request, &parse(body)).unwrap_err();
        assert!(
            error.message().contains("probabilities must sum to 1"),
            "{error}"
        );
    }

    #[test]
    fn scales_probability_sum_tolerance_with_option_count() {
        // Java `scalesProbabilitySumToleranceWithOptionCount`: 255 options, one inflated by 1e-5.
        let mut probabilities = BTreeMap::new();
        for index in 0..255 {
            let mut probability = 1.0 / 255.0;
            if index == 0 {
                probability += 0.00001;
            }
            probabilities.insert(format!("option_{index}"), probability);
        }
        let request = choice_request(255);
        let body = json!({
            "model": "jev-1.13.0",
            "answers": {
                "choice": {
                    "type": "choice",
                    "choice": "option_0",
                    "probabilities": probabilities,
                    "confidence": 0.9
                }
            },
            "usage": { "input_tokens": 100, "output_tokens": 20 }
        });

        validate_result(&request, &parse(body)).unwrap();
    }

    #[test]
    fn validate_response_requires_non_negative_usage() {
        let request = team_request();
        let mut body = success_body();
        body["usage"] = json!({ "input_tokens": 100, "output_tokens": -1 });
        let result = parse(body);
        // The HTTP path (`validate_result`) tolerates it; injected backends do not.
        validate_result(&request, &result).unwrap();
        let error = validate_response(&request, &result).unwrap_err();
        assert_eq!(error.message(), "System One usage must not be negative");
    }

    #[test]
    fn validate_response_also_checks_the_request() {
        let request = SystemOneRequest::builder().build();
        let error = validate_response(&request, &parse(success_body())).unwrap_err();
        assert!(error.is_invalid_argument());
        assert!(error.message().contains("state"), "{error}");
    }

    #[test]
    fn level_keys_are_zero_based_and_ordered() {
        let keys = level_keys(3);
        assert_eq!(keys.len(), 3);
        assert!(keys.contains("0") && keys.contains("1") && keys.contains("2"));
        assert!(level_keys(0).is_empty());
    }

    #[test]
    fn confidence_checks_reject_out_of_range_values() {
        for confidence in [None, Some(f64::NAN), Some(-0.1), Some(1.1)] {
            let error = require_confidence("q", confidence).unwrap_err();
            assert!(error.message().contains("invalid confidence"), "{error}");
        }
        require_confidence("q", Some(0.0)).unwrap();
        require_confidence("q", Some(1.0)).unwrap();

        for probability in [None, Some(f64::INFINITY), Some(-1.0), Some(2.0)] {
            let error = require_probability("q", probability).unwrap_err();
            assert!(error.message().contains("invalid probability"), "{error}");
        }
        require_probability("q", Some(0.5)).unwrap();
    }
}
