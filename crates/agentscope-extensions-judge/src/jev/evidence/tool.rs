//! The agent-visible projection of an evidence report (`JevEvidenceTool`).
//!
//! Java registers [`JevEvidenceTool::search`] on the core toolkit and reads its evidence source and
//! ACL from the runtime context. Runtime registration lives in
//! [`register_jev_tools`](crate::jev::runtime::tools::register_jev_tools);
//! [`JevEvidenceTool::register`] remains only as the fail-closed fallback for hosts that wire the
//! tool themselves. The *projection* — which passages are shown, with which classification and
//! score, and which status string — is ported as a pure function.

use crate::error::JevError;

use super::processor::Report;

/// Why the fail-closed [`JevEvidenceTool::register`] cannot register the tool.
pub const TOOL_UNAVAILABLE: &str = "JevEvidenceTool::register is fail-closed; register the tool through jev::runtime::tools::register_jev_tools";

/// One passage as the agent sees it (`JevEvidenceTool.Passage`).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolPassage {
    /// The passage id; the agent must cite it.
    pub id: String,
    /// The passage version; the agent must cite it.
    pub version: String,
    /// The passage text.
    pub text: String,
    /// The classification, only when the delivered list was the screened one.
    pub classification: Option<String>,
    /// The rerank score, only when the delivered list was the screened one.
    pub score: Option<f64>,
}

/// The tool result the agent sees (`JevEvidenceTool.Result`).
#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceResult {
    /// `RETRIEVED`, `NO_EVIDENCE` or `EVIDENCE_REVIEW_INCOMPLETE`.
    pub status: String,
    /// The delivered passages.
    pub passages: Vec<ToolPassage>,
}

/// Projects a report onto the agent-visible result (`JevEvidenceTool.search`).
///
/// Filtered passages, ACL metadata and raw checks are never exposed.
pub fn evidence_result(report: &Report) -> EvidenceResult {
    let applied = report.applied;
    let passages: Vec<ToolPassage> = report
        .delivered
        .iter()
        .map(|passage| {
            let classified = if applied {
                report
                    .suggested
                    .iter()
                    .find(|assessment| assessment.passage.id == passage.id)
            } else {
                None
            };
            ToolPassage {
                id: passage.id.clone(),
                version: passage.version.clone(),
                text: passage.text.clone(),
                classification: classified.map(|assessment| assessment.classification.as_str().to_string()),
                score: classified.and_then(|assessment| assessment.rerank_score),
            }
        })
        .collect();
    let status = if applied && !report.complete {
        "EVIDENCE_REVIEW_INCOMPLETE"
    } else if passages.is_empty() {
        "NO_EVIDENCE"
    } else {
        "RETRIEVED"
    };
    EvidenceResult {
        status: status.to_string(),
        passages,
    }
}

/// The tool's structural skeleton.
#[derive(Debug, Clone)]
pub struct JevEvidenceTool {
    candidate_limit: usize,
    top_k: usize,
}

impl JevEvidenceTool {
    /// Validates `1 <= topK <= candidateLimit <= 1024` (`JevEvidenceTool` constructor).
    pub fn new(candidate_limit: usize, top_k: usize) -> Result<Self, JevError> {
        if top_k < 1 || candidate_limit < top_k || candidate_limit > 1024 {
            return Err(JevError::invalid(
                "require 1 <= topK <= candidateLimit <= 1024",
            ));
        }
        Ok(Self {
            candidate_limit,
            top_k,
        })
    }

    /// The per-request candidate limit.
    pub fn candidate_limit(&self) -> usize {
        self.candidate_limit
    }

    /// The number of delivered passages.
    pub fn top_k(&self) -> usize {
        self.top_k
    }

    /// Validates a tool call (`JevEvidenceTool.search` guards).
    ///
    /// `scope_present` is whether the runtime context carries a user and session id.
    pub fn validate_call(&self, query: &str, scope_present: bool) -> Result<(), JevError> {
        if !scope_present {
            return Err(JevError::invalid("EVIDENCE_SCOPE_REQUIRED"));
        }
        if query.trim().is_empty() {
            return Err(JevError::invalid("EVIDENCE_SCOPE_REQUIRED"));
        }
        Ok(())
    }

    /// Fails closed: the runtime registration is
    /// [`register_jev_tools`](crate::jev::runtime::tools::register_jev_tools).
    pub fn register(&self) -> Result<(), JevError> {
        Err(JevError::protocol(TOOL_UNAVAILABLE))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::processor::{Assessment, Classification, Limits};
    use crate::jev::evidence::passage::JevPassage;
    use std::collections::BTreeMap;

    fn passage(id: &str) -> JevPassage {
        JevPassage::plain(id, format!("text {id}"), "v1").unwrap()
    }

    fn assessment(id: &str) -> Assessment {
        Assessment {
            passage: passage(id),
            classification: Classification::Conflicting,
            reason: "CLASSIFIED".to_string(),
            checks: BTreeMap::new(),
            rerank_score: Some(0.7),
            ranking_status: "DECIDED".to_string(),
        }
    }

    fn report(applied: bool, complete: bool) -> Report {
        Report {
            delivered: vec![passage("a")],
            suggested: vec![assessment("a")],
            assessments: vec![assessment("a")],
            calls: Vec::new(),
            complete,
            applied,
        }
    }

    #[test]
    fn applied_reports_expose_classification_and_score() {
        let result = evidence_result(&report(true, true));
        assert_eq!(result.status, "RETRIEVED");
        assert_eq!(result.passages[0].classification.as_deref(), Some("CONFLICTING"));
        assert_eq!(result.passages[0].score, Some(0.7));
        assert_eq!(result.passages[0].version, "v1");
    }

    #[test]
    fn shadow_reports_hide_classification_and_incomplete_runs_say_so() {
        let result = evidence_result(&report(false, true));
        assert_eq!(result.status, "RETRIEVED");
        assert_eq!(result.passages[0].classification, None);
        assert_eq!(result.passages[0].score, None);

        let result = evidence_result(&report(true, false));
        assert_eq!(result.status, "EVIDENCE_REVIEW_INCOMPLETE");

        let mut empty = report(true, true);
        empty.delivered.clear();
        assert_eq!(evidence_result(&empty).status, "NO_EVIDENCE");
        assert!(Limits::defaults().max_candidates > 0);
    }

    #[test]
    fn the_tool_validates_limits_scope_and_fails_closed() {
        for (candidate_limit, top_k) in [(0, 1), (5, 6), (1025, 1)] {
            assert_eq!(
                JevEvidenceTool::new(candidate_limit, top_k)
                    .unwrap_err()
                    .message(),
                "require 1 <= topK <= candidateLimit <= 1024"
            );
        }
        let tool = JevEvidenceTool::new(32, 4).unwrap();
        assert_eq!(tool.candidate_limit(), 32);
        assert_eq!(tool.top_k(), 4);
        assert_eq!(
            tool.validate_call("q", false).unwrap_err().message(),
            "EVIDENCE_SCOPE_REQUIRED"
        );
        assert_eq!(
            tool.validate_call(" ", true).unwrap_err().message(),
            "EVIDENCE_SCOPE_REQUIRED"
        );
        assert!(tool.validate_call("q", true).is_ok());
        assert_eq!(tool.register().unwrap_err().message(), TOOL_UNAVAILABLE);
    }
}
