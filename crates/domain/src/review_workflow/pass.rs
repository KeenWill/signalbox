//! Review pass for `docs/spec/review-workflows.md`.

use super::{
    AcceptedInputId, ContextFrontierId, ReviewFindingEventResult, ReviewFindingEventResultKind,
    ReviewPassRef, ReviewPassResult, ReviewReferencedFindingEvidence, ReviewRun, ReviewRunEvidence,
    ReviewRunRef, ReviewRunState, ReviewWorkflowKind, SessionId, TurnId,
    referenced_finding_status_is_eligible, workflow_matches_pass_kind,
};

/// One pass purpose inside a review run.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReviewPassKind {
    /// Import external review context.
    ImportExternalContext,
    /// Produce read-only findings.
    ReadOnlyReview,
    /// Judge findings.
    Judge,
    /// Deduplicate findings.
    Dedupe,
    /// Publish findings externally.
    Publish,
    /// Attempt finding repairs.
    Fix,
    /// Propagate one stack edge.
    PropagateStack,
}

/// Evidence-bearing state of one session-backed review pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewPassState {
    /// The pass input is accepted but no turn is active yet.
    Queued,
    /// The named turn is active.
    Running {
        /// Exact pass turn.
        turn: TurnId,
    },
    /// The named turn succeeded at the exact output frontier.
    Succeeded {
        /// Exact pass turn.
        turn: TurnId,
        /// Exact terminal output frontier.
        output_frontier: ContextFrontierId,
        /// Exact typed effect produced by this pass, when applicable.
        result: Option<ReviewPassResult>,
    },
    /// The named turn failed.
    Failed {
        /// Exact pass turn.
        turn: TurnId,
    },
    /// The named turn requires external resolution.
    Blocked {
        /// Exact pass turn.
        turn: TurnId,
        /// Exact blocked effect produced by this pass, when applicable.
        result: Option<ReviewPassResult>,
    },
    /// The pass was cancelled before or during its turn.
    Cancelled {
        /// Exact pass turn when activation occurred.
        turn: Option<TurnId>,
    },
}

impl ReviewPassState {
    pub(super) fn turn(&self) -> Option<TurnId> {
        match self {
            Self::Queued | Self::Cancelled { turn: None } => None,
            Self::Running { turn }
            | Self::Succeeded { turn, .. }
            | Self::Failed { turn }
            | Self::Blocked { turn, .. }
            | Self::Cancelled { turn: Some(turn) } => Some(*turn),
        }
    }

    pub(super) fn result(&self) -> Option<&ReviewPassResult> {
        match self {
            Self::Succeeded { result, .. } | Self::Blocked { result, .. } => result.as_ref(),
            _ => None,
        }
    }
}

/// Canonical session-turn lifecycle outcome projected into a review pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewPassTurnOutcome {
    /// The turn is active.
    Active,
    /// The turn completed successfully.
    Completed,
    /// The turn ended in a provider refusal.
    Refused,
    /// The turn failed.
    Failed,
    /// The turn was cancelled with authenticated authority.
    Cancelled,
    /// The turn requires external reconciliation.
    ReconciliationRequired,
}

/// Canonical turn facts independently loaded for one review pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewPassTurnEvidence {
    turn: TurnId,
    session: SessionId,
    accepted_input: AcceptedInputId,
    outcome: ReviewPassTurnOutcome,
    terminal_frontier: Option<ContextFrontierId>,
}

impl ReviewPassTurnEvidence {
    /// Supplies the turn's independently stored ownership and terminal facts.
    pub const fn new(
        turn: TurnId,
        session: SessionId,
        accepted_input: AcceptedInputId,
        outcome: ReviewPassTurnOutcome,
        terminal_frontier: Option<ContextFrontierId>,
    ) -> Self {
        Self {
            turn,
            session,
            accepted_input,
            outcome,
            terminal_frontier,
        }
    }

    /// Returns the canonical turn identity.
    pub const fn turn(self) -> TurnId {
        self.turn
    }

    /// Returns the canonical turn session.
    pub const fn session(self) -> SessionId {
        self.session
    }

