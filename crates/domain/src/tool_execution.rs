//! Evidence-bearing logical tool-batch transitions.
//!
//! `docs/spec/tool-loop.md` is normative. This aggregate validates one
//! producing call's complete request, approval, and attempt inventory before
//! it can expose an approval wait, prepare the next serialized physical
//! attempt, or project reference-only results.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
};

use crate::{
    ActiveTurnPhase, ApprovedToolRequest, AuthorizedToolAttempt, CurrentToolAttempt,
    CurrentToolAttemptState, DecideToolRequest, DecideToolRequestResult, DelegateToolApproval,
    EndedToolAttempt, PreparedDecideToolRequest, ReconstitutedToolAttempt,
    ResolvedContextFrontierSnapshot, RunnerToolAttemptAuthorization, SemanticTranscriptEntry,
    SemanticTranscriptEntryId, SemanticTranscriptEntryPayload, SessionId, ToolApprovalDecision,
    ToolApprovalResolution, ToolAttemptCrashOutcome, ToolAttemptEnd, ToolAttemptId,
    ToolDispatchAuthority, ToolEffectClass, ToolExecutionErrorKind, ToolRequest, ToolRequestId,
    TurnAttemptId, TurnId, tool::MAX_TOOL_REQUESTS_PER_RESPONSE,
    tool_attempt::RUNNER_ISSUANCE_AVAILABLE, tool_attempt::RUNNER_ISSUANCE_ISSUED,
    tool_attempt::RUNNER_ISSUANCE_RETIRED,
};

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

/// Canonical active phase derived from a complete tool-batch inventory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolBatchPhase {
    /// No physical attempt exists and one exact decision is next.
    AwaitingApproval {
        /// Earliest undecided request.
        request: ToolRequestId,
    },
    /// All decisions exist and one turn attempt owns serial execution.
    Executing {
        /// Current turn-attempt tenure.
        turn_attempt: TurnAttemptId,
    },
    /// Exact external-effect ambiguity blocks progress.
    AwaitingRecovery {
        /// Terminal ambiguous tool attempt.
        attempt: ToolAttemptId,
    },
    /// One exact foreground delegation result remains pending.
    AwaitingChild {
        /// Await request receiving the result.
        request: ToolRequestId,
        /// Spawn request naming the child relationship.
        spawning_request: ToolRequestId,
        /// Exact child whose result is pending.
        child: SessionId,
    },
}

/// One completely validated active logical tool batch.
#[derive(Clone, Debug)]
pub struct ToolBatch {
    session: SessionId,
    turn: TurnId,
    producing_call: crate::ModelCallId,
    yielded_snapshot: ResolvedContextFrontierSnapshot,
    requests: Box<[ToolRequest]>,
    approvals: BTreeMap<ToolRequestId, ToolApprovalResolution>,
    attempts: BTreeMap<ToolRequestId, ReconstitutedToolAttempt>,
    retired_attempts: BTreeSet<ToolAttemptId>,
    runner_issuance: BTreeMap<ToolAttemptId, Arc<AtomicU8>>,
    phase: ToolBatchPhase,
}

// Runner issuance state is durable identity; atomics are shared by in-memory clones.
impl PartialEq for ToolBatch {
    fn eq(&self, other: &Self) -> bool {
        self.session == other.session
            && self.turn == other.turn
            && self.producing_call == other.producing_call
            && self.yielded_snapshot == other.yielded_snapshot
            && self.requests == other.requests
            && self.approvals == other.approvals
            && self.attempts == other.attempts
            && self.retired_attempts == other.retired_attempts
            && self
                .runner_authorized_attempts()
                .eq(other.runner_authorized_attempts())
            && self.phase == other.phase
    }
}

impl Eq for ToolBatch {}

impl ToolBatch {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the continuing logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the definitive producing call.
    pub const fn producing_call(&self) -> crate::ModelCallId {
        self.producing_call
    }

