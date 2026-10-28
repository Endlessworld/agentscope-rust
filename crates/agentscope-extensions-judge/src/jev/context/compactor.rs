//! Paired call/result pruning adapted from `fast-jev-compaction`, with scoped durable recovery
//! (`JevContextCompactor`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::error::JevError;
use crate::jev::call::{execute_guarded, Caller};
use crate::jev::client::JevClient;
use crate::jev::dto::{Answer, NoulAnswer, NoulQuestion, Question, SystemOneRequest};
use crate::jev::execution::{Decision, JevExecution, Mode, Options, Status};
use crate::jev::validation::validate_response;

use super::archive::{ArchiveScope, JevContextArchive};
use super::message::ContextMessage;
use super::transcript::{estimate_tokens, CompactionTranscript, Fitted};

/// What to do with one call/result pair (`JevContextCompactor.Action`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Action {
    /// Leave the pair untouched.
    #[default]
    Keep,
    /// Replace the result with its head plus a restore notice.
    TruncateResult,
    /// Remove both the call and its result.
    DropPair,
}

impl Action {
    /// The Java enum name, used in scores and recommendations.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Keep => "KEEP",
            Self::TruncateResult => "TRUNCATE_RESULT",
            Self::DropPair => "DROP_PAIR",
        }
    }
}

/// Compaction policy (`JevContextCompactor.Config`).
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// At or below this keep-probability a pair is a removal candidate.
    pub discard_threshold: f64,
    /// At or above this keep-probability a pair is needed verbatim.
    pub keep_threshold: f64,
    /// How many trailing messages are always preserved.
    pub preserve_recent_messages: usize,
    /// Maximum estimated tokens of the compaction state.
    pub max_state_tokens: usize,
    /// Maximum estimated tokens of one request (state plus questions).
    pub max_request_tokens: usize,
    /// Maximum questions per request.
    pub max_questions: usize,
    /// How many characters of a truncated result are kept.
    pub truncate_head_chars: usize,
    /// The minimum size reduction required to propose a plan.
    pub minimum_reduction: f64,
    /// Maximum serialized size of the input transcript.
    pub max_transcript_chars: usize,
    /// Tools whose results may be pruned.
    pub eligible_tools: BTreeSet<String>,
    /// Message ids that are always preserved.
    pub pinned_message_ids: BTreeSet<String>,
}

impl Config {
    /// The Java `Config.defaults()`; thresholds are demonstrations only, calibrate before enforcing.
    pub fn defaults() -> Self {
        Self {
            discard_threshold: 0.2,
            keep_threshold: 0.8,
            preserve_recent_messages: 6,
            max_state_tokens: 25_000,
            max_request_tokens: 30_000,
            max_questions: 128,
            truncate_head_chars: 300,
            minimum_reduction: 0.1,
            max_transcript_chars: 5_000_000,
            eligible_tools: BTreeSet::new(),
            pinned_message_ids: BTreeSet::new(),
        }
    }

    /// Validates the configuration (`Config` compact constructor).
    pub fn validate(&self) -> Result<(), JevError> {
        let invalid = !self.discard_threshold.is_finite()
            || !self.keep_threshold.is_finite()
            || self.discard_threshold < 0.0
            || self.keep_threshold > 1.0
            || self.discard_threshold >= self.keep_threshold
            || self.max_state_tokens < 1
            || self.max_request_tokens <= self.max_state_tokens
            || !(2..=512).contains(&self.max_questions)
            || !self.minimum_reduction.is_finite()
            || self.minimum_reduction < 0.0
            || self.minimum_reduction >= 1.0
            || self.max_transcript_chars < 1;
        if invalid {
            return Err(JevError::invalid("invalid compaction config"));
        }
        Ok(())
    }
}

/// The keep-probabilities of one pair (`JevContextCompactor.Scores`).
#[derive(Debug, Clone, PartialEq)]
pub struct Scores {
    /// Probability that the call itself is still needed.
    pub keep_call: f64,
    /// Probability that the full result is still needed.
    pub keep_result: f64,
    /// Why the pair received its action.
    pub reason: String,
}