    /// Returns the canonical turn-origin input.
    pub const fn accepted_input(self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Returns the canonical turn lifecycle outcome.
    pub const fn outcome(self) -> ReviewPassTurnOutcome {
        self.outcome
    }

    /// Returns the canonical terminal frontier, when the turn is terminal.
    pub const fn terminal_frontier(self) -> Option<ContextFrontierId> {
        self.terminal_frontier
    }
}

/// Canonical accepted-input ownership and queued-origin evidence for one pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReviewPassAcceptedInputEvidence {
    accepted_input: AcceptedInputId,
    session: SessionId,
    origin_turn: Option<TurnId>,
}

impl ReviewPassAcceptedInputEvidence {
    /// Supplies the accepted input's canonical session and optional origin turn.
    pub const fn new(
        accepted_input: AcceptedInputId,
        session: SessionId,
        origin_turn: Option<TurnId>,
    ) -> Self {
        Self {
            accepted_input,
            session,
            origin_turn,
        }
    }

    /// Returns the canonical accepted-input identity.
    pub const fn accepted_input(self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Returns the canonical accepted-input session.
    pub const fn session(self) -> SessionId {
        self.session
    }

    /// Returns the canonical queued origin turn, when one exists.
    pub const fn origin_turn(self) -> Option<TurnId> {
        self.origin_turn
    }
}

/// Complete independently stored facts for review-pass reconstitution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewPassReconstitutionInput {
    reference: ReviewPassRef,
    kind: ReviewPassKind,
    workflow_run: ReviewRunRef,
    workflow: ReviewWorkflowKind,
    session: SessionId,
    accepted_input: AcceptedInputId,
    accepted_input_evidence: ReviewPassAcceptedInputEvidence,
    state: ReviewPassState,
    turn_evidence: Option<ReviewPassTurnEvidence>,
}

impl ReviewPassReconstitutionInput {
    /// Supplies the pass row plus canonical accepted-input and turn evidence.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        reference: ReviewPassRef,
        kind: ReviewPassKind,
        workflow_run: ReviewRunRef,
        workflow: ReviewWorkflowKind,
        session: SessionId,
        accepted_input: AcceptedInputId,
        accepted_input_evidence: ReviewPassAcceptedInputEvidence,
        state: ReviewPassState,
        turn_evidence: Option<ReviewPassTurnEvidence>,
    ) -> Self {
        Self {
            reference,
            kind,
            workflow_run,
            workflow,
            session,
            accepted_input,
            accepted_input_evidence,
            state,
            turn_evidence,
        }
    }

    /// Returns the complete pass reference.
    pub const fn reference(&self) -> ReviewPassRef {
        self.reference
    }

    /// Returns the pass kind.
    pub const fn kind(&self) -> ReviewPassKind {
        self.kind
    }

    /// Returns the run whose canonical row supplied the workflow.
    pub const fn workflow_run(&self) -> ReviewRunRef {
        self.workflow_run
    }

    /// Returns the canonical parent run workflow.
    pub const fn workflow(&self) -> ReviewWorkflowKind {
        self.workflow
    }

    /// Returns the session stored on the pass.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the accepted input stored on the pass.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Returns the canonical evidence loaded for the stored accepted input.
    pub const fn accepted_input_evidence(&self) -> ReviewPassAcceptedInputEvidence {
        self.accepted_input_evidence
    }

    /// Returns the stored pass state.
    pub const fn state(&self) -> &ReviewPassState {
        &self.state
    }

    /// Returns the canonical turn evidence, when the pass names a turn.
    pub const fn turn_evidence(&self) -> Option<ReviewPassTurnEvidence> {
        self.turn_evidence
    }
}

/// One session-backed pass inside a review run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewPass {
    pub(super) reference: ReviewPassRef,
    pub(super) kind: ReviewPassKind,
    pub(super) session: SessionId,
    pub(super) accepted_input: AcceptedInputId,
    pub(super) origin_turn: TurnId,
    pub(super) state: ReviewPassState,
}