    /// Borrows the yielded assistant-content snapshot.
    pub const fn yielded_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.yielded_snapshot
    }

    /// Returns requests in proposal order.
    pub fn requests(&self) -> &[ToolRequest] {
        &self.requests
    }

    /// Returns the decision for one request, if resolved.
    pub fn approval(&self, request: ToolRequestId) -> Option<&ToolApprovalResolution> {
        self.approvals.get(&request)
    }

    /// Returns the physical attempt for one request, if created.
    pub fn attempt(&self, request: ToolRequestId) -> Option<&ReconstitutedToolAttempt> {
        self.attempts.get(&request)
    }

    /// Returns every retired physical-attempt identity in stable order.
    pub fn retired_attempts(&self) -> impl Iterator<Item = ToolAttemptId> + '_ {
        self.retired_attempts.iter().copied()
    }

    /// Returns every physical attempt whose runner authority was durably issued.
    pub fn runner_authorized_attempts(&self) -> impl Iterator<Item = ToolAttemptId> + '_ {
        self.runner_issuance.iter().filter_map(|(attempt, issued)| {
            (issued.load(Ordering::Acquire) != RUNNER_ISSUANCE_AVAILABLE).then_some(*attempt)
        })
    }

    /// Returns the evidence-derived active phase.
    pub const fn phase(&self) -> ToolBatchPhase {
        self.phase
    }

    /// Produces opaque approval-wait evidence only from a matching batch.
    pub fn awaiting_approval(&self) -> Option<AwaitingToolApproval> {
        match self.phase {
            ToolBatchPhase::AwaitingApproval { request } => Some(AwaitingToolApproval {
                session: self.session,
                turn: self.turn,
                request,
            }),
            ToolBatchPhase::Executing { .. }
            | ToolBatchPhase::AwaitingRecovery { .. }
            | ToolBatchPhase::AwaitingChild { .. } => None,
        }
    }

    /// Produces opaque recovery-wait evidence only from a complete matching
    /// batch with one exact ambiguous physical attempt.
    pub fn awaiting_recovery(&self) -> Option<AwaitingToolRecovery> {
        match self.phase {
            ToolBatchPhase::AwaitingRecovery { attempt } => {
                self.attempts
                    .values()
                    .find_map(|candidate| match candidate {
                        ReconstitutedToolAttempt::Ended(ended)
                            if ended.attempt() == attempt
                                && ended.end() == &ToolAttemptEnd::Ambiguous =>
                        {
                            Some(AwaitingToolRecovery {
                                session: self.session,
                                turn: self.turn,
                                producing_call: self.producing_call,
                                yielded_frontier: self.yielded_snapshot.frontier().snapshot(),
                                issuing_attempt: ended.issuing_attempt(),
                                attempt,
                            })
                        }
                        ReconstitutedToolAttempt::Current(_)
                        | ReconstitutedToolAttempt::Ended(_) => None,
                    })
            }
            ToolBatchPhase::AwaitingApproval { .. }
            | ToolBatchPhase::Executing { .. }
            | ToolBatchPhase::AwaitingChild { .. } => None,
        }
    }

    /// Applies or authoritatively rejects one user decision against complete
    /// proposal-order state.
    pub fn prepare_user_decision(
        self,
        command: DecideToolRequest,
        continuation_attempt: Option<TurnAttemptId>,
    ) -> Result<PreparedToolBatchDecision, ToolBatchDecisionError> {
        self.prepare_decision(command, continuation_attempt, |command, request| {
            command.prepare_applied(request)
        })
    }

    /// Applies a committed lifecycle closure's denial to the exact parked
    /// request without attributing user agency.
    pub fn prepare_lifecycle_closure_denial(
        self,
        command: DecideToolRequest,
        continuation_attempt: Option<TurnAttemptId>,
    ) -> Result<PreparedToolBatchDecision, ToolBatchDecisionError> {
        self.prepare_decision(command, continuation_attempt, |command, request| {
            command.prepare_lifecycle_closure_applied(request)
        })
    }

    fn prepare_decision(
        self,
        command: DecideToolRequest,
        continuation_attempt: Option<TurnAttemptId>,
        prepare: impl FnOnce(
            DecideToolRequest,
            &crate::ToolRequest,
        ) -> Result<
            PreparedDecideToolRequest,
            crate::DecideToolRequestPreparationError,
        >,
    ) -> Result<PreparedToolBatchDecision, ToolBatchDecisionError> {
        let ToolBatchPhase::AwaitingApproval {
            request: waiting_on,
        } = self.phase
        else {
            return Err(ToolBatchDecisionError {
                batch: Box::new(self),
                command,
                failure: ToolBatchDecisionFailure::NoUndecidedRequest,
            });
        };
        let request = command.request();
        let Some(request_record) = self
            .requests
            .iter()
            .find(|candidate| candidate.id() == request)
        else {
            return Err(ToolBatchDecisionError {
                batch: Box::new(self),
                command,
                failure: ToolBatchDecisionFailure::CommandCorrelationMismatch,
            });
        };
        if self.approvals.contains_key(&request) {
            return Ok(PreparedToolBatchDecision::rejected(
                self,
                command.prepare_already_resolved(),
                waiting_on,
            ));
        }
        let earliest = self
            .requests
            .iter()
            .find(|candidate| !self.approvals.contains_key(&candidate.id()))
            .map(ToolRequest::id);
        if earliest != Some(request) {
            let earliest = earliest.ok_or(ToolBatchDecisionError {
                batch: Box::new(self.clone()),
                command: command.clone(),
                failure: ToolBatchDecisionFailure::NoUndecidedRequest,
            })?;
            return Ok(PreparedToolBatchDecision::rejected(
                self,
                command.prepare_not_earliest(earliest),
                waiting_on,
            ));
        }
        let prepared =
            prepare(command, request_record).map_err(|error| ToolBatchDecisionError {
                batch: Box::new(self.clone()),
                command: error.command().clone(),
                failure: ToolBatchDecisionFailure::CommandCorrelationMismatch,
            })?;
        let DecideToolRequestResult::Applied(applied) = prepared.result() else {
            return Err(ToolBatchDecisionError {
                batch: Box::new(self),
                command: prepared.command().clone(),
                failure: ToolBatchDecisionFailure::CommandCorrelationMismatch,
            });
        };
        let mut approvals = self.approvals.clone();
        approvals.insert(request, applied.resolution().clone());
        let next_undecided = self
            .requests
            .iter()
            .find(|candidate| !approvals.contains_key(&candidate.id()))
            .map(ToolRequest::id);
        let (phase, active_phase) = match (next_undecided, continuation_attempt) {
            (Some(next), None) => (
                ToolBatchPhase::AwaitingApproval { request: next },
                ActiveTurnPhase::AwaitingApproval { request: next },
            ),
            (None, Some(turn_attempt)) => (
                ToolBatchPhase::Executing { turn_attempt },
                ActiveTurnPhase::Running {
                    current_attempt: crate::CurrentTurnAttempt::prepared(turn_attempt),
                },
            ),
            _ => {
                return Err(ToolBatchDecisionError {
                    batch: Box::new(self),
                    command: prepared.command().clone(),
                    failure: ToolBatchDecisionFailure::ContinuationAttemptMismatch,
                });
            }
        };
        let batch = Self {
            approvals,
            phase,
            ..self
        };
        Ok(PreparedToolBatchDecision {
            batch,
            prepared_command: prepared,
            active_phase,
        })
    }

    /// Applies one authority-checked delegate result to the exact parked request.
    pub fn prepare_delegate_decision(
        self,
        approval: DelegateToolApproval,
        continuation_attempt: Option<TurnAttemptId>,
    ) -> Result<PreparedDelegateToolApproval, DelegateToolApprovalTransitionError> {
        let ToolBatchPhase::AwaitingApproval {
            request: waiting_on,
        } = self.phase
        else {
            return Err(DelegateToolApprovalTransitionError::new(
                self,
                approval,
                DelegateToolApprovalTransitionFailure::NoUndecidedRequest,
            ));
        };
        let Some(request) = self
            .requests
            .iter()
            .find(|request| request.id() == approval.request())
        else {
            return Err(DelegateToolApprovalTransitionError::new(
                self,
                approval,
                DelegateToolApprovalTransitionFailure::RequestMismatch,
            ));
        };
        if waiting_on != approval.request()
            || self.approvals.contains_key(&approval.request())
            || request.approval_posture() != approval.posture()
        {
            return Err(DelegateToolApprovalTransitionError::new(
                self,
                approval,
                DelegateToolApprovalTransitionFailure::RequestMismatch,
            ));
        }
        let resolution = match crate::ToolApprovalResolution::delegate(&approval) {
            Some(resolution) => resolution,
            None if continuation_attempt.is_none() => {
                return Ok(PreparedDelegateToolApproval {
                    batch: self,
                    approval,
                    resolution: None,
                    active_phase: ActiveTurnPhase::AwaitingApproval {
                        request: waiting_on,
                    },
                });
            }
            None => {
                return Err(DelegateToolApprovalTransitionError::new(
                    self,
                    approval,
                    DelegateToolApprovalTransitionFailure::ContinuationAttemptMismatch,
                ));
            }
        };
        let mut approvals = self.approvals.clone();
        approvals.insert(approval.request(), resolution.clone());
        let next_undecided = self
            .requests
            .iter()
            .find(|candidate| !approvals.contains_key(&candidate.id()))
            .map(ToolRequest::id);
        let (phase, active_phase) = match (next_undecided, continuation_attempt) {
            (Some(next), None) => (
                ToolBatchPhase::AwaitingApproval { request: next },
                ActiveTurnPhase::AwaitingApproval { request: next },
            ),
            (None, Some(turn_attempt)) => (
                ToolBatchPhase::Executing { turn_attempt },
                ActiveTurnPhase::Running {
                    current_attempt: crate::CurrentTurnAttempt::prepared(turn_attempt),
                },
            ),
            _ => {
                return Err(DelegateToolApprovalTransitionError::new(
                    self,
                    approval,
                    DelegateToolApprovalTransitionFailure::ContinuationAttemptMismatch,
                ));
            }
        };
        Ok(PreparedDelegateToolApproval {
            batch: Self {
                approvals,
                phase,
                ..self
            },
            approval,
            resolution: Some(resolution),
            active_phase,
        })
    }

    /// Prepares the earliest approved request without a physical attempt.
    pub fn prepare_next_attempt(
        &self,
        attempt: ToolAttemptId,
        effect_class: ToolEffectClass,
    ) -> Result<PreparedToolAttempt, ToolBatchExecutionError> {
        let ToolBatchPhase::Executing { turn_attempt } = self.phase else {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::NotExecuting,
            });
        };
        if self.attempts.values().any(|attempt| {
            matches!(
                attempt,
                ReconstitutedToolAttempt::Current(current)
                    if matches!(
                        current.state(),
                        CurrentToolAttemptState::Prepared | CurrentToolAttemptState::InFlight
                    )
            )
        }) {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::LiveAttemptPresent,
            });
        }
        if self.attempts.values().any(|attempt| {
            matches!(
                attempt,
                ReconstitutedToolAttempt::Ended(ended)
                    if matches!(
                        ended.end(),
                        ToolAttemptEnd::KnownFailed { error }
                            if error.kind() == ToolExecutionErrorKind::CrashLost
                    )
            )
        }) {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::TurnLevelFailure,
            });
        }
        if self.retired_attempts.contains(&attempt)
            || self.attempts.values().any(|candidate| {
                let candidate_id = match candidate {
                    ReconstitutedToolAttempt::Current(current) => current.attempt(),
                    ReconstitutedToolAttempt::Ended(ended) => ended.attempt(),
                };
                candidate_id == attempt
            })
        {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptIdentityReuse,
            });
        }
        let next = self.requests.iter().find(|request| {
            self.approvals
                .get(&request.id())
                .is_some_and(ToolApprovalResolution::is_approved)
                && !self.attempts.contains_key(&request.id())
        });
        let Some(request) = next else {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::ReadyForContinuation,
            });
        };
        let approval = self.approvals[&request.id()].clone();
        let approved = ApprovedToolRequest::try_from_resolution(request.clone(), approval)
            .map_err(|_| ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::ApprovalMismatch,
            })?;
        Ok(PreparedToolAttempt {
            attempt: approved.prepare_attempt(attempt, turn_attempt, effect_class),
        })
    }

    pub(crate) fn replace_claimed_attempt(
        mut self,
        claimed_attempt: ToolAttemptId,
        replacement_attempt: ToolAttemptId,
    ) -> Result<PreparedClaimedToolAttemptReplacement, ToolBatchExecutionError> {
        let ToolBatchPhase::Executing { turn_attempt } = self.phase else {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::NotExecuting,
            });
        };
        if self.retired_attempts.contains(&replacement_attempt)
            || self.attempts.values().any(|candidate| {
                let candidate_id = match candidate {
                    ReconstitutedToolAttempt::Current(current) => current.attempt(),
                    ReconstitutedToolAttempt::Ended(ended) => ended.attempt(),
                };
                candidate_id == replacement_attempt
            })
        {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptIdentityReuse,
            });
        }
        let Some((request, current)) =
            self.attempts
                .iter()
                .find_map(|(request, candidate)| match candidate {
                    ReconstitutedToolAttempt::Current(current)
                        if current.attempt() == claimed_attempt
                            && current.state() == CurrentToolAttemptState::InFlight =>
                    {
                        Some((*request, current.clone()))
                    }
                    ReconstitutedToolAttempt::Current(_) | ReconstitutedToolAttempt::Ended(_) => {
                        None
                    }
                })
        else {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptMissing,
            });
        };
        let request_record = self
            .requests
            .iter()
            .find(|candidate| candidate.id() == request)
            .cloned()
            .ok_or(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptMissing,
            })?;
        let approval = self
            .approvals
            .get(&request)
            .cloned()
            .ok_or(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::ApprovalMismatch,
            })?;
        let approved =
            ApprovedToolRequest::try_from_resolution(request_record, approval).map_err(|_| {
                ToolBatchExecutionError {
                    failure: ToolBatchExecutionFailure::ApprovalMismatch,
                }
            })?;
        let effect_class = current.effect_class();
        let retired = match current.classify_crash_loss() {
            ToolAttemptCrashOutcome::KnownFailed(retired)
            | ToolAttemptCrashOutcome::Ambiguous(retired) => retired,
        };
        let replacement_runner_issuance = Arc::new(AtomicU8::new(RUNNER_ISSUANCE_AVAILABLE));
        self.runner_issuance.insert(
            replacement_attempt,
            Arc::clone(&replacement_runner_issuance),
        );
        let authorized = approved
            .prepare_attempt(replacement_attempt, turn_attempt, effect_class)
            .authorize_with_runner_issuance(replacement_runner_issuance)
            .map_err(|_| ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptStageMismatch,
            })?;
        self.retired_attempts.insert(retired.attempt());
        self.attempts.insert(
            request,
            ReconstitutedToolAttempt::Current(authorized.attempt().clone()),
        );
        Ok(PreparedClaimedToolAttemptReplacement {
            batch: self,
            retired,
            approved,
            authorized,
        })
    }

    /// Authorizes one exact prepared attempt only through this freshly
    /// validated complete batch.
    pub fn authorize_attempt(
        &self,
        attempt: ToolAttemptId,
    ) -> Result<AuthorizedToolAttempt, ToolBatchExecutionError> {
        if !matches!(self.phase, ToolBatchPhase::Executing { .. }) {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::NotExecuting,
            });
        }
        let current = self
            .attempts
            .values()
            .find_map(|candidate| match candidate {
                ReconstitutedToolAttempt::Current(current) if current.attempt() == attempt => {
                    Some(current.clone())
                }
                ReconstitutedToolAttempt::Current(_) | ReconstitutedToolAttempt::Ended(_) => None,
            })
            .ok_or(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptMissing,
            })?;
        let runner_issuance =
            self.runner_issuance
                .get(&attempt)
                .map(Arc::clone)
                .ok_or(ToolBatchExecutionError {
                    failure: ToolBatchExecutionFailure::AttemptMissing,
                })?;
        current
            .authorize_with_runner_issuance(runner_issuance)
            .map_err(|_| ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptStageMismatch,
            })
    }

    /// Authorizes one exact prepared attempt and binds its canonical request.
    pub fn authorize_dispatch(
        &self,
        attempt: ToolAttemptId,
    ) -> Result<ToolDispatchAuthority, ToolBatchExecutionError> {
        let authorized = self.authorize_attempt(attempt)?;
        let request = self
            .requests
            .iter()
            .find(|request| request.id() == authorized.attempt().request())
            .cloned()
            .ok_or(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptMissing,
            })?;
        ToolDispatchAuthority::try_new(request, &authorized).ok_or(ToolBatchExecutionError {
            failure: ToolBatchExecutionFailure::AttemptStageMismatch,
        })
    }

    /// Restores in-flight authority after an ambiguous authorization
    /// acknowledgement only through this freshly validated complete batch.
    pub fn resume_in_flight_attempt(
        &self,
        attempt: ToolAttemptId,
    ) -> Result<AuthorizedToolAttempt, ToolBatchExecutionError> {
        if !matches!(self.phase, ToolBatchPhase::Executing { .. }) {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::NotExecuting,
            });
        }
        let current = self
            .attempts
            .values()
            .find_map(|candidate| match candidate {
                ReconstitutedToolAttempt::Current(current) if current.attempt() == attempt => {
                    Some(current.clone())
                }
                ReconstitutedToolAttempt::Current(_) | ReconstitutedToolAttempt::Ended(_) => None,
            })
            .ok_or(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptMissing,
            })?;
        let runner_issuance =
            self.runner_issuance
                .get(&attempt)
                .map(Arc::clone)
                .ok_or(ToolBatchExecutionError {
                    failure: ToolBatchExecutionFailure::AttemptMissing,
                })?;
        current
            .resume_in_flight_with_runner_issuance(runner_issuance)
            .map_err(|_| ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptStageMismatch,
            })
    }

    /// Restores one exact in-flight attempt with its canonical request.
    pub fn resume_in_flight_dispatch(
        &self,
        attempt: ToolAttemptId,
    ) -> Result<ToolDispatchAuthority, ToolBatchExecutionError> {
        let authorized = self.resume_in_flight_attempt(attempt)?;
        let request = self
            .requests
            .iter()
            .find(|request| request.id() == authorized.attempt().request())
            .cloned()
            .ok_or(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptMissing,
            })?;
        ToolDispatchAuthority::try_new(request, &authorized).ok_or(ToolBatchExecutionError {
            failure: ToolBatchExecutionFailure::AttemptStageMismatch,
        })
    }

    /// Authorizes one runner dispatch while pairing it with this batch's
    /// canonical immutable request and approval.
    pub fn authorize_runner_attempt(
        &self,
        attempt: ToolAttemptId,
    ) -> Result<RunnerToolAttemptAuthorization, ToolBatchExecutionError> {
        let authorized = self.authorize_attempt(attempt)?;
        self.bind_runner_authorization(authorized)
    }

    /// Restores one runner dispatch while pairing it with this batch's
    /// canonical immutable request and approval.
    pub fn resume_runner_attempt(
        &self,
        attempt: ToolAttemptId,
    ) -> Result<RunnerToolAttemptAuthorization, ToolBatchExecutionError> {
        let authorized = self.resume_in_flight_attempt(attempt)?;
        self.bind_runner_authorization(authorized)
    }

    pub(crate) fn reauthorize_unclaimed_runner_attempt(
        mut self,
        attempt: ToolAttemptId,
    ) -> Result<(Self, RunnerToolAttemptAuthorization), ToolBatchExecutionError> {
        if !matches!(self.phase, ToolBatchPhase::Executing { .. }) {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::NotExecuting,
            });
        }
        let current = self
            .attempts
            .values()
            .find_map(|candidate| match candidate {
                ReconstitutedToolAttempt::Current(current)
                    if current.attempt() == attempt
                        && current.state() == CurrentToolAttemptState::InFlight =>
                {
                    Some(current.clone())
                }
                ReconstitutedToolAttempt::Current(_) | ReconstitutedToolAttempt::Ended(_) => None,
            })
            .ok_or(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptMissing,
            })?;
        let prior_issuance = Arc::clone(self.runner_issuance.get(&attempt).ok_or(
            ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptMissing,
            },
        )?);
        if prior_issuance
            .compare_exchange(
                RUNNER_ISSUANCE_ISSUED,
                RUNNER_ISSUANCE_RETIRED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Err(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptStageMismatch,
            });
        }
        let successor_issuance = Arc::new(AtomicU8::new(RUNNER_ISSUANCE_AVAILABLE));
        self.runner_issuance
            .insert(attempt, Arc::clone(&successor_issuance));
        let authorized = current
            .resume_in_flight_with_runner_issuance(successor_issuance)
            .map_err(|_| ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptStageMismatch,
            })?;
        let authorization = self.bind_runner_authorization(authorized)?;
        Ok((self, authorization))
    }

    fn bind_runner_authorization(
        &self,
        authorized: AuthorizedToolAttempt,
    ) -> Result<RunnerToolAttemptAuthorization, ToolBatchExecutionError> {
        let request = self
            .requests
            .iter()
            .find(|request| request.id() == authorized.correlation().request())
            .cloned()
            .ok_or(ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptMissing,
            })?;
        let approval =
            self.approvals
                .get(&request.id())
                .cloned()
                .ok_or(ToolBatchExecutionError {
                    failure: ToolBatchExecutionFailure::ApprovalMismatch,
                })?;
        let approved =
            ApprovedToolRequest::try_from_resolution(request, approval).map_err(|_| {
                ToolBatchExecutionError {
                    failure: ToolBatchExecutionFailure::ApprovalMismatch,
                }
            })?;
        RunnerToolAttemptAuthorization::try_new(approved, authorized).map_err(|_| {
            ToolBatchExecutionError {
                failure: ToolBatchExecutionFailure::AttemptStageMismatch,
            }
        })
    }

    /// Builds one proposal-ordered reference-only result entry per request.
    pub fn prepare_result_projection(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        continuation_frontier: crate::ContextFrontierId,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError> {
        self.prepare_result_projection_with_delegation(entry_ids, continuation_frontier, None)
    }

    /// Builds proposal-ordered results including one delivered foreground child result.
    pub fn prepare_delegation_result_projection(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        continuation_frontier: crate::ContextFrontierId,
        outcome: crate::DelegationOutcome,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError> {
        self.prepare_result_projection_with_delegation(
            entry_ids,
            continuation_frontier,
            Some(outcome),
        )
    }

    fn prepare_result_projection_with_delegation(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        continuation_frontier: crate::ContextFrontierId,
        delegation_outcome: Option<crate::DelegationOutcome>,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError> {
        if !matches!(self.phase, ToolBatchPhase::Executing { .. })
            || entry_ids.len() != self.requests.len()
        {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::BatchNotResolved,
            });
        }
        let mut used = self
            .yielded_snapshot
            .ordered_entries()
            .map(|reference| reference.entry())
            .collect::<BTreeSet<_>>();
        if entry_ids.iter().any(|identity| !used.insert(*identity)) {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::EntryIdentityReuse,
            });
        }
        let child_wait_count = self
            .attempts
            .values()
            .filter(|attempt| {
                matches!(
                    attempt,
                    ReconstitutedToolAttempt::Ended(ended)
                        if matches!(ended.end(), ToolAttemptEnd::AwaitingChild { .. })
                )
            })
            .count();
        if delegation_outcome.is_some() != (child_wait_count == 1) {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::BatchNotResolved,
            });
        }
        let mut delegation_outcome = delegation_outcome.map(Box::new);
        let mut entries = Vec::with_capacity(self.requests.len());
        for (request, identity) in self.requests.iter().zip(entry_ids) {
            let payload = match self.approvals.get(&request.id()) {
                Some(resolution)
                    if matches!(resolution.decision(), ToolApprovalDecision::Deny { .. }) =>
                {
                    SemanticTranscriptEntryPayload::ToolDenied {
                        request: request.id(),
                    }
                }
                Some(resolution) if resolution.is_approved() => {
                    let Some(ReconstitutedToolAttempt::Ended(attempt)) =
                        self.attempts.get(&request.id())
                    else {
                        return Err(ToolResultProjectionError {
                            failure: ToolResultProjectionFailure::BatchNotResolved,
                        });
                    };
                    match attempt.end() {
                        ToolAttemptEnd::Completed { .. } => {}
                        ToolAttemptEnd::KnownFailed { error }
                            if error.kind() != ToolExecutionErrorKind::CrashLost => {}
                        ToolAttemptEnd::AwaitingChild {
                            spawning_request,
                            child,
                        } => {
                            let Some(outcome) = delegation_outcome.take() else {
                                return Err(ToolResultProjectionError {
                                    failure: ToolResultProjectionFailure::BatchNotResolved,
                                });
                            };
                            entries.push(SemanticTranscriptEntry::from_validated_parts(
                                identity,
                                self.session,
                                SemanticTranscriptEntryPayload::DelegationResult {
                                    awaiting_request: request.id(),
                                    spawning_request: *spawning_request,
                                    child: *child,
                                    mode: crate::DelegationWaitMode::Foreground,
                                    delivery_sequence: None,
                                    outcome,
                                },
                            ));
                            continue;
                        }
                        ToolAttemptEnd::KnownFailed { .. } | ToolAttemptEnd::Ambiguous => {
                            return Err(ToolResultProjectionError {
                                failure: ToolResultProjectionFailure::TurnLevelFailure,
                            });
                        }
                    }
                    SemanticTranscriptEntryPayload::ToolExecutionResult {
                        attempt: attempt.attempt(),
                    }
                }
                Some(_) | None => {
                    return Err(ToolResultProjectionError {
                        failure: ToolResultProjectionFailure::BatchNotResolved,
                    });
                }
            };
            entries.push(SemanticTranscriptEntry::from_validated_parts(
                identity,
                self.session,
                payload,
            ));
        }
        if delegation_outcome.is_some() {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::BatchNotResolved,
            });
        }
        let snapshot = self
            .yielded_snapshot
            .derive_appending_candidate(
                continuation_frontier,
                entries
                    .iter()
                    .map(SemanticTranscriptEntry::reference)
                    .collect(),
            )
            .map_err(|_| ToolResultProjectionError {
                failure: ToolResultProjectionFailure::FrontierDerivationFailed,
            })?;
        Ok(PreparedToolResultProjection {
            source_frontier: self.yielded_snapshot.frontier().snapshot(),
            turn: self.turn,
            producing_call: self.producing_call,
            entries: entries.into_boxed_slice(),
            snapshot,
        })
    }

    /// Builds the terminal result suffix for a batch blocked by one
    /// crash-lost physical attempt.
    ///
    /// This is the failure counterpart to cancellation projection: completed,
    /// known-failed, and denied requests retain their ordinary references,
    /// while every not-yet-attempted request closes without fabricated
    /// executor evidence.
    pub fn prepare_failure_projection(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        result_frontier: crate::ContextFrontierId,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError> {
        let crash_lost_count = self
            .attempts
            .values()
            .filter(|attempt| {
                matches!(
                    attempt,
                    ReconstitutedToolAttempt::Ended(ended)
                        if matches!(
                            ended.end(),
                            ToolAttemptEnd::KnownFailed { error }
                                if error.kind() == ToolExecutionErrorKind::CrashLost
                        )
                )
            })
            .count();
        if crash_lost_count != 1
            || self.attempts.values().any(|attempt| match attempt {
                ReconstitutedToolAttempt::Current(_) => true,
                ReconstitutedToolAttempt::Ended(ended) => ended.end() == &ToolAttemptEnd::Ambiguous,
            })
        {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::TurnLevelFailure,
            });
        }
        self.prepare_cancellation_projection(entry_ids, result_frontier)
    }

    /// Builds proposal-ordered results for an interrupt-cancelled executing
    /// batch after every physical attempt has reached a durable end.
    pub fn prepare_cancellation_projection(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        result_frontier: crate::ContextFrontierId,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError> {
        self.prepare_cancellation_projection_with_delegation(
            entry_ids,
            result_frontier,
            false,
            None,
        )
    }

    /// Builds interrupt closure for one foreground child wait, retaining a
    /// delivered result when descendant termination materialized one and
    /// otherwise closing the request with the parent turn.
    pub fn prepare_delegation_cancellation_projection(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        result_frontier: crate::ContextFrontierId,
        outcome: Option<crate::DelegationOutcome>,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError> {
        self.prepare_cancellation_projection_with_delegation(
            entry_ids,
            result_frontier,
            true,
            outcome,
        )
    }

    fn prepare_cancellation_projection_with_delegation(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        result_frontier: crate::ContextFrontierId,
        closes_delegation_wait: bool,
        delegation_outcome: Option<crate::DelegationOutcome>,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError> {
        let phase_can_close = matches!(self.phase, ToolBatchPhase::Executing { .. })
            || (closes_delegation_wait
                && matches!(self.phase, ToolBatchPhase::AwaitingChild { .. }));
        if !phase_can_close
            || entry_ids.len() != self.requests.len()
            || self
                .attempts
                .values()
                .any(|attempt| matches!(attempt, ReconstitutedToolAttempt::Current(_)))
        {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::BatchNotResolved,
            });
        }
        let mut used = self
            .yielded_snapshot
            .ordered_entries()
            .map(|reference| reference.entry())
            .collect::<BTreeSet<_>>();
        if entry_ids.iter().any(|identity| !used.insert(*identity)) {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::EntryIdentityReuse,
            });
        }
        let child_wait_count = self
            .attempts
            .values()
            .filter(|attempt| {
                matches!(
                    attempt,
                    ReconstitutedToolAttempt::Ended(ended)
                        if matches!(ended.end(), ToolAttemptEnd::AwaitingChild { .. })
                )
            })
            .count();
        if closes_delegation_wait != (child_wait_count == 1) {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::BatchNotResolved,
            });
        }
        let mut delegation_outcome = delegation_outcome.map(Box::new);
        let mut entries = Vec::with_capacity(self.requests.len());
        for (request, identity) in self.requests.iter().zip(entry_ids) {
            let payload = match self.approvals.get(&request.id()) {
                Some(resolution)
                    if matches!(resolution.decision(), ToolApprovalDecision::Deny { .. }) =>
                {
                    SemanticTranscriptEntryPayload::ToolDenied {
                        request: request.id(),
                    }
                }
                Some(resolution) if resolution.is_approved() => {
                    match self.attempts.get(&request.id()) {
                        Some(ReconstitutedToolAttempt::Ended(attempt))
                            if matches!(
                                attempt.end(),
                                ToolAttemptEnd::Completed { .. }
                                    | ToolAttemptEnd::KnownFailed { .. }
                            ) =>
                        {
                            SemanticTranscriptEntryPayload::ToolExecutionResult {
                                attempt: attempt.attempt(),
                            }
                        }
                        Some(ReconstitutedToolAttempt::Ended(attempt)) => {
                            let ToolAttemptEnd::AwaitingChild {
                                spawning_request,
                                child,
                            } = attempt.end()
                            else {
                                return Err(ToolResultProjectionError {
                                    failure: ToolResultProjectionFailure::TurnLevelFailure,
                                });
                            };
                            match delegation_outcome.take() {
                                Some(outcome) => SemanticTranscriptEntryPayload::DelegationResult {
                                    awaiting_request: request.id(),
                                    spawning_request: *spawning_request,
                                    child: *child,
                                    mode: crate::DelegationWaitMode::Foreground,
                                    delivery_sequence: None,
                                    outcome,
                                },
                                None => SemanticTranscriptEntryPayload::ToolClosed {
                                    request: request.id(),
                                },
                            }
                        }
                        Some(ReconstitutedToolAttempt::Current(_)) => {
                            return Err(ToolResultProjectionError {
                                failure: ToolResultProjectionFailure::BatchNotResolved,
                            });
                        }
                        None => SemanticTranscriptEntryPayload::ToolClosed {
                            request: request.id(),
                        },
                    }
                }
                Some(_) | None => SemanticTranscriptEntryPayload::ToolClosed {
                    request: request.id(),
                },
            };
            entries.push(SemanticTranscriptEntry::from_validated_parts(
                identity,
                self.session,
                payload,
            ));
        }
        if delegation_outcome.is_some() {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::BatchNotResolved,
            });
        }
        let snapshot = self
            .yielded_snapshot
            .derive_appending_candidate(
                result_frontier,
                entries
                    .iter()
                    .map(SemanticTranscriptEntry::reference)
                    .collect(),
            )
            .map_err(|_| ToolResultProjectionError {
                failure: ToolResultProjectionFailure::FrontierDerivationFailed,
            })?;
        Ok(PreparedToolResultProjection {
            source_frontier: self.yielded_snapshot.frontier().snapshot(),
            turn: self.turn,
            producing_call: self.producing_call,
            entries: entries.into_boxed_slice(),
            snapshot,
        })
    }

    /// Builds proposal-ordered logical closure for an interrupt-terminalized
    /// recovery batch while retaining its physical ambiguity separately.
    pub fn prepare_reconciliation_projection(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        terminal_frontier: crate::ContextFrontierId,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError> {
        if !matches!(self.phase, ToolBatchPhase::AwaitingRecovery { .. })
            || entry_ids.len() != self.requests.len()
            || self
                .attempts
                .values()
                .any(|attempt| matches!(attempt, ReconstitutedToolAttempt::Current(_)))
        {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::BatchNotResolved,
            });
        }
        let mut used = self
            .yielded_snapshot
            .ordered_entries()
            .map(|reference| reference.entry())
            .collect::<BTreeSet<_>>();
        if entry_ids.iter().any(|identity| !used.insert(*identity)) {
            return Err(ToolResultProjectionError {
                failure: ToolResultProjectionFailure::EntryIdentityReuse,
            });
        }
        let mut entries = Vec::with_capacity(self.requests.len());
        for (request, identity) in self.requests.iter().zip(entry_ids) {
            let payload = match self.approvals.get(&request.id()) {
                Some(resolution)
                    if matches!(resolution.decision(), ToolApprovalDecision::Deny { .. }) =>
                {
                    SemanticTranscriptEntryPayload::ToolDenied {
                        request: request.id(),
                    }
                }
                Some(resolution) if resolution.is_approved() => {
                    match self.attempts.get(&request.id()) {
                        Some(ReconstitutedToolAttempt::Ended(attempt))
                            if matches!(attempt.end(), ToolAttemptEnd::Completed { .. })
                                || matches!(
                                    attempt.end(),
                                    ToolAttemptEnd::KnownFailed { error }
                                        if error.kind() != ToolExecutionErrorKind::CrashLost
                                ) =>
                        {
                            SemanticTranscriptEntryPayload::ToolExecutionResult {
                                attempt: attempt.attempt(),
                            }
                        }
                        Some(ReconstitutedToolAttempt::Ended(_)) | None => {
                            SemanticTranscriptEntryPayload::ToolClosed {
                                request: request.id(),
                            }
                        }
                        Some(ReconstitutedToolAttempt::Current(_)) => {
                            return Err(ToolResultProjectionError {
                                failure: ToolResultProjectionFailure::BatchNotResolved,
                            });
                        }
                    }
                }
                Some(_) | None => SemanticTranscriptEntryPayload::ToolClosed {
                    request: request.id(),
                },
            };
            entries.push(SemanticTranscriptEntry::from_validated_parts(
                identity,
                self.session,
                payload,
            ));
        }
        let snapshot = self
            .yielded_snapshot
            .derive_appending_candidate(
                terminal_frontier,
                entries
                    .iter()
                    .map(SemanticTranscriptEntry::reference)
                    .collect(),
            )
            .map_err(|_| ToolResultProjectionError {
                failure: ToolResultProjectionFailure::FrontierDerivationFailed,
            })?;
        Ok(PreparedToolResultProjection {
            source_frontier: self.yielded_snapshot.frontier().snapshot(),
            turn: self.turn,
            producing_call: self.producing_call,
            entries: entries.into_boxed_slice(),
            snapshot,
        })
    }
}

