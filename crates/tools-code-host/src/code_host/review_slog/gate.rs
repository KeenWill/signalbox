//! Pure review-protocol gate composition over typed slog evidence.

use serde_json::{Value, json};

use crate::code_host::{
    ConvergenceStateResult, ReviewGatePurpose, ReviewerVerdictStatus, StackStateResult,
    ThreadInventoryResult,
};

/// Stable blocker vocabulary returned by `review_gate_check`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewGateBlockerCode {
    /// One bounded evidence source requires continuation.
    EvidenceTruncated,
    /// At least one current-head check is not green.
    CiNotGreen,
    /// A review thread has no recognized disposition.
    UndispositionedThreads,
    /// A review thread remains unresolved.
    UnresolvedThreads,
    /// A resolved thread hides the escalation marker.
    BuriedEscalations,
    /// The code host reports a merge conflict.
    MergeConflicting,
    /// No actual reviewer verdict exists.
    ReviewerVerdictMissing,
    /// The latest actual verdict does not cover the current head.
    ReviewerVerdictStale,
    /// Usage-limit starvation followed the latest actual verdict.
    ReviewerStarved,
    /// The immediate base has commits absent from this head.
    ParentNeedsMergeForward,
    /// The default branch has commits absent from the immediate base chain.
    BaseChainMissingMain,
    /// An immediate child lacks commits now present in this branch.
    ChildNeedsMergeForward,
}

impl ReviewGateBlockerCode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::EvidenceTruncated => "evidence_truncated",
            Self::CiNotGreen => "ci_not_green",
            Self::UndispositionedThreads => "undispositioned_threads",
            Self::UnresolvedThreads => "unresolved_threads",
            Self::BuriedEscalations => "buried_escalations",
            Self::MergeConflicting => "merge_conflicting",
            Self::ReviewerVerdictMissing => "reviewer_verdict_missing",
            Self::ReviewerVerdictStale => "reviewer_verdict_stale",
            Self::ReviewerStarved => "reviewer_starved",
            Self::ParentNeedsMergeForward => "parent_needs_merge_forward",
            Self::BaseChainMissingMain => "base_chain_missing_main",
            Self::ChildNeedsMergeForward => "child_needs_merge_forward",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReviewGateBlocker {
    code: ReviewGateBlockerCode,
    subjects: Vec<String>,
}

impl ReviewGateBlocker {
    fn new(code: ReviewGateBlockerCode, subjects: Vec<String>) -> Self {
        Self { code, subjects }
    }

    fn into_value(self) -> Value {
        json!({"code": self.code.as_str(), "subjects": self.subjects})
    }
}

/// Typed result of `review_gate_check`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewGateCheckResult {
    purpose: ReviewGatePurpose,
    head_revision: String,
    blockers: Vec<ReviewGateBlocker>,
}

impl ReviewGateCheckResult {
    /// Purely composes the three typed slog results into one protocol gate.
    pub fn compose(
        purpose: ReviewGatePurpose,
        convergence: &ConvergenceStateResult,
        stack: &StackStateResult,
        inventory: &ThreadInventoryResult,
    ) -> Self {
        let mut blockers = Vec::new();
        if convergence.evidence_truncated() || stack.evidence_truncated() || inventory.truncated() {
            blockers.push(ReviewGateBlocker::new(
                ReviewGateBlockerCode::EvidenceTruncated,
                Vec::new(),
            ));
        }
        if !convergence.ci_green() {
            blockers.push(ReviewGateBlocker::new(
                ReviewGateBlockerCode::CiNotGreen,
                convergence.failing_checks(),
            ));
        }
        let undispositioned = inventory.undispositioned_ids();
        if !undispositioned.is_empty() {
            blockers.push(ReviewGateBlocker::new(
                ReviewGateBlockerCode::UndispositionedThreads,
                undispositioned,
            ));
        }
        let unresolved = inventory.unresolved_ids();
        if !unresolved.is_empty() {
            blockers.push(ReviewGateBlocker::new(
                ReviewGateBlockerCode::UnresolvedThreads,
                unresolved,
            ));
        }
        let buried = convergence.buried_ids();
        if !buried.is_empty() {
            blockers.push(ReviewGateBlocker::new(
                ReviewGateBlockerCode::BuriedEscalations,
                buried,
            ));
        }
        if stack.needs_merge_forward() {
            blockers.push(ReviewGateBlocker::new(
                ReviewGateBlockerCode::ParentNeedsMergeForward,
                vec![stack.base_ref().to_owned()],
            ));
        }
        if stack.main_missing() {
            blockers.push(ReviewGateBlocker::new(
                ReviewGateBlockerCode::BaseChainMissingMain,
                vec![stack.default_ref().to_owned()],
            ));
        }
        if stack.child_needs_merge_forward() {
            blockers.push(ReviewGateBlocker::new(
                ReviewGateBlockerCode::ChildNeedsMergeForward,
                stack.child_numbers_needing_merge(),
            ));
        }
        if purpose == ReviewGatePurpose::DeclareConvergence {
            if convergence.merge_conflicting() {
                blockers.push(ReviewGateBlocker::new(
                    ReviewGateBlockerCode::MergeConflicting,
                    Vec::new(),
                ));
            }
            match convergence.reviewer().status() {
                ReviewerVerdictStatus::Missing => blockers.push(ReviewGateBlocker::new(
                    ReviewGateBlockerCode::ReviewerVerdictMissing,
                    Vec::new(),
                )),
                ReviewerVerdictStatus::StaleHead => blockers.push(ReviewGateBlocker::new(
                    ReviewGateBlockerCode::ReviewerVerdictStale,
                    Vec::new(),
                )),
                ReviewerVerdictStatus::CurrentHead => {}
            }
            if convergence.reviewer().starved() {
                blockers.push(ReviewGateBlocker::new(
                    ReviewGateBlockerCode::ReviewerStarved,
                    Vec::new(),
                ));
            }
        }
        Self {
            purpose,
            head_revision: convergence.head_revision().to_owned(),
            blockers,
        }
    }

