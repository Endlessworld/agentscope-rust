//! Staged, evidence-bound code review (`review/JevCodeReviewer`, `review/JevCodeReviewTool`).
//!
//! Java's reviewer runs a staged pipeline per file (screen → profile → locate → classify →
//! severity → route). Each stage is a System One request and the driver needs the per-stage budget
//! plumbing plus the core `RuntimeContext`/`@Tool` seams.
//!
//! This module ports the **definition layer**, which is what makes the stages reproducible:
//! [`ReviewConfig`] and its validation, the dimension/rubric constants, the screening questions,
//! the category/mechanism/owner vocabularies, the report shapes, the signal ranking and the
//! deterministic [`compute_complete`] rule. The staged driver itself fails closed with
//! [`CODE_REVIEWER_UNAVAILABLE`] (see `docs/extensions/JUDGE__GAP_TRACKER.md` §4.2).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::error::JevError;
use crate::jev::dto::{ChoiceQuestion, NoulQuestion, Question, ScoreQuestion};
use crate::jev::review::{Region, ReviewMode};

/// Why the staged reviewer cannot run from this crate yet.
pub const CODE_REVIEWER_UNAVAILABLE: &str = "JevCodeReviewer's staged screen/profile/locate/classify driver needs the core RuntimeContext seam and the per-stage budget plumbing; only the definition layer is ported";

/// The five review dimensions (`JevCodeReviewer.DIMENSIONS`).
pub const DIMENSIONS: [&str; 5] = [
    "correctness",
    "security",
    "reliability",
    "compatibility",
    "testGap",
];

/// The review-priority rubric (`JevCodeReviewer.PRIORITY`).
pub const PRIORITY: [&str; 4] = [
    "Routine review is sufficient",
    "Focused review is useful",
    "Careful review before merge",
    "Specialist review is needed",
];

/// The severity rubric (`JevCodeReviewer.SEVERITY`).
pub const SEVERITY: [&str; 4] = [
    "No supported issue or meaningful impact",
    "Minor or narrowly limited impact",
    "Significant correctness, reliability, compatibility or security impact",
    "Critical security, data loss or widespread outage impact",
];

/// The bounded review policy (`JevCodeReviewer.Config`).
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewConfig {
    /// At or below this probability a screening concern is low risk.
    pub low_risk_threshold: f64,
    /// At or above this probability a concern becomes a signal.
    pub screen_threshold: f64,
    /// The confidence a stage answer needs to be used.
    pub min_confidence: f64,
    /// At or above this severity a finding is routed to an owner.
    pub route_severity: f64,
    /// At or above this severity a finding recommends requesting changes.
    pub blocking_severity: f64,
    /// Maximum profiled files.
    pub max_profiles: usize,
    /// Maximum follow-up findings.
    pub max_follow_ups: usize,
    /// Maximum System One requests.
    pub max_requests: usize,
    /// Maximum reviewed files.
    pub max_files: usize,
    /// Maximum file characters.
    pub max_file_chars: usize,
    /// Maximum serialized state characters.
    pub max_state_chars: usize,
    /// Maximum patch/source regions per file (1..=254).
    pub max_regions: usize,
}

impl ReviewConfig {
    /// The Java reference policy; the thresholds are demonstrative, not calibrated.
    pub fn reference_policy() -> Self {
        Self {
            low_risk_threshold: 0.2,
            screen_threshold: 0.7,
            min_confidence: 0.55,
            route_severity: 1.5,
            blocking_severity: 2.0,
            max_profiles: 5,
            max_follow_ups: 8,
            max_requests: 64,
            max_files: 50,
            max_file_chars: 100_000,
            max_state_chars: 100_000,
            max_regions: 128,
        }
    }