/// Opaque evidence for one exact approval wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AwaitingToolApproval {
    session: SessionId,
    turn: TurnId,
    request: ToolRequestId,
}

impl AwaitingToolApproval {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the owning logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the exact earliest undecided request.
    pub const fn request(&self) -> ToolRequestId {
        self.request
    }
}

/// Opaque evidence for one exact tool-attempt recovery wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AwaitingToolRecovery {
    session: SessionId,
    turn: TurnId,
    producing_call: crate::ModelCallId,
    yielded_frontier: crate::ContextFrontierId,
    issuing_attempt: TurnAttemptId,
    attempt: ToolAttemptId,
}

impl AwaitingToolRecovery {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the owning logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the model call that produced the ambiguous tool batch.
    pub const fn producing_call(&self) -> crate::ModelCallId {
        self.producing_call
    }

    /// Returns the batch frontier retained while recovery is unresolved.
    pub const fn yielded_frontier(&self) -> crate::ContextFrontierId {
        self.yielded_frontier
    }

    /// Returns the turn attempt that authorized the ambiguous tool attempt.
    pub const fn issuing_attempt(&self) -> TurnAttemptId {
        self.issuing_attempt
    }

    /// Returns the exact ambiguous physical attempt.
    pub const fn attempt(&self) -> ToolAttemptId {
        self.attempt
    }
}