impl ReviewPass {
    /// Constructs a queued pass after its orchestration input is accepted.
    pub fn try_new(
        reference: ReviewPassRef,
        kind: ReviewPassKind,
        run: &mut ReviewRun,
        session: SessionId,
        accepted_input: ReviewPassAcceptedInputEvidence,
    ) -> Result<Self, ReviewPassConstructionError> {
        let run_evidence = run.evidence();
        let failure = if reference.run() != run.reference {
            Some(ReviewPassConstructionFailure::ForeignRun)
        } else if !workflow_matches_pass_kind(run.workflow, kind) {
            Some(ReviewPassConstructionFailure::RunWorkflowMismatch)
        } else if run.state != ReviewRunState::Queued {
            Some(ReviewPassConstructionFailure::RunNotQueued)
        } else if run.recorded_pass.is_some() {
            Some(ReviewPassConstructionFailure::RunAlreadyHasPass)
        } else if session != accepted_input.session {
            Some(ReviewPassConstructionFailure::AcceptedInputSessionMismatch)
        } else if accepted_input.origin_turn.is_none() {
            Some(ReviewPassConstructionFailure::AcceptedInputHasNoOriginTurn)
        } else {
            None
        };
        if let Some(failure) = failure {
            return Err(ReviewPassConstructionError {
                reference,
                kind,
                run_evidence: Box::new(run_evidence),
                session,
                accepted_input: Box::new(accepted_input),
                failure,
            });
        }
        let Some(origin_turn) = accepted_input.origin_turn else {
            return Err(ReviewPassConstructionError {
                reference,
                kind,
                run_evidence: Box::new(run_evidence),
                session,
                accepted_input: Box::new(accepted_input),
                failure: ReviewPassConstructionFailure::AcceptedInputHasNoOriginTurn,
            });
        };
        run.recorded_pass = Some(reference);
        Ok(Self {
            reference,
            kind,
            session,
            accepted_input: accepted_input.accepted_input,
            origin_turn,
            state: ReviewPassState::Queued,
        })
    }

    /// Reconstitutes a pass after validating independently stored evidence.
    pub fn try_reconstitute(
        input: ReviewPassReconstitutionInput,
    ) -> Result<Self, ReviewPassReconstitutionError> {
        let failure = validate_pass_reconstitution(&input);
        if let Some(failure) = failure {
            return Err(ReviewPassReconstitutionError {
                input: Box::new(input),
                failure,
            });
        }
        let Some(origin_turn) = input.accepted_input_evidence.origin_turn else {
            return Err(ReviewPassReconstitutionError {
                input: Box::new(input),
                failure: ReviewPassReconstitutionFailure::AcceptedInputHasNoOriginTurn,
            });
        };
        Ok(Self {
            reference: input.reference,
            kind: input.kind,
            session: input.session,
            accepted_input: input.accepted_input,
            origin_turn,
            state: input.state,
        })
    }