    /// Validates the policy (`invalid bounded review policy`).
    pub fn validate(&self) -> Result<(), JevError> {
        let invalid = !self.low_risk_threshold.is_finite()
            || self.low_risk_threshold < 0.0
            || !self.screen_threshold.is_finite()
            || self.screen_threshold <= self.low_risk_threshold
            || self.screen_threshold > 1.0
            || !self.min_confidence.is_finite()
            || !(0.0..=1.0).contains(&self.min_confidence)
            || !self.route_severity.is_finite()
            || !(0.0..=3.0).contains(&self.route_severity)
            || !self.blocking_severity.is_finite()
            || self.blocking_severity < self.route_severity
            || self.blocking_severity > 3.0
            || self.max_requests < 1
            || self.max_files < 1
            || self.max_file_chars < 1
            || self.max_state_chars < 1
            || !(1..=254).contains(&self.max_regions);
        if invalid {
            return Err(JevError::invalid("invalid bounded review policy"));
        }
        Ok(())
    }
}

/// One file's screening row (`JevCodeReviewer.Screening`).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Screening {
    /// The file path.
    pub path: String,
    /// `DECIDED`, `SKIPPED` or `ERROR`.
    pub status: String,
    /// Why it has that status.
    pub reason: String,
    /// The per-dimension probabilities.
    pub probabilities: BTreeMap<String, f64>,
    /// Whether the related-test context was clipped.
    pub test_context_limited: bool,
}

/// One file's profile (`JevCodeReviewer.Profile`).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    /// The file path.
    pub path: String,
    /// The chosen category.
    pub category: String,
    /// The category confidence.
    pub category_confidence: f64,
    /// The rubric priority score (`0..=3`).
    pub priority: f64,
    /// The priority confidence.
    pub priority_confidence: f64,
}

/// One screening signal: a concern worth investigating (`JevCodeReviewer.Signal`).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Signal {
    /// The file path.
    pub path: String,
    /// The dimension.
    pub dimension: String,
    /// The screening probability.
    pub probability: f64,
}

/// How a follow-up ended (`JevCodeReviewer.Disposition`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Disposition {
    /// Evidence was located.
    Located,
    /// No candidate region supported the concern.
    NoMatch,
    /// No concrete mechanism was supported.
    NoIssue,
    /// An answer was below the confidence floor.
    LowConfidence,
    /// The follow-up was deferred by a limit.
    Deferred,
    /// The follow-up failed.
    Error,
}

impl Disposition {
    /// The Java enum name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Located => "LOCATED",
            Self::NoMatch => "NO_MATCH",
            Self::NoIssue => "NO_ISSUE",
            Self::LowConfidence => "LOW_CONFIDENCE",
            Self::Deferred => "DEFERRED",
            Self::Error => "ERROR",
        }
    }
}

/// The review advice (`JevCodeReviewer.Advice`); a prompt for humans, never an approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewAdvice {
    /// Comment on the change.
    Comment,
    /// Request a changes review.
    RequestChangesReview,
}

impl ReviewAdvice {
    /// The Java enum name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Comment => "COMMENT",
            Self::RequestChangesReview => "REQUEST_CHANGES_REVIEW",
        }
    }
}

/// One located finding (`JevCodeReviewer.Finding`).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    /// The signal that produced it.
    pub signal: Signal,
    /// The evidence region.
    pub evidence: Region,
    /// The locate-stage confidence.
    pub location_confidence: f64,
    /// The concrete mechanism.
    pub mechanism: String,
    /// The mechanism confidence.
    pub mechanism_confidence: f64,
    /// The severity rubric score (`0..=3`).
    pub severity: f64,
    /// The severity confidence.
    pub severity_confidence: f64,
    /// The routed owner, when confident enough.
    pub owner: Option<String>,
    /// The owner confidence, when an owner was asked for.
    pub owner_confidence: Option<f64>,
    /// The advice.
    pub advice: ReviewAdvice,
}

