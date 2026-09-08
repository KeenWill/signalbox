//! Turn scheduling reconstitution input for `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::failure::AcceptedInputSchedulingReconstitutionError;
use super::projection::{AcceptedInputSchedulingProjection, PendingSteeringInput};
use super::reconstitute::reconstitute;
use super::record::{AcceptedInputTurnSchedulingRecord, DelegatedTurnSchedulingFact};
use crate::{
    AcceptedInputId, AcceptedInputLifecycle, ActiveTurnPhase, AppliedInterruptCommandResult,
    AttemptEnd, CancellationStopDisposition, ChildWait, ContextFrontierId, CurrentTurnAttempt,
    DeliveryRequest, DirectModelSelection, ReconstitutedImportedSession,
    ResolvedContextFrontierReconstitutionInput, ResolvedContextFrontierSnapshot,
    SemanticTranscriptEntryReconstitutionInput, Session, SessionId, SessionInputPosition,
    ToolApprovalResolution, ToolRequestId, TurnAttemptId, TurnId, UnstoppedAttemptDisposition,
};

/// Correlated stored execution provenance for one failed terminal turn.
///
/// The optional enclosing value distinguishes a direct failure with no
/// physical attempt. This shape always names an ended attempt, and can name a
/// terminal call only together with that attempt, so call-only provenance is
/// unrepresentable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailedTurnExecutionReconstitutionInput {
    pub(super) owning_turn: TurnId,
    pub(super) ended_attempt: TurnAttemptId,
    pub(super) attempt_end: TerminalAttemptEndReconstitutionInput,
    pub(super) ended_call: Option<crate::ModelCallId>,
    pub(super) terminal_tool_attempts: Vec<crate::EndedToolAttempt>,
    terminal_tool_denials: Vec<ToolApprovalResolution>,
}

impl FailedTurnExecutionReconstitutionInput {
    /// Supplies one ended attempt when no physical call existed.
    pub const fn attempt_only(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        attempt_disposition: UnstoppedAttemptDisposition,
    ) -> Self {
        Self {
            owning_turn,
            ended_attempt,
            attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(attempt_disposition),
            ended_call: None,
            terminal_tool_attempts: Vec::new(),
            terminal_tool_denials: Vec::new(),
        }
    }

    /// Supplies one ended attempt and its exact terminal physical call.
    pub const fn with_call(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        attempt_disposition: UnstoppedAttemptDisposition,
        ended_call: crate::ModelCallId,
    ) -> Self {
        Self {
            owning_turn,
            ended_attempt,
            attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(attempt_disposition),
            ended_call: Some(ended_call),
            terminal_tool_attempts: Vec::new(),
            terminal_tool_denials: Vec::new(),
        }
    }

    /// Supplies one proof-bearing ended attempt when no physical call existed.
    pub const fn attempt_only_after_cancellation(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        disposition: CancellationStopDisposition,
        interrupt: AppliedInterruptCommandResult,
    ) -> Self {
        Self {
            owning_turn,
            ended_attempt,
            attempt_end: TerminalAttemptEndReconstitutionInput::after_cancellation(
                disposition,
                interrupt,
            ),
            ended_call: None,
            terminal_tool_attempts: Vec::new(),
            terminal_tool_denials: Vec::new(),
        }
    }

    /// Supplies one proof-bearing ended attempt and terminal physical call.
    pub const fn with_call_after_cancellation(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        disposition: CancellationStopDisposition,
        interrupt: AppliedInterruptCommandResult,
        ended_call: crate::ModelCallId,
    ) -> Self {
        Self {
            owning_turn,
            ended_attempt,
            attempt_end: TerminalAttemptEndReconstitutionInput::after_cancellation(
                disposition,
                interrupt,
            ),
            ended_call: Some(ended_call),
            terminal_tool_attempts: Vec::new(),
            terminal_tool_denials: Vec::new(),
        }
    }

    /// Supplies the complete independently checked terminal tool-attempt
    /// inventory referenced by a writer-produced tool-result suffix.
    pub fn with_terminal_tool_attempts(
        mut self,
        terminal_tool_attempts: Vec<crate::EndedToolAttempt>,
    ) -> Self {
        self.terminal_tool_attempts = terminal_tool_attempts;
        self
    }

    /// Supplies the complete user-sourced denial resolutions backing every
    /// `ToolDenied` entry in a writer-produced tool-result suffix.
    pub fn with_terminal_tool_denials(
        mut self,
        terminal_tool_denials: Vec<ToolApprovalResolution>,
    ) -> Self {
        self.terminal_tool_denials = terminal_tool_denials;
        self
    }

    /// Returns the stored owning turn.
    pub const fn owning_turn(&self) -> TurnId {
        self.owning_turn
    }

    /// Returns the stored ended attempt.
    pub const fn ended_attempt(&self) -> TurnAttemptId {
        self.ended_attempt
    }

    /// Borrows the stored proof-aware attempt end.
    pub const fn attempt_end(&self) -> &TerminalAttemptEndReconstitutionInput {
        &self.attempt_end
    }

    /// Returns the terminal physical call when one existed.
    pub const fn ended_call(&self) -> Option<crate::ModelCallId> {
        self.ended_call
    }

    /// Borrows every terminal tool attempt supplied for result correlation.
    pub fn terminal_tool_attempts(&self) -> &[crate::EndedToolAttempt] {
        &self.terminal_tool_attempts
    }

    /// Borrows every user denial resolution supplied for result correlation.
    pub fn terminal_tool_denials(&self) -> &[ToolApprovalResolution] {
        &self.terminal_tool_denials
    }
}

