//! Tool-batch stored inputs and reconstitution validation for `docs/spec/tool-loop.md`.

use super::{ToolBatch, ToolBatchPhase};
use crate::{
    ReconstitutedToolAttempt, ResolvedContextFrontierSnapshot, SessionId, ToolApprovalResolution,
    ToolAttemptEnd, ToolAttemptId, ToolEffectClass, ToolExecutionErrorKind, ToolRequest,
    ToolRequestId, TurnAttemptId, TurnId, tool::MAX_TOOL_REQUESTS_PER_RESPONSE,
    tool_attempt::RUNNER_ISSUANCE_AVAILABLE, tool_attempt::RUNNER_ISSUANCE_ISSUED,
};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;

/// Stored active phase for one complete logical tool batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolBatchPhaseReconstitutionInput {
    /// The turn parks on the exact earliest undecided request.
    AwaitingApproval {
        /// Stored approval-wait subject.
        request: ToolRequestId,
    },
    /// One turn attempt owns serialized execution and continuation.
    Executing {
        /// The current prepared/running turn attempt.
        turn_attempt: TurnAttemptId,
    },
    /// One exact external-effect attempt remains ambiguous.
    AwaitingRecovery {
        /// The terminal ambiguous physical attempt.
        attempt: ToolAttemptId,
    },
    /// One foreground delegation wait remains parked on its child result.
    AwaitingChild {
        /// The await request whose result is pending.
        request: ToolRequestId,
        /// The spawn request naming the child relationship.
        spawning_request: ToolRequestId,
        /// The exact child whose result is pending.
        child: SessionId,
    },
}

/// Complete stored facts for one producing call's logical tool batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolBatchReconstitutionInput {
    session: SessionId,
    turn: TurnId,
    producing_call: crate::ModelCallId,
    yielded_snapshot: ResolvedContextFrontierSnapshot,
    requests: Vec<ToolRequest>,
    approvals: Vec<ToolApprovalResolution>,
    attempts: Vec<ReconstitutedToolAttempt>,
    retired_attempts: Vec<ToolAttemptId>,
    runner_authorized_attempts: Vec<ToolAttemptId>,
    phase: ToolBatchPhaseReconstitutionInput,
}

impl ToolBatchReconstitutionInput {
    /// Supplies one complete request/decision/attempt inventory.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session: SessionId,
        turn: TurnId,
        producing_call: crate::ModelCallId,
        yielded_snapshot: ResolvedContextFrontierSnapshot,
        requests: Vec<ToolRequest>,
        approvals: Vec<ToolApprovalResolution>,
        attempts: Vec<ReconstitutedToolAttempt>,
        phase: ToolBatchPhaseReconstitutionInput,
    ) -> Self {
        Self {
            session,
            turn,
            producing_call,
            yielded_snapshot,
            requests,
            approvals,
            attempts,
            retired_attempts: Vec::new(),
            runner_authorized_attempts: Vec::new(),
            phase,
        }
    }

    /// Supplies the complete durable retired-attempt identity inventory.
    pub fn with_retired_attempts(mut self, retired_attempts: Vec<ToolAttemptId>) -> Self {
        self.retired_attempts = retired_attempts;
        self
    }

    /// Supplies the complete durable runner-authorized attempt inventory.
    pub fn with_runner_authorized_attempts(
        mut self,
        runner_authorized_attempts: Vec<ToolAttemptId>,
    ) -> Self {
        self.runner_authorized_attempts = runner_authorized_attempts;
        self
    }

    /// Reconstitutes the canonical batch or rejects the complete input.
    pub fn reconstitute(self) -> Result<ToolBatch, ToolBatchReconstitutionError> {
        reconstitute_batch(self)
    }
}

