//! Independent candidate classification shared by the explicit application workflows
//! (`JevCandidateSelector`).
//!
//! One System One request classifies up to 64 candidates; the results are merged and bucketed into
//! selected (at or above the accept threshold), rejected (at or below the reject threshold) and
//! uncertain (in between).

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::error::JevError;
use crate::jev::call::{execute_guarded, Caller};
use crate::jev::client::JevClient;
use crate::jev::dto::{Answer, NoulQuestion, Question, SystemOneRequest};
use crate::jev::execution::{Decision, JevExecution, Mode, Options, Status};

/// How many candidates one request classifies (`buffer(64)`).
pub const CANDIDATES_PER_REQUEST: usize = 64;

/// The maximum number of candidates per selection.
pub const MAX_CANDIDATES: usize = 1024;

/// The classifier's verdict over a candidate map.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    /// Candidates at or above the accept threshold, best first.
    pub selected: Vec<String>,
    /// Candidates at or below the reject threshold.
    pub rejected: Vec<String>,
    /// Candidates between the two thresholds.
    pub uncertain: Vec<String>,
    /// The probability per candidate, in input order.
    pub scores: BTreeMap<String, f64>,
}

/// Independent candidate classification shared by the application workflows.
pub struct JevCandidateSelector {
    caller: Caller,
    execution: JevExecution,
    accept: f64,
    reject: f64,
}

impl std::fmt::Debug for JevCandidateSelector {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JevCandidateSelector")
            .field("mode", &self.mode())
            .field("accept", &self.accept)
            .field("reject", &self.reject)
            .finish_non_exhaustive()
    }
}

impl JevCandidateSelector {
    /// Creates a selector (`JevCandidateSelector` constructor); requires `0 <= reject < accept <= 1`.
    pub fn new(
        caller: Caller,
        options: Options,
        reject: f64,
        accept: f64,
    ) -> Result<Self, JevError> {
        if !reject.is_finite()
            || !accept.is_finite()
            || reject < 0.0
            || accept > 1.0
            || reject >= accept
        {
            return Err(JevError::invalid("invalid thresholds"));
        }
        Ok(Self {
            caller,
            execution: JevExecution::new("application-selection", options),
            accept,
            reject,
        })
    }

    /// Creates a selector over a [`JevClient`].
    pub fn from_client(
        client: JevClient,
        options: Options,
        reject: f64,
        accept: f64,
    ) -> Result<Self, JevError> {
        Self::new(
            crate::jev::call::caller_from_client(client),
            options,
            reject,
            accept,
        )
    }

    /// The configured mode.
    pub fn mode(&self) -> Mode {
        self.execution.mode()
    }

    /// The accept threshold.
    pub fn accept(&self) -> f64 {
        self.accept
    }

    /// The reject threshold.
    pub fn reject(&self) -> f64 {
        self.reject
    }

    /// Classifies every candidate.
    ///
    /// The question and each candidate are the only things sent; ids must be non-blank.
    pub async fn select(
        &self,
        state: Value,
        question: &str,
        candidates: BTreeMap<String, Value>,
    ) -> Result<Decision<Selection>, JevError> {
        if question.trim().is_empty() || candidates.len() > MAX_CANDIDATES {
            return Err(JevError::invalid("question required; max 1024 candidates"));
        }
        for id in candidates.keys() {
            if id.trim().is_empty() {
                return Err(JevError::invalid("id required"));
            }
        }
        Ok(execute_guarded(&self.execution, self.evaluate(state, question, &candidates)).await)
    }