/// Inert stored attempt-end facts validated with the complete scheduling graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalAttemptEndReconstitutionInput {
    end: AttemptEnd,
    interrupt: Option<AppliedInterruptCommandResult>,
}

impl TerminalAttemptEndReconstitutionInput {
    /// Supplies an attempt end with no stop cause.
    pub const fn without_stop(disposition: UnstoppedAttemptDisposition) -> Self {
        Self {
            end: AttemptEnd::WithoutStop { disposition },
            interrupt: None,
        }
    }

    /// Supplies an attempt end carrying the exact applied interrupt result.
    pub const fn after_cancellation(
        disposition: CancellationStopDisposition,
        interrupt: AppliedInterruptCommandResult,
    ) -> Self {
        Self {
            end: AttemptEnd::AfterCancellation {
                cause: interrupt.proof(),
                disposition,
            },
            interrupt: Some(interrupt),
        }
    }

    /// Supplies an attempt yielded to a runner-recovery wait together with
    /// the exact interrupt that later consumed that wait.
    pub const fn yielded_to_runner_recovery(interrupt: AppliedInterruptCommandResult) -> Self {
        Self {
            end: AttemptEnd::WithoutStop {
                disposition: UnstoppedAttemptDisposition::YieldedToDurableWait,
            },
            interrupt: Some(interrupt),
        }
    }

    /// Borrows the stored typed attempt end.
    pub const fn end(&self) -> &AttemptEnd {
        &self.end
    }

    /// Returns the complete interrupt result when cancellation was requested.
    pub const fn interrupt(&self) -> Option<AppliedInterruptCommandResult> {
        self.interrupt
    }
}

/// Correlated stored execution provenance for one cancelled terminal turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelledTurnExecutionReconstitutionInput {
    pub(super) owning_turn: TurnId,
    pub(super) ended_attempt: TurnAttemptId,
    pub(super) attempt_end: TerminalAttemptEndReconstitutionInput,
    pub(super) ended_call: Option<crate::ModelCallId>,
    pub(super) interrupt: AppliedInterruptCommandResult,
    pub(super) terminal_tool_attempts: Vec<crate::EndedToolAttempt>,
    terminal_tool_denials: Vec<ToolApprovalResolution>,
}

impl CancelledTurnExecutionReconstitutionInput {
    /// Supplies the exact ended attempt, optional unsent call, and applied
    /// interrupt result that caused terminal cancellation.
    pub const fn new(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        attempt_end: TerminalAttemptEndReconstitutionInput,
        ended_call: Option<crate::ModelCallId>,
        interrupt: AppliedInterruptCommandResult,
    ) -> Self {
        Self {
            owning_turn,
            ended_attempt,
            attempt_end,
            ended_call,
            interrupt,
            terminal_tool_attempts: Vec::new(),
            terminal_tool_denials: Vec::new(),
        }
    }

    /// Supplies the complete independently checked terminal tool-attempt
    /// inventory referenced by a writer-produced tool-result suffix.
    pub fn with_terminal_tool_attempts(
        mut self,
        terminal_tool_attempts: Vec<crate::EndedToolAttempt>,
    ) -> Self {
        self.terminal_tool_attempts = terminal_tool_attempts;
        self
    }

    /// Supplies the complete user-sourced denial resolutions backing every
    /// `ToolDenied` entry in a writer-produced tool-result suffix.
    pub fn with_terminal_tool_denials(
        mut self,
        terminal_tool_denials: Vec<ToolApprovalResolution>,
    ) -> Self {
        self.terminal_tool_denials = terminal_tool_denials;
        self
    }

    /// Borrows every terminal tool attempt supplied for result correlation.
    pub fn terminal_tool_attempts(&self) -> &[crate::EndedToolAttempt] {
        &self.terminal_tool_attempts
    }

    /// Borrows every user denial resolution supplied for result correlation.
    pub fn terminal_tool_denials(&self) -> &[ToolApprovalResolution] {
        &self.terminal_tool_denials
    }
}