    /// Applies one permitted transition without changing the pass turn and only
    /// when canonical turn evidence supports the requested projection.
    pub fn transition(
        mut self,
        next: ReviewPassState,
        turn_evidence: Option<ReviewPassTurnEvidence>,
    ) -> Result<Self, ReviewPassTransitionError> {
        let same_turn = match (self.state.turn(), next.turn()) {
            (Some(current), Some(next)) => current == next,
            _ => true,
        };
        if !same_turn {
            return Err(ReviewPassTransitionError {
                attempt: Box::new(ReviewPassTransitionAttempt {
                    current: Box::new(self.clone()),
                    next,
                    turn_evidence,
                }),
                failure: ReviewPassTransitionFailure::TurnChanged,
            });
        }
        if let Some(failure) = validate_pass_turn_evidence(
            self.session,
            self.accepted_input,
            self.origin_turn,
            &next,
            turn_evidence,
        ) {
            return Err(ReviewPassTransitionError {
                attempt: Box::new(ReviewPassTransitionAttempt {
                    current: Box::new(self.clone()),
                    next: next.clone(),
                    turn_evidence,
                }),
                failure: ReviewPassTransitionFailure::Evidence(failure),
            });
        }
        if self.state == ReviewPassState::Queued
            && matches!(next, ReviewPassState::Running { .. })
            && turn_evidence
                .is_some_and(|evidence| evidence.outcome != ReviewPassTurnOutcome::Active)
        {
            return Err(ReviewPassTransitionError {
                attempt: Box::new(ReviewPassTransitionAttempt {
                    current: Box::new(self.clone()),
                    next: next.clone(),
                    turn_evidence,
                }),
                failure: ReviewPassTransitionFailure::TurnNotActive,
            });
        }
        let permitted = match (&self.state, &next) {
            (ReviewPassState::Queued, ReviewPassState::Running { .. }) => true,
            (ReviewPassState::Queued, ReviewPassState::Cancelled { turn: None }) => true,
            (
                ReviewPassState::Running { .. },
                ReviewPassState::Succeeded { .. }
                | ReviewPassState::Failed { .. }
                | ReviewPassState::Blocked { .. }
                | ReviewPassState::Cancelled { turn: Some(_) },
            ) => same_turn,
            _ => false,
        };
        if !permitted {
            return Err(ReviewPassTransitionError {
                attempt: Box::new(ReviewPassTransitionAttempt {
                    current: Box::new(self.clone()),
                    next: next.clone(),
                    turn_evidence,
                }),
                failure: ReviewPassTransitionFailure::InvalidTransition,
            });
        }
        if read_only_success_result_is_absent(self.kind, &next)
            || validate_pass_result(self.reference, self.kind, &next).is_some()
        {
            return Err(ReviewPassTransitionError {
                attempt: Box::new(ReviewPassTransitionAttempt {
                    current: Box::new(self.clone()),
                    next,
                    turn_evidence,
                }),
                failure: ReviewPassTransitionFailure::IncompatibleResult,
            });
        }
        self.state = next;
        Ok(self)
    }

    /// Monotonically binds one exact typed result to a terminal pass.
    pub fn bind_result(
        mut self,
        result: ReviewPassResult,
    ) -> Result<Self, ReviewPassTransitionError> {
        if self.state.result().is_some_and(|bound| bound == &result) {
            return Ok(self);
        }
        let next = match &self.state {
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
            state if state.result().is_some() => {
                return Err(ReviewPassTransitionError {
                    attempt: Box::new(ReviewPassTransitionAttempt {
                        current: Box::new(self.clone()),
                        next: self.state.clone(),
                        turn_evidence: None,
                    }),
                    failure: ReviewPassTransitionFailure::ResultAlreadyBound,
                });
            }
            _ => {
                return Err(ReviewPassTransitionError {
                    attempt: Box::new(ReviewPassTransitionAttempt {
                        current: Box::new(self.clone()),
                        next: self.state.clone(),
                        turn_evidence: None,
                    }),
                    failure: ReviewPassTransitionFailure::IncompatibleResult,
                });
            }
        };
        if validate_pass_result(self.reference, self.kind, &next).is_some() {
            return Err(ReviewPassTransitionError {
                attempt: Box::new(ReviewPassTransitionAttempt {
                    current: Box::new(self.clone()),
                    next,
                    turn_evidence: None,
                }),
                failure: ReviewPassTransitionFailure::IncompatibleResult,
            });
        }
        self.state = next;
        Ok(self)
    }

    /// Returns the complete pass reference.
    pub const fn reference(&self) -> ReviewPassRef {
        self.reference
    }

    /// Returns the pass kind.
    pub const fn kind(&self) -> ReviewPassKind {
        self.kind
    }

    /// Returns the exact execution session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the exact orchestration input.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Returns the accepted input's exact canonical origin turn.
    pub const fn origin_turn(&self) -> TurnId {
        self.origin_turn
    }

    /// Returns the current pass state.
    pub const fn state(&self) -> &ReviewPassState {
        &self.state
    }
}

