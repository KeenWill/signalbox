//! Review run for `docs/spec/review-workflows.md`.

use super::{
    ReviewPass, ReviewPassKind, ReviewPassRef, ReviewPassResult, ReviewPassState, ReviewPolicy,
    ReviewRunRef, referenced_finding_result,
};

/// One workflow operation represented by a review run.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReviewWorkflowKind {
    /// Import current context from an external code host.
    ImportExternalContext,
    /// Produce findings without changing the reviewed workspace.
    ReadOnlyReview,
    /// Judge proposed findings.
    JudgeFindings,
    /// Deduplicate findings.
    DedupeFindings,
    /// Publish accepted findings externally.
    PublishReview,
    /// Attempt repairs for accepted findings.
    FixFindings,
    /// Propagate a merged stack edge.
    PropagateStack,
}

/// Evidence-bearing review-run state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewRunState {
    /// No pass is active yet.
    Queued,
    /// The named pass is active.
    Running {
        /// Exact active pass.
        active_pass: ReviewPassRef,
    },
    /// The named pass concluded the run successfully.
    Succeeded {
        /// Exact concluding pass.
        concluding_pass: ReviewPassRef,
    },
    /// The named pass concluded the run unsuccessfully.
    Failed {
        /// Exact failing pass.
        failed_pass: ReviewPassRef,
    },
    /// The named pass requires external resolution.
    Blocked {
        /// Exact blocking pass.
        blocking_pass: ReviewPassRef,
    },
    /// The run was cancelled, optionally after a recorded pass.
    Cancelled {
        /// Last pass known to the run, when one exists.
        last_pass: Option<ReviewPassRef>,
    },
}

impl ReviewRunState {
    fn pass(self) -> Option<ReviewPassRef> {
        match self {
            Self::Queued => None,
            Self::Running { active_pass } => Some(active_pass),
            Self::Succeeded { concluding_pass } => Some(concluding_pass),
            Self::Failed { failed_pass } => Some(failed_pass),
            Self::Blocked { blocking_pass } => Some(blocking_pass),
            Self::Cancelled { last_pass } => last_pass,
        }
    }
}

/// Canonical run facts independently loaded for child and finding claims.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewRunEvidence {
    reference: ReviewRunRef,
    pub(super) workflow: ReviewWorkflowKind,
    policy: ReviewPolicy,
    state: ReviewRunState,
}

impl ReviewRunEvidence {
    /// Supplies one independently stored run reference, workflow, policy, and
    /// lifecycle projection.
    pub const fn new(
        reference: ReviewRunRef,
        workflow: ReviewWorkflowKind,
        policy: ReviewPolicy,
        state: ReviewRunState,
    ) -> Self {
        Self {
            reference,
            workflow,
            policy,
            state,
        }
    }

    /// Returns the canonical run reference.
    pub const fn reference(self) -> ReviewRunRef {
        self.reference
    }

    /// Returns the canonical workflow.
    pub const fn workflow(self) -> ReviewWorkflowKind {
        self.workflow
    }

    /// Returns the complete frozen policy.
    pub const fn policy(self) -> ReviewPolicy {
        self.policy
    }

    /// Returns the canonical run lifecycle projection.
    pub const fn state(self) -> ReviewRunState {
        self.state
    }
}

/// Canonical pass facts independently loaded for review claims and projections.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewPassEvidence {
    reference: ReviewPassRef,
    kind: ReviewPassKind,
    policy: ReviewPolicy,
    state: ReviewPassState,
}

impl ReviewPassEvidence {
    pub(super) const fn new(
        reference: ReviewPassRef,
        kind: ReviewPassKind,
        policy: ReviewPolicy,
        state: ReviewPassState,
    ) -> Self {
        Self {
            reference,
            kind,
            policy,
            state,
        }
    }