/// Stored facts for one active scheduling phase.
///
/// These constructors preserve inert stored facts only. Complete scheduling
/// reconstitution validates a stop request's applied-interrupt result and call,
/// and validates a recovery call as this turn's exact terminal-ambiguous
/// operation; no constructor independently produces a canonical active phase.
///
/// A bare wait subject is intentionally not a production constructor:
///
/// ```compile_fail
/// use signalbox_domain::{
///     ActiveTurnSchedulingReconstitutionInput, ToolRequestId, TurnId,
/// };
///
/// let turn = TurnId::from_uuid(uuid::Uuid::nil());
/// let request = ToolRequestId::from_uuid(uuid::Uuid::nil());
/// let _ = ActiveTurnSchedulingReconstitutionInput::awaiting_approval(turn, request);
/// ```
///
/// Raw stored facts cannot be used to obtain a canonical active phase before
/// the owning scheduling projection validates them:
///
/// ```compile_fail
/// use signalbox_domain::{ActiveTurnSchedulingReconstitutionInput, TurnAttemptId, TurnId};
///
/// let turn = TurnId::from_uuid(uuid::Uuid::nil());
/// let attempt = TurnAttemptId::from_uuid(uuid::Uuid::nil());
/// let input = ActiveTurnSchedulingReconstitutionInput::running(turn, attempt);
/// let _ = input.phase();
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveTurnSchedulingReconstitutionInput {
    pub(super) owning_turn: TurnId,
    pub(super) current_attempt: Option<TurnAttemptId>,
    pub(super) state: StoredActiveTurnPhase,
    pub(super) executing_tool_batch: Option<ExecutingToolBatchReconstitutionFacts>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ExecutingToolBatchReconstitutionFacts {
    pub(super) session: SessionId,
    pub(super) producing_call: crate::ModelCallId,
    pub(super) yielded_snapshot: ResolvedContextFrontierSnapshot,
    pub(super) batch_attempt: Option<TurnAttemptId>,
    pub(super) awaiting_request: Option<ToolRequestId>,
    pub(super) requests: Box<[ToolRequestId]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum StoredActiveTurnPhase {
    AwaitingCredentialAvailability {
        wait: crate::CredentialAvailabilityWait,
    },
    Prepared,
    Running,
    StopRequested {
        call: crate::ModelCallId,
        interrupt: AppliedInterruptCommandResult,
    },
    AwaitingApproval {
        wait: crate::AwaitingToolApproval,
    },
    AwaitingChild {
        wait: ChildWait,
    },
    AwaitingToolRecovery {
        wait: crate::AwaitingToolRecovery,
        attempt_end: TerminalAttemptEndReconstitutionInput,
    },
    AwaitingModelCallRecovery {
        call: crate::ModelCallId,
        attempt_end: TerminalAttemptEndReconstitutionInput,
    },
    AwaitingRunnerRecovery {
        runner: crate::RunnerId,
        placement_revision: crate::RunnerGeneration,
        interrupted_tool_attempt: Option<crate::ToolAttemptId>,
        source_frontier: Option<ContextFrontierId>,
    },
}

/// Complete independently stored evidence for one delegated model-call
/// recovery wait. The delegated activation aggregate validates every field
/// before exposing canonical recovery history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegatedModelCallRecoveryReconstitutionInput {
    pub(super) phase: ActiveTurnSchedulingReconstitutionInput,
    pub(super) pinned_target: crate::PinnedProviderTargetReconstitutionInput,
    pub(super) call: crate::ModelCallReconstitutionInput,
    pub(super) source_snapshot: ResolvedContextFrontierReconstitutionInput,
    pub(super) pending_steering: Vec<PendingSteeringInput>,
    pub(super) consumed_steering: Vec<ConsumedSteeringReconstitutionInput>,
}

impl DelegatedModelCallRecoveryReconstitutionInput {
    /// Supplies the stored phase, pinned target, exact ambiguous call, and the
    /// resolved snapshot named by that call.
    pub const fn new(
        phase: ActiveTurnSchedulingReconstitutionInput,
        pinned_target: crate::PinnedProviderTargetReconstitutionInput,
        call: crate::ModelCallReconstitutionInput,
        source_snapshot: ResolvedContextFrontierReconstitutionInput,
        pending_steering: Vec<PendingSteeringInput>,
        consumed_steering: Vec<ConsumedSteeringReconstitutionInput>,
    ) -> Self {
        Self {
            phase,
            pinned_target,
            call,
            source_snapshot,
            pending_steering,
            consumed_steering,
        }
    }
}

impl ActiveTurnSchedulingReconstitutionInput {
    /// Supplies inert facts for a stored prepared current attempt.
    pub const fn prepared(owning_turn: TurnId, current_attempt: TurnAttemptId) -> Self {
        Self {
            owning_turn,
            current_attempt: Some(current_attempt),
            state: StoredActiveTurnPhase::Prepared,
            executing_tool_batch: None,
        }
    }

    /// Supplies inert facts for a stored running current attempt.
    pub const fn running(owning_turn: TurnId, current_attempt: TurnAttemptId) -> Self {
        Self {
            owning_turn,
            current_attempt: Some(current_attempt),
            state: StoredActiveTurnPhase::Running,
            executing_tool_batch: None,
        }
    }

    /// Attaches independently reconstituted evidence for an executing tool
    /// batch without changing the stored turn-attempt state.
    pub fn with_executing_tool_batch(mut self, batch: &crate::ToolBatch) -> Self {
        self.executing_tool_batch = Some(ExecutingToolBatchReconstitutionFacts {
            session: batch.session(),
            producing_call: batch.producing_call(),
            yielded_snapshot: batch.yielded_snapshot().clone(),
            batch_attempt: match batch.phase() {
                crate::ToolBatchPhase::Executing { turn_attempt } => Some(turn_attempt),
                crate::ToolBatchPhase::AwaitingApproval { .. }
                | crate::ToolBatchPhase::AwaitingRecovery { .. }
                | crate::ToolBatchPhase::AwaitingChild { .. } => None,
            },
            awaiting_request: None,
            requests: batch
                .requests()
                .iter()
                .map(crate::ToolRequest::id)
                .collect(),
        });
        self
    }

    /// Supplies inert facts for one proof-bearing cancellation request.
    pub const fn stop_requested(
        owning_turn: TurnId,
        current_attempt: TurnAttemptId,
        call: crate::ModelCallId,
        interrupt: AppliedInterruptCommandResult,
    ) -> Self {
        Self {
            owning_turn,
            current_attempt: Some(current_attempt),
            state: StoredActiveTurnPhase::StopRequested { call, interrupt },
            executing_tool_batch: None,
        }
    }

    /// Supplies an evidence-bearing stored approval wait derived from the
    /// complete current tool batch.
    pub fn awaiting_approval(owning_turn: TurnId, batch: &crate::ToolBatch) -> Option<Self> {
        let wait = batch.awaiting_approval()?;
        (batch.turn() == owning_turn).then(|| Self {
            owning_turn,
            current_attempt: None,
            state: StoredActiveTurnPhase::AwaitingApproval { wait },
            executing_tool_batch: Some(ExecutingToolBatchReconstitutionFacts {
                session: batch.session(),
                producing_call: batch.producing_call(),
                yielded_snapshot: batch.yielded_snapshot().clone(),
                batch_attempt: None,
                awaiting_request: Some(wait.request()),
                requests: batch
                    .requests()
                    .iter()
                    .map(crate::ToolRequest::id)
                    .collect(),
            }),
        })
    }

    /// Supplies an evidence-bearing stored foreground child wait derived from
    /// the complete current tool batch.
    pub fn awaiting_child(owning_turn: TurnId, batch: &crate::ToolBatch) -> Option<Self> {
        let wait = match batch.phase() {
            crate::ToolBatchPhase::AwaitingChild {
                request,
                spawning_request,
                child,
            } => ChildWait::from_checked_parts(request, spawning_request, child),
            crate::ToolBatchPhase::AwaitingApproval { .. }
            | crate::ToolBatchPhase::Executing { .. }
            | crate::ToolBatchPhase::AwaitingRecovery { .. } => return None,
        };
        (batch.turn() == owning_turn).then(|| Self {
            owning_turn,
            current_attempt: None,
            state: StoredActiveTurnPhase::AwaitingChild { wait },
            executing_tool_batch: Some(ExecutingToolBatchReconstitutionFacts {
                session: batch.session(),
                producing_call: batch.producing_call(),
                yielded_snapshot: batch.yielded_snapshot().clone(),
                batch_attempt: None,
                awaiting_request: Some(wait.awaiting_request()),
                requests: batch
                    .requests()
                    .iter()
                    .map(crate::ToolRequest::id)
                    .collect(),
            }),
        })
    }

    /// Supplies an evidence-bearing same-process ambiguous tool wait.
    pub const fn awaiting_tool_recovery(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        wait: crate::AwaitingToolRecovery,
    ) -> Self {
        Self {
            owning_turn,
            current_attempt: Some(ended_attempt),
            state: StoredActiveTurnPhase::AwaitingToolRecovery {
                wait,
                attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(
                    UnstoppedAttemptDisposition::Ambiguous,
                ),
            },
            executing_tool_batch: None,
        }
    }

    /// Supplies an evidence-bearing crash-lost ambiguous tool wait.
    pub const fn awaiting_tool_recovery_after_restart(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        wait: crate::AwaitingToolRecovery,
    ) -> Self {
        Self {
            owning_turn,
            current_attempt: Some(ended_attempt),
            state: StoredActiveTurnPhase::AwaitingToolRecovery {
                wait,
                attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(
                    UnstoppedAttemptDisposition::Lost,
                ),
            },
            executing_tool_batch: None,
        }
    }

    /// Supplies a same-process ambiguous tool wait after cancellation.
    pub const fn awaiting_tool_recovery_after_cancellation(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        wait: crate::AwaitingToolRecovery,
        interrupt: AppliedInterruptCommandResult,
    ) -> Self {
        Self {
            owning_turn,
            current_attempt: Some(ended_attempt),
            state: StoredActiveTurnPhase::AwaitingToolRecovery {
                wait,
                attempt_end: TerminalAttemptEndReconstitutionInput::after_cancellation(
                    CancellationStopDisposition::Ambiguous,
                    interrupt,
                ),
            },
            executing_tool_batch: None,
        }
    }

    /// Supplies a crash-lost ambiguous tool wait after cancellation.
    pub const fn awaiting_tool_recovery_after_cancellation_restart(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        wait: crate::AwaitingToolRecovery,
        interrupt: AppliedInterruptCommandResult,
    ) -> Self {
        Self {
            owning_turn,
            current_attempt: Some(ended_attempt),
            state: StoredActiveTurnPhase::AwaitingToolRecovery {
                wait,
                attempt_end: TerminalAttemptEndReconstitutionInput::after_cancellation(
                    CancellationStopDisposition::Lost,
                    interrupt,
                ),
            },
            executing_tool_batch: None,
        }
    }

    /// Supplies inert facts for a live ambiguous call awaiting a user
    /// recovery decision.
    pub const fn awaiting_model_call_recovery(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        ambiguous_call: crate::ModelCallId,
    ) -> Self {
        Self {
            owning_turn,
            current_attempt: Some(ended_attempt),
            state: StoredActiveTurnPhase::AwaitingModelCallRecovery {
                call: ambiguous_call,
                attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(
                    UnstoppedAttemptDisposition::Ambiguous,
                ),
            },
            executing_tool_batch: None,
        }
    }

    /// Supplies inert facts for a prior-process issued call that startup made
    /// ambiguous while ending its attempt as lost.
    pub const fn awaiting_model_call_recovery_after_restart(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        ambiguous_call: crate::ModelCallId,
    ) -> Self {
        Self {
            owning_turn,
            current_attempt: Some(ended_attempt),
            state: StoredActiveTurnPhase::AwaitingModelCallRecovery {
                call: ambiguous_call,
                attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(
                    UnstoppedAttemptDisposition::Lost,
                ),
            },
            executing_tool_batch: None,
        }
    }

    /// Supplies a same-process ambiguous wait after cancellation was requested.
    pub const fn awaiting_model_call_recovery_after_cancellation(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        ambiguous_call: crate::ModelCallId,
        interrupt: AppliedInterruptCommandResult,
    ) -> Self {
        Self {
            owning_turn,
            current_attempt: Some(ended_attempt),
            state: StoredActiveTurnPhase::AwaitingModelCallRecovery {
                call: ambiguous_call,
                attempt_end: TerminalAttemptEndReconstitutionInput::after_cancellation(
                    CancellationStopDisposition::Ambiguous,
                    interrupt,
                ),
            },
            executing_tool_batch: None,
        }
    }

    /// Supplies a prior-process ambiguous wait ended as lost after cancellation.
    pub const fn awaiting_model_call_recovery_after_cancellation_restart(
        owning_turn: TurnId,
        ended_attempt: TurnAttemptId,
        ambiguous_call: crate::ModelCallId,
        interrupt: AppliedInterruptCommandResult,
    ) -> Self {
        Self {
            owning_turn,
            current_attempt: Some(ended_attempt),
            state: StoredActiveTurnPhase::AwaitingModelCallRecovery {
                call: ambiguous_call,
                attempt_end: TerminalAttemptEndReconstitutionInput::after_cancellation(
                    CancellationStopDisposition::Lost,
                    interrupt,
                ),
            },
            executing_tool_batch: None,
        }
    }

    /// Supplies facts for a stored runner-loss wait whose relational evidence
    /// has already been checked by the persistence boundary.
    pub const fn awaiting_runner_recovery(
        owning_turn: TurnId,
        runner: crate::RunnerId,
        placement_revision: crate::RunnerGeneration,
        interrupted_tool_attempt: Option<crate::ToolAttemptId>,
        source_frontier: Option<ContextFrontierId>,
    ) -> Self {
        Self {
            owning_turn,
            current_attempt: None,
            state: StoredActiveTurnPhase::AwaitingRunnerRecovery {
                runner,
                placement_revision,
                interrupted_tool_attempt,
                source_frontier,
            },
            executing_tool_batch: None,
        }
    }

    /// Supplies a credential wait whose attempt, policy and exclusion evidence were checked together.
    pub const fn awaiting_credential_availability(
        owning_turn: TurnId,
        wait: crate::CredentialAvailabilityWait,
    ) -> Self {
        Self {
            owning_turn,
            current_attempt: None,
            state: StoredActiveTurnPhase::AwaitingCredentialAvailability { wait },
            executing_tool_batch: None,
        }
    }

    /// Returns the turn named as owner by the active-phase record.
    pub const fn owning_turn(&self) -> TurnId {
        self.owning_turn
    }

    pub(super) fn canonical_evidence_free_phase(&self) -> Option<ActiveTurnPhase> {
        if let StoredActiveTurnPhase::AwaitingCredentialAvailability { wait } = self.state {
            return self
                .current_attempt
                .is_none()
                .then_some(ActiveTurnPhase::AwaitingCredentialAvailability { wait });
        }
        if let StoredActiveTurnPhase::AwaitingApproval { wait } = &self.state {
            return (self.current_attempt.is_none() && self.owning_turn == wait.turn()).then_some(
                ActiveTurnPhase::AwaitingApproval {
                    request: wait.request(),
                },
            );
        }
        if let StoredActiveTurnPhase::AwaitingChild { wait } = &self.state {
            return self
                .current_attempt
                .is_none()
                .then_some(ActiveTurnPhase::AwaitingChild { wait: *wait });
        }
        if let StoredActiveTurnPhase::AwaitingRunnerRecovery {
            runner,
            placement_revision,
            interrupted_tool_attempt,
            ..
        } = self.state
        {
            return self.current_attempt.is_none().then_some(
                ActiveTurnPhase::AwaitingRunnerRecovery {
                    runner,
                    placement_revision,
                    optional_tool_attempt: interrupted_tool_attempt,
                },
            );
        }
        let current_attempt = CurrentTurnAttempt::prepared(self.current_attempt?);
        let current_attempt = match &self.state {
            StoredActiveTurnPhase::Prepared => current_attempt,
            StoredActiveTurnPhase::Running => current_attempt.begin_running().ok()?,
            StoredActiveTurnPhase::StopRequested { interrupt, .. } => current_attempt
                .begin_running()
                .and_then(|attempt| attempt.request_cancellation(interrupt.proof()))
                .ok()?,
            StoredActiveTurnPhase::AwaitingApproval { .. }
            | StoredActiveTurnPhase::AwaitingChild { .. }
            | StoredActiveTurnPhase::AwaitingToolRecovery { .. }
            | StoredActiveTurnPhase::AwaitingModelCallRecovery { .. }
            | StoredActiveTurnPhase::AwaitingRunnerRecovery { .. }
            | StoredActiveTurnPhase::AwaitingCredentialAvailability { .. } => return None,
        };
        Some(ActiveTurnPhase::Running { current_attempt })
    }
}

/// One accepted input inside an active turn's claimed session tail.
///
/// The repeated session, immutable delivery request, acceptance position, and
/// current disposition are inert facts. They become a canonical tail entry
/// only after the scheduling seam validates the complete interval and every
/// disposition correlation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SessionAcceptanceTailEntryState {
    RuntimeRelevant,
    RetiredGoalOrigin,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionAcceptanceTailEntryReconstitutionInput {
    pub(super) session: SessionId,
    pub(super) accepted_input: AcceptedInputLifecycle,
    pub(super) position: SessionInputPosition,
    pub(super) delivery: DeliveryRequest,
    pub(super) state: SessionAcceptanceTailEntryState,
}

impl SessionAcceptanceTailEntryReconstitutionInput {
    /// Supplies the exact stored facts for one accepted input.
    pub const fn new(
        session: SessionId,
        accepted_input: AcceptedInputLifecycle,
        position: SessionInputPosition,
        delivery: DeliveryRequest,
    ) -> Self {
        Self {
            session,
            accepted_input,
            position,
            delivery,
            state: SessionAcceptanceTailEntryState::RuntimeRelevant,
        }
    }

    /// Supplies an immutable goal origin retired from runtime scheduling.
    ///
    /// The scheduling seam admits this marker only for an `OriginOf` input
    /// whose correlated goal turn is absent from the runtime turn inventory.
    pub const fn retired_goal_origin(
        session: SessionId,
        accepted_input: AcceptedInputLifecycle,
        position: SessionInputPosition,
        delivery: DeliveryRequest,
    ) -> Self {
        Self {
            session,
            accepted_input,
            position,
            delivery,
            state: SessionAcceptanceTailEntryState::RetiredGoalOrigin,
        }
    }

    /// Returns the stored owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Borrows the accepted input and its current disposition.
    pub const fn accepted_input(&self) -> &AcceptedInputLifecycle {
        &self.accepted_input
    }

    /// Returns the immutable session acceptance position.
    pub const fn position(&self) -> SessionInputPosition {
        self.position
    }

    /// Returns the immutable delivery request.
    pub const fn delivery(&self) -> DeliveryRequest {
        self.delivery
    }
}

/// Claimed complete accepted-input interval for one active turn.
///
/// The interval begins at the owning turn's exact origin and ends at the
/// authoritative last session position observed by the same read. A filtered
/// pending-steering list or a bare maximum position cannot substitute for
/// these ordered facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionAcceptanceTailReconstitutionInput {
    pub(super) session: SessionId,
    pub(super) anchor: AcceptedInputId,
    pub(super) observed_last_position: SessionInputPosition,
    pub(super) entries: Vec<SessionAcceptanceTailEntryReconstitutionInput>,
}

