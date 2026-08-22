//! Pure convergence evaluation for daemon-owned pull-request reconciliation.

use signalbox_domain::{CommitSha, MergeableState};

/// Whether the provider reports a pull request as draft.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PullRequestDraftState {
    /// The pull request is ready for review.
    ReadyForReview,
    /// The pull request is a draft.
    Draft,
}

impl PullRequestDraftState {
    /// Constructs the named state from the provider flag.
    pub const fn from_provider_flag(draft: bool) -> Self {
        if draft {
            Self::Draft
        } else {
            Self::ReadyForReview
        }
    }

    /// Returns the provider-compatible draft flag.
    pub const fn is_draft(self) -> bool {
        match self {
            Self::ReadyForReview => false,
            Self::Draft => true,
        }
    }
}

/// Provider check source and its normalized state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PullRequestCheckState {
    /// A GitHub check run.
    CheckRun {
        /// Whether GitHub reports the run complete.
        completed: bool,
        /// Provider conclusion when the run has one.
        conclusion: Option<String>,
    },
    /// A legacy commit-status context.
    StatusContext {
        /// Provider state spelling.
        state: String,
    },
}

/// One check context from the current-head status rollup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequestCheck {
    name: String,
    state: PullRequestCheckState,
}

impl PullRequestCheck {
    /// Constructs one named provider check.
    pub fn new(name: String, state: PullRequestCheckState) -> Self {
        Self { name, state }
    }

    /// Returns the provider-defined check name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the provider check state.
    pub const fn state(&self) -> &PullRequestCheckState {
        &self.state
    }

    /// Whether this exact check is excluded from convergence gating.
    pub fn is_non_gating(&self) -> bool {
        self.name.ends_with("(report only)")
            || matches!(self.state, PullRequestCheckState::StatusContext { .. })
                && self.name.eq_ignore_ascii_case("CodeRabbit")
    }

    /// Whether the provider state satisfies the convergence predicate.
    pub fn is_green(&self) -> bool {
        match &self.state {
            PullRequestCheckState::CheckRun {
                completed: true,
                conclusion: Some(conclusion),
            } => matches!(conclusion.as_str(), "SUCCESS" | "NEUTRAL" | "SKIPPED"),
            PullRequestCheckState::StatusContext { state } => state == "SUCCESS",
            PullRequestCheckState::CheckRun { .. } => false,
        }
    }

    /// Returns the stable provider state rendered into commissioned context.
    pub fn observed_state(&self) -> &str {
        match &self.state {
            PullRequestCheckState::CheckRun {
                conclusion: Some(conclusion),
                ..
            } => conclusion,
            PullRequestCheckState::CheckRun {
                completed: true,
                conclusion: None,
            } => "COMPLETED",
            PullRequestCheckState::CheckRun {
                completed: false, ..
            } => "IN_PROGRESS",
            PullRequestCheckState::StatusContext { state } => state,
        }
    }
}

/// One complete live provider snapshot used for convergence evaluation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequestConvergenceFacts {
    head_sha: CommitSha,
    checked_head_sha: Option<CommitSha>,
    draft: PullRequestDraftState,
    unresolved_review_threads: u64,
    mergeable_state: MergeableState,
    checks: Box<[PullRequestCheck]>,
}

impl PullRequestConvergenceFacts {
    /// Constructs a snapshot whose fields were fetched together.
    pub fn new(
        head_sha: CommitSha,
        checked_head_sha: Option<CommitSha>,
        draft: PullRequestDraftState,
        unresolved_review_threads: u64,
        mergeable_state: MergeableState,
        checks: Vec<PullRequestCheck>,
    ) -> Self {
        Self {
            head_sha,
            checked_head_sha,
            draft,
            unresolved_review_threads,
            mergeable_state,
            checks: checks.into_boxed_slice(),
        }
    }

    /// Returns the current pull-request head.
    pub const fn head_sha(&self) -> &CommitSha {
        &self.head_sha
    }

    /// Returns the commit whose status rollup was evaluated, when present.
    pub const fn checked_head_sha(&self) -> Option<&CommitSha> {
        self.checked_head_sha.as_ref()
    }

    /// Reports the provider draft flag. Draft is context, not a blocker.
    pub const fn draft(&self) -> PullRequestDraftState {
        self.draft
    }

    /// Returns the unresolved review-thread count.
    pub const fn unresolved_review_threads(&self) -> u64 {
        self.unresolved_review_threads
    }

    /// Returns current provider mergeability against the base.
    pub const fn mergeable_state(&self) -> MergeableState {
        self.mergeable_state
    }

    /// Returns every status-rollup check, including non-gating checks.
    pub fn checks(&self) -> &[PullRequestCheck] {
        &self.checks
    }
}

/// Why one live pull-request snapshot is not converged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PullRequestConvergenceBlocker {
    /// At least one review thread remains unresolved.
    UnresolvedReviewThreads(u64),
    /// The status rollup belongs to another head, or no current-head rollup exists.
    ChecksNotForCurrentHead,
    /// One gating check is not green.
    CheckNotGreen {
        /// Provider-defined check name.
        name: String,
        /// Stable provider state.
        state: String,
    },
    /// GitHub reports a base conflict.
    BaseConflict,
    /// GitHub has not resolved mergeability yet.
    MergeabilityUnknown,
}