/// One compaction proposal (`JevContextCompactor.Plan`).
#[derive(Debug, Clone, PartialEq)]
pub struct ContextPlan {
    /// The messages to keep.
    pub messages: Vec<ContextMessage>,
    /// The action per pair key.
    pub decisions: BTreeMap<String, Action>,
    /// The scores per pair key.
    pub scores: BTreeMap<String, Scores>,
    /// How many requests were sent.
    pub requests: usize,
    /// Summed input tokens of the usable responses.
    pub input_tokens: i64,
    /// Summed output tokens of the usable responses.
    pub output_tokens: i64,
    /// The fitting stage that was used.
    pub state_stage: String,
    /// The archive reference, empty unless the mode is `ENFORCE`.
    pub archive_reference: String,
}

/// The inputs a host adapts from its compaction request
/// (Java `ConversationCompactionStrategy.Request`).
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionRequest {
    /// The messages to consider.
    pub messages: Vec<ContextMessage>,
    /// The user the session belongs to.
    pub user_id: String,
    /// The agent the session belongs to.
    pub agent_id: String,
    /// The session id.
    pub session_id: String,
}

impl CompactionRequest {
    /// Creates a request with a validated scope.
    pub fn new(
        messages: Vec<ContextMessage>,
        user_id: impl Into<String>,
        agent_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Result<Self, JevError> {
        let scope = ArchiveScope::new(user_id, agent_id, session_id)?;
        Ok(Self {
            messages,
            user_id: scope.user_id,
            agent_id: scope.agent_id,
            session_id: scope.session_id,
        })
    }
}

/// Proposes reversible pair-level pruning; never mutates agent state or long-term memory.
pub struct JevContextCompactor {
    caller: Caller,
    config: Config,
    execution: JevExecution,
    archive: Option<Arc<dyn JevContextArchive>>,
}

impl std::fmt::Debug for JevContextCompactor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JevContextCompactor")
            .field("mode", &self.mode())
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl JevContextCompactor {
    /// Creates a compactor over an injected caller.
    ///
    /// `ENFORCE` requires a durable archive, because a lossy rewrite must be recoverable.
    pub fn new(
        caller: Caller,
        config: Config,
        options: Options,
        archive: Option<Arc<dyn JevContextArchive>>,
    ) -> Result<Self, JevError> {
        config.validate()?;
        if options.mode == Mode::Enforce && archive.is_none() {
            return Err(JevError::invalid("ENFORCE requires durable archive"));
        }
        Ok(Self {
            caller,
            config,
            execution: JevExecution::new("context_compaction", options),
            archive,
        })
    }

    /// Creates a compactor over a [`JevClient`] with the Java defaults.
    pub fn from_client(client: JevClient) -> Result<Self, JevError> {
        Self::new(
            crate::jev::call::caller_from_client(client),
            Config::defaults(),
            Options::disabled(),
            None,
        )
    }

    /// The configured mode.
    pub fn mode(&self) -> Mode {
        self.execution.mode()
    }

    /// The configured policy.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Produces a plan (`compact`).
    ///
    /// `OFF` and `SHADOW` report `None`, preserving the host's existing compaction flow; `ENFORCE`
    /// reports the retained messages on a decided plan and the original messages otherwise, so a
    /// failed compaction is conservative rather than lossy.
    pub async fn compact(
        &self,
        request: &CompactionRequest,
    ) -> Decision<Vec<ContextMessage>> {
        let original = request.messages.clone();
        let decision = self.plan(request).await;
        let value = if self.mode() == Mode::Enforce {
            Some(match (&decision.status, decision.value.as_ref()) {
                (Status::Decided, Some(plan)) => plan.messages.clone(),
                _ => original,
            })
        } else {
            None
        };
        Decision::new(
            decision.status,
            value,
            decision.reason,
            decision.recommendation,
        )
    }

    /// Produces a plan within the execution budget (`JevContextCompactor.plan`).
    pub async fn plan(&self, request: &CompactionRequest) -> Decision<ContextPlan> {
        execute_guarded(&self.execution, self.evaluate(request)).await
    }

    async fn evaluate(&self, request: &CompactionRequest) -> Result<Decision<ContextPlan>, JevError> {
        let enforce = self.mode() == Mode::Enforce;
        let scope = if enforce {
            Some(ArchiveScope::new(
                request.user_id.clone(),
                request.agent_id.clone(),
                request.session_id.clone(),
            )?)
        } else {
            None
        };
        let transcript = CompactionTranscript::new(&request.messages, &self.config)?;
        let candidates: Vec<_> = transcript
            .pairs
            .iter()
            .filter(|pair| !pair.pinned)
            .cloned()
            .collect();
        if candidates.is_empty() {
            return Ok(Decision::new(
                Status::Skipped,
                None,
                "NO_CANDIDATES",
                BTreeMap::new(),
            ));
        }
        let fitted = transcript.fit(&self.config)?;
        let batches = self.batches(&candidates, &fitted)?;
        let mut actions: BTreeMap<String, Action> = transcript
            .pairs
            .iter()
            .map(|pair| (pair.key.clone(), Action::Keep))
            .collect();
        let mut scores: BTreeMap<String, Scores> = BTreeMap::new();
        for pair in transcript.pairs.iter().filter(|pair| pair.pinned) {
            scores.insert(
                pair.key.clone(),
                Scores {
                    keep_call: 1.0,
                    keep_result: 1.0,
                    reason: "PINNED".to_string(),
                },
            );
        }

        let mut answers: BTreeMap<String, Answer> = BTreeMap::new();
        let mut input_tokens = 0i64;
        let mut output_tokens = 0i64;
        for batch in &batches {
            let result = (self.caller)(batch.clone())
                .await
                .map_err(|failure| JevError::protocol(failure.message().to_string()))?;
            validate_response(batch, &result)?;
            if let Some(batch_answers) = result.answers {
                answers.extend(batch_answers);
            }
            if let Some(usage) = result.usage {
                input_tokens += usage.input_tokens;
                output_tokens += usage.output_tokens;
            }
        }

        let unchanged = serde_json::to_string(&request.messages)
            .map_err(|_error| JevError::invalid("unsupported transcript content"))?;
        if transcript.encoded != unchanged {
            return Ok(Decision::uncertain("TRANSCRIPT_CHANGED"));
        }

        for pair in &candidates {
            let keep_call = noul(&answers, &format!("call_{}", pair.key))?;
            let keep_result = noul(&answers, &format!("result_{}", pair.key))?;
            let action = if keep_result <= self.config.discard_threshold
                && keep_call <= self.config.discard_threshold
            {
                Action::DropPair
            } else if keep_result <= self.config.discard_threshold
                && keep_call >= self.config.keep_threshold
            {
                Action::TruncateResult
            } else {
                Action::Keep
            };
            actions.insert(pair.key.clone(), action);
            scores.insert(
                pair.key.clone(),
                Scores {
                    keep_call,
                    keep_result,
                    reason: if action != Action::Keep {
                        action.as_str().to_string()
                    } else if keep_result >= self.config.keep_threshold {
                        "NEEDED_RESULT".to_string()
                    } else {
                        "UNCERTAIN".to_string()
                    },
                },
            );
        }

        let retained = transcript.apply(&actions, self.config.truncate_head_chars);
        let after = serde_json::to_string(&retained)
            .map_err(|_error| JevError::invalid("unsupported transcript content"))?
            .chars()
            .count();
        let before = transcript.encoded.chars().count();
        let reduction = 1.0 - after as f64 / before as f64;
        if after >= before || reduction < self.config.minimum_reduction {
            return Ok(Decision::new(
                Status::Inconclusive,
                Some(ContextPlan {
                    messages: request.messages.clone(),
                    decisions: actions,
                    scores,
                    requests: batches.len(),
                    input_tokens,
                    output_tokens,
                    state_stage: fitted.stage.clone(),
                    archive_reference: String::new(),
                }),
                "INSUFFICIENT_REDUCTION",
                BTreeMap::from([
                    ("requests".to_string(), batches.len().to_string()),
                    ("state_stage".to_string(), fitted.stage.clone()),
                    ("input_tokens".to_string(), input_tokens.to_string()),
                    ("output_tokens".to_string(), output_tokens.to_string()),
                ]),
            ));
        }

        let archive_reference = match (&scope, &self.archive) {
            (Some(scope), Some(archive)) => {
                let reference = archive.save(scope, &transcript.messages).await?;
                if reference.trim().is_empty() {
                    return Err(JevError::protocol(
                        "archive did not acknowledge snapshot",
                    ));
                }
                reference
            }
            _ => String::new(),
        };
        let plan = ContextPlan {
            messages: retained,
            decisions: actions.clone(),
            scores,
            requests: batches.len(),
            input_tokens,
            output_tokens,
            state_stage: fitted.stage.clone(),
            archive_reference: archive_reference.clone(),
        };
        let removed = actions
            .values()
            .filter(|action| **action == Action::DropPair)
            .count();
        let truncated = actions
            .values()
            .filter(|action| **action == Action::TruncateResult)
            .count();
        Ok(Decision::new(
            Status::Decided,
            Some(plan),
            if enforce {
                "ARCHIVED_PROPOSAL"
            } else {
                "SHADOW_PROPOSAL"
            },
            BTreeMap::from([
                ("requests".to_string(), batches.len().to_string()),
                ("state_stage".to_string(), fitted.stage),
                ("pairs_removed".to_string(), removed.to_string()),
                ("results_truncated".to_string(), truncated.to_string()),
                ("reduction".to_string(), reduction.to_string()),
                ("archive_reference".to_string(), archive_reference),
                ("input_tokens".to_string(), input_tokens.to_string()),
                ("output_tokens".to_string(), output_tokens.to_string()),
            ]),
        ))
    }

    /// Splits the candidate questions into requests that fit the token budget
    /// (`JevContextCompactor.batches`).
    fn batches(
        &self,
        candidates: &[super::transcript::Pair],
        fitted: &Fitted,
    ) -> Result<Vec<SystemOneRequest>, JevError> {
        let mut requests = Vec::new();
        let mut current: BTreeMap<String, Question> = BTreeMap::new();
        for pair in candidates {
            let mut own: BTreeMap<String, Question> = BTreeMap::new();
            own.insert(
                format!("call_{}", pair.key),
                Question::Noul(NoulQuestion::new(
                    json!(format!(
                        "Knowing that tool call {} occurred, with its input, is still needed to continue this task correctly.",
                        pair.key
                    )),
                    None,
                )),
            );
            own.insert(
                format!("result_{}", pair.key),
                Question::Noul(NoulQuestion::new(
                    json!(format!(
                        "The full result of tool call {} is still needed verbatim for the next work; removing it from active context risks losing necessary evidence.",
                        pair.key
                    )),
                    None,
                )),
            );
            let mut next = current.clone();
            next.extend(own.clone());
            if !current.is_empty() && !self.fits(fitted, &next)? {
                requests.push(request_from(fitted, current));
                next = own.clone();
            }
            if !self.fits(fitted, &next)? {
                return Err(JevError::invalid("no room for compaction questions"));
            }
            current = next;
        }
        if !current.is_empty() {
            requests.push(request_from(fitted, current));
        }
        Ok(requests)
    }

    fn fits(
        &self,
        fitted: &Fitted,
        questions: &BTreeMap<String, Question>,
    ) -> Result<bool, JevError> {
        if questions.len() > self.config.max_questions {
            return Ok(false);
        }
        let serialized = serde_json::to_string(questions)
            .map_err(|_error| JevError::invalid("unsupported transcript content"))?;
        Ok(fitted.tokens + estimate_tokens(&serialized) + 20 <= self.config.max_request_tokens)
    }
}

fn request_from(fitted: &Fitted, questions: BTreeMap<String, Question>) -> SystemOneRequest {
    SystemOneRequest::new(
        Some(Value::Object(
            fitted
                .state
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        )),
        None,
        questions,
    )
}

fn noul(answers: &BTreeMap<String, Answer>, id: &str) -> Result<f64, JevError> {
    match answers.get(id) {
        Some(Answer::Noul(NoulAnswer { noul: Some(value) })) => Ok(*value),
        _ => Err(JevError::protocol(format!(
            "System One response missing noul answer '{id}'"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::call::testing::ScriptedCaller;
    use crate::jev::context::message::{
        ContextBlock, ContextResultPart, ToolResultState, ROLE_ASSISTANT, ROLE_TOOL,
    };
    use crate::jev::dto::{SystemOneResult, Usage};

    fn config() -> Config {
        let mut config = Config::defaults();
        config.eligible_tools = BTreeSet::from(["read".to_string()]);
        // Two recent messages kept, so the pair at index 1..2 stays a candidate.
        config.preserve_recent_messages = 2;
        config
    }

    fn messages() -> Vec<ContextMessage> {
        let long = "x".repeat(2000);
        vec![
            ContextMessage::user("m1", "read the file"),
            ContextMessage::new(
                "m2",
                ROLE_ASSISTANT,
                "",
                vec![ContextBlock::ToolUse {
                    id: Some("c1".to_string()),
                    name: "read".to_string(),
                    input: json!({"path": "a.rs"}),
                }],
            ),
            ContextMessage::new(
                "m3",
                ROLE_TOOL,
                "",
                vec![ContextBlock::ToolResult {
                    id: Some("c1".to_string()),
                    name: "read".to_string(),
                    state: ToolResultState::Success,
                    parts: vec![ContextResultPart::Text { text: long }],
                    suspended: false,
                }],
            ),
            ContextMessage::assistant("m4", "done"),
            ContextMessage::user("m5", "thanks"),
            ContextMessage::assistant("m6", "bye"),
        ]
    }

    fn request() -> CompactionRequest {
        CompactionRequest::new(messages(), "u", "a", "s").unwrap()
    }

    fn response(keep_call: f64, keep_result: f64) -> SystemOneResult {
        let mut answers = BTreeMap::new();
        answers.insert(
            "call_t1".to_string(),
            Answer::Noul(NoulAnswer {
                noul: Some(keep_call),
            }),
        );
        answers.insert(
            "result_t1".to_string(),
            Answer::Noul(NoulAnswer {
                noul: Some(keep_result),
            }),
        );
        SystemOneResult {
            model: Some("jev".to_string()),
            answers: Some(answers),
            usage: Some(Usage::new(100, 20)),
        }
    }

    fn shadow(responses: Vec<SystemOneResult>) -> JevContextCompactor {
        let scripted = Arc::new(ScriptedCaller::new(responses.into_iter().map(Ok).collect()));
        JevContextCompactor::new(scripted.as_caller(), config(), shadow_options(), None).unwrap()
    }

    fn shadow_options() -> Options {
        Options::new(Mode::Shadow, std::time::Duration::from_secs(2), "v1").unwrap()
    }

    #[tokio::test]
    async fn a_low_keep_probability_drops_the_pair_in_shadow_mode() {
        let compactor = shadow(vec![response(0.05, 0.05)]);
        let decision = compactor.plan(&request()).await;
        assert_eq!(decision.status, Status::Decided, "{:?}", decision.reason);
        assert_eq!(decision.reason, "SHADOW_PROPOSAL");
        let plan = decision.value.unwrap();
        assert_eq!(plan.decisions["t1"], Action::DropPair);
        assert_eq!(plan.scores["t1"].reason, "DROP_PAIR");
        assert_eq!(plan.scores["t1"].keep_call, 0.05);
        assert_eq!(plan.requests, 1);
        assert_eq!(plan.input_tokens, 100);
        assert_eq!(plan.output_tokens, 20);
        assert_eq!(plan.state_stage, "inputs<=1000");
        assert!(plan.archive_reference.is_empty());
        assert!(plan.messages.iter().all(|message| message.id != "m2"));
        assert_eq!(decision.recommendation["pairs_removed"], "1");

        // SHADOW never rewrites the host's messages.
        let compacted = compactor.compact(&request()).await;
        assert!(compacted.value.is_none());
    }

    #[tokio::test]
    async fn dropping_the_call_but_keeping_the_result_truncates() {
        let compactor = shadow(vec![response(0.9, 0.05)]);
        let decision = compactor.plan(&request()).await;
        assert_eq!(decision.status, Status::Decided);
        let plan = decision.value.unwrap();
        assert_eq!(plan.decisions["t1"], Action::TruncateResult);
        assert_eq!(plan.scores["t1"].reason, "TRUNCATE_RESULT");
        assert_eq!(decision.recommendation["results_truncated"], "1");
        let result_text = plan.messages[2].blocks[0].result_text();
        assert!(result_text.starts_with("xxxx"));
        assert!(result_text.contains("JEV archived the full result"));
    }

    #[tokio::test]
    async fn needed_results_and_uncertain_pairs_are_kept() {
        let compactor = shadow(vec![response(0.9, 0.9)]);
        let plan = compactor.plan(&request()).await.value.unwrap();
        assert_eq!(plan.decisions["t1"], Action::Keep);
        assert_eq!(plan.scores["t1"].reason, "NEEDED_RESULT");

        let compactor = shadow(vec![response(0.5, 0.5)]);
        let decision = compactor.plan(&request()).await;
        assert_eq!(decision.status, Status::Inconclusive);
        assert_eq!(decision.reason, "INSUFFICIENT_REDUCTION");
        let plan = decision.value.unwrap();
        assert_eq!(plan.scores["t1"].reason, "UNCERTAIN");
        assert_eq!(plan.messages, messages(), "an inconclusive plan keeps the input");
    }

    #[tokio::test]
    async fn off_registers_no_work_and_no_candidates_skips() {
        let scripted = Arc::new(ScriptedCaller::new(vec![]));
        let compactor = JevContextCompactor::new(
            scripted.as_caller(),
            config(),
            Options::disabled(),
            None,
        )
        .unwrap();
        let decision = compactor.plan(&request()).await;
        assert_eq!(decision.status, Status::Skipped);
        assert_eq!(decision.reason, "DISABLED");
        assert!(decision.value.is_none());

        let mut pinned_only = config();
        pinned_only.eligible_tools = BTreeSet::new();
        let scripted = Arc::new(ScriptedCaller::new(vec![]));
        let compactor = JevContextCompactor::new(
            scripted.as_caller(),
            pinned_only,
            shadow_options(),
            None,
        )
        .unwrap();
        let decision = compactor.plan(&request()).await;
        assert_eq!(decision.status, Status::Skipped);
        assert_eq!(decision.reason, "NO_CANDIDATES");
    }

    #[tokio::test]
    async fn enforce_requires_and_uses_a_durable_archive() {
        let scripted = Arc::new(ScriptedCaller::new(vec![
            Ok(response(0.05, 0.05)),
            Ok(response(0.05, 0.05)),
        ]));
        let options = Options::new(Mode::Enforce, std::time::Duration::from_secs(2), "v1").unwrap();
        assert_eq!(
            JevContextCompactor::new(scripted.as_caller(), config(), options.clone(), None)
                .unwrap_err()
                .message(),
            "ENFORCE requires durable archive"
        );

        struct Recording(std::sync::Mutex<usize>);
        #[async_trait::async_trait]
        impl JevContextArchive for Recording {
            async fn save(
                &self,
                _scope: &ArchiveScope,
                messages: &[ContextMessage],
            ) -> Result<String, JevError> {
                *self.0.lock().unwrap() = messages.len();
                Ok("a".repeat(64))
            }
            async fn restore(
                &self,
                _scope: &ArchiveScope,
                _reference: &str,
            ) -> Result<Vec<ContextMessage>, JevError> {
                Ok(Vec::new())
            }
        }
        let archive = Arc::new(Recording(std::sync::Mutex::new(0)));
        let compactor = JevContextCompactor::new(
            scripted.as_caller(),
            config(),
            options,
            Some(archive.clone()),
        )
        .unwrap();
        assert_eq!(compactor.mode(), Mode::Enforce);
        let decision = compactor.plan(&request()).await;
        assert_eq!(decision.reason, "ARCHIVED_PROPOSAL");
        assert_eq!(
            decision.value.as_ref().unwrap().archive_reference,
            "a".repeat(64)
        );
        assert_eq!(
            *archive.0.lock().unwrap(),
            6,
            "the pre-compaction snapshot is archived"
        );

        let compacted = compactor.compact(&request()).await;
        let kept = compacted.value.unwrap();
        assert!(kept.iter().all(|message| message.id != "m2"));
        assert_eq!(compactor.config().truncate_head_chars, 300);
        assert!(format!("{compactor:?}").contains("JevContextCompactor"));
    }

    #[tokio::test]
    async fn enforce_falls_back_to_the_original_messages_when_inconclusive() {
        let scripted = Arc::new(ScriptedCaller::new(vec![Ok(response(0.5, 0.5))]));
        let options = Options::new(Mode::Enforce, std::time::Duration::from_secs(2), "v1").unwrap();
        struct Noop;
        #[async_trait::async_trait]
        impl JevContextArchive for Noop {
            async fn save(
                &self,
                _scope: &ArchiveScope,
                _messages: &[ContextMessage],
            ) -> Result<String, JevError> {
                Ok("b".repeat(64))
            }
            async fn restore(
                &self,
                _scope: &ArchiveScope,
                _reference: &str,
            ) -> Result<Vec<ContextMessage>, JevError> {
                Ok(Vec::new())
            }
        }
        let compactor = JevContextCompactor::new(
            scripted.as_caller(),
            config(),
            options,
            Some(Arc::new(Noop)),
        )
        .unwrap();
        let compacted = compactor.compact(&request()).await;
        assert_eq!(compacted.status, Status::Inconclusive);
        assert_eq!(compacted.value.unwrap(), messages());
    }

    #[tokio::test]
    async fn backend_failures_and_bad_answers_surface_as_error_decisions() {
        let scripted = Arc::new(ScriptedCaller::new(vec![Err(
            crate::jev::call::CallFailure::backend("down"),
        )]));
        let compactor =
            JevContextCompactor::new(scripted.as_caller(), config(), shadow_options(), None)
                .unwrap();
        let decision = compactor.plan(&request()).await;
        assert_eq!(decision.status, Status::Error);
        assert_eq!(decision.reason, "CALL_OR_RESPONSE_ERROR");

        let mut wrong = response(0.05, 0.05);
        wrong.answers.as_mut().unwrap().insert(
            "call_t1".to_string(),
            Answer::Noul(NoulAnswer { noul: None }),
        );
        let compactor = shadow(vec![wrong]);
        assert_eq!(compactor.plan(&request()).await.status, Status::Error);

    }

    #[test]
    fn config_and_request_validation_match_java() {
        assert!(config().validate().is_ok());
        let mutations: Vec<fn(&mut Config)> = vec![
            |config: &mut Config| config.discard_threshold = 0.9,
            |config: &mut Config| config.keep_threshold = 1.5,
            |config: &mut Config| config.max_state_tokens = 0,
            |config: &mut Config| config.max_request_tokens = config.max_state_tokens,
            |config: &mut Config| config.max_questions = 1,
            |config: &mut Config| config.max_questions = 513,
            |config: &mut Config| config.minimum_reduction = 1.0,
            |config: &mut Config| config.max_transcript_chars = 0,
        ];
        for mutate in mutations {
            let mut config = config();
            mutate(&mut config);
            assert_eq!(
                config.validate().unwrap_err().message(),
                "invalid compaction config"
            );
        }
        assert!(CompactionRequest::new(vec![], "", "a", "s").is_err());
        let request = request();
        assert_eq!(request.user_id, "u");
        assert_eq!(request.agent_id, "a");
        assert_eq!(request.session_id, "s");
    }
}