impl SessionAcceptanceTailReconstitutionInput {
    /// Supplies one claimed complete session-scoped interval.
    pub fn new(
        session: SessionId,
        anchor: AcceptedInputId,
        observed_last_position: SessionInputPosition,
        entries: Vec<SessionAcceptanceTailEntryReconstitutionInput>,
    ) -> Self {
        Self {
            session,
            anchor,
            observed_last_position,
            entries,
        }
    }

    /// Returns the session whose observation supplied the interval.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the accepted-input identity anchoring the interval.
    pub const fn anchor(&self) -> AcceptedInputId {
        self.anchor
    }

    /// Returns the authoritative last position claimed by the observation.
    pub const fn observed_last_position(&self) -> SessionInputPosition {
        self.observed_last_position
    }

    /// Returns every ordered entry supplied for validation.
    pub fn entries(&self) -> &[SessionAcceptanceTailEntryReconstitutionInput] {
        &self.entries
    }
}

/// Complete stored subject facts for one consumed steering semantic entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConsumedSteeringReconstitutionInput {
    pub(super) session: SessionId,
    pub(super) accepted_input: AcceptedInputLifecycle,
    pub(super) acceptance_position: SessionInputPosition,
    pub(super) source_turn: TurnId,
}

impl ConsumedSteeringReconstitutionInput {
    /// Supplies the accepted input's exact stored consumption and source turn.
    pub const fn new(
        session: SessionId,
        accepted_input: AcceptedInputLifecycle,
        acceptance_position: SessionInputPosition,
        source_turn: TurnId,
    ) -> Self {
        Self {
            session,
            accepted_input,
            acceptance_position,
            source_turn,
        }
    }