fn validate_pass_reconstitution(
    input: &ReviewPassReconstitutionInput,
) -> Option<ReviewPassReconstitutionFailure> {
    if input.workflow_run != input.reference.run() {
        return Some(ReviewPassReconstitutionFailure::ForeignWorkflowRun);
    }
    if !workflow_matches_pass_kind(input.workflow, input.kind) {
        return Some(ReviewPassReconstitutionFailure::RunWorkflowMismatch);
    }
    if input.accepted_input != input.accepted_input_evidence.accepted_input {
        return Some(ReviewPassReconstitutionFailure::AcceptedInputEvidenceMismatch);
    }
    if input.session != input.accepted_input_evidence.session {
        return Some(ReviewPassReconstitutionFailure::AcceptedInputSessionMismatch);
    }
    let Some(origin_turn) = input.accepted_input_evidence.origin_turn else {
        return Some(ReviewPassReconstitutionFailure::AcceptedInputHasNoOriginTurn);
    };
    if let Some(failure) = validate_pass_result(input.reference, input.kind, &input.state) {
        return Some(failure);
    }
    if let Some(failure) = validate_pass_turn_evidence(
        input.session,
        input.accepted_input,
        origin_turn,
        &input.state,
        input.turn_evidence,
    ) {
        return Some(failure);
    }
    read_only_success_result_is_absent(input.kind, &input.state)
        .then_some(ReviewPassReconstitutionFailure::IncompatibleResult)
}

fn validate_pass_turn_evidence(
    session: SessionId,
    accepted_input: AcceptedInputId,
    origin_turn: TurnId,
    state: &ReviewPassState,
    turn_evidence: Option<ReviewPassTurnEvidence>,
) -> Option<ReviewPassReconstitutionFailure> {
    let Some(turn) = state.turn() else {
        return turn_evidence
            .is_some()
            .then_some(ReviewPassReconstitutionFailure::UnexpectedTurnEvidence);
    };
    let Some(evidence) = turn_evidence else {
        return Some(ReviewPassReconstitutionFailure::MissingTurnEvidence);
    };
    if evidence.turn != turn {
        return Some(ReviewPassReconstitutionFailure::TurnMismatch);
    }
    if evidence.turn != origin_turn {
        return Some(ReviewPassReconstitutionFailure::TurnOriginMismatch);
    }
    if evidence.session != session {
        return Some(ReviewPassReconstitutionFailure::TurnSessionMismatch);
    }
    if evidence.accepted_input != accepted_input {
        return Some(ReviewPassReconstitutionFailure::TurnAcceptedInputMismatch);
    }
    let frontier_matches_outcome = match evidence.outcome {
        ReviewPassTurnOutcome::Active => evidence.terminal_frontier.is_none(),
        _ => evidence.terminal_frontier.is_some(),
    };
    if !frontier_matches_outcome {
        return Some(ReviewPassReconstitutionFailure::TurnFrontierShapeMismatch);
    }
    if !pass_state_matches_turn_outcome(state, evidence.outcome) {
        return Some(ReviewPassReconstitutionFailure::TurnOutcomeMismatch);
    }
    if let ReviewPassState::Succeeded {
        output_frontier, ..
    } = state
        && evidence.terminal_frontier != Some(*output_frontier)
    {
        return Some(ReviewPassReconstitutionFailure::OutputFrontierMismatch);
    }
    None
}

fn pass_state_matches_turn_outcome(
    state: &ReviewPassState,
    outcome: ReviewPassTurnOutcome,
) -> bool {
    matches!(
        (state, outcome),
        (
            ReviewPassState::Running { .. },
            ReviewPassTurnOutcome::Active
                | ReviewPassTurnOutcome::Completed
                | ReviewPassTurnOutcome::Refused
                | ReviewPassTurnOutcome::Failed
                | ReviewPassTurnOutcome::Cancelled
                | ReviewPassTurnOutcome::ReconciliationRequired
        ) | (
            ReviewPassState::Succeeded { .. },
            ReviewPassTurnOutcome::Completed
        ) | (
            ReviewPassState::Failed { .. },
            ReviewPassTurnOutcome::Completed
                | ReviewPassTurnOutcome::Failed
                | ReviewPassTurnOutcome::Refused
        ) | (
            ReviewPassState::Blocked { .. },
            ReviewPassTurnOutcome::ReconciliationRequired
        ) | (
            ReviewPassState::Cancelled { turn: Some(_) },
            ReviewPassTurnOutcome::Cancelled
        )
    )
}
fn read_only_success_result_is_absent(kind: ReviewPassKind, state: &ReviewPassState) -> bool {
    kind == ReviewPassKind::ReadOnlyReview
        && matches!(state, ReviewPassState::Succeeded { result: None, .. })
}