/// One follow-up (`JevCodeReviewer.FollowUp`).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FollowUp {
    /// The signal.
    pub signal: Signal,
    /// The disposition.
    pub disposition: Disposition,
    /// Why.
    pub reason: String,
    /// The finding, when located.
    pub finding: Option<Finding>,
}

/// One model call (`JevCodeReviewer.Call`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCall {
    /// `screen`, `profile`, `locate`, `mechanism`, `severity` or `route`.
    pub stage: String,
    /// The file path.
    pub path: String,
    /// The dimension, empty outside follow-ups.
    pub dimension: String,
    /// `DECIDED`, `ERROR` or `CANCELLED`.
    pub status: String,
    /// The model that answered.
    pub model: Option<String>,
    /// Wall-clock time spent.
    pub elapsed_millis: u64,
}

/// The review report (`JevCodeReviewer.Report`).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewReport {
    /// The reviewed revision.
    pub revision: String,
    /// How the snapshot was assembled.
    pub mode: ReviewMode,
    /// How many files were requested.
    pub requested_files: usize,
    /// The screening matrix.
    pub matrix: Vec<Screening>,
    /// The profiles.
    pub profiles: Vec<Profile>,
    /// The follow-ups.
    pub follow_ups: Vec<FollowUp>,
    /// Files that were never screened.
    pub unscreened_files: Vec<String>,
    /// Files without a profile.
    pub unprofiled_files: usize,
    /// Whether every file was screened and every follow-up decided.
    pub complete: bool,
    /// The calls that were made.
    pub calls: Vec<ReviewCall>,
    /// Wall-clock time spent.
    pub elapsed_millis: u64,
}