    /// Returns the stored owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Borrows the accepted input and its consumed disposition.
    pub const fn accepted_input(&self) -> &AcceptedInputLifecycle {
        &self.accepted_input
    }

    /// Returns the accepted input's immutable session position.
    pub const fn acceptance_position(&self) -> SessionInputPosition {
        self.acceptance_position
    }

    /// Returns the exact turn the input was accepted to steer.
    pub const fn source_turn(&self) -> TurnId {
        self.source_turn
    }
}

/// Complete stored tool-round result evidence for one steering-consuming call
/// prepared at a tool-round continuation boundary.
///
/// A continuation call's frontier extends the round's completed producing
/// call by that call's proposals and one batch-correlated result entry per
/// request in proposal order before the consumed steering suffix, so the
/// consumed-steering frontier law validates that window from this evidence. A
/// call prepared against its turn's starting frontier never carries this
/// evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SteeringContinuationRoundReconstitutionInput {
    call: crate::ModelCallId,
    round_tool_attempts: Vec<crate::EndedToolAttempt>,
    round_tool_denials: Vec<ToolApprovalResolution>,
}

impl SteeringContinuationRoundReconstitutionInput {
    /// Supplies the consuming call with its round's complete independently
    /// checked terminal tool attempts and user-sourced denial resolutions.
    pub const fn new(
        call: crate::ModelCallId,
        round_tool_attempts: Vec<crate::EndedToolAttempt>,
        round_tool_denials: Vec<ToolApprovalResolution>,
    ) -> Self {
        Self {
            call,
            round_tool_attempts,
            round_tool_denials,
        }
    }