    /// Derives authenticated evidence from one fully validated pass and its
    /// canonical run policy.
    pub fn from_pass(pass: &ReviewPass, policy: ReviewPolicy) -> Self {
        Self::new(pass.reference, pass.kind, policy, pass.state.clone())
    }

    /// Projects one exact effect result from already authenticated terminal
    /// pass evidence while preserving its identity, purpose, policy, turn, and
    /// terminal outcome.
    ///
    /// The effect aggregate remains responsible for validating that the
    /// proposed result belongs to that effect. Persistence may use this
    /// projection while atomically deciding which exact result first binds an
    /// otherwise result-free terminal pass.
    pub fn project_result(&self, result: ReviewPassResult) -> Option<Self> {
        if matches!(
            &result,
            ReviewPassResult::FindingEvent(event)
                if referenced_finding_result(event)
                    .is_some_and(|referenced| referenced.producer_policy() != self.policy)
        ) {
            return None;
        }
        let state = match &self.state {
            ReviewPassState::Succeeded {
                turn,
                output_frontier,
                result: None,
            } => ReviewPassState::Succeeded {
                turn: *turn,
                output_frontier: *output_frontier,
                result: Some(result),
            },
            ReviewPassState::Blocked { turn, result: None } => ReviewPassState::Blocked {
                turn: *turn,
                result: Some(result),
            },
            ReviewPassState::Succeeded {
                result: Some(current),
                ..
            }
            | ReviewPassState::Blocked {
                result: Some(current),
                ..
            } if current == &result => return Some(self.clone()),
            ReviewPassState::Queued
            | ReviewPassState::Running { .. }
            | ReviewPassState::Failed { .. }
            | ReviewPassState::Cancelled { .. }
            | ReviewPassState::Succeeded { .. }
            | ReviewPassState::Blocked { .. } => return None,
        };
        Some(Self::new(self.reference, self.kind, self.policy, state))
    }

    /// Returns the canonical pass reference.
    pub const fn reference(&self) -> ReviewPassRef {
        self.reference
    }

    /// Returns the canonical pass kind.
    pub const fn kind(&self) -> ReviewPassKind {
        self.kind
    }

    /// Returns the complete policy frozen by the pass's run.
    pub const fn policy(&self) -> ReviewPolicy {
        self.policy
    }

    /// Returns the canonical pass state.
    pub const fn state(&self) -> &ReviewPassState {
        &self.state
    }
}

/// Complete independently stored facts for review-run reconstitution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewRunReconstitutionInput {
    reference: ReviewRunRef,
    workflow: ReviewWorkflowKind,
    policy: ReviewPolicy,
    state: ReviewRunState,
    pass_evidence: Option<ReviewPassEvidence>,
}

impl ReviewRunReconstitutionInput {
    /// Supplies the run row and canonical referenced-pass projection.
    pub const fn new(
        reference: ReviewRunRef,
        workflow: ReviewWorkflowKind,
        policy: ReviewPolicy,
        state: ReviewRunState,
        pass_evidence: Option<ReviewPassEvidence>,
    ) -> Self {
        Self {
            reference,
            workflow,
            policy,
            state,
            pass_evidence,
        }
    }

    /// Returns the complete run reference.
    pub const fn reference(&self) -> ReviewRunRef {
        self.reference
    }

    /// Returns the workflow kind.
    pub const fn workflow(&self) -> ReviewWorkflowKind {
        self.workflow
    }

    /// Returns the frozen policy.
    pub const fn policy(&self) -> ReviewPolicy {
        self.policy
    }

    /// Returns the stored run state.
    pub const fn state(&self) -> ReviewRunState {
        self.state
    }

    /// Returns canonical pass evidence, when the run names a pass.
    pub const fn pass_evidence(&self) -> Option<&ReviewPassEvidence> {
        self.pass_evidence.as_ref()
    }
}

/// One review workflow execution against a frozen target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewRun {
    pub(super) reference: ReviewRunRef,
    pub(super) workflow: ReviewWorkflowKind,
    policy: ReviewPolicy,
    pub(super) state: ReviewRunState,
    pub(super) recorded_pass: Option<ReviewPassRef>,
}