fn validate_pass_result(
    reference: ReviewPassRef,
    kind: ReviewPassKind,
    state: &ReviewPassState,
) -> Option<ReviewPassReconstitutionFailure> {
    let Some(result) = state.result() else {
        return (kind == ReviewPassKind::Publish
            && matches!(state, ReviewPassState::Blocked { .. }))
        .then_some(ReviewPassReconstitutionFailure::IncompatibleResult);
    };
    let foreign_target = match result {
        ReviewPassResult::ProducedFindings(findings) => findings
            .findings()
            .iter()
            .any(|finding| finding.target() != reference.target()),
        ReviewPassResult::FindingEvent(event) => {
            event.finding().target() != reference.target()
                || referenced_finding_result(event)
                    .is_some_and(|referenced| referenced.reference().target() != reference.target())
        }
        ReviewPassResult::ExternalLinkAttachment(result) => result
            .finding_event()
            .is_some_and(|event| event.finding().target() != reference.target()),
        ReviewPassResult::ExternalLinkObservation(_)
        | ReviewPassResult::ExternalLinkNoChange(_)
        | ReviewPassResult::ExternalLinkPublicationBlocked(_) => false,
    };
    if foreign_target {
        return Some(ReviewPassReconstitutionFailure::ForeignResultTarget);
    }
    let compatible = match (state, result) {
        (ReviewPassState::Succeeded { .. }, ReviewPassResult::ProducedFindings(findings))
            if kind == ReviewPassKind::ReadOnlyReview =>
        {
            findings
                .findings()
                .iter()
                .all(|finding| finding.pass() == reference)
        }
        (ReviewPassState::Succeeded { .. }, ReviewPassResult::FindingEvent(event)) => {
            !matches!(event.kind(), ReviewFindingEventResultKind::Posted { .. })
                && event.finding().target() == reference.target()
                && finding_event_result_reference_is_compatible(event)
                && finding_event_result_matches_pass(
                    event.kind(),
                    kind,
                    ReviewPassTurnOutcome::Completed,
                )
        }
        (ReviewPassState::Blocked { .. }, ReviewPassResult::FindingEvent(event)) => {
            event.finding().target() == reference.target()
                && finding_event_result_reference_is_compatible(event)
                && finding_event_result_matches_pass(
                    event.kind(),
                    kind,
                    ReviewPassTurnOutcome::ReconciliationRequired,
                )
        }
        (ReviewPassState::Succeeded { .. }, ReviewPassResult::ExternalLinkAttachment(result)) => {
            matches!(
                kind,
                ReviewPassKind::Publish | ReviewPassKind::ImportExternalContext
            ) && result.finding_event().is_none_or(|event| {
                matches!(
                    event.kind(),
                    ReviewFindingEventResultKind::Posted { link }
                        if *link == result.link()
                ) && finding_event_result_matches_pass(
                    event.kind(),
                    kind,
                    ReviewPassTurnOutcome::Completed,
                )
            })
        }
        (ReviewPassState::Succeeded { .. }, ReviewPassResult::ExternalLinkObservation(_)) => {
            kind == ReviewPassKind::ImportExternalContext
        }
        (ReviewPassState::Succeeded { .. }, ReviewPassResult::ExternalLinkNoChange(_)) => {
            kind == ReviewPassKind::ImportExternalContext
        }
        (ReviewPassState::Blocked { .. }, ReviewPassResult::ExternalLinkPublicationBlocked(_)) => {
            kind == ReviewPassKind::Publish
        }
        _ => false,
    };
    (!compatible).then_some(ReviewPassReconstitutionFailure::IncompatibleResult)
}

pub(super) fn referenced_finding_result(
    event: &ReviewFindingEventResult,
) -> Option<ReviewReferencedFindingEvidence> {
    match event.kind() {
        ReviewFindingEventResultKind::Duplicate { canonical } => Some(*canonical),
        ReviewFindingEventResultKind::Superseded { successor } => Some(*successor),
        _ => None,
    }
}