/// One approval-command candidate plus the exact successor active phase.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedToolBatchDecision {
    batch: ToolBatch,
    prepared_command: PreparedDecideToolRequest,
    active_phase: ActiveTurnPhase,
}

/// One checked delegate result plus its exact successor active phase.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedDelegateToolApproval {
    batch: ToolBatch,
    approval: DelegateToolApproval,
    resolution: Option<crate::ToolApprovalResolution>,
    active_phase: ActiveTurnPhase,
}

impl PreparedDelegateToolApproval {
    /// Borrows the updated or unchanged canonical batch.
    pub const fn batch(&self) -> &ToolBatch {
        &self.batch
    }

    /// Borrows the checked delegate result.
    pub const fn approval(&self) -> &DelegateToolApproval {
        &self.approval
    }

    /// Borrows the resulting approve-or-deny resolution, absent on escalation.
    pub const fn resolution(&self) -> Option<&crate::ToolApprovalResolution> {
        self.resolution.as_ref()
    }

    /// Borrows the exact active phase to store atomically.
    pub const fn active_phase(&self) -> &ActiveTurnPhase {
        &self.active_phase
    }
}

/// Why a checked delegate result could not advance this exact batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegateToolApprovalTransitionFailure {
    /// No request remains undecided.
    NoUndecidedRequest,
    /// The result does not name the exact earliest request and posture.
    RequestMismatch,
    /// The next phase and supplied continuation identity disagreed.
    ContinuationAttemptMismatch,
}

/// Failed delegate transition retaining every unchanged input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegateToolApprovalTransitionError {
    batch: Box<ToolBatch>,
    approval: DelegateToolApproval,
    failure: DelegateToolApprovalTransitionFailure,
}

impl DelegateToolApprovalTransitionError {
    fn new(
        batch: ToolBatch,
        approval: DelegateToolApproval,
        failure: DelegateToolApprovalTransitionFailure,
    ) -> Self {
        Self {
            batch: Box::new(batch),
            approval,
            failure,
        }
    }

    /// Borrows the unchanged batch.
    pub const fn batch(&self) -> &ToolBatch {
        &self.batch
    }

    /// Borrows the unchanged delegate result.
    pub const fn approval(&self) -> &DelegateToolApproval {
        &self.approval
    }

    /// Returns the exact failure.
    pub const fn failure(&self) -> DelegateToolApprovalTransitionFailure {
        self.failure
    }
}

impl std::fmt::Display for DelegateToolApprovalTransitionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "delegate approval transition failed: {:?}",
            self.failure
        )
    }
}