    /// Returns the steering-consuming continuation call.
    pub const fn call(&self) -> crate::ModelCallId {
        self.call
    }

    /// Borrows every terminal tool attempt backing the round's result window.
    pub fn round_tool_attempts(&self) -> &[crate::EndedToolAttempt] {
        &self.round_tool_attempts
    }

    /// Borrows every user denial resolution backing the round's `ToolDenied`
    /// entries.
    pub fn round_tool_denials(&self) -> &[ToolApprovalResolution] {
        &self.round_tool_denials
    }
}

/// Complete stored tool-round result evidence for one steering-free call
/// prepared at a tool-round continuation boundary and named by a terminal or
/// recovery gate.
///
/// Such a call's whole frontier is the round's completed producing call's
/// frontier extended by that call's proposals and one batch-correlated result
/// entry per request in proposal order, with no trailing suffix, so the gate
/// naming the call validates that window from this evidence. A named call
/// whose frontier is its turn's starting frontier never carries this
/// evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContinuationRoundReconstitutionInput {
    call: crate::ModelCallId,
    round_tool_attempts: Vec<crate::EndedToolAttempt>,
    round_tool_denials: Vec<ToolApprovalResolution>,
}

impl ContinuationRoundReconstitutionInput {
    /// Supplies the named call with its round's complete independently
    /// checked terminal tool attempts and user-sourced denial resolutions.
    pub const fn new(
        call: crate::ModelCallId,
        round_tool_attempts: Vec<crate::EndedToolAttempt>,
        round_tool_denials: Vec<ToolApprovalResolution>,
    ) -> Self {
        Self {
            call,
            round_tool_attempts,
            round_tool_denials,
        }
    }