fn finding_event_result_reference_is_compatible(event: &ReviewFindingEventResult) -> bool {
    referenced_finding_result(event).is_none_or(|referenced| {
        referenced.reference().finding() != event.finding().finding()
            && referenced.reference().target() == event.finding().target()
            && referenced_finding_status_is_eligible(referenced.status())
    })
}

pub(super) fn finding_event_result_matches_pass(
    event: &ReviewFindingEventResultKind,
    pass: ReviewPassKind,
    outcome: ReviewPassTurnOutcome,
) -> bool {
    matches!(
        (event, pass, outcome),
        (
            ReviewFindingEventResultKind::Accepted
                | ReviewFindingEventResultKind::Rejected { .. }
                | ReviewFindingEventResultKind::Stale,
            ReviewPassKind::Judge,
            ReviewPassTurnOutcome::Completed
        ) | (
            ReviewFindingEventResultKind::Duplicate { .. }
                | ReviewFindingEventResultKind::Superseded { .. },
            ReviewPassKind::Dedupe,
            ReviewPassTurnOutcome::Completed
        ) | (
            ReviewFindingEventResultKind::Posted { .. },
            ReviewPassKind::Publish | ReviewPassKind::ImportExternalContext,
            ReviewPassTurnOutcome::Completed
        ) | (
            ReviewFindingEventResultKind::Fixed,
            ReviewPassKind::Fix,
            ReviewPassTurnOutcome::Completed
        ) | (
            ReviewFindingEventResultKind::BlockedWithReason { link: Some(_), .. },
            ReviewPassKind::Publish,
            ReviewPassTurnOutcome::ReconciliationRequired
        ) | (
            ReviewFindingEventResultKind::BlockedWithReason { link: None, .. },
            ReviewPassKind::Fix,
            ReviewPassTurnOutcome::ReconciliationRequired
        )
    )
}

/// Why queued-pass construction was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewPassConstructionFailure {
    /// The supplied run evidence belongs to another run.
    ForeignRun,
    /// The pass kind is incompatible with its canonical run workflow.
    RunWorkflowMismatch,
    /// A pass can be admitted only while its run is queued.
    RunNotQueued,
    /// The run already records its one permitted pass.
    RunAlreadyHasPass,
    /// The accepted input belongs to another session.
    AcceptedInputSessionMismatch,
    /// The accepted input is not canonically classified as a queued turn origin.
    AcceptedInputHasNoOriginTurn,
}

/// Rejected queued-pass construction retaining all supplied facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewPassConstructionError {
    reference: ReviewPassRef,
    kind: ReviewPassKind,
    run_evidence: Box<ReviewRunEvidence>,
    session: SessionId,
    accepted_input: Box<ReviewPassAcceptedInputEvidence>,
    failure: ReviewPassConstructionFailure,
}

impl ReviewPassConstructionError {
    /// Returns the rejected pass reference.
    pub const fn reference(&self) -> ReviewPassRef {
        self.reference
    }

    /// Returns the rejected pass kind.
    pub const fn kind(&self) -> ReviewPassKind {
        self.kind
    }

    /// Returns the canonical parent workflow supplied for the pass.
    pub const fn workflow(&self) -> ReviewWorkflowKind {
        self.run_evidence.workflow
    }

    /// Returns the independently supplied canonical parent-run evidence.
    pub const fn run_evidence(&self) -> ReviewRunEvidence {
        *self.run_evidence
    }

    /// Returns the pass and canonical accepted-input sessions.
    pub const fn sessions(&self) -> (SessionId, SessionId) {
        (self.session, self.accepted_input.session)
    }

    /// Returns the accepted input whose association did not match.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        self.accepted_input.accepted_input
    }

    /// Returns the claimed canonical queued-origin turn, when one exists.
    pub const fn origin_turn(&self) -> Option<TurnId> {
        self.accepted_input.origin_turn
    }

    /// Returns why construction was rejected.
    pub const fn failure(&self) -> ReviewPassConstructionFailure {
        self.failure
    }
}