/// Why stored tool-batch facts cannot confer orchestration authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolBatchReconstitutionFailure {
    /// A producing call cannot yield an empty request batch.
    EmptyRequestBatch,
    /// A producing call cannot exceed the per-response request bound.
    TooManyRequests,
    /// A request belongs to a different session, turn, or producing call.
    RequestOwnershipMismatch,
    /// Request identity or ordinal is duplicated or noncontiguous.
    RequestOrderMismatch,
    /// The yielded snapshot belongs to a different session.
    YieldedSnapshotSessionMismatch,
    /// A decision is duplicated or names a request outside the batch.
    ApprovalInventoryMismatch,
    /// An attempt is duplicated or names a request outside the batch.
    AttemptInventoryMismatch,
    /// An attempt contradicts batch ownership or execution approval.
    AttemptAuthorizationMismatch,
    /// More than one physical attempt remains nonterminal.
    MultipleLiveAttempts,
    /// An attempt exists after an earlier approved request without one.
    AttemptOrderMismatch,
    /// The stored phase does not match the earliest undecided request.
    ApprovalPhaseMismatch,
    /// The stored execution phase does not match complete approval and attempt state.
    ExecutionPhaseMismatch,
    /// The stored recovery phase does not name the exact ambiguous attempt.
    RecoveryPhaseMismatch,
    /// The stored child-wait phase does not match its exact ended await attempt.
    ChildWaitPhaseMismatch,
}

/// Failed batch reconstitution retaining every stored fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolBatchReconstitutionError {
    input: Box<ToolBatchReconstitutionInput>,
    failure: ToolBatchReconstitutionFailure,
}

impl ToolBatchReconstitutionError {
    /// Borrows the complete unchanged input.
    pub const fn input(&self) -> &ToolBatchReconstitutionInput {
        &self.input
    }

    /// Returns the exact validation failure.
    pub const fn failure(&self) -> ToolBatchReconstitutionFailure {
        self.failure
    }

    /// Returns the complete input and failure.
    pub fn into_parts(self) -> (ToolBatchReconstitutionInput, ToolBatchReconstitutionFailure) {
        (*self.input, self.failure)
    }
}