    /// Reports whether no blocker was derived.
    pub fn ready(&self) -> bool {
        self.blockers.is_empty()
    }

    pub(super) fn into_value(self) -> Value {
        let purpose = match self.purpose {
            ReviewGatePurpose::RequestReviewWave => "request_review_wave",
            ReviewGatePurpose::DeclareConvergence => "declare_convergence",
        };
        let ready = self.blockers.is_empty();
        json!({
            "blockers": self.blockers.into_iter().map(ReviewGateBlocker::into_value).collect::<Vec<_>>(),
            "head_revision": self.head_revision,
            "purpose": purpose,
            "ready": ready,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::code_host::{
        ConvergenceStateFields, ReviewerVerdictEvidence, ReviewerVerdictFields, StackStateFields,
    };

    use super::*;

    const HEAD_REVISION: &str = "0123456789abcdef0123456789abcdef01234567";
    const BASE_REVISION: &str = "1111111111111111111111111111111111111111";
    const REVIEWED_AT: &str = "2026-07-27T10:00:00Z";
    const STARVED_AT: &str = "2026-07-27T11:00:00Z";

    fn current_reviewer() -> ReviewerVerdictEvidence {
        ReviewerVerdictEvidence::try_new(ReviewerVerdictFields {
            status: ReviewerVerdictStatus::CurrentHead,
            reviewed_revision: Some(String::from(HEAD_REVISION)),
            reviewed_at: Some(String::from(REVIEWED_AT)),
            starvation_after_verdict: false,
            latest_starvation_at: None,
            source_truncated: false,
            comments_previous_cursor: None,
            reviews_previous_cursor: None,
        })
        .expect("fixture reviewer evidence is admitted")
    }

    fn starved_reviewer() -> ReviewerVerdictEvidence {
        ReviewerVerdictEvidence::try_new(ReviewerVerdictFields {
            status: ReviewerVerdictStatus::CurrentHead,
            reviewed_revision: Some(String::from(HEAD_REVISION)),
            reviewed_at: Some(String::from(REVIEWED_AT)),
            starvation_after_verdict: true,
            latest_starvation_at: Some(String::from(STARVED_AT)),
            source_truncated: false,
            comments_previous_cursor: None,
            reviews_previous_cursor: None,
        })
        .expect("fixture reviewer evidence is admitted")
    }

    fn convergence(reviewer: ReviewerVerdictEvidence) -> ConvergenceStateResult {
        ConvergenceStateResult::try_new(ConvergenceStateFields {
            head_revision: String::from(HEAD_REVISION),
            mergeable_state: String::from("mergeable"),
            ci_rollup_state: Some(String::from("success")),
            checks: Vec::new(),
            checks_truncated: false,
            checks_next_cursor: None,
            unresolved_threads: Vec::new(),
            open_escalations: Vec::new(),
            buried_escalations: Vec::new(),
            threads_truncated: false,
            threads_next_cursor: None,
            reviewer,
        })
        .expect("fixture convergence evidence is admitted")
    }

    fn stack() -> StackStateResult {
        StackStateResult::try_new(StackStateFields {
            number: 17,
            base_ref: String::from("main"),
            base_revision: String::from(BASE_REVISION),
            head_ref: String::from("feature"),
            head_revision: String::from(HEAD_REVISION),
            default_ref: String::from("main"),
            default_revision: String::from(BASE_REVISION),
            base_commits_not_in_head: 0,
            main_commits_not_in_base: 0,
            children: Vec::new(),
            children_truncated: false,
            children_next_cursor: None,
        })
        .expect("fixture stack evidence is admitted")
    }

    fn inventory() -> ThreadInventoryResult {
        ThreadInventoryResult::try_new(Vec::new(), false, None)
            .expect("fixture inventory is admitted")
    }

    /// Complete green evidence with a current actual verdict opens the
    /// convergence gate.
    #[test]
    fn convergence_gate_is_ready_for_complete_green_evidence() {
        let convergence = convergence(current_reviewer());
        let stack = stack();
        let inventory = inventory();

        let gate = ReviewGateCheckResult::compose(
            ReviewGatePurpose::DeclareConvergence,
            &convergence,
            &stack,
            &inventory,
        );

        assert!(gate.ready());
    }

    /// A later usage-limit response blocks convergence even though an earlier
    /// actual verdict covered the same head.
    #[test]
    fn convergence_gate_blocks_starved_wave() {
        let convergence = convergence(starved_reviewer());
        let stack = stack();
        let inventory = inventory();

        let gate = ReviewGateCheckResult::compose(
            ReviewGatePurpose::DeclareConvergence,
            &convergence,
            &stack,
            &inventory,
        );

        assert_eq!(
            gate.into_value(),
            serde_json::json!({
                "blockers": [{"code": "reviewer_starved", "subjects": []}],
                "head_revision": HEAD_REVISION,
                "purpose": "declare_convergence",
                "ready": false,
            })
        );
    }
}