/// Result of evaluating one complete provider snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequestConvergence {
    blockers: Box<[PullRequestConvergenceBlocker]>,
}

impl PullRequestConvergence {
    /// Whether the snapshot satisfies every convergence leg.
    pub fn is_converged(&self) -> bool {
        self.blockers.is_empty()
    }

    /// Returns the complete ordered blocker set.
    pub fn blockers(&self) -> &[PullRequestConvergenceBlocker] {
        &self.blockers
    }
}

/// Evaluates the closed convergence predicate over one atomic provider snapshot.
pub fn evaluate_pull_request_convergence(
    facts: &PullRequestConvergenceFacts,
) -> PullRequestConvergence {
    let mut blockers = Vec::new();
    if facts.unresolved_review_threads > 0 {
        blockers.push(PullRequestConvergenceBlocker::UnresolvedReviewThreads(
            facts.unresolved_review_threads,
        ));
    }
    if facts.checked_head_sha.as_ref() != Some(&facts.head_sha) {
        blockers.push(PullRequestConvergenceBlocker::ChecksNotForCurrentHead);
    }
    blockers.extend(
        facts
            .checks
            .iter()
            .filter(|check| !check.is_non_gating() && !check.is_green())
            .map(|check| PullRequestConvergenceBlocker::CheckNotGreen {
                name: check.name().to_owned(),
                state: check.observed_state().to_owned(),
            }),
    );
    match facts.mergeable_state {
        MergeableState::Mergeable => {}
        MergeableState::Conflicting => blockers.push(PullRequestConvergenceBlocker::BaseConflict),
        MergeableState::Unknown => {
            blockers.push(PullRequestConvergenceBlocker::MergeabilityUnknown);
        }
    }
    PullRequestConvergence {
        blockers: blockers.into_boxed_slice(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(value: char) -> CommitSha {
        CommitSha::try_new(std::iter::repeat_n(value, 40).collect()).expect("fixture SHA is valid")
    }

    fn facts(
        checked_head_sha: Option<CommitSha>,
        unresolved: u64,
        mergeable: MergeableState,
        checks: Vec<PullRequestCheck>,
    ) -> PullRequestConvergenceFacts {
        PullRequestConvergenceFacts::new(
            sha('a'),
            checked_head_sha,
            PullRequestDraftState::ReadyForReview,
            unresolved,
            mergeable,
            checks,
        )
    }

    #[test]
    fn resolved_green_current_head_is_converged() {
        let evaluation = evaluate_pull_request_convergence(&facts(
            Some(sha('a')),
            0,
            MergeableState::Mergeable,
            vec![PullRequestCheck::new(
                String::from("test"),
                PullRequestCheckState::CheckRun {
                    completed: true,
                    conclusion: Some(String::from("SUCCESS")),
                },
            )],
        ));
        assert!(evaluation.is_converged());
        assert_eq!(evaluation.blockers(), []);
    }

    #[test]
    fn every_predicate_leg_reports_its_blocker() {
        let evaluation = evaluate_pull_request_convergence(&facts(
            Some(sha('b')),
            3,
            MergeableState::Conflicting,
            vec![PullRequestCheck::new(
                String::from("test"),
                PullRequestCheckState::CheckRun {
                    completed: false,
                    conclusion: None,
                },
            )],
        ));
        assert_eq!(
            evaluation.blockers(),
            [
                PullRequestConvergenceBlocker::UnresolvedReviewThreads(3),
                PullRequestConvergenceBlocker::ChecksNotForCurrentHead,
                PullRequestConvergenceBlocker::CheckNotGreen {
                    name: String::from("test"),
                    state: String::from("IN_PROGRESS"),
                },
                PullRequestConvergenceBlocker::BaseConflict,
            ]
        );
    }

    #[test]
    fn exact_non_gating_names_do_not_block() {
        let evaluation = evaluate_pull_request_convergence(&facts(
            Some(sha('a')),
            0,
            MergeableState::Mergeable,
            vec![
                PullRequestCheck::new(
                    String::from("advisory (report only)"),
                    PullRequestCheckState::CheckRun {
                        completed: true,
                        conclusion: Some(String::from("FAILURE")),
                    },
                ),
                PullRequestCheck::new(
                    String::from("coderabbit"),
                    PullRequestCheckState::StatusContext {
                        state: String::from("FAILURE"),
                    },
                ),
            ],
        ));
        assert!(evaluation.is_converged());
    }

    #[test]
    fn coderabbit_check_run_remains_gating() {
        let name = String::from("CodeRabbit");
        let failure = String::from("FAILURE");
        let evaluation = evaluate_pull_request_convergence(&facts(
            Some(sha('a')),
            0,
            MergeableState::Mergeable,
            vec![PullRequestCheck::new(
                name.clone(),
                PullRequestCheckState::CheckRun {
                    completed: true,
                    conclusion: Some(failure.clone()),
                },
            )],
        ));
        assert_eq!(
            evaluation.blockers(),
            [PullRequestConvergenceBlocker::CheckNotGreen {
                name,
                state: failure,
            }]
        );
    }
}