impl std::error::Error for DelegateToolApprovalTransitionError {}

impl PreparedToolBatchDecision {
    fn rejected(
        batch: ToolBatch,
        prepared_command: PreparedDecideToolRequest,
        waiting_on: ToolRequestId,
    ) -> Self {
        Self {
            batch,
            prepared_command,
            active_phase: ActiveTurnPhase::AwaitingApproval {
                request: waiting_on,
            },
        }
    }

    /// Borrows the updated or unchanged canonical batch.
    pub const fn batch(&self) -> &ToolBatch {
        &self.batch
    }

    /// Borrows the command and terminal result candidate.
    pub const fn prepared_command(&self) -> &PreparedDecideToolRequest {
        &self.prepared_command
    }

    /// Borrows the exact active phase to store atomically with the decision.
    pub const fn active_phase(&self) -> &ActiveTurnPhase {
        &self.active_phase
    }

    /// Returns every transaction value.
    pub fn into_parts(self) -> (ToolBatch, PreparedDecideToolRequest, ActiveTurnPhase) {
        (self.batch, self.prepared_command, self.active_phase)
    }
}

/// Why decision preparation found inconsistent adapter input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolBatchDecisionFailure {
    /// No request remains undecided.
    NoUndecidedRequest,
    /// The command and located request did not correlate.
    CommandCorrelationMismatch,
    /// The next phase and supplied continuation identity disagreed.
    ContinuationAttemptMismatch,
}

/// Nonterminal decision-preparation error retaining the batch and command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolBatchDecisionError {
    batch: Box<ToolBatch>,
    command: DecideToolRequest,
    failure: ToolBatchDecisionFailure,
}

impl ToolBatchDecisionError {
    /// Borrows the unchanged batch.
    pub const fn batch(&self) -> &ToolBatch {
        &self.batch
    }

    /// Borrows the unchanged command.
    pub const fn command(&self) -> &DecideToolRequest {
        &self.command
    }

    /// Returns the exact preparation failure.
    pub const fn failure(&self) -> ToolBatchDecisionFailure {
        self.failure
    }
}

/// One pre-commit first-generation physical-attempt candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedToolAttempt {
    attempt: CurrentToolAttempt,
}

impl PreparedToolAttempt {
    /// Borrows the prepared attempt.
    pub const fn attempt(&self) -> &CurrentToolAttempt {
        &self.attempt
    }

    /// Returns the prepared attempt.
    pub fn into_attempt(self) -> CurrentToolAttempt {
        self.attempt
    }
}

pub(crate) struct PreparedClaimedToolAttemptReplacement {
    pub(crate) batch: ToolBatch,
    pub(crate) retired: EndedToolAttempt,
    pub(crate) approved: ApprovedToolRequest,
    pub(crate) authorized: AuthorizedToolAttempt,
}

/// Why no next serialized attempt can be prepared.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolBatchExecutionFailure {
    /// The batch is parked on approval or recovery.
    NotExecuting,
    /// One attempt already remains prepared or in flight.
    LiveAttemptPresent,
    /// The requested current attempt is absent from the complete batch.
    AttemptMissing,
    /// The requested attempt is not in the required durable stage.
    AttemptStageMismatch,
    /// Every approved request has terminal attempt evidence.
    ReadyForContinuation,
    /// A prior crash-lost attempt requires turn-level failure.
    TurnLevelFailure,
    /// The proposed physical-attempt identity already belongs to the batch.
    AttemptIdentityReuse,
    /// Approval evidence did not authorize the selected request.
    ApprovalMismatch,
}

/// Rejected next-attempt preparation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolBatchExecutionError {
    failure: ToolBatchExecutionFailure,
}

impl ToolBatchExecutionError {
    /// Returns the exact preparation failure.
    pub const fn failure(&self) -> ToolBatchExecutionFailure {
        self.failure
    }
}

/// One proposal-ordered result projection and prefix-preserving snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedToolResultProjection {
    source_frontier: crate::ContextFrontierId,
    turn: TurnId,
    producing_call: crate::ModelCallId,
    entries: Box<[SemanticTranscriptEntry]>,
    snapshot: ResolvedContextFrontierSnapshot,
}

impl PreparedToolResultProjection {
    #[cfg(test)]
    pub(crate) fn from_validated_parts(
        source_frontier: crate::ContextFrontierId,
        turn: TurnId,
        producing_call: crate::ModelCallId,
        entries: Vec<SemanticTranscriptEntry>,
        snapshot: ResolvedContextFrontierSnapshot,
    ) -> Self {
        Self {
            source_frontier,
            turn,
            producing_call,
            entries: entries.into_boxed_slice(),
            snapshot,
        }
    }

    /// Returns the exact yielded frontier from which the results were derived.
    pub(crate) const fn source_frontier(&self) -> crate::ContextFrontierId {
        self.source_frontier
    }

    pub(crate) const fn turn(&self) -> TurnId {
        self.turn
    }

    pub(crate) const fn producing_call(&self) -> crate::ModelCallId {
        self.producing_call
    }

    /// Returns reference-only result entries in proposal order.
    pub fn entries(&self) -> &[SemanticTranscriptEntry] {
        &self.entries
    }

    /// Borrows the yielded-plus-results snapshot.
    pub const fn snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.snapshot
    }

    /// Returns both atomic projection values.
    pub fn into_parts(
        self,
    ) -> (
        Box<[SemanticTranscriptEntry]>,
        ResolvedContextFrontierSnapshot,
    ) {
        (self.entries, self.snapshot)
    }
}

/// Why result projection cannot yet form a continuation boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolResultProjectionFailure {
    /// At least one request lacks a continuation-safe logical resolution.
    BatchNotResolved,
    /// Crash or ambiguity requires turn-level failure/recovery instead.
    TurnLevelFailure,
    /// A fresh semantic-entry identity was not distinct.
    EntryIdentityReuse,
    /// The new snapshot could not preserve the yielded prefix.
    FrontierDerivationFailed,
}

/// Rejected continuation result projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolResultProjectionError {
    failure: ToolResultProjectionFailure,
}