/// Why stored pass facts cannot reconstruct one canonical pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewPassReconstitutionFailure {
    /// The workflow discriminator was loaded from another run's row.
    ForeignWorkflowRun,
    /// The pass kind is incompatible with its canonical run workflow.
    RunWorkflowMismatch,
    /// The accepted-input identity stored on the pass differs from the
    /// supplied canonical accepted-input evidence.
    AcceptedInputEvidenceMismatch,
    /// The accepted input belongs to another session.
    AcceptedInputSessionMismatch,
    /// The accepted input is pending or consumed steering with no origin turn.
    AcceptedInputHasNoOriginTurn,
    /// The pass names a turn but no canonical turn row was supplied.
    MissingTurnEvidence,
    /// A turn row was supplied for a pass state with no turn.
    UnexpectedTurnEvidence,
    /// The canonical turn identity differs from the pass state.
    TurnMismatch,
    /// The pass turn differs from the accepted input's canonical origin turn.
    TurnOriginMismatch,
    /// The canonical turn belongs to another session.
    TurnSessionMismatch,
    /// The canonical turn was originated by another accepted input.
    TurnAcceptedInputMismatch,
    /// The canonical turn lifecycle outcome differs from the pass projection.
    TurnOutcomeMismatch,
    /// The canonical turn's terminal frontier contradicts its outcome: a
    /// terminal turn always carries one and an active turn never does.
    TurnFrontierShapeMismatch,
    /// The successful output is not the canonical terminal frontier.
    OutputFrontierMismatch,
    /// The typed pass result is incompatible with its pass kind, outcome, or
    /// ownership.
    IncompatibleResult,
    /// A pass result names a finding from another target.
    ForeignResultTarget,
}

/// Rejected pass reconstitution retaining every independently stored fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewPassReconstitutionError {
    input: Box<ReviewPassReconstitutionInput>,
    failure: ReviewPassReconstitutionFailure,
}

impl ReviewPassReconstitutionError {
    /// Returns why the stored facts were rejected.
    pub const fn failure(&self) -> ReviewPassReconstitutionFailure {
        self.failure
    }

    /// Borrows the unchanged stored facts.
    pub const fn input(&self) -> &ReviewPassReconstitutionInput {
        &self.input
    }

    /// Returns the unchanged stored facts.
    pub fn into_input(self) -> ReviewPassReconstitutionInput {
        *self.input
    }
}

/// Why a review-pass transition was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewPassTransitionFailure {
    /// Canonical turn evidence does not support the requested projection.
    Evidence(ReviewPassReconstitutionFailure),
    /// The requested lifecycle edge is not permitted.
    InvalidTransition,
    /// The transition names a different turn.
    TurnChanged,
    /// A queued pass starts only while its canonical turn is active; terminal
    /// lag is reserved for a pass that already projected the running start.
    TurnNotActive,
    /// A typed result does not match this pass kind, outcome, or ownership.
    IncompatibleResult,
    /// A distinct result was supplied after the pass result became immutable.
    ResultAlreadyBound,
}

/// Rejected pass transition retaining both states.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewPassTransitionError {
    attempt: Box<ReviewPassTransitionAttempt>,
    failure: ReviewPassTransitionFailure,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReviewPassTransitionAttempt {
    current: Box<ReviewPass>,
    next: ReviewPassState,
    turn_evidence: Option<ReviewPassTurnEvidence>,
}

impl ReviewPassTransitionError {
    /// Returns why the transition was rejected.
    pub const fn failure(&self) -> ReviewPassTransitionFailure {
        self.failure
    }

    /// Returns the current and requested states.
    pub fn states(&self) -> (ReviewPassState, ReviewPassState) {
        (
            self.attempt.current.state.clone(),
            self.attempt.next.clone(),
        )
    }

    /// Returns the rejected canonical turn evidence.
    pub const fn turn_evidence(&self) -> Option<ReviewPassTurnEvidence> {
        self.attempt.turn_evidence
    }

    /// Borrows the unchanged pass rejected by the transition.
    pub const fn current(&self) -> &ReviewPass {
        &self.attempt.current
    }

    /// Returns the unchanged pass rejected by the transition.
    pub fn into_current(self) -> ReviewPass {
        *self.attempt.current
    }
}