impl ReviewRun {
    /// Constructs a queued review run.
    pub const fn new(
        reference: ReviewRunRef,
        workflow: ReviewWorkflowKind,
        policy: ReviewPolicy,
    ) -> Self {
        Self {
            reference,
            workflow,
            policy,
            state: ReviewRunState::Queued,
            recorded_pass: None,
        }
    }

    /// Reconstitutes a run after validating its canonical pass evidence.
    pub fn try_reconstitute(
        input: ReviewRunReconstitutionInput,
    ) -> Result<Self, ReviewRunReconstitutionError> {
        validate_run_state(
            input.reference,
            input.workflow,
            input.policy,
            input.state,
            input.pass_evidence.as_ref(),
        )
        .map_err(|failure| ReviewRunReconstitutionError {
            input: Box::new(input.clone()),
            failure,
        })?;
        Ok(Self {
            reference: input.reference,
            workflow: input.workflow,
            policy: input.policy,
            state: input.state,
            recorded_pass: input
                .pass_evidence
                .as_ref()
                .map(ReviewPassEvidence::reference),
        })
    }

    /// Applies one permitted state transition authenticated by canonical pass
    /// evidence.
    pub fn transition(
        mut self,
        next: ReviewRunState,
        pass_evidence: Option<ReviewPassEvidence>,
    ) -> Result<Self, ReviewRunTransitionError> {
        validate_run_state(
            self.reference,
            self.workflow,
            self.policy,
            next,
            pass_evidence.as_ref(),
        )
        .map_err(|failure| ReviewRunTransitionError {
            attempt: Box::new(ReviewRunTransitionAttempt {
                current: Box::new(self.clone()),
                next,
                pass_evidence: pass_evidence.clone(),
            }),
            failure: ReviewRunTransitionFailure::Evidence(failure),
        })?;
        let next_recorded_pass = pass_evidence.as_ref().map(ReviewPassEvidence::reference);
        if self.recorded_pass != next_recorded_pass {
            return Err(ReviewRunTransitionError {
                attempt: Box::new(ReviewRunTransitionAttempt {
                    current: Box::new(self.clone()),
                    next,
                    pass_evidence: pass_evidence.clone(),
                }),
                failure: ReviewRunTransitionFailure::Evidence(
                    ReviewRunEvidenceFailure::PassMismatch,
                ),
            });
        }
        let permitted = match (self.state, next) {
            (ReviewRunState::Queued, ReviewRunState::Running { .. }) => true,
            (ReviewRunState::Queued, ReviewRunState::Cancelled { last_pass: None }) => true,
            (ReviewRunState::Queued, ReviewRunState::Cancelled { last_pass: Some(_) }) => {
                pass_evidence.as_ref().is_some_and(|evidence| {
                    matches!(evidence.state(), ReviewPassState::Cancelled { turn: None })
                })
            }
            (
                ReviewRunState::Running {
                    active_pass: current,
                },
                ReviewRunState::Succeeded {
                    concluding_pass: next,
                }
                | ReviewRunState::Failed { failed_pass: next }
                | ReviewRunState::Blocked {
                    blocking_pass: next,
                },
            ) => current == next,
            (
                ReviewRunState::Running {
                    active_pass: current,
                },
                ReviewRunState::Cancelled {
                    last_pass: Some(next),
                },
            ) => {
                current == next
                    && pass_evidence.as_ref().is_some_and(|evidence| {
                        matches!(
                            evidence.state(),
                            ReviewPassState::Cancelled { turn: Some(_) }
                        )
                    })
            }
            _ => false,
        };
        if !permitted {
            return Err(ReviewRunTransitionError {
                attempt: Box::new(ReviewRunTransitionAttempt {
                    current: Box::new(self),
                    next,
                    pass_evidence,
                }),
                failure: ReviewRunTransitionFailure::InvalidTransition,
            });
        }
        self.state = next;
        self.recorded_pass = next_recorded_pass;
        Ok(self)
    }

