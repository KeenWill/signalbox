//! Evidence-bearing logical tool-batch transitions.
//!
//! `docs/spec/tool-loop.md` is normative. This aggregate validates one
//! producing call's complete request, approval, and attempt inventory before
//! it can expose an approval wait, prepare the next serialized physical
//! attempt, or project reference-only results.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    ActiveTurnPhase, ApprovedToolRequest, CurrentToolAttempt, CurrentToolAttemptState,
    DecideToolRequest, DecideToolRequestResult, PreparedDecideToolRequest,
    ReconstitutedToolAttempt, ResolvedContextFrontierSnapshot, SemanticTranscriptEntry,
    SemanticTranscriptEntryId, SemanticTranscriptEntryPayload, SessionId, ToolApprovalDecision,
    ToolApprovalResolution, ToolAttemptEnd, ToolAttemptId, ToolEffectClass, ToolExecutionErrorKind,
    ToolRequest, ToolRequestId, TurnAttemptId, TurnId,
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
            phase,
        }
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
}

/// One completely validated active logical tool batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolBatch {
    session: SessionId,
    turn: TurnId,
    producing_call: crate::ModelCallId,
    yielded_snapshot: ResolvedContextFrontierSnapshot,
    requests: Box<[ToolRequest]>,
    approvals: BTreeMap<ToolRequestId, ToolApprovalResolution>,
    attempts: BTreeMap<ToolRequestId, ReconstitutedToolAttempt>,
    phase: ToolBatchPhase,
}

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
            ToolBatchPhase::Executing { .. } | ToolBatchPhase::AwaitingRecovery { .. } => None,
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
            ToolBatchPhase::AwaitingApproval { .. } | ToolBatchPhase::Executing { .. } => None,
        }
    }

    /// Applies or authoritatively rejects one owner decision against complete
    /// proposal-order state.
    pub fn prepare_owner_decision(
        self,
        command: DecideToolRequest,
        continuation_attempt: Option<TurnAttemptId>,
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
            return Ok(PreparedToolBatchDecision::rejected(
                self,
                command.prepare_request_not_found(),
                waiting_on,
            ));
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
            command
                .prepare_applied(request_record)
                .map_err(|error| ToolBatchDecisionError {
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
        if self.attempts.values().any(|candidate| {
            let candidate_id = match candidate {
                ReconstitutedToolAttempt::Current(current) => current.attempt(),
                ReconstitutedToolAttempt::Ended(ended) => ended.attempt(),
            };
            candidate_id == attempt
        }) {
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

    /// Builds one proposal-ordered reference-only result entry per request.
    pub fn prepare_result_projection(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        continuation_frontier: crate::ContextFrontierId,
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

    /// Builds proposal-ordered results for an interrupt-cancelled executing
    /// batch after every physical attempt has reached a durable end.
    pub fn prepare_cancellation_projection(
        &self,
        entry_ids: Vec<SemanticTranscriptEntryId>,
        result_frontier: crate::ContextFrontierId,
    ) -> Result<PreparedToolResultProjection, ToolResultProjectionError> {
        if !matches!(self.phase, ToolBatchPhase::Executing { .. })
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
                            if matches!(
                                attempt.end(),
                                ToolAttemptEnd::KnownFailed { error }
                                    if error.kind() == ToolExecutionErrorKind::CrashLost
                            ) =>
                        {
                            SemanticTranscriptEntryPayload::ToolClosed {
                                request: request.id(),
                            }
                        }
                        Some(ReconstitutedToolAttempt::Ended(attempt))
                            if attempt.end() != &ToolAttemptEnd::Ambiguous =>
                        {
                            SemanticTranscriptEntryPayload::ToolExecutionResult {
                                attempt: attempt.attempt(),
                            }
                        }
                        Some(ReconstitutedToolAttempt::Ended(_)) => {
                            return Err(ToolResultProjectionError {
                                failure: ToolResultProjectionFailure::TurnLevelFailure,
                            });
                        }
                        Some(ReconstitutedToolAttempt::Current(_)) => unreachable!(
                            "the live-attempt guard rejects current cancellation evidence"
                        ),
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
                        Some(ReconstitutedToolAttempt::Current(_)) => unreachable!(
                            "the live-attempt guard rejects current reconciliation evidence"
                        ),
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

/// Why no next serialized attempt can be prepared.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolBatchExecutionFailure {
    /// The batch is parked on approval or recovery.
    NotExecuting,
    /// One attempt already remains prepared or in flight.
    LiveAttemptPresent,
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
    if requests
        .iter()
        .take(approvals.len())
        .any(|request| !approvals.contains_key(&request.id()))
    {
        return Err(fail(
            input,
            ToolBatchReconstitutionFailure::ApprovalInventoryMismatch,
        ));
    }
    let expected_issuing_attempt = match input.phase {
        ToolBatchPhaseReconstitutionInput::Executing { turn_attempt } => Some(turn_attempt),
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
    let phase = match input.phase {
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
            if earliest_undecided.is_none() && ambiguous_attempts.is_empty() =>
        {
            if attempts.values().any(|attempt| {
                let (_, _, _, _, issuing_attempt, _) = attempt_facts(attempt);
                issuing_attempt != turn_attempt
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
    };
    Ok(ToolBatch {
        session: input.session,
        turn: input.turn,
        producing_call: input.producing_call,
        yielded_snapshot: input.yielded_snapshot,
        requests: requests.into_boxed_slice(),
        approvals,
        attempts,
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
        DurableCommandId, NormalizedToolArguments, ToolApprovalResolutionReconstitutionInput,
        ToolArgumentsKind, ToolAttemptReconstitutionInput, ToolAttemptReconstitutionState,
        ToolDecisionSource, ToolDispatchGeneration, ToolName, ToolRequestOrdinal,
        ToolRequestReconstitutionInput, ToolResultContent, ToolResultText,
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
        ToolApprovalResolutionReconstitutionInput::new(
            request,
            decision,
            ToolDecisionSource::OwnerCommand,
        )
        .reconstitute()
        .expect("owner decisions are implemented")
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

    /// S10 / INV-010 / INV-012 / INV-020: owner decisions advance exactly one
    /// earliest wait and retain explicit owner provenance.
    #[test]
    fn s10_inv010_inv012_inv020_owner_decision_advances_to_next_wait() {
        let command = DecideToolRequest::new(
            DurableCommandId::from_uuid(uuid::Uuid::from_u128(20)),
            tool_request_id(10),
            ToolApprovalDecision::Approve,
        );
        let prepared = awaiting_batch()
            .prepare_owner_decision(command, None)
            .expect("the earliest decision needs no continuation yet");
        let DecideToolRequestResult::Applied(applied) = prepared.prepared_command().result() else {
            panic!("the earliest exact decision applies");
        };

        assert_eq!(
            applied.resolution().source(),
            ToolDecisionSource::OwnerCommand
        );
        assert!(matches!(
            prepared.active_phase(),
            ActiveTurnPhase::AwaitingApproval { request }
                if *request == tool_request_id(11)
        ));
    }

    /// S10 / INV-010: durable approval history is exactly a proposal-order
    /// prefix and cannot skip the current wait.
    #[test]
    fn s10_inv010_reconstitution_rejects_nonprefix_approval_inventory() {
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

    /// S10 / INV-010: an owner decision is admissible only at the exact
    /// durable approval wait and cannot manufacture a wait from execution.
    #[test]
    fn s10_inv010_owner_decision_rejects_nonwaiting_batch_unchanged() {
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
            .prepare_owner_decision(command, None)
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

    /// S10 / INV-019 / INV-024: serialized execution prepares only the first
    /// approved request without terminal attempt evidence.
    #[test]
    fn s10_inv019_inv024_execution_prepares_first_unattempted_request() {
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

    /// S06 / INV-025 / INV-026: only a completely reconstituted ambiguous
    /// batch can expose the exact tool recovery-wait subject.
    #[test]
    fn s06_inv025_inv026_ambiguous_batch_exposes_opaque_recovery_wait() {
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
        .reconstitute();
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

    /// S06 / INV-025 / INV-026: impossible effect-free ambiguity cannot
    /// manufacture recovery-wait authority during checked reconstitution.
    #[test]
    fn s06_inv025_inv026_effect_free_ambiguous_history_fails_closed() {
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
        .reconstitute();
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

    /// S10 / INV-006 / INV-011: a live serialized attempt is the last
    /// attempt that can exist in proposal order.
    #[test]
    fn s10_inv006_inv011_reconstitution_rejects_attempt_after_live_attempt() {
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
        .reconstitute();
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
        .reconstitute();
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

    /// S06 / INV-004 / INV-006: recovery evidence belongs to one issuing
    /// continuation tenure throughout the complete batch.
    #[test]
    fn s06_inv004_inv006_recovery_rejects_mixed_issuing_attempts() {
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
        .reconstitute();
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
        .reconstitute();
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

    /// S05 / INV-019 / INV-026: crash-lost evidence is a turn-level blocker,
    /// so no later approved request can be prepared or already attempted.
    #[test]
    fn s05_inv019_inv026_crash_loss_stops_serial_batch_execution() {
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
        .reconstitute();
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
        .reconstitute();
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

    /// S11 / INV-005 / INV-027: result projection uses only attempt/request
    /// references and preserves proposal order.
    #[test]
    fn s11_inv005_inv027_result_projection_is_reference_only_and_ordered() {
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
        .reconstitute();
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

    /// S06 / INV-005 / INV-006 / INV-025: terminal recovery closes every
    /// logical request in proposal order without rewriting physical ambiguity.
    #[test]
    fn s06_inv005_inv006_inv025_reconciliation_projection_closes_ambiguity() {
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
        .reconstitute();
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
}