impl ToolResultProjectionError {
    /// Returns the exact projection failure.
    pub const fn failure(&self) -> ToolResultProjectionFailure {
        self.failure
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
        if !request_ids.contains(&approval.request())
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
    if let Some(first_undecided) = requests
        .iter()
        .position(|request| !approvals.contains_key(&request.id()))
        && requests.iter().skip(first_undecided + 1).any(|request| {
            approvals
                .get(&request.id())
                .is_some_and(|approval| approval.source().requires_ordered_prefix())
        })
    {
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
        .find(|request| !approvals.contains_key(&request.id()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        DelegationContent, DelegationOutcome, DelegationOutcomeKind, DelegationOutcomeReason,
        DelegationProvenanceReconstitutionInput, DurableCommandId, NormalizedToolArguments,
        ToolApprovalResolutionReconstitutionInput, ToolArgumentsKind,
        ToolAttemptReconstitutionInput, ToolAttemptReconstitutionState, ToolDecisionSource,
        ToolDispatchGeneration, ToolName, ToolRequestOrdinal, ToolRequestReconstitutionInput,
        ToolResultContent, ToolResultText,
        test_support::{
            context_frontier_id, model_call_id, semantic_transcript_entry_id, session_id,
            tool_attempt_id, tool_request_id, turn_attempt_id, turn_id,
        },
    };

    fn request(id: u128, ordinal: u32) -> ToolRequest {
        ToolRequestReconstitutionInput::new(
            tool_request_id(id),
            session_id(1),
            turn_id(2),
            model_call_id(3),
            ToolRequestOrdinal::from_u32(ordinal),
            ToolName::try_new(format!("tool_{id}")).expect("fixture name is valid"),
            NormalizedToolArguments::try_from_stored(ToolArgumentsKind::Json, String::from("{}"))
                .expect("fixture arguments are canonical"),
        )
        .into_request()
    }

    fn approval(request: ToolRequestId, decision: ToolApprovalDecision) -> ToolApprovalResolution {
        ToolApprovalResolutionReconstitutionInput::user_fixture(request, decision)
            .reconstitute()
            .expect("user decisions are implemented")
    }

    fn automatic_approval(request: ToolRequestId) -> ToolApprovalResolution {
        ToolApprovalResolutionReconstitutionInput::policy_auto(request)
            .reconstitute()
            .expect("automatic approval is implemented")
    }

    fn yielded_snapshot() -> ResolvedContextFrontierSnapshot {
        ResolvedContextFrontierSnapshot::try_from_candidate(
            session_id(1),
            context_frontier_id(4),
            Vec::new(),
        )
        .expect("an empty fixture snapshot is valid")
    }

    fn awaiting_batch() -> ToolBatch {
        ToolBatchReconstitutionInput::new(
            session_id(1),
            turn_id(2),
            model_call_id(3),
            yielded_snapshot(),
            vec![request(10, 0), request(11, 1)],
            vec![],
            vec![],
            ToolBatchPhaseReconstitutionInput::AwaitingApproval {
                request: tool_request_id(10),
            },
        )
        .reconstitute()
        .expect("the first undecided request is exact")
    }

    /// S10: user decisions advance exactly one
    /// earliest wait and retain explicit user provenance.
    #[test]
    fn s10_user_decision_advances_to_next_wait() {
        let batch = awaiting_batch();
        let current_request = batch
            .requests()
            .first()
            .expect("the fixture has a current approval request")
            .id();
        let expected_next_request = batch
            .requests()
            .get(1)
            .expect("the fixture has one following approval request")
            .id();
        let command = DecideToolRequest::new(
            DurableCommandId::from_uuid(uuid::Uuid::from_u128(20)),
            current_request,
            ToolApprovalDecision::Approve,
        );
        let prepared = batch
            .prepare_user_decision(command, None)
            .expect("the earliest decision needs no continuation yet");
        let DecideToolRequestResult::Applied(applied) = prepared.prepared_command().result() else {
            panic!("the earliest exact decision applies");
        };

        assert_eq!(
            applied.resolution().source(),
            ToolDecisionSource::UserCommand
        );
        let ActiveTurnPhase::AwaitingApproval {
            request: next_request,
        } = prepared.active_phase()
        else {
            panic!("one decision advances to the next approval wait");
        };
        assert_eq!(*next_request, expected_next_request);
    }

    /// S10: durable approval history is exactly a proposal-order
    /// prefix and cannot skip the current wait.
    #[test]
    fn s10_reconstitution_rejects_nonprefix_approval_inventory() {
        let first = request(10, 0);
        let second = request(11, 1);
        let input = ToolBatchReconstitutionInput::new(
            session_id(1),
            turn_id(2),
            model_call_id(3),
            yielded_snapshot(),
            vec![first.clone(), second.clone()],
            vec![approval(second.id(), ToolApprovalDecision::Approve)],
            vec![],
            ToolBatchPhaseReconstitutionInput::AwaitingApproval {
                request: first.id(),
            },
        );

        let error = input
            .reconstitute()
            .expect_err("a later approval cannot bypass the earliest request");
        assert_eq!(
            error.failure(),
            ToolBatchReconstitutionFailure::ApprovalInventoryMismatch
        );
    }

    /// stored delegate evidence cannot be cross-wired to a request
    /// whose frozen posture reserves the decision for a human.
    #[test]
    fn reconstitution_rejects_delegate_resolution_for_human_request() {
        const SUBJECT_REQUEST_SEED: u128 = 10;
        const SUBJECT_SESSION_SEED: u128 = 1;
        const SUBJECT_TURN_SEED: u128 = 2;
        const ISSUING_CALL_SEED: u128 = 3;
        const SUBJECT_ORDINAL: u32 = 0;
        const JUDGE_MODEL_SEED: u128 = 11;
        const JUDGE_CALL_SEED: u128 = 12;
        const EXECUTION_ATTEMPT_SEED: u128 = 4;
        const SUBJECT_TOOL_NAME: &str = "tool_10";
        const SUBJECT_ARGUMENTS: &str = "{}";
        const JUDGE_RATIONALE: &str = "bounded request";

        let request_id = tool_request_id(SUBJECT_REQUEST_SEED);
        let session = session_id(SUBJECT_SESSION_SEED);
        let turn = turn_id(SUBJECT_TURN_SEED);
        let issuing_call = model_call_id(ISSUING_CALL_SEED);
        let ordinal = ToolRequestOrdinal::from_u32(SUBJECT_ORDINAL);
        let name =
            ToolName::try_new(String::from(SUBJECT_TOOL_NAME)).expect("fixture name is valid");
        let arguments = NormalizedToolArguments::try_from_stored(
            ToolArgumentsKind::Json,
            String::from(SUBJECT_ARGUMENTS),
        )
        .expect("fixture arguments are canonical");
        let request_with_posture = |posture| {
            ToolRequestReconstitutionInput::new(
                request_id,
                session,
                turn,
                issuing_call,
                ordinal,
                name.clone(),
                arguments.clone(),
            )
            .with_approval_posture(posture)
            .into_request()
        };
        let delegated_request = request_with_posture(crate::ToolApprovalPosture::Delegated);
        let stored_request = request_with_posture(crate::ToolApprovalPosture::Human);
        let rationale = crate::ToolDecisionRationale::try_new(String::from(JUDGE_RATIONALE))
            .expect("fixture rationale is admitted");
        let delegated = crate::DelegateToolApproval::try_new(
            &delegated_request,
            crate::DirectModelSelection::from_uuid(uuid::Uuid::from_u128(JUDGE_MODEL_SEED)),
            model_call_id(JUDGE_CALL_SEED),
            crate::DelegateApprovalRecommendation::Approve,
            rationale,
        )
        .expect("the delegated fixture permits approval");
        let resolution = ToolApprovalResolutionReconstitutionInput::delegate(delegated, None)
            .reconstitute()
            .expect("the delegate evidence is internally valid");
        let input = ToolBatchReconstitutionInput::new(
            session,
            turn,
            issuing_call,
            yielded_snapshot(),
            vec![stored_request],
            vec![resolution],
            vec![],
            ToolBatchPhaseReconstitutionInput::Executing {
                turn_attempt: turn_attempt_id(EXECUTION_ATTEMPT_SEED),
            },
        );

        let error = input
            .reconstitute()
            .expect_err("delegate evidence cannot widen human-only authority");

        assert_eq!(
            error.failure(),
            ToolBatchReconstitutionFailure::ApprovalInventoryMismatch
        );
    }

    /// S10: reconstitution enforces the same 32-request bound as
    /// provider-response admission instead of granting authority to oversized
    /// stored batches.
    #[test]
    fn s10_reconstitution_rejects_oversized_request_batch() {
        let requests = (0..33)
            .map(|ordinal| request(u128::from(ordinal) + 10, ordinal))
            .collect();
        let input = ToolBatchReconstitutionInput::new(
            session_id(1),
            turn_id(2),
            model_call_id(3),
            yielded_snapshot(),
            requests,
            vec![],
            vec![],
            ToolBatchPhaseReconstitutionInput::AwaitingApproval {
                request: tool_request_id(10),
            },
        );

        let error = input
            .reconstitute()
            .expect_err("stored batches above the response bound are rejected");
        assert_eq!(
            error.failure(),
            ToolBatchReconstitutionFailure::TooManyRequests
        );
    }

    /// S10: model-call completion may freeze automatic
    /// approval for a later request while an earlier confirmation still waits.
    #[test]
    fn s10_later_automatic_approval_survives_reconstitution() {
        let first = request(10, 0);
        let second = request(11, 1);
        let batch = ToolBatchReconstitutionInput::new(
            session_id(1),
            turn_id(2),
            model_call_id(3),
            yielded_snapshot(),
            vec![first.clone(), second.clone()],
            vec![automatic_approval(second.id())],
            vec![],
            ToolBatchPhaseReconstitutionInput::AwaitingApproval {
                request: first.id(),
            },
        )
        .reconstitute()
        .expect("later frozen policy authority does not bypass the earlier wait");

        assert_eq!(
            batch
                .approval(second.id())
                .map(ToolApprovalResolution::source),
            Some(ToolDecisionSource::PolicyAuto)
        );
    }

    /// S10: a user decision is admissible only at the exact
    /// durable approval wait and cannot manufacture a wait from execution.
    #[test]
    fn s10_user_decision_rejects_nonwaiting_batch_unchanged() {
        let only = request(10, 0);
        let batch = ToolBatchReconstitutionInput::new(
            session_id(1),
            turn_id(2),
            model_call_id(3),
            yielded_snapshot(),
            vec![only.clone()],
            vec![approval(only.id(), ToolApprovalDecision::Approve)],
            vec![],
            ToolBatchPhaseReconstitutionInput::Executing {
                turn_attempt: turn_attempt_id(12),
            },
        )
        .reconstitute()
        .expect("complete approval admits execution");
        let command = DecideToolRequest::new(
            DurableCommandId::from_uuid(uuid::Uuid::from_u128(20)),
            only.id(),
            ToolApprovalDecision::Deny { reason: None },
        );
        let error = batch
            .prepare_user_decision(command, None)
            .expect_err("execution is not an approval decision point");

        assert_eq!(
            error.failure(),
            ToolBatchDecisionFailure::NoUndecidedRequest
        );
        assert_eq!(
            error.batch().phase(),
            ToolBatchPhase::Executing {
                turn_attempt: turn_attempt_id(12)
            }
        );
    }

    /// S10: one active batch cannot turn an existing request from a
    /// different aggregate into a user-global not-found result.
    #[test]
    fn s10_out_of_batch_decision_is_a_correlation_error() {
        let command = DecideToolRequest::new(
            DurableCommandId::from_uuid(uuid::Uuid::from_u128(20)),
            tool_request_id(99),
            ToolApprovalDecision::Approve,
        );
        let error = awaiting_batch()
            .prepare_user_decision(command, None)
            .expect_err("batch-local absence cannot establish global absence");

        assert_eq!(
            error.failure(),
            ToolBatchDecisionFailure::CommandCorrelationMismatch
        );
        assert_eq!(error.command().request(), tool_request_id(99));
        assert_eq!(
            error.batch().phase(),
            ToolBatchPhase::AwaitingApproval {
                request: tool_request_id(10)
            }
        );
    }

    /// S10: serialized execution prepares only the first
    /// approved request without terminal attempt evidence.
    #[test]
    fn s10_execution_prepares_first_unattempted_request() {
        let first = request(10, 0);
        let second = request(11, 1);
        let batch = ToolBatchReconstitutionInput::new(
            session_id(1),
            turn_id(2),
            model_call_id(3),
            yielded_snapshot(),
            vec![first.clone(), second],
            vec![
                approval(first.id(), ToolApprovalDecision::Approve),
                approval(tool_request_id(11), ToolApprovalDecision::Approve),
            ],
            vec![],
            ToolBatchPhaseReconstitutionInput::Executing {
                turn_attempt: turn_attempt_id(12),
            },
        )
        .reconstitute()
        .expect("complete approvals admit execution");
        let prepared = batch
            .prepare_next_attempt(tool_attempt_id(13), ToolEffectClass::EffectFree)
            .expect("the first approved request is next");

        assert_eq!(prepared.attempt().request(), first.id());
        assert_eq!(
            prepared.attempt().state(),
            CurrentToolAttemptState::Prepared
        );
    }

    /// S06: only a completely reconstituted ambiguous
    /// batch can expose the exact tool recovery-wait subject.
    #[test]
    fn s06_ambiguous_batch_exposes_opaque_recovery_wait() {
        let only = request(10, 0);
        let attempt = ToolAttemptReconstitutionInput::new(
            tool_attempt_id(13),
            only.id(),
            session_id(1),
            turn_id(2),
            turn_attempt_id(12),
            ToolEffectClass::ExternalEffect,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Ambiguous),
        )
        .reconstitute()
        .expect("the first tool dispatch generation is supported");
        let batch = ToolBatchReconstitutionInput::new(
            session_id(1),
            turn_id(2),
            model_call_id(3),
            yielded_snapshot(),
            vec![only.clone()],
            vec![approval(only.id(), ToolApprovalDecision::Approve)],
            vec![attempt],
            ToolBatchPhaseReconstitutionInput::AwaitingRecovery {
                attempt: tool_attempt_id(13),
            },
        )
        .reconstitute()
        .expect("the exact ambiguous attempt admits recovery");
        let wait = batch
            .awaiting_recovery()
            .expect("a validated recovery batch exposes its opaque wait");

        assert_eq!(wait.session(), session_id(1));
        assert_eq!(wait.turn(), turn_id(2));
        assert_eq!(wait.issuing_attempt(), turn_attempt_id(12));
        assert_eq!(wait.attempt(), tool_attempt_id(13));
    }

    /// S06: impossible effect-free ambiguity cannot
    /// manufacture recovery-wait authority during checked reconstitution.
    #[test]
    fn s06_effect_free_ambiguous_history_fails_closed() {
        let only = request(10, 0);
        let attempt = ToolAttemptReconstitutionInput::new(
            tool_attempt_id(13),
            only.id(),
            session_id(1),
            turn_id(2),
            turn_attempt_id(12),
            ToolEffectClass::EffectFree,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Ambiguous),
        )
        .reconstitute()
        .expect("the first tool dispatch generation is supported");
        let error = ToolBatchReconstitutionInput::new(
            session_id(1),
            turn_id(2),
            model_call_id(3),
            yielded_snapshot(),
            vec![only.clone()],
            vec![approval(only.id(), ToolApprovalDecision::Approve)],
            vec![attempt],
            ToolBatchPhaseReconstitutionInput::AwaitingRecovery {
                attempt: tool_attempt_id(13),
            },
        )
        .reconstitute()
        .expect_err("effect-free ambiguity is not trusted recovery evidence");

        assert_eq!(
            error.failure(),
            ToolBatchReconstitutionFailure::AttemptAuthorizationMismatch
        );
    }

    /// S10: a live serialized attempt is the last
    /// attempt that can exist in proposal order.
    #[test]
    fn s10_reconstitution_rejects_attempt_after_live_attempt() {
        let first = request(10, 0);
        let second = request(11, 1);
        let current = ToolAttemptReconstitutionInput::new(
            tool_attempt_id(13),
            first.id(),
            session_id(1),
            turn_id(2),
            turn_attempt_id(12),
            ToolEffectClass::EffectFree,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Prepared,
        )
        .reconstitute()
        .expect("the first tool dispatch generation is supported");
        let later = ToolAttemptReconstitutionInput::new(
            tool_attempt_id(14),
            second.id(),
            session_id(1),
            turn_id(2),
            turn_attempt_id(12),
            ToolEffectClass::EffectFree,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::KnownFailed {
                error: crate::ToolExecutionError::new(
                    ToolExecutionErrorKind::ExecutionFailed,
                    None,
                ),
            }),
        )
        .reconstitute()
        .expect("the first tool dispatch generation is supported");
        let error = ToolBatchReconstitutionInput::new(
            session_id(1),
            turn_id(2),
            model_call_id(3),
            yielded_snapshot(),
            vec![first.clone(), second.clone()],
            vec![
                approval(first.id(), ToolApprovalDecision::Approve),
                approval(second.id(), ToolApprovalDecision::Approve),
            ],
            vec![current, later],
            ToolBatchPhaseReconstitutionInput::Executing {
                turn_attempt: turn_attempt_id(12),
            },
        )
        .reconstitute()
        .expect_err("serialized execution cannot create work after a live attempt");

        assert_eq!(
            error.failure(),
            ToolBatchReconstitutionFailure::AttemptOrderMismatch
        );
    }