    /// Returns the gate-named continuation call.
    pub const fn call(&self) -> crate::ModelCallId {
        self.call
    }

    /// Borrows every terminal tool attempt backing the round's result window.
    pub fn round_tool_attempts(&self) -> &[crate::EndedToolAttempt] {
        &self.round_tool_attempts
    }

    /// Borrows every user denial resolution backing the round's `ToolDenied`
    /// entries.
    pub fn round_tool_denials(&self) -> &[ToolApprovalResolution] {
        &self.round_tool_denials
    }
}

/// Complete purpose-specific stored facts for one session's scheduling read.
///
/// The input owns the already-checked current [`Session`], every currently
/// known accepted-input turn record, and complete semantic-entry and snapshot
/// collections needed by any stored start or failed-terminal frontier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputSchedulingReconstitutionInput {
    pub(super) inadmissible_requests: Vec<crate::ToolRequest>,
    pub(super) runner_placement_frontiers: Vec<ContextFrontierId>,
    pub(super) session: Session,
    pub(super) imported_session: Option<ReconstitutedImportedSession>,
    pub(super) turns: Vec<AcceptedInputTurnSchedulingRecord>,
    pub(super) semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
    pub(super) snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
    pub(super) pinned_targets: Vec<crate::PinnedProviderTargetReconstitutionInput>,
    pub(super) model_calls: Vec<crate::ModelCallReconstitutionInput>,
    pub(super) compaction_calls: Vec<crate::ContextCompactionModelCallReconstitutionInput>,
    pub(super) compactions: Vec<crate::ContextCompactionReconstitutionInput>,
    pub(super) consumed_steering: Vec<ConsumedSteeringReconstitutionInput>,
    pub(super) delegated_consumed_steering: Vec<ConsumedSteeringReconstitutionInput>,
    pub(super) delegated_turns: Vec<DelegatedTurnSchedulingFact>,
    pub(super) steering_continuation_rounds: Vec<SteeringContinuationRoundReconstitutionInput>,
    pub(super) continuation_rounds: Vec<ContinuationRoundReconstitutionInput>,
    pub(super) active_acceptance_tail: Option<SessionAcceptanceTailReconstitutionInput>,
    pub(super) preceding_non_accepted_terminals: Vec<(
        SessionId,
        TurnId,
        TurnId,
        ContextFrontierId,
        DirectModelSelection,
    )>,
}

impl AcceptedInputSchedulingReconstitutionInput {
    /// Supplies request-level terminal reasons referenced by this projection.
    pub fn with_inadmissible_requests(mut self, requests: Vec<crate::ToolRequest>) -> Self {
        self.inadmissible_requests = requests;
        self
    }

    /// Supplies every checked placement boundary in placement-revision order.
    pub fn with_runner_placement_frontiers(mut self, frontiers: Vec<ContextFrontierId>) -> Self {
        self.runner_placement_frontiers = frontiers;
        self
    }
    /// Supplies one complete typed scheduling projection.
    pub fn new(
        session: Session,
        turns: Vec<AcceptedInputTurnSchedulingRecord>,
        semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
        snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
        active_acceptance_tail: Option<SessionAcceptanceTailReconstitutionInput>,
    ) -> Self {
        Self {
            session,
            imported_session: None,
            runner_placement_frontiers: Vec::new(),
            inadmissible_requests: Vec::new(),
            turns,
            semantic_entries,
            snapshots,
            pinned_targets: Vec::new(),
            model_calls: Vec::new(),
            compaction_calls: Vec::new(),
            compactions: Vec::new(),
            consumed_steering: Vec::new(),
            delegated_consumed_steering: Vec::new(),
            delegated_turns: Vec::new(),
            steering_continuation_rounds: Vec::new(),
            continuation_rounds: Vec::new(),
            active_acceptance_tail,
            preceding_non_accepted_terminals: Vec::new(),
        }
    }

    /// Supplies one immediate terminal predecessor that is not an
    /// accepted-input-origin turn, its exact accepted-input successor, and its
    /// retained terminal frontier and selected model identity. Repeated calls
    /// retain distinct predecessor chains.
    pub fn with_preceding_non_accepted_terminal(
        mut self,
        session: SessionId,
        predecessor: TurnId,
        successor: TurnId,
        terminal_frontier: ContextFrontierId,
        selected: DirectModelSelection,
    ) -> Self {
        self.preceding_non_accepted_terminals.push((
            session,
            predecessor,
            successor,
            terminal_frontier,
            selected,
        ));
        self
    }