    /// Returns the complete run reference.
    pub const fn reference(&self) -> ReviewRunRef {
        self.reference
    }

    /// Returns the workflow kind.
    pub const fn workflow(&self) -> ReviewWorkflowKind {
        self.workflow
    }

    /// Returns the frozen policy.
    pub const fn policy(&self) -> ReviewPolicy {
        self.policy
    }

    /// Returns the current state.
    pub const fn state(&self) -> ReviewRunState {
        self.state
    }

    /// Returns the pass already recorded for this run, when one exists.
    pub const fn recorded_pass(&self) -> Option<ReviewPassRef> {
        self.recorded_pass
    }

    /// Returns canonical identity, workflow, policy, and lifecycle evidence.
    pub const fn evidence(&self) -> ReviewRunEvidence {
        ReviewRunEvidence::new(self.reference, self.workflow, self.policy, self.state)
    }
}

fn validate_run_state(
    reference: ReviewRunRef,
    workflow: ReviewWorkflowKind,
    policy: ReviewPolicy,
    state: ReviewRunState,
    pass_evidence: Option<&ReviewPassEvidence>,
) -> Result<(), ReviewRunEvidenceFailure> {
    if let Some(pass) = state.pass()
        && pass.run() != reference
    {
        return Err(ReviewRunEvidenceFailure::ForeignPass);
    }
    if let Some(evidence) = pass_evidence {
        if evidence.reference.run() != reference {
            return Err(ReviewRunEvidenceFailure::ForeignPass);
        }
        if !workflow_matches_pass_kind(workflow, evidence.kind) {
            return Err(ReviewRunEvidenceFailure::PassKindMismatch);
        }
        if evidence.policy != policy {
            return Err(ReviewRunEvidenceFailure::PassPolicyMismatch);
        }
    }
    match state {
        ReviewRunState::Queued => match pass_evidence {
            None => Ok(()),
            Some(evidence) if evidence.state == ReviewPassState::Queued => Ok(()),
            Some(_) => Err(ReviewRunEvidenceFailure::PassStateMismatch),
        },
        ReviewRunState::Cancelled { last_pass: None } => {
            if pass_evidence.is_some() {
                Err(ReviewRunEvidenceFailure::UnexpectedPassEvidence)
            } else {
                Ok(())
            }
        }
        _ => {
            let Some(pass) = state.pass() else {
                return Err(ReviewRunEvidenceFailure::MissingPassEvidence);
            };
            let Some(evidence) = pass_evidence else {
                return Err(ReviewRunEvidenceFailure::MissingPassEvidence);
            };
            if evidence.reference != pass {
                return Err(ReviewRunEvidenceFailure::PassMismatch);
            }
            if !run_state_matches_pass(state, &evidence.state) {
                return Err(ReviewRunEvidenceFailure::PassStateMismatch);
            }
            Ok(())
        }
    }
}

pub(super) fn workflow_matches_pass_kind(
    workflow: ReviewWorkflowKind,
    pass: ReviewPassKind,
) -> bool {
    matches!(
        (workflow, pass),
        (
            ReviewWorkflowKind::ImportExternalContext,
            ReviewPassKind::ImportExternalContext
        ) | (
            ReviewWorkflowKind::ReadOnlyReview,
            ReviewPassKind::ReadOnlyReview
        ) | (ReviewWorkflowKind::JudgeFindings, ReviewPassKind::Judge)
            | (ReviewWorkflowKind::DedupeFindings, ReviewPassKind::Dedupe)
            | (ReviewWorkflowKind::PublishReview, ReviewPassKind::Publish)
            | (ReviewWorkflowKind::FixFindings, ReviewPassKind::Fix)
            | (
                ReviewWorkflowKind::PropagateStack,
                ReviewPassKind::PropagateStack
            )
    )
}