    /// S06: recovery evidence belongs to one issuing
    /// continuation tenure throughout the complete batch.
    #[test]
    fn s06_recovery_rejects_mixed_issuing_attempts() {
        let first = request(10, 0);
        let second = request(11, 1);
        let completed = ToolAttemptReconstitutionInput::new(
            tool_attempt_id(13),
            first.id(),
            session_id(1),
            turn_id(2),
            turn_attempt_id(12),
            ToolEffectClass::EffectFree,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Completed {
                result: ToolResultContent::Text(
                    ToolResultText::try_new(String::from("ok")).expect("bounded result is valid"),
                ),
            }),
        )
        .reconstitute()
        .expect("the first tool dispatch generation is supported");
        let ambiguous = ToolAttemptReconstitutionInput::new(
            tool_attempt_id(14),
            second.id(),
            session_id(1),
            turn_id(2),
            turn_attempt_id(15),
            ToolEffectClass::ExternalEffect,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Ambiguous),
        )
        .reconstitute()
        .expect("the first tool dispatch generation is supported");
        let error = ToolBatchReconstitutionInput::new(
            session_id(1),
            turn_id(2),
            model_call_id(3),
            yielded_snapshot(),
            vec![first.clone(), second.clone()],
            vec![
                approval(first.id(), ToolApprovalDecision::Approve),
                approval(second.id(), ToolApprovalDecision::Approve),
            ],
            vec![completed, ambiguous],
            ToolBatchPhaseReconstitutionInput::AwaitingRecovery {
                attempt: tool_attempt_id(14),
            },
        )
        .reconstitute()
        .expect_err("one recovery batch cannot cross continuation tenures");

        assert_eq!(
            error.failure(),
            ToolBatchReconstitutionFailure::AttemptAuthorizationMismatch
        );
    }

    /// S05: crash-lost evidence is a turn-level blocker,
    /// so no later approved request can be prepared or already attempted.
    #[test]
    fn s05_crash_loss_stops_serial_batch_execution() {
        let first = request(10, 0);
        let second = request(11, 1);
        let crash_lost = ToolAttemptReconstitutionInput::new(
            tool_attempt_id(13),
            first.id(),
            session_id(1),
            turn_id(2),
            turn_attempt_id(12),
            ToolEffectClass::EffectFree,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::KnownFailed {
                error: crate::ToolExecutionError::new(ToolExecutionErrorKind::CrashLost, None),
            }),
        )
        .reconstitute()
        .expect("the first tool dispatch generation is supported");
        let approvals = vec![
            approval(first.id(), ToolApprovalDecision::Approve),
            approval(second.id(), ToolApprovalDecision::Approve),
        ];
        let batch = ToolBatchReconstitutionInput::new(
            session_id(1),
            turn_id(2),
            model_call_id(3),
            yielded_snapshot(),
            vec![first.clone(), second.clone()],
            approvals.clone(),
            vec![crash_lost.clone()],
            ToolBatchPhaseReconstitutionInput::Executing {
                turn_attempt: turn_attempt_id(12),
            },
        )
        .reconstitute()
        .expect("crash-loss history remains inspectable for terminalization");
        assert_eq!(
            batch
                .prepare_next_attempt(tool_attempt_id(14), ToolEffectClass::ExternalEffect)
                .expect_err("no later tool may run after crash loss")
                .failure(),
            ToolBatchExecutionFailure::TurnLevelFailure
        );
        let failure_projection = batch
            .prepare_failure_projection(
                vec![
                    semantic_transcript_entry_id(15),
                    semantic_transcript_entry_id(16),
                ],
                context_frontier_id(17),
            )
            .expect("the blocked batch has a public failure projection");
        assert_eq!(
            failure_projection.entries()[0].payload(),
            &SemanticTranscriptEntryPayload::ToolExecutionResult {
                attempt: tool_attempt_id(13),
            }
        );
        assert_eq!(
            failure_projection.entries()[1].payload(),
            &SemanticTranscriptEntryPayload::ToolClosed {
                request: tool_request_id(11),
            }
        );

        let later = ToolAttemptReconstitutionInput::new(
            tool_attempt_id(14),
            second.id(),
            session_id(1),
            turn_id(2),
            turn_attempt_id(12),
            ToolEffectClass::ExternalEffect,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::KnownFailed {
                error: crate::ToolExecutionError::new(
                    ToolExecutionErrorKind::ExecutionFailed,
                    None,
                ),
            }),
        )
        .reconstitute()
        .expect("the first tool dispatch generation is supported");
        let error = ToolBatchReconstitutionInput::new(
            session_id(1),
            turn_id(2),
            model_call_id(3),
            yielded_snapshot(),
            vec![first, second],
            approvals,
            vec![crash_lost, later],
            ToolBatchPhaseReconstitutionInput::Executing {
                turn_attempt: turn_attempt_id(12),
            },
        )
        .reconstitute()
        .expect_err("stored execution after crash loss is impossible history");
        assert_eq!(
            error.failure(),
            ToolBatchReconstitutionFailure::AttemptOrderMismatch
        );
    }

    /// S11: result projection uses only attempt/request
    /// references and preserves proposal order.
    #[test]
    fn s11_result_projection_is_reference_only_and_ordered() {
        let executed = request(10, 0);
        let denied = request(11, 1);
        let success = ToolAttemptEnd::Completed {
            result: ToolResultContent::Text(
                ToolResultText::try_new(String::from("ok")).expect("bounded result is valid"),
            ),
        };
        let attempt = ToolAttemptReconstitutionInput::new(
            tool_attempt_id(12),
            executed.id(),
            session_id(1),
            turn_id(2),
            turn_attempt_id(13),
            ToolEffectClass::EffectFree,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Ended(success),
        )
        .reconstitute()
        .expect("the first tool dispatch generation is supported");
        let batch = ToolBatchReconstitutionInput::new(
            session_id(1),
            turn_id(2),
            model_call_id(3),
            yielded_snapshot(),
            vec![executed, denied],
            vec![
                approval(tool_request_id(10), ToolApprovalDecision::Approve),
                approval(
                    tool_request_id(11),
                    ToolApprovalDecision::Deny { reason: None },
                ),
            ],
            vec![attempt],
            ToolBatchPhaseReconstitutionInput::Executing {
                turn_attempt: turn_attempt_id(13),
            },
        )
        .reconstitute()
        .expect("terminal evidence and denial resolve the batch");
        let projection = batch
            .prepare_result_projection(
                vec![
                    semantic_transcript_entry_id(14),
                    semantic_transcript_entry_id(15),
                ],
                context_frontier_id(16),
            )
            .expect("all logical results can be projected");

        assert_eq!(
            projection.entries()[0].payload(),
            &SemanticTranscriptEntryPayload::ToolExecutionResult {
                attempt: tool_attempt_id(12),
            }
        );
        assert_eq!(
            projection.entries()[1].payload(),
            &SemanticTranscriptEntryPayload::ToolDenied {
                request: tool_request_id(11),
            }
        );
    }

    /// S17: a delivered foreground child wait reopens the
    /// batch under a fresh turn attempt and projects the typed result once.
    #[test]
    fn s17_foreground_child_wait_resumes_and_projects_typed_result() {
        let awaited = request(10, 0);
        let spawning_request = tool_request_id(11);
        let child = session_id(9);
        let waiting_attempt = ToolAttemptReconstitutionInput::new(
            tool_attempt_id(12),
            awaited.id(),
            session_id(1),
            turn_id(2),
            turn_attempt_id(13),
            ToolEffectClass::EffectFree,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::AwaitingChild {
                spawning_request,
                child,
            }),
        )
        .reconstitute()
        .expect("the first tool dispatch generation is supported");
        let approvals = vec![approval(awaited.id(), ToolApprovalDecision::Approve)];
        let waiting = ToolBatchReconstitutionInput::new(
            session_id(1),
            turn_id(2),
            model_call_id(3),
            yielded_snapshot(),
            vec![awaited.clone()],
            approvals.clone(),
            vec![waiting_attempt.clone()],
            ToolBatchPhaseReconstitutionInput::AwaitingChild {
                request: awaited.id(),
                spawning_request,
                child,
            },
        )
        .reconstitute()
        .expect("the exact foreground child wait reconstitutes");
        let resumed = ToolBatchReconstitutionInput::new(
            session_id(1),
            turn_id(2),
            model_call_id(3),
            yielded_snapshot(),
            vec![awaited.clone()],
            approvals,
            vec![waiting_attempt],
            ToolBatchPhaseReconstitutionInput::Executing {
                turn_attempt: turn_attempt_id(14),
            },
        )
        .reconstitute()
        .expect("a delivered wait resumes under a fresh turn attempt");
        let content = DelegationContent::try_new(String::from("checked child result"))
            .expect("the child result is bounded");
        let outcome = DelegationOutcome::reconstitute(
            DelegationOutcomeKind::ResultReturned,
            Some(content.clone()),
            DelegationOutcomeReason::ChildCompleted,
            DelegationProvenanceReconstitutionInput::ChildTurn {
                session: child,
                turn: turn_id(15),
            },
        )
        .expect("the child completion outcome is exact");
        let projection = resumed
            .prepare_delegation_result_projection(
                vec![semantic_transcript_entry_id(16)],
                context_frontier_id(17),
                outcome.clone(),
            )
            .expect("the delivered child result closes the logical request");
        let interrupted = waiting
            .prepare_delegation_cancellation_projection(
                vec![semantic_transcript_entry_id(18)],
                context_frontier_id(19),
                None,
            )
            .expect("a parent-only interrupt closes the child wait without a result");

        assert_eq!(
            waiting.phase(),
            ToolBatchPhase::AwaitingChild {
                request: awaited.id(),
                spawning_request,
                child,
            }
        );
        assert_eq!(
            projection.entries()[0].payload(),
            &SemanticTranscriptEntryPayload::DelegationResult {
                awaiting_request: awaited.id(),
                spawning_request,
                child,
                mode: crate::DelegationWaitMode::Foreground,
                delivery_sequence: None,
                outcome: Box::new(outcome),
            }
        );
        assert_eq!(
            interrupted.entries()[0].payload(),
            &SemanticTranscriptEntryPayload::ToolClosed {
                request: awaited.id(),
            }
        );
    }

    /// S06: terminal recovery closes every
    /// logical request in proposal order without rewriting physical ambiguity.
    #[test]
    fn s06_reconciliation_projection_closes_ambiguity() {
        let ambiguous = request(10, 0);
        let unresolved = request(11, 1);
        let attempt = ToolAttemptReconstitutionInput::new(
            tool_attempt_id(12),
            ambiguous.id(),
            session_id(1),
            turn_id(2),
            turn_attempt_id(13),
            ToolEffectClass::ExternalEffect,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Ambiguous),
        )
        .reconstitute()
        .expect("the first tool dispatch generation is supported");
        let batch = ToolBatchReconstitutionInput::new(
            session_id(1),
            turn_id(2),
            model_call_id(3),
            yielded_snapshot(),
            vec![ambiguous, unresolved],
            vec![
                approval(tool_request_id(10), ToolApprovalDecision::Approve),
                approval(tool_request_id(11), ToolApprovalDecision::Approve),
            ],
            vec![attempt],
            ToolBatchPhaseReconstitutionInput::AwaitingRecovery {
                attempt: tool_attempt_id(12),
            },
        )
        .reconstitute()
        .expect("the exact external-effect ambiguity admits recovery");
        let projection = batch
            .prepare_reconciliation_projection(
                vec![
                    semantic_transcript_entry_id(14),
                    semantic_transcript_entry_id(15),
                ],
                context_frontier_id(16),
            )
            .expect("terminal recovery closes every logical request");

        assert_eq!(
            projection
                .entries()
                .iter()
                .map(SemanticTranscriptEntry::payload)
                .collect::<Vec<_>>(),
            vec![
                &SemanticTranscriptEntryPayload::ToolClosed {
                    request: tool_request_id(10),
                },
                &SemanticTranscriptEntryPayload::ToolClosed {
                    request: tool_request_id(11),
                },
            ]
        );
        assert_eq!(projection.snapshot().entry_count(), 2);
    }
    /// S31: every clone of one checked batch shares one
    /// runner-authorization issuance capability.
    #[test]
    fn s31_runner_authorization_is_single_use_across_batch_clones() {
        let only = request(10, 0);
        let attempt_id = tool_attempt_id(12);
        let attempt = ToolAttemptReconstitutionInput::new(
            attempt_id,
            only.id(),
            session_id(1),
            turn_id(2),
            turn_attempt_id(13),
            ToolEffectClass::EffectFree,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Prepared,
        )
        .reconstitute()
        .expect("the prepared attempt is valid");
        let batch = ToolBatchReconstitutionInput::new(
            session_id(1),
            turn_id(2),
            model_call_id(3),
            yielded_snapshot(),
            vec![only.clone()],
            vec![approval(only.id(), ToolApprovalDecision::Approve)],
            vec![attempt],
            ToolBatchPhaseReconstitutionInput::Executing {
                turn_attempt: turn_attempt_id(13),
            },
        )
        .reconstitute()
        .expect("the prepared batch is complete");
        let duplicate = batch.clone();

        batch
            .authorize_runner_attempt(attempt_id)
            .expect("the batch atomically pairs canonical request authority once");
        let duplicate = duplicate
            .authorize_runner_attempt(attempt_id)
            .expect_err("the shared runner authority cannot be paired twice");

        assert_eq!(
            duplicate.failure(),
            ToolBatchExecutionFailure::AttemptStageMismatch
        );
    }

    /// S31: restored in-flight authority is also
    /// single-use for runner conversion across clones of one checked batch.
    #[test]
    fn s31_in_flight_runner_authorization_is_single_use_across_clones() {
        let only = request(10, 0);
        let attempt_id = tool_attempt_id(12);
        let approval = approval(only.id(), ToolApprovalDecision::Approve);
        let attempt = ToolAttemptReconstitutionInput::new(
            attempt_id,
            only.id(),
            session_id(1),
            turn_id(2),
            turn_attempt_id(13),
            ToolEffectClass::EffectFree,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::InFlight,
        )
        .reconstitute()
        .expect("the in-flight attempt is valid");
        let batch = ToolBatchReconstitutionInput::new(
            session_id(1),
            turn_id(2),
            model_call_id(3),
            yielded_snapshot(),
            vec![only.clone()],
            vec![approval.clone()],
            vec![attempt],
            ToolBatchPhaseReconstitutionInput::Executing {
                turn_attempt: turn_attempt_id(13),
            },
        )
        .reconstitute()
        .expect("the in-flight batch is complete");
        let duplicate = batch.clone();

        batch
            .resume_runner_attempt(attempt_id)
            .expect("the batch atomically restores canonical runner authority once");
        let durable_issuance = batch.runner_authorized_attempts().collect::<Vec<_>>();
        let current = batch
            .attempt(only.id())
            .expect("the durable batch retains the in-flight attempt")
            .clone();
        let restored = ToolBatchReconstitutionInput::new(
            batch.session(),
            batch.turn(),
            batch.producing_call(),
            batch.yielded_snapshot().clone(),
            vec![only],
            vec![approval],
            vec![current],
            ToolBatchPhaseReconstitutionInput::Executing {
                turn_attempt: turn_attempt_id(13),
            },
        )
        .with_runner_authorized_attempts(durable_issuance.clone())
        .reconstitute()
        .expect("durable runner issuance restores with the batch");
        let duplicate = duplicate
            .resume_runner_attempt(attempt_id)
            .expect_err("the clone shares the consumed runner authority");
        let restored = restored
            .resume_runner_attempt(attempt_id)
            .expect_err("durable reconstitution preserves consumed runner authority");

        assert_eq!(durable_issuance, vec![attempt_id]);
        assert_eq!(
            duplicate.failure(),
            ToolBatchExecutionFailure::AttemptStageMismatch
        );
        assert_eq!(
            restored.failure(),
            ToolBatchExecutionFailure::AttemptStageMismatch
        );
    }

    /// S31: reconstitution restores every retired
    /// identity and rejects it as a later claimed-attempt replacement.
    #[test]
    fn s31_reconstituted_batch_rejects_retired_identity_reuse() {
        let only = request(10, 0);
        let current_id = tool_attempt_id(12);
        let retired_id = tool_attempt_id(11);
        let current = ToolAttemptReconstitutionInput::new(
            current_id,
            only.id(),
            session_id(1),
            turn_id(2),
            turn_attempt_id(13),
            ToolEffectClass::EffectFree,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::InFlight,
        )
        .reconstitute()
        .expect("the in-flight attempt is valid");
        let batch = ToolBatchReconstitutionInput::new(
            session_id(1),
            turn_id(2),
            model_call_id(3),
            yielded_snapshot(),
            vec![only.clone()],
            vec![approval(only.id(), ToolApprovalDecision::Approve)],
            vec![current],
            ToolBatchPhaseReconstitutionInput::Executing {
                turn_attempt: turn_attempt_id(13),
            },
        )
        .with_retired_attempts(vec![retired_id])
        .reconstitute()
        .expect("the durable retired inventory is complete");

        assert_eq!(
            batch.retired_attempts().collect::<Vec<_>>(),
            vec![retired_id]
        );
        assert_eq!(
            batch
                .replace_claimed_attempt(current_id, retired_id)
                .err()
                .expect("a retired identity cannot be reused")
                .failure(),
            ToolBatchExecutionFailure::AttemptIdentityReuse
        );
    }

    /// S31: ordinary preparation rejects every durably retired identity.
    #[test]
    fn s31_ordinary_preparation_rejects_retired_identity_reuse() {
        let first = request(10, 0);
        let second = request(11, 1);
        let ended_id = tool_attempt_id(13);
        let retired_id = tool_attempt_id(12);
        let ended = ToolAttemptReconstitutionInput::new(
            ended_id,
            first.id(),
            session_id(1),
            turn_id(2),
            turn_attempt_id(14),
            ToolEffectClass::EffectFree,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::KnownFailed {
                error: crate::ToolExecutionError::new(
                    ToolExecutionErrorKind::ExecutionFailed,
                    None,
                ),
            }),
        )
        .reconstitute()
        .expect("the ended attempt is valid");
        let batch = ToolBatchReconstitutionInput::new(
            session_id(1),
            turn_id(2),
            model_call_id(3),
            yielded_snapshot(),
            vec![first.clone(), second.clone()],
            vec![
                approval(first.id(), ToolApprovalDecision::Approve),
                approval(second.id(), ToolApprovalDecision::Approve),
            ],
            vec![ended],
            ToolBatchPhaseReconstitutionInput::Executing {
                turn_attempt: turn_attempt_id(14),
            },
        )
        .with_retired_attempts(vec![retired_id])
        .reconstitute()
        .expect("the retired inventory and ended history are complete");

        assert_eq!(
            batch
                .prepare_next_attempt(retired_id, ToolEffectClass::EffectFree)
                .expect_err("ordinary preparation cannot reuse a retired identity")
                .failure(),
            ToolBatchExecutionFailure::AttemptIdentityReuse
        );
    }

    /// S31: retired and current inventories must be disjoint.
    #[test]
    fn s31_reconstitution_rejects_current_identity_as_retired() {
        let only = request(10, 0);
        let current_id = tool_attempt_id(12);
        let current = ToolAttemptReconstitutionInput::new(
            current_id,
            only.id(),
            session_id(1),
            turn_id(2),
            turn_attempt_id(13),
            ToolEffectClass::EffectFree,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::InFlight,
        )
        .reconstitute()
        .expect("the in-flight attempt is valid");
        let input = ToolBatchReconstitutionInput::new(
            session_id(1),
            turn_id(2),
            model_call_id(3),
            yielded_snapshot(),
            vec![only.clone()],
            vec![approval(only.id(), ToolApprovalDecision::Approve)],
            vec![current],
            ToolBatchPhaseReconstitutionInput::Executing {
                turn_attempt: turn_attempt_id(13),
            },
        )
        .with_retired_attempts(vec![current_id]);

        assert_eq!(
            input
                .reconstitute()
                .expect_err("current and retired identities cannot overlap")
                .failure(),
            ToolBatchReconstitutionFailure::AttemptInventoryMismatch
        );
    }
}