    /// Supplies the complete independently checked imported seed projection
    /// required by an imported session.
    pub fn with_imported_session(mut self, imported_session: ReconstitutedImportedSession) -> Self {
        self.imported_session = Some(imported_session);
        self
    }

    /// Supplies the independently stored turn-level targets and complete call
    /// facts referenced by this scheduling projection.
    pub fn with_model_call_facts(
        mut self,
        pinned_targets: Vec<crate::PinnedProviderTargetReconstitutionInput>,
        model_calls: Vec<crate::ModelCallReconstitutionInput>,
    ) -> Self {
        self.pinned_targets = pinned_targets;
        self.model_calls = model_calls;
        self
    }

    /// Supplies every dedicated compaction call and correlated compaction.
    pub fn with_context_compaction_facts(
        mut self,
        calls: Vec<crate::ContextCompactionModelCallReconstitutionInput>,
        compactions: Vec<crate::ContextCompactionReconstitutionInput>,
    ) -> Self {
        self.compaction_calls = calls;
        self.compactions = compactions;
        self
    }

    /// Supplies every independently stored consumed-steering subject fact.
    pub fn with_consumed_steering_facts(
        mut self,
        consumed_steering: Vec<ConsumedSteeringReconstitutionInput>,
    ) -> Self {
        self.consumed_steering = consumed_steering;
        self
    }

    /// Supplies consumed steering whose source is a delegation-origin turn
    /// outside this accepted-input scheduling projection.
    pub fn with_delegated_consumed_steering_facts(
        mut self,
        consumed_steering: Vec<ConsumedSteeringReconstitutionInput>,
    ) -> Self {
        self.delegated_consumed_steering = consumed_steering;
        self
    }

    /// Supplies delegation-origin turns outside this accepted-input projection
    /// whose semantic entries and completed model calls remain in its frontiers.
    pub fn with_delegated_turn_facts(
        mut self,
        delegated_turns: Vec<DelegatedTurnSchedulingFact>,
    ) -> Self {
        self.delegated_turns = delegated_turns;
        self
    }

    /// Supplies the complete tool-round result evidence for every
    /// steering-consuming call prepared at a continuation boundary.
    pub fn with_steering_continuation_rounds(
        mut self,
        steering_continuation_rounds: Vec<SteeringContinuationRoundReconstitutionInput>,
    ) -> Self {
        self.steering_continuation_rounds = steering_continuation_rounds;
        self
    }

    /// Supplies the complete tool-round result evidence for every
    /// steering-free continuation call a terminal or recovery gate names.
    pub fn with_continuation_rounds(
        mut self,
        continuation_rounds: Vec<ContinuationRoundReconstitutionInput>,
    ) -> Self {
        self.continuation_rounds = continuation_rounds;
        self
    }

    /// Borrows the complete current-session snapshot.
    pub const fn session(&self) -> &Session {
        &self.session
    }

    /// Borrows the complete imported seed projection, when supplied.
    pub const fn imported_session(&self) -> Option<&ReconstitutedImportedSession> {
        self.imported_session.as_ref()
    }

    /// Returns every stored turn record supplied as complete.
    pub fn turns(&self) -> &[AcceptedInputTurnSchedulingRecord] {
        &self.turns
    }

    /// Returns every stored initial semantic entry supplied as complete.
    pub fn semantic_entries(&self) -> &[SemanticTranscriptEntryReconstitutionInput] {
        &self.semantic_entries
    }

    /// Returns every stored context snapshot supplied as complete.
    pub fn snapshots(&self) -> &[ResolvedContextFrontierReconstitutionInput] {
        &self.snapshots
    }

    /// Returns every model call required by terminal semantic content.
    pub fn model_calls(&self) -> &[crate::ModelCallReconstitutionInput] {
        &self.model_calls
    }

    /// Returns every independently stored turn-level target fact.
    pub fn pinned_targets(&self) -> &[crate::PinnedProviderTargetReconstitutionInput] {
        &self.pinned_targets
    }

    /// Returns every consumed-steering subject fact supplied as complete.
    pub fn consumed_steering(&self) -> &[ConsumedSteeringReconstitutionInput] {
        &self.consumed_steering
    }

    /// Returns consumed steering owned by delegation-origin turns.
    pub fn delegated_consumed_steering(&self) -> &[ConsumedSteeringReconstitutionInput] {
        &self.delegated_consumed_steering
    }

    /// Returns delegation-origin turns represented only by semantic facts.
    pub fn delegated_turns(&self) -> &[DelegatedTurnSchedulingFact] {
        &self.delegated_turns
    }

    /// Returns every steering continuation-round evidence fact supplied.
    pub fn steering_continuation_rounds(&self) -> &[SteeringContinuationRoundReconstitutionInput] {
        &self.steering_continuation_rounds
    }

    /// Returns every gate-named continuation-round evidence fact supplied.
    pub fn continuation_rounds(&self) -> &[ContinuationRoundReconstitutionInput] {
        &self.continuation_rounds
    }

    /// Borrows the claimed complete tail required by an active turn.
    pub const fn active_acceptance_tail(
        &self,
    ) -> Option<&SessionAcceptanceTailReconstitutionInput> {
        self.active_acceptance_tail.as_ref()
    }

    /// Reconstructs the canonical scheduling projection without effects.
    pub fn reconstitute(
        self,
    ) -> Result<AcceptedInputSchedulingProjection, AcceptedInputSchedulingReconstitutionError> {
        reconstitute(self)
    }
}