impl ReviewReport {
    /// The located findings, most severe first (`Report.findings`).
    pub fn findings(&self) -> Vec<&Finding> {
        let mut findings: Vec<&Finding> = self
            .follow_ups
            .iter()
            .filter_map(|follow_up| follow_up.finding.as_ref())
            .collect();
        findings.sort_by(|left, right| {
            right
                .severity
                .partial_cmp(&left.severity)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        findings
    }
}

/// The completeness rule (`JevCodeReviewer.report`).
///
/// Complete means: the run finished, no file was left unscreened, every screening decided with all
/// probabilities outside the uncertainty band, no follow-up was deferred/errored/low-confidence,
/// and every profile cleared the confidence floor.
pub fn compute_complete(
    finished: bool,
    matrix: &[Screening],
    follow_ups: &[FollowUp],
    profiles: &[Profile],
    unscreened: &[String],
    config: &ReviewConfig,
) -> bool {
    finished
        && unscreened.is_empty()
        && matrix.iter().all(|screening| {
            screening.status == "DECIDED"
                && !screening.test_context_limited
                && screening.probabilities.values().all(|probability| {
                    *probability <= config.low_risk_threshold
                        || *probability >= config.screen_threshold
                })
        })
        && follow_ups.iter().all(|follow_up| {
            !matches!(
                follow_up.disposition,
                Disposition::Deferred | Disposition::Error | Disposition::LowConfidence
            )
        })
        && profiles.iter().all(|profile| {
            profile.category_confidence >= config.min_confidence
                && profile.priority_confidence >= config.min_confidence
        })
}

/// The screening questions for one mode (`JevCodeReviewer.screeningQuestions`).
pub fn screening_questions(mode: ReviewMode) -> BTreeMap<String, Question> {
    let scope = match mode {
        ReviewMode::Changes => "introduced by added or modified lines in this patch",
        ReviewMode::Codebase => "directly visible in this source region",
    };
    let mut questions = BTreeMap::new();
    for dimension in DIMENSIONS {
        let concern = match dimension {
            "correctness" => "incorrect runtime behavior, data flow, conditions or state",
            "security" => {
                "a weakened authorization/trust boundary, injection, secret exposure or unsafe default"
            }
            "reliability" => {
                "realistic crash, resource leak, race, deadlock or incomplete failure/cancellation recovery"
            }
            "compatibility" => {
                "a concrete caller, public behavior, persisted format or protocol mismatch (not speculation about unseen historical contracts)"
            }
            _ => {
                "important branches, boundaries or component interactions without targeted evidence in the supplied relatedTests (not proof no other tests exist)"
            }
        };
        questions.insert(
            dimension.to_string(),
            Question::Noul(NoulQuestion::new(
                json!({
                    "question": format!("Is there direct evidence of {concern} {scope}?"),
                    "ignore": ["style preferences", "names alone", "unsupported speculation"],
                }),
                None,
            )),
        );
    }
    questions
}

/// The file categories for one mode (`JevCodeReviewer.categories`).
pub fn categories(mode: ReviewMode) -> BTreeMap<String, Value> {
    let pairs: Vec<(&str, &str)> = match mode {
        ReviewMode::Changes => vec![
            ("behavior", "Runtime behavior change"),
            ("interface", "Public API or contract"),
            ("infrastructure", "Execution, build or operations"),
            ("observability", "Events and diagnostics"),
            ("refactor", "Behavior-preserving restructuring"),
            ("routine", "Routine change"),
        ],
        ReviewMode::Codebase => vec![
            ("entrypoint", "Application or public entry"),
            ("boundary", "Validation or external boundary"),
            ("domain", "Business rules and state"),
            ("persistence", "Durable storage"),
            ("infrastructure", "Runtime and operations"),
            ("utility", "Shared helper"),
        ],
    };
    pairs
        .into_iter()
        .map(|(id, text)| (id.to_string(), json!(text)))
        .collect()
}

/// The concrete mechanisms for one dimension (`JevCodeReviewer.mechanisms`).
pub fn mechanisms(dimension: &str) -> Result<BTreeMap<String, Value>, JevError> {
    let pairs: Vec<(&str, &str)> = match dimension {
        "correctness" => vec![
            ("condition", "Condition handles wrong cases"),
            ("state", "State read or updated incorrectly"),
            ("dataFlow", "Incorrect transformation or passing of data"),
            ("asyncControl", "Incorrect asynchronous ordering or errors"),
        ],
        "security" => vec![
            ("authorization", "Weakened authorization or trust boundary"),
            ("injection", "Untrusted input reaches unsafe sink"),
            ("exposure", "Sensitive data disclosure"),
            ("unsafeDefault", "Default creates exposure"),
        ],
        "reliability" => vec![
            ("cleanup", "Resource not cleaned up"),
            ("concurrency", "Race, deadlock or lost work"),
            ("recovery", "Incomplete failure or cancellation recovery"),
            ("crash", "Reachable crash"),
        ],
        "compatibility" => vec![
            ("api", "Incompatible public API"),
            ("behavior", "Changed caller-visible behavior"),
            ("dataFormat", "Incompatible stored or exchanged format"),
            ("protocol", "Changed external protocol"),
        ],
        "testGap" => vec![
            ("branch", "Important branch not covered"),
            ("failure", "Failure or cancellation not covered"),
            ("boundary", "Boundary not covered"),
            ("integration", "Interaction not covered"),
        ],
        _ => return Err(JevError::invalid("unknown review dimension")),
    };
    let mut result: BTreeMap<String, Value> = pairs
        .into_iter()
        .map(|(id, text)| (id.to_string(), json!(text)))
        .collect();
    result.insert("other".to_string(), json!("Another concrete mechanism"));
    result.insert(
        "noIssue".to_string(),
        json!("No concrete issue supported by this evidence"),
    );
    Ok(result)
}

/// The reviewer owners (`JevCodeReviewer.owners`).
pub fn owners() -> BTreeMap<String, Value> {
    [
        ("security", "Security and trust boundaries"),
        ("api", "APIs, schemas and compatibility"),
        ("runtime", "Execution and failure handling"),
        ("testing", "Coverage and regression tests"),
        ("maintainer", "Owning feature maintainer"),
    ]
    .into_iter()
    .map(|(id, text)| (id.to_string(), json!(text)))
    .collect()
}

/// Keeps the head and tail of `text` with a `[...]` marker (`JevCodeReviewer.snippet`).
pub fn snippet(text: &str, chars: usize) -> String {
    let count = text.chars().count();
    if count <= chars {
        return text.to_string();
    }
    let side = chars.saturating_sub(7) / 2;
    let head: String = text.chars().take(side).collect();
    let tail: String = text.chars().skip(count - side).collect();
    format!("{head}\n[...]\n{tail}")
}

/// Turns a screening matrix into signals, most suspicious first (`JevCodeReviewer.execute`).
pub fn signals(matrix: &[Screening], config: &ReviewConfig) -> Vec<Signal> {
    let mut signals: Vec<Signal> = Vec::new();
    for screening in matrix {
        for dimension in DIMENSIONS {
            if let Some(probability) = screening.probabilities.get(dimension) {
                if *probability >= config.screen_threshold {
                    signals.push(Signal {
                        path: screening.path.clone(),
                        dimension: dimension.to_string(),
                        probability: *probability,
                    });
                }
            }
        }
    }
    signals.sort_by(|left, right| {
        right
            .probability
            .partial_cmp(&left.probability)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    signals
}

/// The staged reviewer skeleton (`JevCodeReviewer`).
#[derive(Debug, Clone)]
pub struct JevCodeReviewer {
    config: ReviewConfig,
}

impl JevCodeReviewer {
    /// Creates the reviewer over a validated policy (`JevCodeReviewer` constructor).
    pub fn new(config: ReviewConfig) -> Result<Self, JevError> {
        config.validate()?;
        Ok(Self { config })
    }

    /// The configured policy.
    pub fn config(&self) -> &ReviewConfig {
        &self.config
    }

    /// Fails closed: the staged driver needs the core seam.
    pub fn review(&self) -> Result<ReviewReport, JevError> {
        Err(JevError::protocol(CODE_REVIEWER_UNAVAILABLE))
    }

    /// The dimensions a screen reports.
    pub fn dimensions(&self) -> Vec<&'static str> {
        DIMENSIONS.to_vec()
    }

    /// The set of dimensions a screening must answer.
    pub fn required_dimensions(&self) -> BTreeSet<&'static str> {
        DIMENSIONS.into_iter().collect()
    }

    /// The profile questions (`JevCodeReviewer.profile`).
    pub fn profile_questions(&self, mode: ReviewMode) -> BTreeMap<String, Question> {
        let mut questions = BTreeMap::new();
        questions.insert(
            "category".to_string(),
            Question::Choice(ChoiceQuestion::new(
                json!("Classify the primary role/purpose of the supplied file."),
                categories(mode).into_iter().collect(),
            )),
        );
        questions.insert(
            "reviewPriority".to_string(),
            Question::Score(ScoreQuestion::new(
                json!("How closely should a human review this evidence?"),
                PRIORITY.iter().map(|level| json!(level)).collect(),
            )),
        );
        questions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::review::Side;

    fn region() -> Region {
        Region {
            id: "R1".to_string(),
            start_line: 1,
            end_line: 2,
            side: Side::New,
            text: "content".to_string(),
        }
    }

    fn screening(path: &str, probabilities: &[(&str, f64)]) -> Screening {
        Screening {
            path: path.to_string(),
            status: "DECIDED".to_string(),
            reason: "SCREENED".to_string(),
            probabilities: probabilities
                .iter()
                .map(|(dimension, probability)| ((*dimension).to_string(), *probability))
                .collect(),
            test_context_limited: false,
        }
    }

    fn profile(path: &str, confidence: f64) -> Profile {
        Profile {
            path: path.to_string(),
            category: "routine".to_string(),
            category_confidence: confidence,
            priority: 1.0,
            priority_confidence: confidence,
        }
    }

    fn located(severity: f64) -> FollowUp {
        let signal = Signal {
            path: "a.rs".to_string(),
            dimension: "security".to_string(),
            probability: 0.9,
        };
        FollowUp {
            signal: signal.clone(),
            disposition: Disposition::Located,
            reason: "REVIEW_PROMPT_NOT_PROOF".to_string(),
            finding: Some(Finding {
                signal,
                evidence: region(),
                location_confidence: 0.9,
                mechanism: "injection".to_string(),
                mechanism_confidence: 0.8,
                severity,
                severity_confidence: 0.9,
                owner: Some("security".to_string()),
                owner_confidence: Some(0.9),
                advice: ReviewAdvice::Comment,
            }),
        }
    }

    #[test]
    fn the_reference_policy_validates_and_rejects_bad_bounds() {
        assert!(ReviewConfig::reference_policy().validate().is_ok());
        let mutations: Vec<fn(&mut ReviewConfig)> = vec![
            |policy: &mut ReviewConfig| policy.screen_threshold = 0.1,
            |policy: &mut ReviewConfig| policy.screen_threshold = 1.5,
            |policy: &mut ReviewConfig| policy.min_confidence = -0.1,
            |policy: &mut ReviewConfig| policy.route_severity = 3.5,
            |policy: &mut ReviewConfig| policy.blocking_severity = 0.5,
            |policy: &mut ReviewConfig| policy.max_requests = 0,
            |policy: &mut ReviewConfig| policy.max_regions = 0,
            |policy: &mut ReviewConfig| policy.max_regions = 255,
            |policy: &mut ReviewConfig| policy.low_risk_threshold = -1.0,
        ];
        for mutate in mutations {
            let mut policy = ReviewConfig::reference_policy();
            mutate(&mut policy);
            assert_eq!(
                policy.validate().unwrap_err().message(),
                "invalid bounded review policy"
            );
        }
        assert!(JevCodeReviewer::new(ReviewConfig::reference_policy()).is_ok());
    }

    #[test]
    fn screening_questions_cover_every_dimension_with_the_mode_scope() {
        let questions = screening_questions(ReviewMode::Changes);
        assert_eq!(questions.len(), 5);
        for dimension in DIMENSIONS {
            assert!(matches!(questions.get(dimension), Some(Question::Noul(_))));
        }
        let Some(Question::Noul(question)) = questions.get("correctness") else {
            panic!("noul");
        };
        assert!(question.instructions.as_ref().unwrap()["question"]
            .as_str()
            .unwrap()
            .contains("added or modified lines"));
        let codebase = screening_questions(ReviewMode::Codebase);
        let Some(Question::Noul(question)) = codebase.get("testGap") else {
            panic!("noul");
        };
        assert!(question.instructions.as_ref().unwrap()["question"]
            .as_str()
            .unwrap()
            .contains("directly visible in this source region"));

        let reviewer = JevCodeReviewer::new(ReviewConfig::reference_policy()).unwrap();
        assert_eq!(reviewer.dimensions().len(), 5);
        assert_eq!(reviewer.required_dimensions().len(), 5);
        assert_eq!(reviewer.profile_questions(ReviewMode::Changes).len(), 2);
        assert_eq!(
            reviewer.profile_questions(ReviewMode::Changes)["category"].type_name(),
            "choice"
        );
        assert_eq!(categories(ReviewMode::Changes).len(), 6);
        assert_eq!(categories(ReviewMode::Codebase).len(), 6);
    }

    #[test]
    fn vocabularies_and_snippets_match_java() {
        let gap = mechanisms("testGap").unwrap();
        assert_eq!(gap.len(), 6, "four plus other and noIssue");
        assert_eq!(
            gap["noIssue"],
            json!("No concrete issue supported by this evidence")
        );
        assert_eq!(owners().len(), 5);
        assert_eq!(
            mechanisms("unknown").unwrap_err().message(),
            "unknown review dimension"
        );
        assert_eq!(Disposition::Located.as_str(), "LOCATED");
        assert_eq!(
            ReviewAdvice::RequestChangesReview.as_str(),
            "REQUEST_CHANGES_REVIEW"
        );
        assert_eq!(PRIORITY.len(), 4);
        assert_eq!(SEVERITY.len(), 4);
        assert_eq!(snippet("abc", 5), "abc");
        let long = "x".repeat(100);
        let cut = snippet(&long, 11);
        assert!(cut.starts_with("xx"));
        assert!(cut.contains("[...]"));
        assert_eq!(cut.chars().count(), 2 + 7 + 2);
        assert_eq!(snippet("abcdef", 0), "\n[...]\n");
    }

    #[test]
    fn signals_are_sorted_by_probability() {
        let matrix = vec![
            screening("a.rs", &[("correctness", 0.8), ("security", 0.1)]),
            screening("b.rs", &[("security", 0.95)]),
        ];
        let signals = signals(&matrix, &ReviewConfig::reference_policy());
        assert_eq!(signals.len(), 2);
        assert_eq!(signals[0].path, "b.rs");
        assert_eq!(signals[0].dimension, "security");
        assert_eq!(signals[1].path, "a.rs");
    }

    #[test]
    fn completeness_follows_the_java_rule() {
        let config = ReviewConfig::reference_policy();
        let matrix = vec![screening("a.rs", &[("correctness", 0.9), ("security", 0.1)])];
        let profiles = vec![profile("a.rs", 0.9)];
        let follow_ups = vec![located(2.5)];
        assert!(compute_complete(
            true,
            &matrix,
            &follow_ups,
            &profiles,
            &[],
            &config
        ));
        assert!(!compute_complete(
            false,
            &matrix,
            &follow_ups,
            &profiles,
            &[],
            &config
        ));
        assert!(!compute_complete(
            true,
            &matrix,
            &follow_ups,
            &profiles,
            &["b.rs".to_string()],
            &config
        ));
        let uncertain = vec![screening("a.rs", &[("correctness", 0.5)])];
        assert!(!compute_complete(
            true,
            &uncertain,
            &follow_ups,
            &profiles,
            &[],
            &config
        ));
        let mut limited = matrix.clone();
        limited[0].test_context_limited = true;
        assert!(!compute_complete(
            true,
            &limited,
            &follow_ups,
            &profiles,
            &[],
            &config
        ));
        let mut deferred = follow_ups.clone();
        deferred[0].disposition = Disposition::Deferred;
        assert!(!compute_complete(
            true,
            &matrix,
            &deferred,
            &profiles,
            &[],
            &config
        ));
        let low = vec![profile("a.rs", 0.1)];
        assert!(!compute_complete(true, &matrix, &follow_ups, &low, &[], &config));
    }

    #[test]
    fn the_report_sorts_findings_by_severity_and_the_driver_fails_closed() {
        let report = ReviewReport {
            revision: "rev1".to_string(),
            mode: ReviewMode::Changes,
            requested_files: 1,
            matrix: vec![screening("a.rs", &[("security", 0.9)])],
            profiles: vec![profile("a.rs", 0.9)],
            follow_ups: vec![located(1.0), located(3.0)],
            unscreened_files: Vec::new(),
            unprofiled_files: 0,
            complete: true,
            calls: Vec::new(),
            elapsed_millis: 5,
        };
        let findings = report.findings();
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].severity, 3.0);
        assert_eq!(findings[1].severity, 1.0);
        assert_eq!(findings[0].advice, ReviewAdvice::Comment);

        let reviewer = JevCodeReviewer::new(ReviewConfig::reference_policy()).unwrap();
        assert_eq!(
            reviewer.review().unwrap_err().message(),
            CODE_REVIEWER_UNAVAILABLE
        );
        assert_eq!(reviewer.config().max_regions, 128);
    }
}