    async fn evaluate(
        &self,
        state: Value,
        question: &str,
        candidates: &BTreeMap<String, Value>,
    ) -> Result<Decision<Selection>, JevError> {
        let ids: Vec<&String> = candidates.keys().collect();
        let mut scores: BTreeMap<String, f64> = BTreeMap::new();
        for chunk in ids.chunks(CANDIDATES_PER_REQUEST) {
            let mut builder = SystemOneRequest::builder().state(state.clone());
            for (index, id) in chunk.iter().enumerate() {
                builder = builder.question(
                    format!("item_{index}"),
                    Question::Noul(NoulQuestion::new(
                        json!({
                            "question": question,
                            "candidate": candidates.get(*id).cloned().unwrap_or(Value::Null),
                        }),
                        None,
                    )),
                );
            }
            let request = builder.build();
            let result = (self.caller)(request)
                .await
                .map_err(|failure| JevError::protocol(failure.message().to_string()))?;
            let answers = result
                .answers
                .ok_or_else(|| JevError::invalid("invalid response"))?;
            if answers.len() != chunk.len() {
                return Err(JevError::invalid("invalid response"));
            }
            for (index, id) in chunk.iter().enumerate() {
                let Some(Answer::Noul(answer)) = answers.get(&format!("item_{index}")) else {
                    return Err(JevError::invalid("invalid score"));
                };
                let Some(probability) = answer.noul else {
                    return Err(JevError::invalid("invalid score"));
                };
                if !probability.is_finite() || !(0.0..=1.0).contains(&probability) {
                    return Err(JevError::invalid("invalid score"));
                }
                scores.insert((*id).clone(), probability);
            }
        }

        let mut selected: Vec<(String, f64)> = scores
            .iter()
            .filter(|(_, probability)| **probability >= self.accept)
            .map(|(id, probability)| (id.clone(), *probability))
            .collect();
        selected.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.0.cmp(&right.0))
        });
        let selected: Vec<String> = selected.into_iter().map(|(id, _)| id).collect();
        let mut rejected = Vec::new();
        let mut uncertain = Vec::new();
        for (id, probability) in &scores {
            if *probability <= self.reject {
                rejected.push(id.clone());
            } else if *probability < self.accept {
                uncertain.push(id.clone());
            }
        }
        let mut recommendation = BTreeMap::new();
        recommendation.insert("selected".to_string(), selected.join(","));
        recommendation.insert("uncertain".to_string(), uncertain.join(","));
        Ok(Decision::new(
            Status::Decided,
            Some(Selection {
                selected,
                rejected,
                uncertain,
                scores,
            }),
            "CLASSIFIED",
            recommendation,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::call::testing::ScriptedCaller;
    use crate::jev::dto::{SystemOneResult, Usage};
    use std::sync::Arc;
    use std::time::Duration;

    fn options() -> Options {
        Options::new(Mode::Shadow, Duration::from_secs(2), "v1").unwrap()
    }

    fn response(values: &[f64]) -> SystemOneResult {
        let mut answers = BTreeMap::new();
        for (index, value) in values.iter().enumerate() {
            answers.insert(
                format!("item_{index}"),
                Answer::Noul(crate::jev::dto::NoulAnswer { noul: Some(*value) }),
            );
        }
        SystemOneResult {
            model: Some("jev".to_string()),
            answers: Some(answers),
            usage: Some(Usage::new(1, 1)),
        }
    }

    fn selector(values: Vec<f64>) -> (JevCandidateSelector, Arc<ScriptedCaller>) {
        let scripted = Arc::new(ScriptedCaller::new(vec![Ok(response(&values))]));
        let selector =
            JevCandidateSelector::new(scripted.as_caller(), options(), 0.2, 0.8).unwrap();
        (selector, scripted)
    }

    fn candidates(ids: &[&str]) -> BTreeMap<String, Value> {
        ids.iter()
            .map(|id| ((*id).to_string(), json!({ "label": id })))
            .collect()
    }

    #[tokio::test]
    async fn candidates_are_bucketed_and_sorted_by_probability() {
        let (selector, scripted) = selector(vec![0.9, 0.1, 0.5, 0.85]);
        let decision = selector
            .select(
                json!({ "task": "t" }),
                "is it suitable?",
                candidates(&["a", "b", "c", "d"]),
            )
            .await
            .unwrap();
        assert_eq!(decision.status, Status::Decided);
        assert_eq!(decision.reason, "CLASSIFIED");
        let selection = decision.value.unwrap();
        assert_eq!(selection.selected, vec!["a", "d"]);
        assert_eq!(selection.rejected, vec!["b"]);
        assert_eq!(selection.uncertain, vec!["c"]);
        assert_eq!(selection.scores["d"], 0.85);
        assert_eq!(decision.recommendation["selected"], "a,d");
        assert_eq!(decision.recommendation["uncertain"], "c");

        let requests = scripted.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].questions.len(), 4);
        let Some(Question::Noul(question)) = requests[0].questions.get("item_0") else {
            panic!("noul question");
        };
        let instructions = question.instructions.as_ref().unwrap();
        assert_eq!(instructions["question"], json!("is it suitable?"));
        assert_eq!(instructions["candidate"], json!({ "label": "a" }));
    }

    #[tokio::test]
    async fn large_candidate_sets_are_batched_by_sixty_four() {
        let scripted = Arc::new(ScriptedCaller::new(vec![
            Ok(response(&vec![0.9; 64])),
            Ok(response(&vec![0.1; 10])),
        ]));
        let selector =
            JevCandidateSelector::new(scripted.as_caller(), options(), 0.2, 0.8).unwrap();
        let ids: Vec<String> = (0..74).map(|index| format!("c{index}")).collect();
        let candidates = ids
            .iter()
            .map(|id| (id.clone(), Value::Null))
            .collect::<BTreeMap<_, _>>();
        let decision = selector.select(json!({}), "q", candidates).await.unwrap();
        let selection = decision.value.unwrap();
        assert_eq!(selection.selected.len(), 64);
        assert_eq!(selection.rejected.len(), 10);
        assert_eq!(scripted.requests().len(), 2);
        assert_eq!(scripted.requests()[1].questions.len(), 10);
    }

    #[tokio::test]
    async fn invalid_inputs_and_replies_fail_without_panicking() {
        let (selector, _) = selector(vec![0.9]);
        assert_eq!(
            selector
                .select(json!({}), "  ", candidates(&["a"]))
                .await
                .unwrap_err()
                .message(),
            "question required; max 1024 candidates"
        );
        assert_eq!(
            selector
                .select(
                    json!({}),
                    "q",
                    BTreeMap::from([(" ".to_string(), Value::Null)])
                )
                .await
                .unwrap_err()
                .message(),
            "id required"
        );

        let scripted = Arc::new(ScriptedCaller::new(vec![Ok(response(&[0.9]))]));
        let selector =
            JevCandidateSelector::new(scripted.as_caller(), options(), 0.2, 0.8).unwrap();
        // Two candidates but one answer: the response is rejected.
        let mismatched = selector
            .select(json!({}), "q", candidates(&["a", "b"]))
            .await
            .unwrap();
        assert_eq!(mismatched.status, Status::Error);

        let scripted = Arc::new(ScriptedCaller::new(vec![Err(
            crate::jev::call::CallFailure::backend("down"),
        )]));
        let selector =
            JevCandidateSelector::new(scripted.as_caller(), options(), 0.2, 0.8).unwrap();
        let failed = selector
            .select(json!({}), "q", candidates(&["a"]))
            .await
            .unwrap();
        assert_eq!(failed.status, Status::Error);
        assert_eq!(failed.reason, "CALL_OR_RESPONSE_ERROR");
    }

    #[tokio::test]
    async fn off_mode_skips_without_calling_the_backend() {
        let scripted = Arc::new(ScriptedCaller::new(vec![]));
        let selector =
            JevCandidateSelector::new(scripted.as_caller(), Options::disabled(), 0.2, 0.8).unwrap();
        assert_eq!(selector.mode(), Mode::Off);
        let decision = selector
            .select(json!({}), "q", candidates(&["a"]))
            .await
            .unwrap();
        assert_eq!(decision.status, Status::Skipped);
        assert_eq!(scripted.requests().len(), 0);
    }

    #[test]
    fn thresholds_and_constructors_validate() {
        let caller: Caller = Arc::new(|_request| {
            Box::pin(async { Err(crate::jev::call::CallFailure::backend("x")) })
        });
        for (reject, accept) in [(0.8, 0.2), (-0.1, 0.5), (0.2, 1.5), (0.5, 0.5)] {
            assert_eq!(
                JevCandidateSelector::new(caller.clone(), options(), reject, accept)
                    .unwrap_err()
                    .message(),
                "invalid thresholds"
            );
        }
        let selector = JevCandidateSelector::new(caller, options(), 0.2, 0.8).unwrap();
        assert_eq!(selector.accept(), 0.8);
        assert_eq!(selector.reject(), 0.2);
        assert!(format!("{selector:?}").contains("JevCandidateSelector"));
    }
}