fn run_state_matches_pass(run: ReviewRunState, pass: &ReviewPassState) -> bool {
    matches!(
        (run, pass),
        (
            ReviewRunState::Running { .. },
            ReviewPassState::Running { .. }
        ) | (
            ReviewRunState::Succeeded { .. },
            ReviewPassState::Succeeded { .. }
        ) | (
            ReviewRunState::Failed { .. },
            ReviewPassState::Failed { .. }
        ) | (
            ReviewRunState::Blocked { .. },
            ReviewPassState::Blocked { .. }
        ) | (
            ReviewRunState::Cancelled { last_pass: Some(_) },
            ReviewPassState::Cancelled { .. }
        )
    )
}

pub(super) fn run_evidence_matches_pass(run: ReviewRunEvidence, pass: &ReviewPassEvidence) -> bool {
    run.reference() == pass.reference().run()
        && run.state().pass() == Some(pass.reference())
        && workflow_matches_pass_kind(run.workflow(), pass.kind())
        && run.policy() == pass.policy()
        && run_state_matches_pass(run.state(), pass.state())
}

/// Why canonical pass facts cannot support a run projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewRunEvidenceFailure {
    /// The run state's pass belongs to another run.
    ForeignPass,
    /// The run names a pass but no canonical pass row was supplied.
    MissingPassEvidence,
    /// A canonical pass row was supplied for a run state with no pass.
    UnexpectedPassEvidence,
    /// The canonical pass identity differs from the run projection.
    PassMismatch,
    /// The canonical pass kind is incompatible with the run workflow.
    PassKindMismatch,
    /// The canonical pass evidence carries another policy than the run.
    PassPolicyMismatch,
    /// The canonical pass outcome differs from the run projection.
    PassStateMismatch,
}

/// Rejected run reconstitution retaining every independently stored fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewRunReconstitutionError {
    input: Box<ReviewRunReconstitutionInput>,
    failure: ReviewRunEvidenceFailure,
}

impl ReviewRunReconstitutionError {
    /// Returns why the stored facts were rejected.
    pub const fn failure(&self) -> ReviewRunEvidenceFailure {
        self.failure
    }

    /// Borrows the unchanged stored facts.
    pub const fn input(&self) -> &ReviewRunReconstitutionInput {
        &self.input
    }

    /// Returns the unchanged stored facts.
    pub fn into_input(self) -> ReviewRunReconstitutionInput {
        *self.input
    }
}

/// Why a review-run state cannot be admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewRunTransitionFailure {
    /// Canonical pass evidence does not support the requested projection.
    Evidence(ReviewRunEvidenceFailure),
    /// The requested lifecycle edge is not permitted.
    InvalidTransition,
}

/// Rejected run transition retaining both states.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewRunTransitionError {
    attempt: Box<ReviewRunTransitionAttempt>,
    failure: ReviewRunTransitionFailure,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReviewRunTransitionAttempt {
    current: Box<ReviewRun>,
    next: ReviewRunState,
    pass_evidence: Option<ReviewPassEvidence>,
}

impl ReviewRunTransitionError {
    /// Returns why the state was rejected.
    pub const fn failure(&self) -> ReviewRunTransitionFailure {
        self.failure
    }

    /// Returns the current and requested states.
    pub const fn states(&self) -> (ReviewRunState, ReviewRunState) {
        (self.attempt.current.state, self.attempt.next)
    }

    /// Returns the rejected canonical pass evidence.
    pub const fn pass_evidence(&self) -> Option<&ReviewPassEvidence> {
        self.attempt.pass_evidence.as_ref()
    }

    /// Borrows the unchanged run rejected by the transition.
    pub const fn current(&self) -> &ReviewRun {
        &self.attempt.current
    }

    /// Returns the unchanged run rejected by the transition.
    pub fn into_current(self) -> ReviewRun {
        *self.attempt.current
    }
}