fn reconstitute_batch(
    input: ToolBatchReconstitutionInput,
) -> Result<ToolBatch, ToolBatchReconstitutionError> {
    let fail = |input, failure| ToolBatchReconstitutionError {
        input: Box::new(input),
        failure,
    };
    if input.requests.is_empty() {
        return Err(fail(
            input,
            ToolBatchReconstitutionFailure::EmptyRequestBatch,
        ));
    }
    if input.requests.len() > MAX_TOOL_REQUESTS_PER_RESPONSE {
        return Err(fail(input, ToolBatchReconstitutionFailure::TooManyRequests));
    }
    if input.yielded_snapshot.frontier().owning_session() != input.session {
        return Err(fail(
            input,
            ToolBatchReconstitutionFailure::YieldedSnapshotSessionMismatch,
        ));
    }
    let mut requests = input.requests.clone();
    requests.sort_by_key(ToolRequest::ordinal);
    let mut request_ids = BTreeSet::new();
    for (index, request) in requests.iter().enumerate() {
        if request.session() != input.session
            || request.turn() != input.turn
            || request.producing_call() != input.producing_call
        {
            return Err(fail(
                input,
                ToolBatchReconstitutionFailure::RequestOwnershipMismatch,
            ));
        }
        if !request_ids.insert(request.id())
            || request.ordinal()
                != crate::ToolRequestOrdinal::try_from_usize(index).ok_or_else(|| {
                    fail(
                        input.clone(),
                        ToolBatchReconstitutionFailure::RequestOrderMismatch,
                    )
                })?
        {
            return Err(fail(
                input,
                ToolBatchReconstitutionFailure::RequestOrderMismatch,
            ));
        }
    }
    let mut approvals = BTreeMap::new();
    for approval in &input.approvals {
        if requests.iter().any(|request| {
            request.id() == approval.request() && request.inadmissible_reason().is_some()
        }) || !request_ids.contains(&approval.request())
            || approvals
                .insert(approval.request(), approval.clone())
                .is_some()
        {
            return Err(fail(
                input,
                ToolBatchReconstitutionFailure::ApprovalInventoryMismatch,
            ));
        }
    }
    if requests.iter().any(|request| {
        approvals.get(&request.id()).is_some_and(|approval| {
            approval.source() == crate::ToolDecisionSource::Delegate
                && request.approval_posture() != crate::ToolApprovalPosture::Delegated
        })
    }) {
        return Err(fail(
            input,
            ToolBatchReconstitutionFailure::ApprovalInventoryMismatch,
        ));
    }
    if let Some(first_undecided) = requests.iter().position(|request| {
        request.inadmissible_reason().is_none() && !approvals.contains_key(&request.id())
    }) && requests.iter().skip(first_undecided + 1).any(|request| {
        approvals
            .get(&request.id())
            .is_some_and(|approval| approval.source().requires_ordered_prefix())
    }) {
        return Err(fail(
            input,
            ToolBatchReconstitutionFailure::ApprovalInventoryMismatch,
        ));
    }
    let has_child_wait_attempt = input.attempts.iter().any(|attempt| {
        matches!(
            attempt,
            ReconstitutedToolAttempt::Ended(ended)
                if matches!(ended.end(), ToolAttemptEnd::AwaitingChild { .. })
        )
    });
    let expected_issuing_attempt = match input.phase {
        ToolBatchPhaseReconstitutionInput::Executing { turn_attempt }
            if !has_child_wait_attempt =>
        {
            Some(turn_attempt)
        }
        ToolBatchPhaseReconstitutionInput::Executing { .. }
        | ToolBatchPhaseReconstitutionInput::AwaitingChild { .. } => None,
        ToolBatchPhaseReconstitutionInput::AwaitingRecovery { attempt } => input
            .attempts
            .iter()
            .find(|candidate| attempt_facts(candidate).0 == attempt)
            .map(|candidate| attempt_facts(candidate).4),
        ToolBatchPhaseReconstitutionInput::AwaitingApproval { .. } => None,
    };
    let mut attempts = BTreeMap::new();
    let mut attempt_ids = BTreeSet::new();
    let mut live_attempt_count = 0usize;
    for attempt in &input.attempts {
        let (attempt_id, request, session, turn, issuing_attempt, is_live) = attempt_facts(attempt);
        if matches!(
            attempt,
            ReconstitutedToolAttempt::Ended(ended)
                if ended.end() == &ToolAttemptEnd::Ambiguous
                    && ended.effect_class() != ToolEffectClass::ExternalEffect
        ) {
            return Err(fail(
                input,
                ToolBatchReconstitutionFailure::AttemptAuthorizationMismatch,
            ));
        }
        if !attempt_ids.insert(attempt_id)
            || !request_ids.contains(&request)
            || attempts.insert(request, attempt.clone()).is_some()
        {
            return Err(fail(
                input,
                ToolBatchReconstitutionFailure::AttemptInventoryMismatch,
            ));
        }
        if session != input.session
            || turn != input.turn
            || !approvals
                .get(&request)
                .is_some_and(ToolApprovalResolution::is_approved)
        {
            return Err(fail(
                input,
                ToolBatchReconstitutionFailure::AttemptAuthorizationMismatch,
            ));
        }
        if is_live {
            live_attempt_count += 1;
        }
        if expected_issuing_attempt.is_some_and(|expected| issuing_attempt != expected) {
            return Err(fail(
                input,
                ToolBatchReconstitutionFailure::AttemptAuthorizationMismatch,
            ));
        }
    }
    let mut retired_attempts = BTreeSet::new();
    for retired in &input.retired_attempts {
        if attempt_ids.contains(retired) || !retired_attempts.insert(*retired) {
            return Err(fail(
                input,
                ToolBatchReconstitutionFailure::AttemptInventoryMismatch,
            ));
        }
    }
    let mut runner_authorized_attempts = BTreeSet::new();
    for authorized in &input.runner_authorized_attempts {
        if !(attempt_ids.contains(authorized) || retired_attempts.contains(authorized))
            || !runner_authorized_attempts.insert(*authorized)
        {
            return Err(fail(
                input,
                ToolBatchReconstitutionFailure::AttemptInventoryMismatch,
            ));
        }
    }
    if live_attempt_count > 1 {
        return Err(fail(
            input,
            ToolBatchReconstitutionFailure::MultipleLiveAttempts,
        ));
    }
    let mut missing_approved_attempt = false;
    let mut terminal_blocker_seen = false;
    let mut live_attempt_seen = false;
    for request in &requests {
        match approvals.get(&request.id()) {
            Some(approval) if approval.is_approved() => {
                if let Some(attempt) = attempts.get(&request.id()) {
                    if missing_approved_attempt || terminal_blocker_seen || live_attempt_seen {
                        return Err(fail(
                            input,
                            ToolBatchReconstitutionFailure::AttemptOrderMismatch,
                        ));
                    }
                    live_attempt_seen = matches!(attempt, ReconstitutedToolAttempt::Current(_));
                    terminal_blocker_seen = matches!(
                        attempt,
                        ReconstitutedToolAttempt::Ended(ended)
                            if ended.end() == &ToolAttemptEnd::Ambiguous
                                || (matches!(
                                    ended.end(),
                                    ToolAttemptEnd::AwaitingChild { .. }
                                ) && matches!(
                                    input.phase,
                                    ToolBatchPhaseReconstitutionInput::AwaitingChild { .. }
                                ))
                                || matches!(
                                    ended.end(),
                                    ToolAttemptEnd::KnownFailed { error }
                                        if error.kind() == ToolExecutionErrorKind::CrashLost
                                )
                    );
                } else {
                    missing_approved_attempt = true;
                }
            }
            Some(_) | None => {
                if attempts.contains_key(&request.id()) {
                    return Err(fail(
                        input,
                        ToolBatchReconstitutionFailure::AttemptAuthorizationMismatch,
                    ));
                }
            }
        }
    }
    let earliest_undecided = requests
        .iter()
        .find(|request| {
            request.inadmissible_reason().is_none() && !approvals.contains_key(&request.id())
        })
        .map(ToolRequest::id);
    let ambiguous_attempts = attempts
        .values()
        .filter_map(|attempt| match attempt {
            ReconstitutedToolAttempt::Ended(ended)
                if ended.end() == &ToolAttemptEnd::Ambiguous
                    && ended.effect_class() == ToolEffectClass::ExternalEffect =>
            {
                Some(ended.attempt())
            }
            ReconstitutedToolAttempt::Current(_) | ReconstitutedToolAttempt::Ended(_) => None,
        })
        .collect::<Vec<_>>();
    let child_waits = attempts
        .values()
        .filter_map(|attempt| match attempt {
            ReconstitutedToolAttempt::Ended(ended) => match ended.end() {
                ToolAttemptEnd::AwaitingChild {
                    spawning_request,
                    child,
                } => Some((ended.request(), *spawning_request, *child)),
                ToolAttemptEnd::Completed { .. }
                | ToolAttemptEnd::KnownFailed { .. }
                | ToolAttemptEnd::Ambiguous => None,
            },
            ReconstitutedToolAttempt::Current(_) => None,
        })
        .collect::<Vec<_>>();
    let phase =
        match input.phase {
            ToolBatchPhaseReconstitutionInput::AwaitingApproval { request }
                if earliest_undecided == Some(request) && attempts.is_empty() =>
            {
                ToolBatchPhase::AwaitingApproval { request }
            }
            ToolBatchPhaseReconstitutionInput::AwaitingApproval { .. } => {
                return Err(fail(
                    input,
                    ToolBatchReconstitutionFailure::ApprovalPhaseMismatch,
                ));
            }
            ToolBatchPhaseReconstitutionInput::Executing { turn_attempt }
                if earliest_undecided.is_none()
                    && ambiguous_attempts.is_empty()
                    && child_waits.len() <= 1 =>
            {
                let child_wait_position = child_waits.first().and_then(|(request, _, _)| {
                    requests
                        .iter()
                        .position(|candidate| candidate.id() == *request)
                });
                if attempts.iter().any(|(request, attempt)| {
                    let (_, _, _, _, issuing_attempt, live) = attempt_facts(attempt);
                    let request_position = requests
                        .iter()
                        .position(|candidate| candidate.id() == *request);
                    live && issuing_attempt != turn_attempt
                        || child_wait_position.is_none() && issuing_attempt != turn_attempt
                        || child_wait_position.zip(request_position).is_some_and(
                            |(wait, request)| request > wait && issuing_attempt != turn_attempt,
                        )
                }) {
                    return Err(fail(
                        input,
                        ToolBatchReconstitutionFailure::ExecutionPhaseMismatch,
                    ));
                }
                ToolBatchPhase::Executing { turn_attempt }
            }
            ToolBatchPhaseReconstitutionInput::Executing { .. } => {
                return Err(fail(
                    input,
                    ToolBatchReconstitutionFailure::ExecutionPhaseMismatch,
                ));
            }
            ToolBatchPhaseReconstitutionInput::AwaitingRecovery { attempt }
                if earliest_undecided.is_none()
                    && live_attempt_count == 0
                    && ambiguous_attempts == [attempt] =>
            {
                ToolBatchPhase::AwaitingRecovery { attempt }
            }
            ToolBatchPhaseReconstitutionInput::AwaitingRecovery { .. } => {
                return Err(fail(
                    input,
                    ToolBatchReconstitutionFailure::RecoveryPhaseMismatch,
                ));
            }
            ToolBatchPhaseReconstitutionInput::AwaitingChild {
                request,
                spawning_request,
                child,
            } if earliest_undecided.is_none()
                && live_attempt_count == 0
                && ambiguous_attempts.is_empty()
                && child_waits == [(request, spawning_request, child)] =>
            {
                ToolBatchPhase::AwaitingChild {
                    request,
                    spawning_request,
                    child,
                }
            }
            ToolBatchPhaseReconstitutionInput::AwaitingChild { .. } => {
                return Err(fail(
                    input,
                    ToolBatchReconstitutionFailure::ChildWaitPhaseMismatch,
                ));
            }
        };
    let runner_issuance = attempt_ids
        .iter()
        .chain(&retired_attempts)
        .map(|attempt| {
            (
                *attempt,
                Arc::new(AtomicU8::new(
                    if runner_authorized_attempts.contains(attempt) {
                        RUNNER_ISSUANCE_ISSUED
                    } else {
                        RUNNER_ISSUANCE_AVAILABLE
                    },
                )),
            )
        })
        .collect();
    Ok(ToolBatch {
        session: input.session,
        turn: input.turn,
        producing_call: input.producing_call,
        yielded_snapshot: input.yielded_snapshot,
        requests: requests.into_boxed_slice(),
        approvals,
        attempts,
        retired_attempts,
        runner_issuance,
        phase,
    })
}

fn attempt_facts(
    attempt: &ReconstitutedToolAttempt,
) -> (
    ToolAttemptId,
    ToolRequestId,
    SessionId,
    TurnId,
    TurnAttemptId,
    bool,
) {
    match attempt {
        ReconstitutedToolAttempt::Current(current) => (
            current.attempt(),
            current.request(),
            current.session(),
            current.turn(),
            current.issuing_attempt(),
            true,
        ),
        ReconstitutedToolAttempt::Ended(ended) => (
            ended.attempt(),
            ended.request(),
            ended.session(),
            ended.turn(),
            ended.issuing_attempt(),
            false,
        ),
    }
}
