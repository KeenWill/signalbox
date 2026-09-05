//! Complete accepted-input scheduling projection and pure eligibility.
//!
//! docs/spec/turn-lifecycle-and-scheduling.md,
//! docs/spec/sessions-and-transcript.md, and
//! docs/spec/persistence-protocol.md are normative. This purpose-specific
//! projection reconstructs every fact that can change accepted-input
//! eligibility or slot ownership in the implemented semantic-entry slice. It
//! supports an ancestry-free session or a fully reconstituted imported seed
//! whose durable total order consists of a terminal prefix, at most one active
//! slot, and a queued suffix.
//!
//! Active records carry one exact checked phase and a validated,
//! session-scoped acceptance tail. Prepared and running attempts need no
//! external evidence; stop-requested and recovery phases require their complete
//! correlated model-call and applied-interrupt facts.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU64,
};

use crate::context_frontier::ContextFrontierEntryValidationCache;
use crate::model_execution::reclassify_pending_steering_inputs;
use crate::{
    AcceptedInputDisposition, AcceptedInputId, AcceptedInputLifecycle, AcceptedInputQueueOrder,
    AcceptedInputQueueOrderError, AcceptedInputQueuePriority, AcceptedInputQueueWork,
    AcceptedInputStartingLineage, AcceptedInputTurnStart, ActiveTurnPhase,
    AppliedInterruptCommandResult, AttemptEnd, CancellationStopDisposition, ChildWait,
    ContextFrontierId, ContextFrontierProjection, ContextFrontierProjectionFailure,
    CurrentTurnAttempt, DelegationContent, DelegationWaitMode, DeliveryRequest,
    DirectModelSelection, EndedTurnAttempt, InitialSemanticTranscriptEntryPayload,
    ModelCallDisposition, NonEmptyIssuedOperationRefs, OriginConfiguration,
    PendingSteeringReclassificationIdentity, ReclassifiedPendingSteeringTurn,
    ReconstitutedImportedSession, ReconstitutedModelCall,
    ResolvedContextFrontierReconstitutionInput, ResolvedContextFrontierSnapshot,
    SemanticTranscriptEntry, SemanticTranscriptEntryId, SemanticTranscriptEntryPayload,
    SemanticTranscriptEntryReconstitutionInput, SemanticTranscriptEntryRef, Session, SessionId,
    SessionInputPosition, ToolApprovalDecision, ToolApprovalResolution, ToolRequestId,
    TranscriptAncestry, TurnAttemptId, TurnConfigurationProvenance, TurnDisposition, TurnId,
    UnstoppedAttemptDisposition, derive_accepted_input_total_order,
};

/// The lifecycle fact stored for one accepted-input scheduling record.
///
/// Started variants name raw lineage and snapshot identities only as
/// reconstitution candidates. They become opaque [`AcceptedInputTurnStart`]
/// values solely after collection-wide validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AcceptedInputTurnSchedulingRecordState {
    /// No start, semantic origin entry, snapshot, or attempt exists.
    Queued,
    /// The turn owns the session's progressing slot.
    Active {
        /// The stored lineage selected at eligibility.
        starting_lineage: AcceptedInputStartingLineage,
        /// The stored starting snapshot identity.
        starting_frontier: ContextFrontierId,
        /// The exact phase and its asserted owning turn.
        phase: ActiveTurnSchedulingReconstitutionInput,
    },
    /// The turn reached a known-failure disposition.
    TerminalFailed {
        /// The stored lineage selected at eligibility.
        starting_lineage: AcceptedInputStartingLineage,
        /// The stored starting snapshot identity.
        starting_frontier: ContextFrontierId,
        /// The complete terminal execution provenance, when the failure
        /// followed a physical attempt.
        terminal_execution: Option<FailedTurnExecutionReconstitutionInput>,
        /// The complete frontier through the appended failed marker.
        terminal_frontier: ContextFrontierId,
    },
    /// The turn committed a complete assistant response and completion marker.
    TerminalCompleted {
        /// The stored lineage selected at eligibility.
        starting_lineage: AcceptedInputStartingLineage,
        /// The stored starting snapshot identity.
        starting_frontier: ContextFrontierId,
        /// The ended physical attempt that supplied the completed call.
        completing_attempt: TurnAttemptId,
        /// The complete stored end classification for that attempt.
        completing_attempt_end: TerminalAttemptEndReconstitutionInput,
        /// The outcome-authoritative call that completed the turn.
        completing_call: crate::ModelCallId,
        /// The complete frontier through the final completion marker.
        terminal_frontier: ContextFrontierId,
    },
    /// The turn committed an explicit refusal without semantic response content.
    TerminalRefused {
        /// The stored lineage selected at eligibility.
        starting_lineage: AcceptedInputStartingLineage,
        /// The stored starting snapshot identity.
        starting_frontier: ContextFrontierId,
        /// The ended physical attempt that supplied the refusal.
        refusing_attempt: TurnAttemptId,
        /// The complete stored end classification for that attempt.
        refusing_attempt_end: TerminalAttemptEndReconstitutionInput,
        /// The outcome-authoritative call that refused the request.
        refusing_call: crate::ModelCallId,
        /// The equal-content terminal frontier identifying the turn boundary.
        terminal_frontier: ContextFrontierId,
    },
    /// The turn ended from one exactly applied and confirmed interrupt.
    TerminalCancelled {
        /// The stored lineage selected at eligibility.
        starting_lineage: AcceptedInputStartingLineage,
        /// The stored starting snapshot identity.
        starting_frontier: ContextFrontierId,
        /// The complete proof-bearing terminal execution provenance.
        terminal_execution: CancelledTurnExecutionReconstitutionInput,
        /// The complete frontier through the cancellation marker.
        terminal_frontier: ContextFrontierId,
    },
    /// The turn released its slot while one interrupted call remains
    /// durably ambiguous.
    TerminalReconciliationRequired {
        /// The stored lineage selected at eligibility.
        starting_lineage: AcceptedInputStartingLineage,
        /// The stored starting snapshot identity.
        starting_frontier: ContextFrontierId,
        /// The ended attempt that owns the ambiguous call.
        reconciling_attempt: TurnAttemptId,
        /// The preserved stored end classification for that attempt.
        reconciling_attempt_end: TerminalAttemptEndReconstitutionInput,
        /// The exact ambiguous physical call.
        ambiguous_call: crate::ModelCallId,
        /// The exact durable authority that requires reconciliation.
        authority: AutomaticReconciliationAuthority,
        /// The equal-content terminal frontier identifying the turn boundary.
        terminal_frontier: ContextFrontierId,
    },
    /// The turn released its slot while one interrupted tool attempt remains
    /// durably ambiguous.
    TerminalToolReconciliationRequired {
        /// The stored lineage selected at eligibility.
        starting_lineage: AcceptedInputStartingLineage,
        /// The stored starting snapshot identity.
        starting_frontier: ContextFrontierId,
        /// The ended turn attempt that owns the ambiguous tool attempt.
        reconciling_attempt: TurnAttemptId,
        /// The preserved stored end classification for that attempt.
        reconciling_attempt_end: TerminalAttemptEndReconstitutionInput,
        /// Complete checked batch carrying the exact ambiguous tool attempt.
        tool_batch: crate::ToolBatch,
        /// The exact durable authority that requires reconciliation.
        authority: AutomaticReconciliationAuthority,
        /// The exact proposal-ordered result-suffix terminal frontier.
        terminal_frontier: ContextFrontierId,
    },
}

/// Durable authority for one automatic reconciliation terminal boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutomaticReconciliationAuthority {
    /// A later or already-applied interrupt left the operation ambiguous.
    AppliedInterrupt(AppliedInterruptCommandResult),
    /// The daemon spent one recorded automatic recovery attempt.
    AutomaticRecovery {
        /// The one-based durable recovery attempt that terminalized the turn.
        attempt: std::num::NonZeroU32,
    },
}

/// Stored lifecycle classification for one delegation-origin turn retained by
/// an accepted-input scheduling projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelegatedTurnSchedulingState {
    /// The delegated turn still owns its physical runtime slot.
    Active,
    /// Parent-command authority made the delegated turn logically terminal
    /// without rewriting its retained physical lifecycle state.
    RuntimeTerminal,
    /// The delegated turn completed with delivered assistant content.
    TerminalCompleted,
    /// The delegated turn completed with an explicit refusal.
    TerminalRefused,
    /// The delegated turn ended with a known failure.
    TerminalFailed,
    /// The delegated turn ended from applied cancellation authority.
    TerminalCancelled,
    /// The delegated turn ended with unresolved physical ambiguity.
    TerminalReconciliationRequired,
}

/// Complete configuration and lifecycle facts for one delegation-origin turn
/// referenced outside the accepted-input turn collection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelegatedTurnSchedulingFact {
    turn: TurnId,
    defaults_version: crate::SessionConfigurationDefaultsVersion,
    selected: DirectModelSelection,
    state: DelegatedTurnSchedulingState,
}

impl DelegatedTurnSchedulingFact {
    /// Records the exact stored configuration and lifecycle projection.
    pub const fn new(
        turn: TurnId,
        defaults_version: crate::SessionConfigurationDefaultsVersion,
        selected: DirectModelSelection,
        state: DelegatedTurnSchedulingState,
    ) -> Self {
        Self {
            turn,
            defaults_version,
            selected,
            state,
        }
    }

    /// Returns the delegation-origin turn identity.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the defaults epoch frozen by the delegated origin.
    pub const fn defaults_version(&self) -> crate::SessionConfigurationDefaultsVersion {
        self.defaults_version
    }

    /// Returns the exact selected direct model frozen by the delegated origin.
    pub const fn selected(&self) -> DirectModelSelection {
        self.selected
    }

    /// Returns the stored lifecycle classification.
    pub const fn state(&self) -> DelegatedTurnSchedulingState {
        self.state
    }
}

/// Correlated stored execution provenance for one failed terminal turn.
///
/// The optional enclosing value distinguishes a direct failure with no
/// physical attempt. This shape always names an ended attempt, and can name a
/// terminal call only together with that attempt, so call-only provenance is
/// unrepresentable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailedTurnExecutionReconstitutionInput {
    owning_turn: TurnId,
    ended_attempt: TurnAttemptId,
    attempt_end: TerminalAttemptEndReconstitutionInput,
    ended_call: Option<crate::ModelCallId>,
    terminal_tool_attempts: Vec<crate::EndedToolAttempt>,
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
    owning_turn: TurnId,
    ended_attempt: TurnAttemptId,
    attempt_end: TerminalAttemptEndReconstitutionInput,
    ended_call: Option<crate::ModelCallId>,
    interrupt: AppliedInterruptCommandResult,
    terminal_tool_attempts: Vec<crate::EndedToolAttempt>,
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
    owning_turn: TurnId,
    current_attempt: Option<TurnAttemptId>,
    state: StoredActiveTurnPhase,
    executing_tool_batch: Option<ExecutingToolBatchReconstitutionFacts>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExecutingToolBatchReconstitutionFacts {
    session: SessionId,
    producing_call: crate::ModelCallId,
    yielded_snapshot: ResolvedContextFrontierSnapshot,
    batch_attempt: Option<TurnAttemptId>,
    awaiting_request: Option<ToolRequestId>,
    requests: Box<[ToolRequestId]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum StoredActiveTurnPhase {
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
    phase: ActiveTurnSchedulingReconstitutionInput,
    pinned_target: crate::PinnedProviderTargetReconstitutionInput,
    call: crate::ModelCallReconstitutionInput,
    source_snapshot: ResolvedContextFrontierReconstitutionInput,
    pending_steering: Vec<PendingSteeringInput>,
    consumed_steering: Vec<ConsumedSteeringReconstitutionInput>,
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

    /// Returns the turn named as owner by the active-phase record.
    pub const fn owning_turn(&self) -> TurnId {
        self.owning_turn
    }

    fn canonical_evidence_free_phase(&self) -> Option<ActiveTurnPhase> {
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
            | StoredActiveTurnPhase::AwaitingRunnerRecovery { .. } => return None,
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
enum SessionAcceptanceTailEntryState {
    RuntimeRelevant,
    RetiredGoalOrigin,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionAcceptanceTailEntryReconstitutionInput {
    session: SessionId,
    accepted_input: AcceptedInputLifecycle,
    position: SessionInputPosition,
    delivery: DeliveryRequest,
    state: SessionAcceptanceTailEntryState,
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
    session: SessionId,
    anchor: AcceptedInputId,
    observed_last_position: SessionInputPosition,
    entries: Vec<SessionAcceptanceTailEntryReconstitutionInput>,
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
    session: SessionId,
    accepted_input: AcceptedInputLifecycle,
    acceptance_position: SessionInputPosition,
    source_turn: TurnId,
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

/// One validated accepted input in an active turn's session tail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionAcceptanceTailEntry {
    accepted_input: AcceptedInputLifecycle,
    position: SessionInputPosition,
    delivery: DeliveryRequest,
}

/// One pending steering input proven by the complete active-session tail.
///
/// Construction stays inside checked scheduling reconstitution so an input's
/// identity, source-turn binding, and immutable acceptance position cannot be
/// cross-wired at an execution or terminalization boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingSteeringInput {
    accepted_input: AcceptedInputLifecycle,
    acceptance_position: SessionInputPosition,
}

/// One consumed steering input proven by the complete active-session tail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConsumedSteeringInput {
    accepted_input: AcceptedInputLifecycle,
    acceptance_position: SessionInputPosition,
    source_turn: TurnId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActiveModelCallRecoveryWait {
    call: crate::EndedModelCall,
    attempt: EndedTurnAttempt,
    source_snapshot: ResolvedContextFrontierSnapshot,
}

impl ConsumedSteeringInput {
    /// Returns the accepted input already consumed by a prepared call.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        self.accepted_input.id()
    }

    /// Borrows the exact checked consumed lifecycle.
    pub const fn lifecycle(&self) -> &AcceptedInputLifecycle {
        &self.accepted_input
    }

    /// Returns the immutable session acceptance position.
    pub const fn acceptance_position(&self) -> SessionInputPosition {
        self.acceptance_position
    }

    /// Returns the exact active turn this input was accepted to steer.
    pub const fn source_turn(&self) -> TurnId {
        self.source_turn
    }
}

impl PendingSteeringInput {
    /// Reconstitutes one pending tail member bound to its exact active turn.
    pub fn reconstitute(
        accepted_input: AcceptedInputLifecycle,
        acceptance_position: SessionInputPosition,
        source_turn: TurnId,
    ) -> Option<Self> {
        matches!(
            accepted_input.disposition(),
            AcceptedInputDisposition::PendingSteering { binding }
                if binding.source_turn() == source_turn
        )
        .then_some(Self {
            accepted_input,
            acceptance_position,
        })
    }

    /// Returns the accepted input awaiting disposition.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        self.accepted_input.id()
    }

    /// Borrows the exact checked pending lifecycle.
    pub const fn lifecycle(&self) -> &AcceptedInputLifecycle {
        &self.accepted_input
    }

    /// Returns the immutable session acceptance position.
    pub const fn acceptance_position(&self) -> SessionInputPosition {
        self.acceptance_position
    }
}

/// Canonical complete accepted-input interval for one active turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionAcceptanceTail {
    session: SessionId,
    anchor: AcceptedInputId,
    observed_last_position: SessionInputPosition,
    entries: Box<[SessionAcceptanceTailEntry]>,
}

impl SessionAcceptanceTail {
    pub(crate) const fn observed_last_position(&self) -> SessionInputPosition {
        self.observed_last_position
    }
}

/// Complete checked values supplied for one accepted-input scheduling record.
///
/// Repeated session and turn correlations retain independently stored facts so
/// reconstitution rejects cross-wired accepted-input and queue records.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputTurnSchedulingRecord {
    stored_session: SessionId,
    turn: TurnId,
    accepted_input_session: SessionId,
    accepted_input: AcceptedInputLifecycle,
    queue_session: SessionId,
    queue_turn: TurnId,
    order: AcceptedInputQueueOrder,
    origin_delivery: DeliveryRequest,
    origin_configuration: OriginConfiguration,
    configuration_provenance: TurnConfigurationProvenance,
    model_identity_boundary_required: bool,
    state: AcceptedInputTurnSchedulingRecordState,
}

impl AcceptedInputTurnSchedulingRecord {
    /// Supplies all typed stored facts for one scheduling record.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stored_session: SessionId,
        turn: TurnId,
        accepted_input_session: SessionId,
        accepted_input: AcceptedInputLifecycle,
        queue_session: SessionId,
        queue_turn: TurnId,
        order: AcceptedInputQueueOrder,
        origin_delivery: DeliveryRequest,
        origin_configuration: OriginConfiguration,
        state: AcceptedInputTurnSchedulingRecordState,
    ) -> Self {
        Self {
            stored_session,
            turn,
            accepted_input_session,
            accepted_input,
            queue_session,
            queue_turn,
            order,
            origin_delivery,
            configuration_provenance: TurnConfigurationProvenance::ExplicitOrigin(
                origin_configuration.clone(),
            ),
            origin_configuration,
            model_identity_boundary_required: true,
            state,
        }
    }

    /// Supplies a reclassified steering origin using its immutable receipt,
    /// original position, source binding, and source-derived configuration.
    #[allow(clippy::too_many_arguments)]
    pub fn reclassified(
        stored_session: SessionId,
        turn: TurnId,
        accepted_input_session: SessionId,
        accepted_input: AcceptedInputLifecycle,
        queue_session: SessionId,
        queue_turn: TurnId,
        order: AcceptedInputQueueOrder,
        origin_delivery: DeliveryRequest,
        binding: crate::SteeringBinding,
        source_configuration: OriginConfiguration,
        state: AcceptedInputTurnSchedulingRecordState,
    ) -> Self {
        Self {
            stored_session,
            turn,
            accepted_input_session,
            accepted_input,
            queue_session,
            queue_turn,
            order,
            origin_delivery,
            origin_configuration: source_configuration,
            configuration_provenance: TurnConfigurationProvenance::InheritedForReclassifiedSteering(
                binding,
            ),
            model_identity_boundary_required: true,
            state,
        }
    }

    /// Marks a started record as predating durable model-identity boundaries.
    ///
    /// This is only for reconstituting frontiers committed before the boundary
    /// law existed. Newly accepted queued work remains subject to the law.
    pub fn without_legacy_model_identity_boundary(mut self) -> Self {
        self.model_identity_boundary_required = false;
        self
    }

    /// Returns the session identity on the stored turn record.
    pub const fn stored_session(&self) -> SessionId {
        self.stored_session
    }

    /// Returns the stored turn identity.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the session identity on the accepted-input record.
    pub const fn accepted_input_session(&self) -> SessionId {
        self.accepted_input_session
    }

    /// Borrows the accepted input and its exact stored disposition.
    pub const fn accepted_input(&self) -> &AcceptedInputLifecycle {
        &self.accepted_input
    }

    /// Returns the session identity on the queue record.
    pub const fn queue_session(&self) -> SessionId {
        self.queue_session
    }

    /// Returns the turn identity on the queue record.
    pub const fn queue_turn(&self) -> TurnId {
        self.queue_turn
    }

    /// Returns the immutable queue-order facts.
    pub const fn order(&self) -> AcceptedInputQueueOrder {
        self.order
    }

    /// Returns the immutable accepted delivery that created this origin.
    pub const fn origin_delivery(&self) -> DeliveryRequest {
        self.origin_delivery
    }

    /// Borrows the complete canonical configuration, whether explicit or
    /// inherited from reclassified steering's source turn.
    pub const fn origin_configuration(&self) -> &OriginConfiguration {
        &self.origin_configuration
    }

    /// Borrows the checked explicit or inherited configuration provenance.
    pub const fn configuration_provenance(&self) -> &TurnConfigurationProvenance {
        &self.configuration_provenance
    }

    /// Returns the stored lifecycle projection.
    pub const fn state(&self) -> &AcceptedInputTurnSchedulingRecordState {
        &self.state
    }
}

/// Complete purpose-specific stored facts for one session's scheduling read.
///
/// The input owns the already-checked current [`Session`], every currently
/// known accepted-input turn record, and complete semantic-entry and snapshot
/// collections needed by any stored start or failed-terminal frontier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputSchedulingReconstitutionInput {
    session: Session,
    imported_session: Option<ReconstitutedImportedSession>,
    turns: Vec<AcceptedInputTurnSchedulingRecord>,
    semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
    snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
    pinned_targets: Vec<crate::PinnedProviderTargetReconstitutionInput>,
    model_calls: Vec<crate::ModelCallReconstitutionInput>,
    compaction_calls: Vec<crate::ContextCompactionModelCallReconstitutionInput>,
    compactions: Vec<crate::ContextCompactionReconstitutionInput>,
    consumed_steering: Vec<ConsumedSteeringReconstitutionInput>,
    delegated_consumed_steering: Vec<ConsumedSteeringReconstitutionInput>,
    delegated_turns: Vec<DelegatedTurnSchedulingFact>,
    steering_continuation_rounds: Vec<SteeringContinuationRoundReconstitutionInput>,
    continuation_rounds: Vec<ContinuationRoundReconstitutionInput>,
    active_acceptance_tail: Option<SessionAcceptanceTailReconstitutionInput>,
    preceding_non_accepted_terminals: Vec<(
        SessionId,
        TurnId,
        TurnId,
        ContextFrontierId,
        DirectModelSelection,
    )>,
}

impl AcceptedInputSchedulingReconstitutionInput {
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

/// Why complete stored facts cannot reconstruct the closed scheduling model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AcceptedInputSchedulingReconstitutionFailure {
    /// This slice cannot resolve a first frontier from native session
    /// ancestry.
    UnsupportedSessionAncestry,
    /// An imported session omitted its complete checked seed projection.
    MissingImportedSession,
    /// A non-imported session supplied an imported seed projection.
    UnexpectedImportedSession,
    /// The supplied imported projection is not the exact current session.
    ImportedSessionMismatch,
    /// A semantic entry was supplied outside its purpose-specific producer.
    UnsupportedSemanticEntry {
        /// The affected entry.
        entry: SemanticTranscriptEntryId,
    },
    /// One turn record belongs to a different session.
    TurnSessionMismatch {
        /// The cross-wired turn.
        turn: TurnId,
    },
    /// One accepted-input record belongs to a different session.
    AcceptedInputSessionMismatch {
        /// The affected turn.
        turn: TurnId,
    },
    /// One queue record belongs to a different session.
    QueueSessionMismatch {
        /// The affected turn.
        turn: TurnId,
    },
    /// One queue record names a different turn.
    QueueTurnMismatch {
        /// The affected turn.
        turn: TurnId,
    },
    /// The accepted input is not the exact typed origin of its turn.
    AcceptedInputOriginMismatch {
        /// The affected turn.
        turn: TurnId,
    },
    /// One origin's accepted delivery contradicts its durable queue facts or
    /// historical target.
    OriginDeliveryMismatch {
        /// The affected turn.
        turn: TurnId,
    },
    /// Two turn records referenced the same accepted input.
    DuplicateAcceptedInput {
        /// The duplicated accepted input.
        accepted_input: crate::AcceptedInputId,
    },
    /// The immutable queue facts cannot form one durable total order.
    InvalidQueueOrder {
        /// The complete queue-order rejection.
        error: AcceptedInputQueueOrderError,
    },
    /// A semantic entry belongs to a different source session.
    SemanticEntrySourceSessionMismatch {
        /// The affected entry.
        entry: SemanticTranscriptEntryId,
    },
    /// The same source-qualified semantic entry appeared more than once.
    DuplicateSemanticEntry {
        /// The duplicated exact reference.
        entry: SemanticTranscriptEntryRef,
    },
    /// A semantic payload names no accepted input or turn in the projection.
    SemanticEntrySubjectMissing {
        /// The affected entry.
        entry: SemanticTranscriptEntryId,
    },
    /// A semantic payload disagrees with its subject's lifecycle state.
    SemanticEntryStateMismatch {
        /// The affected entry.
        entry: SemanticTranscriptEntryId,
    },
    /// More than one origin or failure entry names the same subject.
    DuplicateSemanticEntryForSubject {
        /// The later duplicate entry.
        entry: SemanticTranscriptEntryId,
    },
    /// A supplied delegation-origin turn duplicates another fact or belongs to
    /// the accepted-input projection.
    DelegatedTurnFactMismatch {
        /// The affected delegation-origin turn.
        turn: TurnId,
    },
    /// A consumed-steering subject fact belongs to another session.
    ConsumedSteeringSessionMismatch {
        /// The cross-wired accepted input.
        accepted_input: AcceptedInputId,
    },
    /// The same consumed accepted input appeared more than once.
    DuplicateConsumedSteering {
        /// The duplicated accepted input.
        accepted_input: AcceptedInputId,
    },
    /// A steering semantic entry has no exact consumed lifecycle/source fact.
    SteeringSemanticEntryMismatch {
        /// The affected semantic entry.
        entry: SemanticTranscriptEntryId,
    },
    /// A consumed lifecycle/source fact has no exact semantic entry or call.
    ConsumedSteeringMismatch {
        /// The affected accepted input.
        accepted_input: AcceptedInputId,
    },
    /// Continuation-round evidence duplicates a call or names a call that
    /// consumed no steering.
    SteeringContinuationRoundMismatch {
        /// The affected call.
        call: crate::ModelCallId,
    },
    /// Gate-named continuation-round evidence duplicates a call or names a
    /// call no terminal or recovery gate proved against it.
    ContinuationRoundMismatch {
        /// The affected call.
        call: crate::ModelCallId,
    },
    /// A semantic entry names a model call absent from the purpose-specific
    /// complete call facts.
    SemanticEntryCallMissing {
        /// The affected semantic entry.
        entry: SemanticTranscriptEntryId,
        /// The absent producing call.
        call: crate::ModelCallId,
    },
    /// Assistant content names a call that did not complete successfully.
    SemanticEntryCallMismatch {
        /// The affected semantic entry.
        entry: SemanticTranscriptEntryId,
        /// The non-completing producing call.
        call: crate::ModelCallId,
    },
    /// The same model-call identity appeared more than once.
    DuplicateModelCall {
        /// The duplicated call.
        call: crate::ModelCallId,
    },
    /// One global model-call identity appeared in both ordinary and compaction facts.
    DuplicateModelCallIdentityAcrossKinds {
        /// The cross-kind identity collision.
        call: crate::ModelCallId,
    },
    /// The same turn-level pinned-target fact appeared more than once.
    DuplicatePinnedTarget {
        /// The turn whose target was duplicated.
        turn: TurnId,
    },
    /// A call has no independently stored turn-level pinned target.
    PinnedTargetMissing {
        /// The affected call.
        call: crate::ModelCallId,
    },
    /// A turn-level pinned target is unrelated to every supplied call.
    UnreferencedPinnedTarget {
        /// The unrelated turn.
        turn: TurnId,
    },
    /// A model call references a snapshot absent from this complete read.
    ModelCallSnapshotMissing {
        /// The affected call.
        call: crate::ModelCallId,
    },
    /// Stored model-call facts cannot reconstruct canonical call history.
    InvalidModelCall {
        /// The affected call.
        call: crate::ModelCallId,
    },
    /// A dedicated compaction call references an absent source snapshot.
    CompactionCallSnapshotMissing { call: crate::ModelCallId },
    /// The same dedicated compaction-call identity appeared more than once.
    DuplicateCompactionCall { call: crate::ModelCallId },
    /// Stored dedicated compaction-call facts are inconsistent.
    InvalidCompactionCall { call: crate::ModelCallId },
    /// A compaction references an absent source or result snapshot.
    CompactionSnapshotMissing {
        compaction: crate::ContextCompactionId,
    },
    /// A compaction's completed call or summary entry is absent.
    CompactionEvidenceMissing {
        compaction: crate::ContextCompactionId,
    },
    /// Stored compaction facts fail exact provenance reconstruction.
    InvalidCompaction {
        compaction: crate::ContextCompactionId,
    },
    /// The same compaction identity appeared more than once.
    DuplicateCompaction {
        compaction: crate::ContextCompactionId,
    },
    /// A summary or dedicated call is unrelated to every compaction record.
    UnreferencedCompactionEvidence { call: crate::ModelCallId },
    /// A predecessor link is absent, duplicated as a root, or not a prefix.
    InvalidCompactionChain {
        compaction: crate::ContextCompactionId,
    },
    /// A supplied model call is not the terminal call named by its turn.
    UnreferencedModelCall {
        /// The unrelated call.
        call: crate::ModelCallId,
    },
    /// A completed or refused turn names a model call absent from the complete
    /// terminal-call facts.
    TerminalModelCallMissing {
        /// The affected turn.
        turn: TurnId,
        /// The absent terminal call.
        call: crate::ModelCallId,
    },
    /// The named terminal call disagrees with its turn, selection, frontier,
    /// or required physical disposition.
    TerminalModelCallMismatch {
        /// The affected turn.
        turn: TurnId,
    },
    /// A recovery wait names an ambiguous call absent from the complete call
    /// facts.
    RecoveryModelCallMissing {
        /// The affected active turn.
        turn: TurnId,
        /// The absent ambiguous call.
        call: crate::ModelCallId,
    },
    /// The recovery call disagrees with its turn, selection, frontier, or
    /// required ambiguous physical disposition.
    RecoveryModelCallMismatch {
        /// The affected active turn.
        turn: TurnId,
    },
    /// A started turn has no exact origin entry.
    MissingOriginEntry {
        /// The affected turn.
        turn: TurnId,
    },
    /// A failed turn has no exact failure marker.
    MissingFailureEntry {
        /// The affected turn.
        turn: TurnId,
    },
    /// A completed turn has no exact final completion marker.
    MissingCompletionEntry {
        /// The affected turn.
        turn: TurnId,
    },
    /// A cancelled turn has no exact final cancellation marker.
    MissingCancellationEntry {
        /// The affected turn.
        turn: TurnId,
    },
    /// The current attempt record names a different owning turn.
    CurrentAttemptOwnershipMismatch {
        /// The active turn whose attempt is cross-wired.
        turn: TurnId,
        /// The affected attempt.
        attempt: TurnAttemptId,
    },
    /// A failed terminal's ended attempt names a different owning turn.
    TerminalAttemptOwnershipMismatch {
        /// The failed turn being reconstructed.
        turn: TurnId,
        /// The cross-wired ended attempt.
        attempt: TurnAttemptId,
    },
    /// A failed terminal's ended attempt has an ineligible disposition.
    TerminalAttemptEndMismatch {
        /// The failed turn being reconstructed.
        turn: TurnId,
        /// The incorrectly ended attempt.
        attempt: TurnAttemptId,
    },
    /// The same attempt identity appeared on multiple active or terminal
    /// records represented by this projection.
    DuplicateCurrentAttempt {
        /// The duplicated attempt.
        attempt: TurnAttemptId,
    },
    /// The complete acceptance tail contains applied interrupt evidence that
    /// requires a proof-bearing phase outside this evidence-free seam.
    ActivePhaseEvidenceMismatch {
        /// The active turn whose phase cannot remain evidence-free.
        turn: TurnId,
        /// The accepted interrupt that requires a different phase.
        accepted_input: AcceptedInputId,
    },
    /// An active turn was supplied without its complete session acceptance
    /// tail.
    MissingActiveAcceptanceTail {
        /// The active turn requiring the tail.
        turn: TurnId,
    },
    /// A tail was supplied even though the session has no active turn.
    UnexpectedActiveAcceptanceTail,
    /// The claimed tail belongs to a different session.
    AcceptanceTailSessionMismatch {
        /// The current scheduling session.
        expected: SessionId,
        /// The session asserted by the tail.
        actual: SessionId,
    },
    /// The claimed tail does not begin with the active turn's exact origin.
    AcceptanceTailAnchorMismatch {
        /// The active turn whose origin anchors the tail.
        turn: TurnId,
        /// The active turn's exact origin accepted input.
        expected: AcceptedInputId,
        /// The accepted input asserted as the anchor.
        actual: AcceptedInputId,
    },
    /// One tail entry belongs to a different session.
    AcceptanceTailEntrySessionMismatch {
        /// The cross-wired accepted input.
        accepted_input: AcceptedInputId,
    },
    /// The same accepted-input identity appeared more than once in the tail.
    DuplicateAcceptanceTailEntry {
        /// The duplicated accepted input.
        accepted_input: AcceptedInputId,
    },
    /// A tail entry is not at the exact next claimed session position.
    AcceptanceTailPositionMismatch {
        /// The affected accepted input.
        accepted_input: AcceptedInputId,
        /// The exact position required by the interval.
        expected: SessionInputPosition,
        /// The inconsistent supplied position.
        actual: SessionInputPosition,
    },
    /// The ordered entries do not end at the claimed session observation.
    AcceptanceTailLastPositionMismatch {
        /// The authoritative last position claimed by the input.
        expected: SessionInputPosition,
        /// The last position actually represented, if any.
        actual: Option<SessionInputPosition>,
    },
    /// One immutable delivery request and current disposition do not form an
    /// accepted lifecycle correlation.
    AcceptanceTailDispositionMismatch {
        /// The affected accepted input.
        accepted_input: AcceptedInputId,
    },
    /// A stored snapshot belongs to a different consuming session.
    SnapshotOwningSessionMismatch {
        /// The affected snapshot.
        snapshot: ContextFrontierId,
    },
    /// The same session-scoped snapshot identity appeared more than once.
    DuplicateSnapshot {
        /// The duplicated snapshot.
        snapshot: ContextFrontierId,
    },
    /// A snapshot's complete membership contains a duplicate entry.
    InvalidSnapshotMembership {
        /// The affected snapshot.
        snapshot: ContextFrontierId,
    },
    /// A snapshot references an entry absent from the complete entry set.
    SnapshotEntryMissing {
        /// The affected snapshot.
        snapshot: ContextFrontierId,
        /// The absent exact semantic entry.
        entry: SemanticTranscriptEntryRef,
    },
    /// A started turn names a snapshot absent from the complete snapshot set.
    StartingSnapshotMissing {
        /// The affected turn.
        turn: TurnId,
    },
    /// A failed turn names a terminal snapshot absent from the complete set.
    TerminalSnapshotMissing {
        /// The affected turn.
        turn: TurnId,
    },
    /// Lifecycle states do not form terminal prefix, optional active slot, and
    /// queued suffix in durable total order.
    InvalidLifecycleOrder {
        /// The first affected turn.
        turn: TurnId,
    },
    /// The stored start does not name the derived exact lineage.
    StartingLineageMismatch {
        /// The affected turn.
        turn: TurnId,
        /// The exact lineage required by total order.
        expected: AcceptedInputStartingLineage,
        /// The inconsistent stored lineage.
        actual: AcceptedInputStartingLineage,
    },
    /// The stored start snapshot is not the predecessor prefix plus the exact
    /// origin entry.
    StartingFrontierMismatch {
        /// The affected turn.
        turn: TurnId,
    },
    /// The failed terminal frontier is not the start prefix plus its exact
    /// failed marker.
    TerminalFrontierMismatch {
        /// The affected turn.
        turn: TurnId,
    },
    /// A complete snapshot was supplied but no lifecycle fact references it.
    UnreferencedSnapshot {
        /// The unreferenced snapshot.
        snapshot: ContextFrontierId,
    },
}

/// Failed scheduling reconstitution retaining every supplied fact unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputSchedulingReconstitutionError {
    input: Box<AcceptedInputSchedulingReconstitutionInput>,
    failure: AcceptedInputSchedulingReconstitutionFailure,
}

impl AcceptedInputSchedulingReconstitutionError {
    /// Borrows every unchanged reconstitution input.
    pub const fn input(&self) -> &AcceptedInputSchedulingReconstitutionInput {
        &self.input
    }

    /// Borrows the exact integrity failure.
    pub const fn failure(&self) -> &AcceptedInputSchedulingReconstitutionFailure {
        &self.failure
    }

    /// Returns every unchanged input and the exact integrity failure.
    pub fn into_parts(
        self,
    ) -> (
        AcceptedInputSchedulingReconstitutionInput,
        AcceptedInputSchedulingReconstitutionFailure,
    ) {
        (*self.input, self.failure)
    }
}

/// The scheduling-visible lifecycle classification.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AcceptedInputTurnSchedulingStatus {
    /// No start or semantic projection exists.
    Queued,
    /// The turn owns the session's progressing slot.
    Active,
    /// The turn terminalized as failed and has a complete closed semantic
    /// frontier through its failed marker.
    TerminalFailed,
    /// The turn committed a complete assistant response and completion marker.
    TerminalCompleted,
    /// The turn committed an explicit refusal.
    TerminalRefused,
    /// The turn committed a proof-bearing cancellation marker.
    TerminalCancelled,
    /// The turn released its slot with proof-bearing ambiguous work.
    TerminalReconciliationRequired,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ReconstitutedSchedulingState {
    Queued,
    Active {
        start: AcceptedInputTurnStart,
        phase: ActiveTurnPhase,
    },
    TerminalFailed {
        start: AcceptedInputTurnStart,
        terminal_frontier: ResolvedContextFrontierSnapshot,
    },
    TerminalCompleted {
        start: AcceptedInputTurnStart,
        terminal_frontier: ResolvedContextFrontierSnapshot,
    },
    TerminalRefused {
        start: AcceptedInputTurnStart,
        terminal_frontier: ResolvedContextFrontierSnapshot,
    },
    TerminalCancelled {
        start: AcceptedInputTurnStart,
        terminal_frontier: ResolvedContextFrontierSnapshot,
    },
    TerminalReconciliationRequired {
        start: AcceptedInputTurnStart,
        terminal_frontier: ResolvedContextFrontierSnapshot,
    },
}

/// One canonical turn inside the complete scheduling projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputTurnSchedulingProjection {
    session: SessionId,
    turn: TurnId,
    accepted_input: AcceptedInputLifecycle,
    order: AcceptedInputQueueOrder,
    origin_configuration: OriginConfiguration,
    configuration_provenance: TurnConfigurationProvenance,
    state: ReconstitutedSchedulingState,
}

impl AcceptedInputTurnSchedulingProjection {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the accepted-input-origin turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Borrows the exact accepted input whose disposition is `OriginOf(turn)`.
    pub const fn accepted_input(&self) -> &AcceptedInputLifecycle {
        &self.accepted_input
    }

    /// Returns the immutable durable queue-order facts.
    pub const fn order(&self) -> AcceptedInputQueueOrder {
        self.order
    }

    /// Borrows the complete frozen origin configuration.
    pub const fn origin_configuration(&self) -> &OriginConfiguration {
        &self.origin_configuration
    }

    /// Borrows the explicit or inherited configuration provenance.
    pub const fn configuration_provenance(&self) -> &TurnConfigurationProvenance {
        &self.configuration_provenance
    }

    /// Returns the scheduling-visible lifecycle classification.
    pub const fn status(&self) -> AcceptedInputTurnSchedulingStatus {
        match &self.state {
            ReconstitutedSchedulingState::Queued => AcceptedInputTurnSchedulingStatus::Queued,
            ReconstitutedSchedulingState::Active { .. } => {
                AcceptedInputTurnSchedulingStatus::Active
            }
            ReconstitutedSchedulingState::TerminalFailed { .. } => {
                AcceptedInputTurnSchedulingStatus::TerminalFailed
            }
            ReconstitutedSchedulingState::TerminalCompleted { .. } => {
                AcceptedInputTurnSchedulingStatus::TerminalCompleted
            }
            ReconstitutedSchedulingState::TerminalRefused { .. } => {
                AcceptedInputTurnSchedulingStatus::TerminalRefused
            }
            ReconstitutedSchedulingState::TerminalCancelled { .. } => {
                AcceptedInputTurnSchedulingStatus::TerminalCancelled
            }
            ReconstitutedSchedulingState::TerminalReconciliationRequired { .. } => {
                AcceptedInputTurnSchedulingStatus::TerminalReconciliationRequired
            }
        }
    }

    /// Returns the opaque validated start for started work.
    pub const fn start(&self) -> Option<AcceptedInputTurnStart> {
        match &self.state {
            ReconstitutedSchedulingState::Queued => None,
            ReconstitutedSchedulingState::Active { start, .. }
            | ReconstitutedSchedulingState::TerminalFailed { start, .. }
            | ReconstitutedSchedulingState::TerminalCompleted { start, .. }
            | ReconstitutedSchedulingState::TerminalRefused { start, .. }
            | ReconstitutedSchedulingState::TerminalCancelled { start, .. }
            | ReconstitutedSchedulingState::TerminalReconciliationRequired { start, .. } => {
                Some(*start)
            }
        }
    }

    /// Borrows the exact current active phase, when this turn owns the slot.
    pub const fn active_phase(&self) -> Option<&ActiveTurnPhase> {
        match &self.state {
            ReconstitutedSchedulingState::Active { phase, .. } => Some(phase),
            ReconstitutedSchedulingState::Queued
            | ReconstitutedSchedulingState::TerminalFailed { .. }
            | ReconstitutedSchedulingState::TerminalCompleted { .. }
            | ReconstitutedSchedulingState::TerminalRefused { .. }
            | ReconstitutedSchedulingState::TerminalCancelled { .. }
            | ReconstitutedSchedulingState::TerminalReconciliationRequired { .. } => None,
        }
    }

    fn active_turn_execution_with_pending(
        &self,
        pending_steering: Box<[PendingSteeringInput]>,
        consumed_steering: Box<[ConsumedSteeringInput]>,
    ) -> Option<ActivatedAcceptedInputTurn> {
        let ReconstitutedSchedulingState::Active { start, phase } = &self.state else {
            return None;
        };
        Some(ActivatedAcceptedInputTurn {
            session: self.session,
            turn: self.turn,
            accepted_input: self.accepted_input.clone(),
            order: self.order,
            configuration: self.origin_configuration.clone(),
            configuration_provenance: self.configuration_provenance.clone(),
            start: *start,
            phase: phase.clone(),
            pending_steering,
            consumed_steering,
        })
    }

    /// Borrows the complete semantic frontier through a failed marker.
    pub const fn failed_terminal_frontier(&self) -> Option<&ResolvedContextFrontierSnapshot> {
        match &self.state {
            ReconstitutedSchedulingState::TerminalFailed {
                terminal_frontier, ..
            } => Some(terminal_frontier),
            ReconstitutedSchedulingState::Queued | ReconstitutedSchedulingState::Active { .. } => {
                None
            }
            ReconstitutedSchedulingState::TerminalCompleted { .. }
            | ReconstitutedSchedulingState::TerminalRefused { .. }
            | ReconstitutedSchedulingState::TerminalCancelled { .. }
            | ReconstitutedSchedulingState::TerminalReconciliationRequired { .. } => None,
        }
    }

    /// Borrows the complete semantic frontier of any terminal turn.
    pub const fn terminal_frontier(&self) -> Option<&ResolvedContextFrontierSnapshot> {
        match &self.state {
            ReconstitutedSchedulingState::TerminalFailed {
                terminal_frontier, ..
            }
            | ReconstitutedSchedulingState::TerminalCompleted {
                terminal_frontier, ..
            }
            | ReconstitutedSchedulingState::TerminalRefused {
                terminal_frontier, ..
            }
            | ReconstitutedSchedulingState::TerminalCancelled {
                terminal_frontier, ..
            }
            | ReconstitutedSchedulingState::TerminalReconciliationRequired {
                terminal_frontier,
                ..
            } => Some(terminal_frontier),
            ReconstitutedSchedulingState::Queued | ReconstitutedSchedulingState::Active { .. } => {
                None
            }
        }
    }
}

/// Canonical complete scheduling state for one session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputSchedulingProjection {
    session: Session,
    initial_seed_frontier: Option<ContextFrontierId>,
    latest_compaction_result: Option<ContextFrontierId>,
    active_compaction_call: Option<crate::ModelCallId>,
    turns: Box<[AcceptedInputTurnSchedulingProjection]>,
    active_acceptance_tail: Option<SessionAcceptanceTail>,
    semantic_entries: BTreeMap<SemanticTranscriptEntryRef, SemanticTranscriptEntry>,
    snapshots: BTreeMap<ContextFrontierId, ResolvedContextFrontierSnapshot>,
    attempt_owners: BTreeMap<TurnAttemptId, TurnId>,
    active_model_call_recovery: Option<ActiveModelCallRecoveryWait>,
    active_stop_requested_frontier: Option<ContextFrontierId>,
    active_tool_recovery_attempt: Option<EndedTurnAttempt>,
    active_tool_recovery_frontier: Option<ContextFrontierId>,
    active_executing_tool_batch: Option<ActiveExecutingToolBatchCorrelation>,
    preceding_non_accepted_successors: BTreeMap<TurnId, TurnId>,
    preceding_non_accepted_terminals:
        BTreeMap<TurnId, (ResolvedContextFrontierSnapshot, DirectModelSelection)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActiveExecutingToolBatchCorrelation {
    session: SessionId,
    turn: TurnId,
    producing_call: crate::ModelCallId,
    yielded_frontier: ContextFrontierId,
    turn_attempt: Option<TurnAttemptId>,
}

impl AcceptedInputSchedulingProjection {
    /// Borrows the complete current-session snapshot.
    pub const fn session(&self) -> &Session {
        &self.session
    }

    pub(crate) const fn active_acceptance_tail(&self) -> Option<&SessionAcceptanceTail> {
        self.active_acceptance_tail.as_ref()
    }

    /// Iterates over every turn in derived durable total order.
    pub fn turns(&self) -> impl ExactSizeIterator<Item = &AcceptedInputTurnSchedulingProjection> {
        self.turns.iter()
    }

    /// Looks up one turn in the complete scheduling projection.
    pub fn turn(&self, turn: TurnId) -> Option<&AcceptedInputTurnSchedulingProjection> {
        self.turns.iter().find(|candidate| candidate.turn == turn)
    }

    /// Returns the sole active slot owner, when present.
    pub fn active_turn(&self) -> Option<&AcceptedInputTurnSchedulingProjection> {
        self.turns
            .iter()
            .find(|turn| turn.status() == AcceptedInputTurnSchedulingStatus::Active)
    }

    /// Reconstructs the sealed active-turn facts and complete pending-steering
    /// inventory for an execution aggregate.
    pub fn active_turn_execution(&self) -> Option<ActivatedAcceptedInputTurn> {
        let active = self.active_turn()?;
        let tail = self.active_acceptance_tail.as_ref()?;
        let (pending_steering, consumed_steering) =
            active_execution_steering_inputs(active.turn, tail);
        active.active_turn_execution_with_pending(pending_steering, consumed_steering)
    }

    /// Returns accepted-input origins retained by the active turn's exact
    /// model-visible frontier.
    pub fn active_rendered_frontier_origins(&self) -> Option<Vec<AcceptedInputId>> {
        let active = self.active_turn()?;
        if matches!(
            active.active_phase(),
            Some(ActiveTurnPhase::AwaitingRunnerRecovery { .. })
        ) {
            return None;
        }
        let snapshot = self
            .active_model_call_recovery
            .as_ref()
            .map(|recovery| recovery.source_snapshot.frontier().snapshot())
            .or(self.active_stop_requested_frontier)
            .or_else(|| {
                self.active_executing_tool_batch
                    .map(|batch| batch.yielded_frontier)
            })
            .or(self.active_tool_recovery_frontier)
            .or_else(|| active.start().map(|start| start.frontier().snapshot()))
            .and_then(|frontier| self.snapshots.get(&frontier));
        Self::rendered_frontier_origins(snapshot, &self.semantic_entries)
    }

    /// Returns the earliest queued work in durable total order.
    pub fn earliest_queued_turn(&self) -> Option<&AcceptedInputTurnSchedulingProjection> {
        self.turns
            .iter()
            .find(|turn| turn.status() == AcceptedInputTurnSchedulingStatus::Queued)
    }

    /// Returns accepted-input origins retained by the exact base from which
    /// the earliest queued turn would be rendered.
    ///
    /// The base is reported as the model would see it: when it carries a
    /// context summary, the entries that summary hides are not retained
    /// origins, exactly as the live-execution path projects its own frontier
    /// before collecting origins. Counting hidden origins here would sum
    /// attachments no render ever clones, and a submission whose visible
    /// frontier fits the byte bound would be durably rejected because a
    /// summarized-away one did not.
    ///
    /// The outer absence means a turn is active or no queued turn exists. The
    /// inner failure means the base's own summary range is unprojectable, a
    /// durable corruption the caller must surface rather than read as an empty
    /// base. It excludes the queued turn's own origin; callers can append
    /// queued origins in [`Self::turns`] order to project each eventual
    /// frontier.
    pub fn earliest_queued_rendered_base_origins(
        &self,
    ) -> Option<Result<Vec<AcceptedInputId>, ContextFrontierProjectionFailure>> {
        if self.active_turn().is_some() {
            return None;
        }
        let index = self
            .turns
            .iter()
            .position(|turn| turn.status() == AcceptedInputTurnSchedulingStatus::Queued)?;
        let queued = &self.turns[index];
        let preceding_non_accepted_terminal = self
            .preceding_non_accepted_successors
            .get(&queued.turn())
            .and_then(|predecessor| self.preceding_non_accepted_terminals.get(predecessor))
            .map(|(snapshot, _)| snapshot);
        let base = if index == 0 && preceding_non_accepted_terminal.is_none() {
            let seed = self
                .initial_seed_frontier
                .and_then(|frontier| self.snapshots.get(&frontier));
            self.latest_compaction_result
                .and_then(|frontier| self.snapshots.get(&frontier))
                .filter(|latest| seed.is_some_and(|seed| seed.is_semantic_prefix_of(latest)))
                .or(seed)
        } else {
            let terminal = preceding_non_accepted_terminal.or_else(|| {
                index
                    .checked_sub(1)
                    .and_then(|predecessor| self.turns[predecessor].terminal_frontier())
            })?;
            self.latest_compaction_result
                .and_then(|frontier| self.snapshots.get(&frontier))
                .filter(|latest| terminal.is_semantic_prefix_of(latest))
                .or(Some(terminal))
        };
        Self::projected_rendered_frontier_origins(base, &self.semantic_entries)
    }

    /// Returns the rendered base origins for a queued turn rooted directly at
    /// a terminal non-accepted predecessor.
    ///
    /// Absence means this turn continues the accepted-input chain and does not
    /// reset prospective frontier accounting.
    pub fn external_predecessor_rendered_base_origins(
        &self,
        turn: TurnId,
    ) -> Option<Vec<AcceptedInputId>> {
        let predecessor = self.preceding_non_accepted_successors.get(&turn)?;
        let terminal = &self.preceding_non_accepted_terminals.get(predecessor)?.0;
        let base = self
            .latest_compaction_result
            .and_then(|frontier| self.snapshots.get(&frontier))
            .filter(|latest| terminal.is_semantic_prefix_of(latest))
            .unwrap_or(terminal);
        Self::rendered_frontier_origins(Some(base), &self.semantic_entries)
    }

    fn rendered_frontier_origins(
        snapshot: Option<&ResolvedContextFrontierSnapshot>,
        semantic_entries: &BTreeMap<SemanticTranscriptEntryRef, SemanticTranscriptEntry>,
    ) -> Option<Vec<AcceptedInputId>> {
        Self::projected_rendered_frontier_origins(snapshot, semantic_entries)?.ok()
    }

    fn projected_rendered_frontier_origins(
        snapshot: Option<&ResolvedContextFrontierSnapshot>,
        semantic_entries: &BTreeMap<SemanticTranscriptEntryRef, SemanticTranscriptEntry>,
    ) -> Option<Result<Vec<AcceptedInputId>, ContextFrontierProjectionFailure>> {
        let complete_entries = snapshot
            .into_iter()
            .flat_map(ResolvedContextFrontierSnapshot::ordered_entries)
            .map(|reference| semantic_entries.get(&reference).cloned())
            .collect::<Option<Vec<_>>>()?;
        let projection = match ContextFrontierProjection::from_complete_entries(&complete_entries) {
            Ok(projection) => projection,
            Err(failure) => return Some(Err(failure)),
        };
        let entries_by_reference = complete_entries
            .iter()
            .map(|entry| (entry.reference(), entry))
            .collect::<BTreeMap<_, _>>();
        let mut origins = Vec::new();
        let mut distinct = BTreeSet::new();
        for reference in projection.ordered_entries() {
            let accepted_input = match entries_by_reference.get(&reference)?.payload() {
                SemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input }
                | SemanticTranscriptEntryPayload::SteeringAcceptedInput {
                    accepted_input, ..
                } => Some(*accepted_input),
                SemanticTranscriptEntryPayload::TurnFailed { .. }
                | SemanticTranscriptEntryPayload::DelegatedTask { .. }
                | SemanticTranscriptEntryPayload::DelegationMessage { .. }
                | SemanticTranscriptEntryPayload::DelegationResult { .. }
                | SemanticTranscriptEntryPayload::ModelIdentityChanged { .. }
                | SemanticTranscriptEntryPayload::ContextSummary { .. }
                | SemanticTranscriptEntryPayload::TurnCancelled { .. }
                | SemanticTranscriptEntryPayload::AssistantText { .. }
                | SemanticTranscriptEntryPayload::ProviderCompaction { .. }
                | SemanticTranscriptEntryPayload::AssistantToolUse { .. }
                | SemanticTranscriptEntryPayload::ToolExecutionResult { .. }
                | SemanticTranscriptEntryPayload::ToolDenied { .. }
                | SemanticTranscriptEntryPayload::ToolClosed { .. }
                | SemanticTranscriptEntryPayload::TurnCompleted { .. }
                | SemanticTranscriptEntryPayload::Imported { .. } => None,
            };
            if let Some(accepted_input) = accepted_input.filter(|value| distinct.insert(*value)) {
                origins.push(accepted_input);
            }
        }
        Some(Ok(origins))
    }

    /// Borrows one complete resolved snapshot from this checked projection.
    pub fn resolved_snapshot(
        &self,
        snapshot: ContextFrontierId,
    ) -> Option<&ResolvedContextFrontierSnapshot> {
        self.snapshots.get(&snapshot)
    }

    /// Borrows one canonical semantic entry from this checked projection.
    pub fn semantic_entry(
        &self,
        entry: SemanticTranscriptEntryRef,
    ) -> Option<&SemanticTranscriptEntry> {
        self.semantic_entries.get(&entry)
    }

    /// Closes the active model-call recovery wait under one newly applied
    /// interrupt while preserving its exact ambiguity set.
    pub fn apply_interrupt_to_model_call_recovery(
        self,
        interrupt: AppliedInterruptCommandResult,
        identities: crate::AmbiguousModelCallTurnIdentities,
    ) -> Result<crate::ReconciliationRequiredModelCallTurn, crate::ModelCallClosureError> {
        let active_turn = self
            .active_turn_execution()
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        let recovery = self
            .active_model_call_recovery
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        crate::model_execution::apply_interrupt_to_recovery_wait(
            active_turn.into(),
            recovery.call,
            recovery.attempt,
            recovery.source_snapshot,
            interrupt,
            identities,
        )
    }

    /// Closes the active model-call recovery wait under a daemon-owned durable
    /// attempt while preserving its exact ambiguity set.
    pub fn apply_automatic_reconciliation(
        self,
        attempt: std::num::NonZeroU32,
        identities: crate::AmbiguousModelCallTurnIdentities,
    ) -> Result<crate::ReconciliationRequiredModelCallTurn, crate::ModelCallClosureError> {
        let active_turn = self
            .active_turn_execution()
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        let recovery = self
            .active_model_call_recovery
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        crate::model_execution::apply_automatic_reconciliation(
            active_turn.into(),
            recovery.call,
            recovery.attempt,
            recovery.source_snapshot,
            attempt,
            identities,
        )
    }

    /// Cancels a turn parked on runner loss without claiming that any retained
    /// runner effect failed. The supplied source is the latest already-durable
    /// semantic boundary; runner-loss evidence remains on the placement.
    pub fn apply_interrupt_to_runner_recovery(
        self,
        source_snapshot: ResolvedContextFrontierSnapshot,
        result_projection: Option<crate::PreparedToolResultProjection>,
        interrupt: AppliedInterruptCommandResult,
        identities: crate::CancelledModelCallTurnIdentities,
    ) -> Result<crate::CancelledModelCallTurn, crate::ModelCallClosureError> {
        let active_turn = self
            .active_turn_execution()
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        let starting_snapshot = self
            .snapshots
            .get(&active_turn.start().frontier().snapshot())
            .cloned()
            .ok_or(crate::ModelCallClosureError::FrontierDerivationFailed)?;
        ActivatedTurn::from(active_turn).apply_interrupt_to_runner_recovery(
            starting_snapshot,
            source_snapshot,
            result_projection,
            interrupt,
            identities,
        )
    }

    /// Closes a runner-loss wait that retained one ambiguous physical tool
    /// attempt, preserving that ambiguity as reconciliation-required.
    pub fn apply_interrupt_to_runner_tool_recovery(
        self,
        wait: crate::AwaitingToolRecovery,
        tool_attempt: crate::EndedToolAttempt,
        yielded_attempt: TurnAttemptId,
        result_projection: crate::PreparedToolResultProjection,
        interrupt: AppliedInterruptCommandResult,
        identities: crate::AmbiguousModelCallTurnIdentities,
    ) -> Result<crate::ReconciliationRequiredToolTurn, crate::ModelCallClosureError> {
        let active_turn = self
            .active_turn_execution()
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        crate::model_execution::apply_interrupt_to_runner_tool_recovery_wait(
            active_turn.into(),
            wait,
            tool_attempt,
            yielded_attempt,
            result_projection,
            interrupt,
            identities,
        )
    }

    /// Cancels a runner-loss wait after its retryable physical attempt has
    /// been retired as a known crash loss.
    pub fn apply_interrupt_to_retryable_runner_tool_recovery(
        self,
        batch: crate::ToolBatch,
        result_projection: crate::PreparedToolResultProjection,
        interrupt: AppliedInterruptCommandResult,
        identities: crate::CancelledModelCallTurnIdentities,
    ) -> Result<crate::CancelledModelCallTurn, crate::ModelCallClosureError> {
        let active_turn = self
            .active_turn_execution()
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        let starting_snapshot = self
            .snapshots
            .get(&active_turn.start().frontier().snapshot())
            .cloned()
            .ok_or(crate::ModelCallClosureError::FrontierDerivationFailed)?;
        crate::model_execution::apply_interrupt_to_retryable_runner_tool_recovery_wait(
            active_turn.into(),
            starting_snapshot,
            batch,
            result_projection,
            interrupt,
            identities,
        )
    }

    /// Closes one executing tool batch under a newly applied interrupt.
    ///
    /// The checked scheduling projection supplies the current active phase;
    /// the batch supplies its exact yielded frontier and complete physical
    /// attempt inventory, while the result projection supplies the already
    /// checked logical closures. Result identities are consumed only after
    /// all three projections agree.
    pub fn apply_interrupt_to_tool_batch(
        self,
        batch: crate::ToolBatch,
        result_projection: crate::PreparedToolResultProjection,
        interrupt: AppliedInterruptCommandResult,
        identities: crate::CancelledModelCallTurnIdentities,
    ) -> Result<crate::CancelledModelCallTurn, crate::ModelCallClosureError> {
        let Some(correlation) = self.active_executing_tool_batch else {
            return Err(crate::ModelCallClosureError::InterruptCorrelationMismatch);
        };
        let turn_attempt = match batch.phase() {
            crate::ToolBatchPhase::Executing { turn_attempt } => Some(turn_attempt),
            crate::ToolBatchPhase::AwaitingChild { .. } => None,
            crate::ToolBatchPhase::AwaitingApproval { .. }
            | crate::ToolBatchPhase::AwaitingRecovery { .. } => {
                return Err(crate::ModelCallClosureError::AttemptStateMismatch);
            }
        };
        if correlation.session != batch.session()
            || correlation.turn != batch.turn()
            || correlation.producing_call != batch.producing_call()
            || correlation.yielded_frontier != batch.yielded_snapshot().frontier().snapshot()
            || correlation.turn_attempt != turn_attempt
        {
            return Err(crate::ModelCallClosureError::InterruptCorrelationMismatch);
        }
        let active_turn = self
            .active_turn_execution()
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        crate::model_execution::apply_interrupt_to_executing_tool_batch(
            active_turn.into(),
            batch,
            result_projection,
            interrupt,
            identities,
        )
    }

    /// Closes the active tool-attempt recovery wait under one newly applied
    /// interrupt while preserving its exact ambiguity.
    pub fn apply_interrupt_to_tool_recovery(
        self,
        wait: crate::AwaitingToolRecovery,
        tool_attempt: crate::EndedToolAttempt,
        result_projection: crate::PreparedToolResultProjection,
        interrupt: AppliedInterruptCommandResult,
        identities: crate::AmbiguousModelCallTurnIdentities,
    ) -> Result<crate::ReconciliationRequiredToolTurn, crate::ModelCallClosureError> {
        let active_turn = self
            .active_turn_execution()
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        let attempt = self
            .active_tool_recovery_attempt
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        crate::model_execution::apply_interrupt_to_tool_recovery_wait(
            active_turn.into(),
            wait,
            tool_attempt,
            attempt,
            result_projection,
            interrupt,
            identities,
        )
    }

    /// Closes the active tool-attempt recovery wait under one daemon-owned
    /// durable attempt while preserving its exact physical ambiguity.
    pub fn apply_automatic_tool_reconciliation(
        self,
        wait: crate::AwaitingToolRecovery,
        tool_attempt: crate::EndedToolAttempt,
        result_projection: crate::PreparedToolResultProjection,
        recovery_attempt: std::num::NonZeroU32,
        identities: crate::AmbiguousModelCallTurnIdentities,
    ) -> Result<crate::ReconciliationRequiredToolTurn, crate::ModelCallClosureError> {
        let active_turn = self
            .active_turn_execution()
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        let attempt = self
            .active_tool_recovery_attempt
            .ok_or(crate::ModelCallClosureError::AttemptStateMismatch)?;
        crate::model_execution::apply_automatic_tool_reconciliation(
            active_turn.into(),
            wait,
            tool_attempt,
            attempt,
            result_projection,
            recovery_attempt,
            identities,
        )
    }

    /// Consumes this complete projection and prepares the earliest queued turn
    /// as one sealed commit candidate.
    pub fn prepare_earliest_queued_activation(
        self,
        identities: AcceptedInputTurnActivationIdentities,
    ) -> Result<PreparedAcceptedInputTurnActivation, AcceptedInputEligibilityError> {
        prepare_earliest_queued_activation(self, identities)
    }

    /// Consumes this complete projection and prepares the active prior-process
    /// attempt as one failed-terminal startup-recovery candidate.
    pub fn prepare_active_turn_lost_failure(
        self,
        identities: AcceptedInputTurnFailureIdentities,
    ) -> Result<PreparedAcceptedInputTurnFailure, AcceptedInputTurnFailureError> {
        prepare_active_turn_lost_failure(self, identities)
    }
}

fn active_execution_steering_inputs(
    active_turn: TurnId,
    tail: &SessionAcceptanceTail,
) -> (Box<[PendingSteeringInput]>, Box<[ConsumedSteeringInput]>) {
    let pending = tail
        .entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.accepted_input.disposition(),
                AcceptedInputDisposition::PendingSteering { .. }
            )
        })
        .map(|entry| PendingSteeringInput {
            accepted_input: entry.accepted_input.clone(),
            acceptance_position: entry.position,
        })
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let consumed = tail
        .entries
        .iter()
        .filter_map(|entry| {
            let AcceptedInputDisposition::ConsumedAsSteering { .. } =
                entry.accepted_input.disposition()
            else {
                return None;
            };
            let DeliveryRequest::NextSafePoint {
                expected_active_turn,
            } = entry.delivery
            else {
                return None;
            };
            (expected_active_turn == active_turn).then(|| ConsumedSteeringInput {
                accepted_input: entry.accepted_input.clone(),
                acceptance_position: entry.position,
                source_turn: expected_active_turn,
            })
        })
        .collect::<Vec<_>>()
        .into_boxed_slice();
    (pending, consumed)
}

/// Fresh identities supplied for one eligibility-time activation candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AcceptedInputTurnActivationIdentities {
    model_identity_entry: SemanticTranscriptEntryId,
    origin_entry: SemanticTranscriptEntryId,
    starting_frontier: ContextFrontierId,
    initial_attempt: TurnAttemptId,
}

impl AcceptedInputTurnActivationIdentities {
    /// Supplies all four candidates, including the optional injected entry.
    pub const fn new(
        model_identity_entry: SemanticTranscriptEntryId,
        origin_entry: SemanticTranscriptEntryId,
        starting_frontier: ContextFrontierId,
        initial_attempt: TurnAttemptId,
    ) -> Self {
        Self {
            model_identity_entry,
            origin_entry,
            starting_frontier,
            initial_attempt,
        }
    }

    /// Returns the proposed injected model-identity entry.
    pub const fn model_identity_entry(&self) -> SemanticTranscriptEntryId {
        self.model_identity_entry
    }

    /// Returns the proposed origin semantic-entry identity.
    pub const fn origin_entry(&self) -> SemanticTranscriptEntryId {
        self.origin_entry
    }

    /// Returns the proposed starting snapshot identity.
    pub const fn starting_frontier(&self) -> ContextFrontierId {
        self.starting_frontier
    }

    /// Returns the proposed initial attempt identity.
    pub const fn initial_attempt(&self) -> TurnAttemptId {
        self.initial_attempt
    }
}

/// Exact checked active turn state prepared or reconstituted by eligibility.
///
/// Raw aggregate facts cannot construct this state:
///
/// ```compile_fail
/// use signalbox_domain::{
///     AcceptedInputLifecycle, AcceptedInputQueueOrder, AcceptedInputTurnStart,
///     ActivatedAcceptedInputTurn, ActiveTurnPhase, OriginConfiguration, SessionId,
///     TurnConfigurationProvenance, TurnId,
/// };
///
/// fn raw_facts_are_not_an_activation(
///     session: SessionId,
///     turn: TurnId,
///     accepted_input: AcceptedInputLifecycle,
///     order: AcceptedInputQueueOrder,
///     configuration: OriginConfiguration,
///     configuration_provenance: TurnConfigurationProvenance,
///     start: AcceptedInputTurnStart,
///     phase: ActiveTurnPhase,
///     pending_steering: Box<[PendingSteeringInput]>,
///     consumed_steering: Box<[ConsumedSteeringInput]>,
/// ) {
///     let _ = ActivatedAcceptedInputTurn {
///         session,
///         turn,
///         accepted_input,
///         order,
///         configuration,
///         configuration_provenance,
///         start,
///         phase,
///         pending_steering,
///         consumed_steering,
///     };
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivatedAcceptedInputTurn {
    session: SessionId,
    turn: TurnId,
    accepted_input: AcceptedInputLifecycle,
    order: AcceptedInputQueueOrder,
    configuration: OriginConfiguration,
    configuration_provenance: TurnConfigurationProvenance,
    start: AcceptedInputTurnStart,
    phase: ActiveTurnPhase,
    pending_steering: Box<[PendingSteeringInput]>,
    consumed_steering: Box<[ConsumedSteeringInput]>,
}

impl ActivatedAcceptedInputTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the activated logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Borrows the exact accepted origin input.
    pub const fn accepted_input(&self) -> &AcceptedInputLifecycle {
        &self.accepted_input
    }

    /// Returns the immutable accepted-input queue order.
    pub const fn order(&self) -> AcceptedInputQueueOrder {
        self.order
    }

    /// Borrows the complete frozen origin configuration.
    pub const fn configuration(&self) -> &OriginConfiguration {
        &self.configuration
    }

    /// Borrows the explicit or inherited configuration provenance.
    pub const fn configuration_provenance(&self) -> &TurnConfigurationProvenance {
        &self.configuration_provenance
    }

    /// Returns the exact eligibility-fixed lineage and frontier.
    pub const fn start(&self) -> AcceptedInputTurnStart {
        self.start
    }

    /// Borrows the exact initial active phase.
    pub const fn phase(&self) -> &ActiveTurnPhase {
        &self.phase
    }

    /// Returns the complete accepted inputs that still await this turn's next
    /// model-call safe point or terminal reclassification.
    pub fn pending_steering(&self) -> &[PendingSteeringInput] {
        &self.pending_steering
    }

    /// Returns consumed steering in immutable acceptance order.
    pub fn consumed_steering(&self) -> &[ConsumedSteeringInput] {
        &self.consumed_steering
    }

    #[cfg(test)]
    pub(crate) fn with_phase_for_test(&self, phase: ActiveTurnPhase) -> Self {
        Self {
            session: self.session,
            turn: self.turn,
            accepted_input: self.accepted_input.clone(),
            order: self.order,
            configuration: self.configuration.clone(),
            configuration_provenance: self.configuration_provenance.clone(),
            start: self.start,
            phase,
            pending_steering: self.pending_steering.clone(),
            consumed_steering: self.consumed_steering.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_start_for_test(&self, start: AcceptedInputTurnStart) -> Self {
        Self {
            session: self.session,
            turn: self.turn,
            accepted_input: self.accepted_input.clone(),
            order: self.order,
            configuration: self.configuration.clone(),
            configuration_provenance: self.configuration_provenance.clone(),
            start,
            phase: self.phase.clone(),
            pending_steering: self.pending_steering.clone(),
            consumed_steering: self.consumed_steering.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_pending_steering_for_test(
        &self,
        pending_steering: Box<[(AcceptedInputId, SessionInputPosition)]>,
    ) -> Self {
        let pending_steering = pending_steering
            .into_vec()
            .into_iter()
            .map(
                |(accepted_input, acceptance_position)| PendingSteeringInput {
                    accepted_input: AcceptedInputLifecycle::new(
                        accepted_input,
                        AcceptedInputDisposition::PendingSteering {
                            binding: crate::SteeringBinding::new(self.turn),
                        },
                    ),
                    acceptance_position,
                },
            )
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            session: self.session,
            turn: self.turn,
            accepted_input: self.accepted_input.clone(),
            order: self.order,
            configuration: self.configuration.clone(),
            configuration_provenance: self.configuration_provenance.clone(),
            start: self.start,
            phase: self.phase.clone(),
            pending_steering,
            consumed_steering: self.consumed_steering.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_consumed_steering_for_test(
        &self,
        consumed_steering: Box<[(AcceptedInputId, SessionInputPosition, crate::ModelCallId)]>,
    ) -> Self {
        let consumed_steering = consumed_steering
            .into_vec()
            .into_iter()
            .map(
                |(accepted_input, acceptance_position, call)| ConsumedSteeringInput {
                    accepted_input: AcceptedInputLifecycle::new(
                        accepted_input,
                        AcceptedInputDisposition::ConsumedAsSteering { call },
                    ),
                    acceptance_position,
                    source_turn: self.turn,
                },
            )
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            session: self.session,
            turn: self.turn,
            accepted_input: self.accepted_input.clone(),
            order: self.order,
            configuration: self.configuration.clone(),
            configuration_provenance: self.configuration_provenance.clone(),
            start: self.start,
            phase: self.phase.clone(),
            pending_steering: Box::new([]),
            consumed_steering,
        }
    }
}

/// Checked active turn whose immutable origin is a delegated task rather than
/// an accepted input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivatedDelegatedTurn {
    session: SessionId,
    turn: TurnId,
    origin: ActivatedDelegatedTurnOrigin,
    configuration: OriginConfiguration,
    start: AcceptedInputTurnStart,
    phase: ActiveTurnPhase,
    pending_steering: Box<[PendingSteeringInput]>,
    consumed_steering: Box<[ConsumedSteeringInput]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ActivatedDelegatedTurnOrigin {
    InitialTask {
        spawning_request: ToolRequestId,
        task: DelegationContent,
    },
    PendingDeliveries {
        first: NonZeroU64,
        through: NonZeroU64,
    },
}

impl ActivatedDelegatedTurn {
    pub const fn session(&self) -> SessionId {
        self.session
    }

    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    pub const fn spawning_request(&self) -> Option<ToolRequestId> {
        match &self.origin {
            ActivatedDelegatedTurnOrigin::InitialTask {
                spawning_request, ..
            } => Some(*spawning_request),
            ActivatedDelegatedTurnOrigin::PendingDeliveries { .. } => None,
        }
    }

    pub const fn task(&self) -> Option<&DelegationContent> {
        match &self.origin {
            ActivatedDelegatedTurnOrigin::InitialTask { task, .. } => Some(task),
            ActivatedDelegatedTurnOrigin::PendingDeliveries { .. } => None,
        }
    }

    pub const fn delivery_range(&self) -> Option<(NonZeroU64, NonZeroU64)> {
        match &self.origin {
            ActivatedDelegatedTurnOrigin::InitialTask { .. } => None,
            ActivatedDelegatedTurnOrigin::PendingDeliveries { first, through } => {
                Some((*first, *through))
            }
        }
    }

    pub const fn configuration(&self) -> &OriginConfiguration {
        &self.configuration
    }

    pub const fn start(&self) -> AcceptedInputTurnStart {
        self.start
    }

    pub const fn phase(&self) -> &ActiveTurnPhase {
        &self.phase
    }

    /// Attaches the complete accepted-input steering tail targeting this turn.
    pub fn with_pending_steering(
        mut self,
        pending_steering: Vec<PendingSteeringInput>,
    ) -> Option<Self> {
        if pending_steering.iter().any(|pending| {
            !matches!(
                pending.lifecycle().disposition(),
                AcceptedInputDisposition::PendingSteering { binding }
                    if binding.source_turn() == self.turn
            )
        }) {
            return None;
        }
        self.pending_steering = pending_steering.into_boxed_slice();
        Some(self)
    }

    /// Attaches every stored steering input consumed by this delegated turn.
    pub fn with_consumed_steering(
        mut self,
        consumed_steering: Vec<ConsumedSteeringReconstitutionInput>,
    ) -> Option<Self> {
        self.consumed_steering = consumed_steering
            .into_iter()
            .map(|consumed| {
                (consumed.session() == self.session
                    && consumed.source_turn() == self.turn
                    && matches!(
                        consumed.accepted_input().disposition(),
                        AcceptedInputDisposition::ConsumedAsSteering { .. }
                    ))
                .then(|| ConsumedSteeringInput {
                    accepted_input: consumed.accepted_input().clone(),
                    acceptance_position: consumed.acceptance_position(),
                    source_turn: consumed.source_turn(),
                })
            })
            .collect::<Option<Vec<_>>>()?
            .into_boxed_slice();
        Some(self)
    }

    pub fn pending_steering(&self) -> &[PendingSteeringInput] {
        &self.pending_steering
    }

    /// Returns consumed steering in immutable acceptance order.
    pub fn consumed_steering(&self) -> &[ConsumedSteeringInput] {
        &self.consumed_steering
    }
}

/// Origin-agnostic active turn consumed by model execution.
#[derive(Clone, Debug, Eq, PartialEq)]
// Both variants remain inline so activation reconstitution preserves the
// established public value shape across accepted-input and delegation origins.
#[allow(clippy::large_enum_variant)]
pub enum ActivatedTurn {
    Accepted(ActivatedAcceptedInputTurn),
    Delegated(ActivatedDelegatedTurn),
}

impl From<ActivatedAcceptedInputTurn> for ActivatedTurn {
    fn from(value: ActivatedAcceptedInputTurn) -> Self {
        Self::Accepted(value)
    }
}

impl From<ActivatedDelegatedTurn> for ActivatedTurn {
    fn from(value: ActivatedDelegatedTurn) -> Self {
        Self::Delegated(value)
    }
}

impl ActivatedTurn {
    /// Borrows the accepted-input origin when this is an accepted-input turn.
    pub const fn accepted_input(&self) -> Option<&AcceptedInputLifecycle> {
        match self {
            Self::Accepted(turn) => Some(turn.accepted_input()),
            Self::Delegated(_) => None,
        }
    }

    /// Borrows the delegated origin when this is a delegated turn.
    pub const fn delegated(&self) -> Option<&ActivatedDelegatedTurn> {
        match self {
            Self::Accepted(_) => None,
            Self::Delegated(turn) => Some(turn),
        }
    }

    /// Seals stored semantic entries for this active turn's model frontier.
    pub fn reconstitute_frontier_entries(
        &self,
        entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
    ) -> Option<Vec<SemanticTranscriptEntry>> {
        entries
            .into_iter()
            .map(|entry| {
                (entry.source_session() == self.session()).then(|| {
                    SemanticTranscriptEntry::from_validated_parts(
                        entry.identity(),
                        entry.source_session(),
                        entry.payload().clone(),
                    )
                })
            })
            .collect()
    }

    pub const fn session(&self) -> SessionId {
        match self {
            Self::Accepted(turn) => turn.session(),
            Self::Delegated(turn) => turn.session(),
        }
    }

    pub const fn turn(&self) -> TurnId {
        match self {
            Self::Accepted(turn) => turn.turn(),
            Self::Delegated(turn) => turn.turn(),
        }
    }

    pub const fn configuration(&self) -> &OriginConfiguration {
        match self {
            Self::Accepted(turn) => turn.configuration(),
            Self::Delegated(turn) => turn.configuration(),
        }
    }

    pub fn configuration_provenance(&self) -> TurnConfigurationProvenance {
        match self {
            Self::Accepted(turn) => turn.configuration_provenance().clone(),
            Self::Delegated(turn) => {
                TurnConfigurationProvenance::ExplicitOrigin(turn.configuration().clone())
            }
        }
    }

    pub const fn start(&self) -> AcceptedInputTurnStart {
        match self {
            Self::Accepted(turn) => turn.start(),
            Self::Delegated(turn) => turn.start(),
        }
    }

    pub const fn phase(&self) -> &ActiveTurnPhase {
        match self {
            Self::Accepted(turn) => turn.phase(),
            Self::Delegated(turn) => turn.phase(),
        }
    }

    pub fn pending_steering(&self) -> &[PendingSteeringInput] {
        match self {
            Self::Accepted(turn) => turn.pending_steering(),
            Self::Delegated(turn) => turn.pending_steering(),
        }
    }

    pub fn consumed_steering(&self) -> &[ConsumedSteeringInput] {
        match self {
            Self::Accepted(turn) => turn.consumed_steering(),
            Self::Delegated(turn) => turn.consumed_steering(),
        }
    }

    /// Applies one daemon-owned reconciliation attempt to a checked
    /// origin-agnostic model-call recovery wait.
    pub fn apply_automatic_model_call_reconciliation(
        self,
        call: crate::EndedModelCall,
        attempt: EndedTurnAttempt,
        source_snapshot: ResolvedContextFrontierSnapshot,
        recovery_attempt: std::num::NonZeroU32,
        identities: crate::AmbiguousModelCallTurnIdentities,
    ) -> Result<crate::ReconciliationRequiredModelCallTurn, crate::ModelCallClosureError> {
        crate::model_execution::apply_automatic_reconciliation(
            self,
            call,
            attempt,
            source_snapshot,
            recovery_attempt,
            identities,
        )
    }

    /// Cancels this turn while it is parked on exact runner-loss evidence.
    pub fn apply_interrupt_to_runner_recovery(
        self,
        starting_snapshot: ResolvedContextFrontierSnapshot,
        source_snapshot: ResolvedContextFrontierSnapshot,
        result_projection: Option<crate::PreparedToolResultProjection>,
        interrupt: AppliedInterruptCommandResult,
        identities: crate::CancelledModelCallTurnIdentities,
    ) -> Result<crate::CancelledModelCallTurn, crate::ModelCallClosureError> {
        crate::model_execution::apply_interrupt_to_runner_recovery_wait(
            self,
            starting_snapshot,
            source_snapshot,
            result_projection,
            interrupt,
            identities,
        )
    }

    /// Closes a delegated runner-loss wait that retained one ambiguous
    /// physical tool attempt without erasing that ambiguity.
    pub fn apply_interrupt_to_runner_tool_recovery(
        self,
        wait: crate::AwaitingToolRecovery,
        tool_attempt: crate::EndedToolAttempt,
        yielded_attempt: TurnAttemptId,
        result_projection: crate::PreparedToolResultProjection,
        interrupt: AppliedInterruptCommandResult,
        identities: crate::AmbiguousModelCallTurnIdentities,
    ) -> Result<crate::ReconciliationRequiredToolTurn, crate::ModelCallClosureError> {
        crate::model_execution::apply_interrupt_to_runner_tool_recovery_wait(
            self,
            wait,
            tool_attempt,
            yielded_attempt,
            result_projection,
            interrupt,
            identities,
        )
    }

    /// Cancels a delegated runner-loss wait after its retryable physical
    /// attempt has been retired as a known crash loss.
    pub fn apply_interrupt_to_retryable_runner_tool_recovery(
        self,
        starting_snapshot: ResolvedContextFrontierSnapshot,
        batch: crate::ToolBatch,
        result_projection: crate::PreparedToolResultProjection,
        interrupt: AppliedInterruptCommandResult,
        identities: crate::CancelledModelCallTurnIdentities,
    ) -> Result<crate::CancelledModelCallTurn, crate::ModelCallClosureError> {
        crate::model_execution::apply_interrupt_to_retryable_runner_tool_recovery_wait(
            self,
            starting_snapshot,
            batch,
            result_projection,
            interrupt,
            identities,
        )
    }

    #[cfg(test)]
    pub(crate) fn with_phase_for_test(&self, phase: ActiveTurnPhase) -> Self {
        match self {
            Self::Accepted(turn) => Self::Accepted(turn.with_phase_for_test(phase)),
            Self::Delegated(turn) => {
                let mut delegated = turn.clone();
                delegated.phase = phase;
                Self::Delegated(delegated)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn with_start_for_test(&self, start: AcceptedInputTurnStart) -> Self {
        match self {
            Self::Accepted(turn) => Self::Accepted(turn.with_start_for_test(start)),
            Self::Delegated(turn) => {
                let mut delegated = turn.clone();
                delegated.start = start;
                Self::Delegated(delegated)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn with_pending_steering_for_test(
        &self,
        pending: Box<[(AcceptedInputId, SessionInputPosition)]>,
    ) -> Self {
        match self {
            Self::Accepted(turn) => Self::Accepted(turn.with_pending_steering_for_test(pending)),
            Self::Delegated(turn) => {
                let pending = pending
                    .into_vec()
                    .into_iter()
                    .map(
                        |(accepted_input, acceptance_position)| PendingSteeringInput {
                            accepted_input: AcceptedInputLifecycle::new(
                                accepted_input,
                                AcceptedInputDisposition::PendingSteering {
                                    binding: crate::SteeringBinding::new(turn.turn),
                                },
                            ),
                            acceptance_position,
                        },
                    )
                    .collect::<Vec<_>>();
                Self::Delegated(
                    turn.clone()
                        .with_pending_steering(pending)
                        .expect("the test steering targets the delegated turn"),
                )
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn with_consumed_steering_for_test(
        &self,
        consumed: Box<[(AcceptedInputId, SessionInputPosition, crate::ModelCallId)]>,
    ) -> Self {
        match self {
            Self::Accepted(turn) => Self::Accepted(turn.with_consumed_steering_for_test(consumed)),
            Self::Delegated(turn) => {
                let consumed = consumed
                    .into_vec()
                    .into_iter()
                    .map(|(accepted_input, acceptance_position, call)| {
                        ConsumedSteeringReconstitutionInput::new(
                            turn.session,
                            AcceptedInputLifecycle::new(
                                accepted_input,
                                AcceptedInputDisposition::ConsumedAsSteering { call },
                            ),
                            acceptance_position,
                            turn.turn,
                        )
                    })
                    .collect();
                Self::Delegated(
                    turn.clone()
                        .with_consumed_steering(consumed)
                        .expect("the test steering targets the delegated turn"),
                )
            }
        }
    }
}

/// Complete durable facts for preparing one delegated initial-task activation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegatedTurnActivationInput {
    pub session: SessionId,
    pub turn: TurnId,
    pub spawning_request: ToolRequestId,
    pub task: DelegationContent,
    pub task_entry: SemanticTranscriptEntryReconstitutionInput,
    pub configuration: OriginConfiguration,
    pub starting_frontier: ContextFrontierId,
    pub initial_attempt: TurnAttemptId,
}

/// Complete durable facts for preparing one idle delegation-delivery wake.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegatedWakeTurnActivationInput {
    pub session: SessionId,
    pub turn: TurnId,
    pub first_delivery_sequence: NonZeroU64,
    pub through_delivery_sequence: NonZeroU64,
    pub deliveries: Vec<SemanticTranscriptEntryReconstitutionInput>,
    pub predecessor: TurnId,
    pub predecessor_snapshot: ResolvedContextFrontierSnapshot,
    pub configuration: OriginConfiguration,
    pub starting_frontier: ContextFrontierId,
    pub initial_attempt: TurnAttemptId,
}

/// Sealed delegated activation candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedDelegatedTurnActivation {
    turn: ActivatedDelegatedTurn,
    starting_entries: Vec<SemanticTranscriptEntry>,
    starting_snapshot: ResolvedContextFrontierSnapshot,
}

impl PreparedDelegatedTurnActivation {
    pub fn prepare(input: DelegatedTurnActivationInput) -> Option<Self> {
        if input.task_entry.source_session() != input.session
            || !matches!(
                input.task_entry.payload(),
                SemanticTranscriptEntryPayload::DelegatedTask {
                    spawning_request,
                    content,
                    ..
                } if *spawning_request == input.spawning_request && content == &input.task
            )
        {
            return None;
        }
        let task_entry = SemanticTranscriptEntry::from_validated_parts(
            input.task_entry.identity(),
            input.task_entry.source_session(),
            input.task_entry.payload().clone(),
        );
        let starting_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
            input.session,
            input.starting_frontier,
            vec![task_entry.reference()],
        )
        .ok()?;
        let start = AcceptedInputTurnStart::from_validated_eligibility(
            AcceptedInputStartingLineage::FirstInSession,
            starting_snapshot.frontier(),
        );
        let phase = ActiveTurnPhase::Running {
            current_attempt: CurrentTurnAttempt::prepared(input.initial_attempt),
        };
        Some(Self {
            turn: ActivatedDelegatedTurn {
                session: input.session,
                turn: input.turn,
                origin: ActivatedDelegatedTurnOrigin::InitialTask {
                    spawning_request: input.spawning_request,
                    task: input.task,
                },
                configuration: input.configuration,
                start,
                phase,
                pending_steering: Box::new([]),
                consumed_steering: Box::new([]),
            },
            starting_entries: vec![task_entry],
            starting_snapshot,
        })
    }

    pub fn prepare_wake(input: DelegatedWakeTurnActivationInput) -> Option<Self> {
        let expected_count = input
            .through_delivery_sequence
            .get()
            .checked_sub(input.first_delivery_sequence.get())?
            .checked_add(1)?;
        if usize::try_from(expected_count).ok()? != input.deliveries.len()
            || input.predecessor_snapshot.frontier().owning_session() != input.session
        {
            return None;
        }
        let mut entries = Vec::with_capacity(input.deliveries.len());
        for (offset, delivery) in input.deliveries.into_iter().enumerate() {
            let expected_sequence = input
                .first_delivery_sequence
                .get()
                .checked_add(u64::try_from(offset).ok()?)?;
            if delivery.source_session() != input.session
                || delegation_delivery_sequence(delivery.payload())?.get() != expected_sequence
            {
                return None;
            }
            entries.push(SemanticTranscriptEntry::from_validated_parts(
                delivery.identity(),
                delivery.source_session(),
                delivery.payload().clone(),
            ));
        }
        let starting_snapshot = input
            .predecessor_snapshot
            .derive_appending_candidate(
                input.starting_frontier,
                entries
                    .iter()
                    .map(SemanticTranscriptEntry::reference)
                    .collect(),
            )
            .ok()?;
        let start = AcceptedInputTurnStart::from_validated_eligibility(
            AcceptedInputStartingLineage::After {
                immediate_predecessor: input.predecessor,
            },
            starting_snapshot.frontier(),
        );
        Some(Self {
            turn: ActivatedDelegatedTurn {
                session: input.session,
                turn: input.turn,
                origin: ActivatedDelegatedTurnOrigin::PendingDeliveries {
                    first: input.first_delivery_sequence,
                    through: input.through_delivery_sequence,
                },
                configuration: input.configuration,
                start,
                phase: ActiveTurnPhase::Running {
                    current_attempt: CurrentTurnAttempt::prepared(input.initial_attempt),
                },
                pending_steering: Box::new([]),
                consumed_steering: Box::new([]),
            },
            starting_entries: entries,
            starting_snapshot,
        })
    }

    pub fn into_parts(
        self,
    ) -> (
        ActivatedDelegatedTurn,
        Vec<SemanticTranscriptEntry>,
        ResolvedContextFrontierSnapshot,
    ) {
        (self.turn, self.starting_entries, self.starting_snapshot)
    }

    /// Reconstitutes the same delegated origin under its exact stored live
    /// phase after the initial activation transaction.
    pub fn with_reconstituted_phase(
        mut self,
        phase: ActiveTurnSchedulingReconstitutionInput,
    ) -> Option<(
        ActivatedDelegatedTurn,
        Vec<SemanticTranscriptEntry>,
        ResolvedContextFrontierSnapshot,
    )> {
        if phase.owning_turn() != self.turn.turn {
            return None;
        }
        self.turn.phase = phase.canonical_evidence_free_phase()?;
        Some((self.turn, self.starting_entries, self.starting_snapshot))
    }

    /// Reconstitutes a delegated ambiguous model-call wait from its complete
    /// stored evidence. The returned call, attempt, and snapshots are checked
    /// against the delegated origin and its pinned configuration.
    pub fn with_reconstituted_model_call_recovery(
        mut self,
        input: DelegatedModelCallRecoveryReconstitutionInput,
    ) -> Option<(
        ActivatedTurn,
        crate::EndedModelCall,
        EndedTurnAttempt,
        ResolvedContextFrontierSnapshot,
        ResolvedContextFrontierSnapshot,
    )> {
        let ActiveTurnSchedulingReconstitutionInput {
            owning_turn,
            current_attempt: Some(current_attempt),
            state:
                StoredActiveTurnPhase::AwaitingModelCallRecovery {
                    call: recovery_call,
                    attempt_end,
                },
            executing_tool_batch: None,
        } = input.phase
        else {
            return None;
        };
        if owning_turn != self.turn.turn
            || input.call.id() != recovery_call
            || input.call.turn() != owning_turn
            || input.call.attempt() != current_attempt
            || input.call.selection() != *self.turn.configuration.effective().model()
            || input.call.state()
                != crate::ModelCallReconstitutionState::Terminal(ModelCallDisposition::Ambiguous)
        {
            return None;
        }
        let pinned = input.pinned_target.reconstitute_for_turn(owning_turn)?;
        let source_snapshot = input.source_snapshot.reconstitute()?;
        if !self
            .starting_snapshot
            .is_semantic_prefix_of(&source_snapshot)
        {
            return None;
        }
        let crate::ReconstitutedModelCall::Ended(call) =
            input.call.reconstitute(&source_snapshot, pinned).ok()?
        else {
            return None;
        };
        let running_attempt = CurrentTurnAttempt::prepared(current_attempt)
            .begin_running()
            .ok()?;
        let attempt = match attempt_end.end() {
            AttemptEnd::WithoutStop {
                disposition:
                    disposition @ (UnstoppedAttemptDisposition::Ambiguous
                    | UnstoppedAttemptDisposition::Lost),
            } => running_attempt.end_without_stop(*disposition).ok()?,
            AttemptEnd::AfterCancellation {
                cause,
                disposition:
                    disposition @ (CancellationStopDisposition::Ambiguous
                    | CancellationStopDisposition::Lost),
            } => {
                let interrupt = attempt_end.interrupt()?;
                if interrupt.session() != self.turn.session
                    || interrupt.proof() != *cause
                    || cause.predecessor() != owning_turn
                {
                    return None;
                }
                running_attempt
                    .request_cancellation(*cause)
                    .and_then(|attempt| attempt.end_after_cancellation(*cause, *disposition))
                    .ok()?
            }
            _ => return None,
        };
        self.turn.phase = ActiveTurnPhase::AwaitingRecoveryDecision {
            ambiguous_operations: NonEmptyIssuedOperationRefs::singleton(
                crate::IssuedOperationRef::ModelCall(recovery_call),
            ),
            applied_interrupt: attempt_end.interrupt().map(|interrupt| interrupt.proof()),
        };
        self.turn = self
            .turn
            .with_pending_steering(input.pending_steering)?
            .with_consumed_steering(input.consumed_steering)?;
        Some((
            self.turn.into(),
            call,
            attempt,
            source_snapshot,
            self.starting_snapshot,
        ))
    }
}

fn delegation_delivery_sequence(payload: &SemanticTranscriptEntryPayload) -> Option<NonZeroU64> {
    match payload {
        SemanticTranscriptEntryPayload::DelegationMessage {
            delivery_sequence, ..
        } => Some(*delivery_sequence),
        SemanticTranscriptEntryPayload::DelegationResult {
            mode: DelegationWaitMode::Background,
            delivery_sequence: Some(delivery_sequence),
            ..
        } => Some(*delivery_sequence),
        _ => None,
    }
}

/// Origin-agnostic sealed candidate for an atomic turn activation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreparedTurnActivation {
    Accepted(Box<PreparedAcceptedInputTurnActivation>),
    Delegated(Box<PreparedDelegatedTurnActivation>),
}

impl From<PreparedAcceptedInputTurnActivation> for PreparedTurnActivation {
    fn from(value: PreparedAcceptedInputTurnActivation) -> Self {
        Self::Accepted(Box::new(value))
    }
}

impl From<PreparedDelegatedTurnActivation> for PreparedTurnActivation {
    fn from(value: PreparedDelegatedTurnActivation) -> Self {
        Self::Delegated(Box::new(value))
    }
}

impl PreparedTurnActivation {
    pub fn turn(&self) -> ActivatedTurn {
        match self {
            Self::Accepted(prepared) => prepared.turn().clone().into(),
            Self::Delegated(prepared) => prepared.turn.clone().into(),
        }
    }

    pub fn starting_entries(&self) -> &[SemanticTranscriptEntry] {
        match self {
            Self::Accepted(prepared) => prepared.starting_entries(),
            Self::Delegated(prepared) => &prepared.starting_entries,
        }
    }

    pub const fn starting_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        match self {
            Self::Accepted(prepared) => prepared.starting_snapshot(),
            Self::Delegated(prepared) => &prepared.starting_snapshot,
        }
    }
}

/// One sealed candidate for the atomic eligibility commit.
///
/// The candidate contains the exact origin entry, prefix-preserving starting
/// snapshot, opaque start, and active turn with one prepared initial attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedAcceptedInputTurnActivation {
    turn: ActivatedAcceptedInputTurn,
    starting_entries: AcceptedInputTurnStartingEntries,
    starting_snapshot: ResolvedContextFrontierSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AcceptedInputTurnStartingEntries {
    Origin([SemanticTranscriptEntry; 1]),
    ModelIdentityThenOrigin([SemanticTranscriptEntry; 2]),
}

impl AcceptedInputTurnStartingEntries {
    fn as_slice(&self) -> &[SemanticTranscriptEntry] {
        match self {
            Self::Origin(entries) => entries,
            Self::ModelIdentityThenOrigin(entries) => entries,
        }
    }

    fn origin(&self) -> &SemanticTranscriptEntry {
        match self {
            Self::Origin([origin]) | Self::ModelIdentityThenOrigin([_, origin]) => origin,
        }
    }

    fn into_boxed_slice(self) -> Box<[SemanticTranscriptEntry]> {
        match self {
            Self::Origin(entries) => Box::new(entries),
            Self::ModelIdentityThenOrigin(entries) => Box::new(entries),
        }
    }
}

impl PreparedAcceptedInputTurnActivation {
    /// Borrows the exact initial active turn.
    pub const fn turn(&self) -> &ActivatedAcceptedInputTurn {
        &self.turn
    }

    /// Returns the newly created origin semantic entry.
    pub fn origin_entry(&self) -> SemanticTranscriptEntry {
        self.starting_entries.origin().clone()
    }

    /// Borrows the ordered entries appended at the turn-start boundary.
    pub fn starting_entries(&self) -> &[SemanticTranscriptEntry] {
        self.starting_entries.as_slice()
    }

    /// Borrows the new immutable starting snapshot.
    pub const fn starting_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.starting_snapshot
    }

    /// Returns the opaque eligibility-fixed start.
    pub const fn start(&self) -> AcceptedInputTurnStart {
        self.turn.start
    }

    /// Returns all atomic commit values.
    pub fn into_parts(
        self,
    ) -> (
        ActivatedAcceptedInputTurn,
        Box<[SemanticTranscriptEntry]>,
        ResolvedContextFrontierSnapshot,
    ) {
        (
            self.turn,
            self.starting_entries.into_boxed_slice(),
            self.starting_snapshot,
        )
    }
}

/// Why the complete scheduling projection cannot prepare an activation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcceptedInputEligibilityFailure {
    /// A turn already owns the session's progressing slot.
    ActiveTurnPresent {
        /// The exact active slot owner.
        turn: TurnId,
    },
    /// A dedicated compaction call owns the session-wide execution slot.
    ContextCompactionInProgress {
        /// The exact unfinished compaction call.
        call: crate::ModelCallId,
    },
    /// No queued accepted-input turn exists.
    NoQueuedTurn,
    /// The proposed origin entry identity is already present.
    OriginEntryIdentityAlreadyExists,
    /// The proposed injected model-identity entry is already present.
    ModelIdentityEntryIdentityAlreadyExists,
    /// The proposed session-scoped snapshot identity is already present.
    StartingFrontierIdentityAlreadyExists,
    /// The proposed initial-attempt identity already appears in the complete
    /// scheduling projection's represented attempt history.
    InitialAttemptIdentityAlreadyExists,
    /// Internal preparation could not construct the origin-only first
    /// frontier from the already-validated projection.
    InternalOriginFrontierConstructionFailed,
    /// Internal preparation found earliest queued work after a predecessor
    /// without the terminal frontier guaranteed by scheduling reconstitution.
    InternalPredecessorTerminalFrontierMissing {
        /// The predecessor whose validated terminal frontier was absent.
        predecessor: TurnId,
    },
    /// Internal preparation could not append the fresh origin entry to the
    /// predecessor frontier guaranteed by scheduling reconstitution.
    InternalStartingFrontierDerivationFailed,
}

/// Rejected eligibility preparation retaining the complete projection and
/// supplied identities unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputEligibilityError {
    projection: Box<AcceptedInputSchedulingProjection>,
    identities: AcceptedInputTurnActivationIdentities,
    failure: AcceptedInputEligibilityFailure,
}

impl AcceptedInputEligibilityError {
    /// Borrows the unchanged complete scheduling projection.
    pub const fn projection(&self) -> &AcceptedInputSchedulingProjection {
        &self.projection
    }

    /// Returns the unchanged supplied identities.
    pub const fn identities(&self) -> AcceptedInputTurnActivationIdentities {
        self.identities
    }

    /// Returns the exact eligibility failure.
    pub const fn failure(&self) -> AcceptedInputEligibilityFailure {
        self.failure
    }

    /// Returns every unchanged input and the exact failure.
    pub fn into_parts(
        self,
    ) -> (
        AcceptedInputSchedulingProjection,
        AcceptedInputTurnActivationIdentities,
        AcceptedInputEligibilityFailure,
    ) {
        (*self.projection, self.identities, self.failure)
    }
}

/// Fresh identities supplied for one failed-terminal startup candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputTurnFailureIdentities {
    failure_entry: SemanticTranscriptEntryId,
    terminal_frontier: ContextFrontierId,
    pending_steering_reclassifications: Vec<PendingSteeringReclassificationIdentity>,
}

impl AcceptedInputTurnFailureIdentities {
    /// Supplies the semantic failure-entry and terminal-frontier identities.
    pub const fn new(
        failure_entry: SemanticTranscriptEntryId,
        terminal_frontier: ContextFrontierId,
    ) -> Self {
        Self {
            failure_entry,
            terminal_frontier,
            pending_steering_reclassifications: Vec::new(),
        }
    }

    /// Supplies one fresh successor identity per pending steering input, in
    /// session acceptance order.
    pub fn with_pending_steering_reclassifications(
        mut self,
        identities: Vec<PendingSteeringReclassificationIdentity>,
    ) -> Self {
        self.pending_steering_reclassifications = identities;
        self
    }

    /// Returns the proposed failed-marker identity.
    pub const fn failure_entry(&self) -> SemanticTranscriptEntryId {
        self.failure_entry
    }

    /// Returns the proposed terminal-frontier identity.
    pub const fn terminal_frontier(&self) -> ContextFrontierId {
        self.terminal_frontier
    }

    /// Borrows the proposed successor identities for pending steering.
    pub fn pending_steering_reclassifications(&self) -> &[PendingSteeringReclassificationIdentity] {
        &self.pending_steering_reclassifications
    }
}

/// Exact failed turn state prepared by the startup scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailedAcceptedInputTurn {
    session: SessionId,
    turn: TurnId,
    accepted_input: AcceptedInputLifecycle,
    order: AcceptedInputQueueOrder,
    start: AcceptedInputTurnStart,
    ended_attempt: EndedTurnAttempt,
    disposition: TurnDisposition,
    terminal_frontier: ContextFrontierId,
}

impl FailedAcceptedInputTurn {
    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the failed logical turn.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Borrows the exact accepted origin input.
    pub const fn accepted_input(&self) -> &AcceptedInputLifecycle {
        &self.accepted_input
    }

    /// Returns the immutable accepted-input queue order.
    pub const fn order(&self) -> AcceptedInputQueueOrder {
        self.order
    }

    /// Returns the eligibility-fixed lineage and starting frontier.
    pub const fn start(&self) -> AcceptedInputTurnStart {
        self.start
    }

    /// Borrows the exact Lost physical-attempt history.
    pub const fn ended_attempt(&self) -> &EndedTurnAttempt {
        &self.ended_attempt
    }

    /// Borrows the failed logical-turn disposition.
    pub const fn disposition(&self) -> &TurnDisposition {
        &self.disposition
    }

    /// Returns the complete terminal-frontier identity.
    pub const fn terminal_frontier(&self) -> ContextFrontierId {
        self.terminal_frontier
    }
}

/// One sealed atomic failed-terminal startup-recovery candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedAcceptedInputTurnFailure {
    turn: FailedAcceptedInputTurn,
    failure_entry: SemanticTranscriptEntry,
    terminal_snapshot: ResolvedContextFrontierSnapshot,
    reclassified_pending_steering: Box<[ReclassifiedPendingSteeringTurn]>,
}

impl PreparedAcceptedInputTurnFailure {
    /// Borrows the exact failed logical turn and ended physical attempt.
    pub const fn turn(&self) -> &FailedAcceptedInputTurn {
        &self.turn
    }

    /// Returns the newly created `TurnFailed` semantic entry.
    pub fn failure_entry(&self) -> SemanticTranscriptEntry {
        self.failure_entry.clone()
    }

    /// Borrows the start-prefix-preserving terminal snapshot.
    pub const fn terminal_snapshot(&self) -> &ResolvedContextFrontierSnapshot {
        &self.terminal_snapshot
    }

    /// Borrows the queued successors reclassified from pending steering.
    pub fn reclassified_pending_steering(&self) -> &[ReclassifiedPendingSteeringTurn] {
        &self.reclassified_pending_steering
    }

    /// Returns all atomic commit values.
    pub fn into_parts(
        self,
    ) -> (
        FailedAcceptedInputTurn,
        SemanticTranscriptEntry,
        ResolvedContextFrontierSnapshot,
        Box<[ReclassifiedPendingSteeringTurn]>,
    ) {
        (
            self.turn,
            self.failure_entry,
            self.terminal_snapshot,
            self.reclassified_pending_steering,
        )
    }
}

/// Why the complete scheduling projection cannot prepare startup failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcceptedInputTurnFailureFailure {
    /// No turn currently owns the session's progressing slot.
    NoActiveTurn,
    /// The supplied successor identities do not match the pending steering.
    PendingSteeringReclassificationMismatch,
    /// The proposed failed-marker identity is already present.
    FailureEntryIdentityAlreadyExists,
    /// The proposed terminal-frontier identity is already present.
    TerminalFrontierIdentityAlreadyExists,
    /// Canonical active attempt facts unexpectedly rejected a Lost end.
    ActiveAttemptCannotEndLost,
    /// Canonical active scheduling facts unexpectedly omitted their start.
    ActiveStartMissing,
    /// Canonical scheduling facts unexpectedly omitted the starting snapshot.
    StartingSnapshotMissing,
    /// Canonical fresh failure facts unexpectedly could not append.
    TerminalFrontierCannotAppend,
}

/// Rejected startup-failure preparation retaining every input unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputTurnFailureError {
    projection: Box<AcceptedInputSchedulingProjection>,
    identities: AcceptedInputTurnFailureIdentities,
    failure: AcceptedInputTurnFailureFailure,
}

impl AcceptedInputTurnFailureError {
    /// Borrows the unchanged complete scheduling projection.
    pub const fn projection(&self) -> &AcceptedInputSchedulingProjection {
        &self.projection
    }

    /// Borrows the unchanged supplied identities.
    pub const fn identities(&self) -> &AcceptedInputTurnFailureIdentities {
        &self.identities
    }

    /// Returns the exact preparation failure.
    pub const fn failure(&self) -> AcceptedInputTurnFailureFailure {
        self.failure
    }

    /// Returns every unchanged input and the exact failure.
    pub fn into_parts(
        self,
    ) -> (
        AcceptedInputSchedulingProjection,
        AcceptedInputTurnFailureIdentities,
        AcceptedInputTurnFailureFailure,
    ) {
        (*self.projection, self.identities, self.failure)
    }
}

fn reconstitute(
    input: AcceptedInputSchedulingReconstitutionInput,
) -> Result<AcceptedInputSchedulingProjection, AcceptedInputSchedulingReconstitutionError> {
    match reconstitute_inner(&input) {
        Ok(projection) => Ok(projection),
        Err(failure) => Err(AcceptedInputSchedulingReconstitutionError {
            input: Box::new(input),
            failure,
        }),
    }
}

fn applied_interrupt_matches_scheduling(
    interrupt: AppliedInterruptCommandResult,
    session: SessionId,
    predecessor: TurnId,
    records_by_turn: &BTreeMap<TurnId, &AcceptedInputTurnSchedulingRecord>,
) -> bool {
    let successor = records_by_turn.get(&interrupt.successor());
    interrupt.session() == session
        && interrupt.proof().predecessor() == predecessor
        && successor.is_some_and(|successor| {
            successor.stored_session == session
                && successor.accepted_input.id() == interrupt.accepted_input()
                && successor.order == interrupt.successor_order()
        })
}

fn terminal_attempt_end_matches(
    attempt_end: &TerminalAttemptEndReconstitutionInput,
    session: SessionId,
    turn: TurnId,
    records_by_turn: &BTreeMap<TurnId, &AcceptedInputTurnSchedulingRecord>,
    allowed_without_stop: &[UnstoppedAttemptDisposition],
    allowed_after_cancellation: &[CancellationStopDisposition],
) -> bool {
    match attempt_end.end() {
        AttemptEnd::WithoutStop { disposition } => {
            attempt_end.interrupt().is_none() && allowed_without_stop.contains(disposition)
        }
        AttemptEnd::AfterCancellation { cause, disposition } => {
            allowed_after_cancellation.contains(disposition)
                && attempt_end.interrupt().is_some_and(|interrupt| {
                    interrupt.proof() == *cause
                        && applied_interrupt_matches_scheduling(
                            interrupt,
                            session,
                            turn,
                            records_by_turn,
                        )
                })
        }
        AttemptEnd::AfterFatalMismatch { .. } => false,
    }
}

fn reconstitute_inner(
    input: &AcceptedInputSchedulingReconstitutionInput,
) -> Result<AcceptedInputSchedulingProjection, AcceptedInputSchedulingReconstitutionFailure> {
    let imported_session = match (
        input.session.creation_provenance().ancestry(),
        input.imported_session.as_ref(),
    ) {
        (TranscriptAncestry::None, None) => None,
        (TranscriptAncestry::None, Some(_)) => {
            return Err(AcceptedInputSchedulingReconstitutionFailure::UnexpectedImportedSession);
        }
        (TranscriptAncestry::ImportedConversation { .. }, None) => {
            return Err(AcceptedInputSchedulingReconstitutionFailure::MissingImportedSession);
        }
        (TranscriptAncestry::ImportedConversation { .. }, Some(imported))
            if imported.session() == &input.session =>
        {
            Some(imported)
        }
        (TranscriptAncestry::ImportedConversation { .. }, Some(_)) => {
            return Err(AcceptedInputSchedulingReconstitutionFailure::ImportedSessionMismatch);
        }
        (TranscriptAncestry::SingleSource { .. }, _) => {
            return Err(AcceptedInputSchedulingReconstitutionFailure::UnsupportedSessionAncestry);
        }
    };

    let session = input.session.id();
    let mut accepted_input_turns = BTreeMap::new();
    for record in &input.turns {
        validate_record_correlations(session, record)?;
        if accepted_input_turns
            .insert(record.accepted_input.id(), record.turn)
            .is_some()
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateAcceptedInput {
                    accepted_input: record.accepted_input.id(),
                },
            );
        }
    }

    let records_by_turn = input
        .turns
        .iter()
        .map(|record| (record.turn, record))
        .collect::<BTreeMap<_, _>>();
    let mut preceding_non_accepted_terminal_turns = BTreeSet::new();
    let mut preceding_non_accepted_successors = BTreeMap::new();
    for (stored_session, predecessor, successor, _, _) in &input.preceding_non_accepted_terminals {
        if *stored_session != session
            || records_by_turn.contains_key(predecessor)
            || !records_by_turn.contains_key(successor)
            || !preceding_non_accepted_terminal_turns.insert(*predecessor)
            || preceding_non_accepted_successors
                .insert(*successor, *predecessor)
                .is_some()
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::TurnSessionMismatch {
                    turn: *predecessor,
                },
            );
        }
    }
    let mut external_interrupt_successors = BTreeSet::new();
    for (successor, predecessor) in &preceding_non_accepted_successors {
        match records_by_turn[successor].order.priority() {
            AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                predecessor: recorded,
            } if recorded == *predecessor => {
                external_interrupt_successors.insert(*successor);
            }
            AcceptedInputQueuePriority::Ordinary => {}
            AcceptedInputQueuePriority::InterruptImmediatelyAfter { .. } => {
                return Err(
                    AcceptedInputSchedulingReconstitutionFailure::TurnSessionMismatch {
                        turn: *predecessor,
                    },
                );
            }
        }
    }
    // The generic accepted-input order owns only accepted origins. A
    // runtime-relevant record can instead be the immediate successor of a
    // separately authenticated non-accepted terminal boundary; treat every
    // such external edge as a derivation root while retaining and validating
    // interrupt priority below.
    let ordinary_roots = input
        .turns
        .iter()
        .filter(|record| record.order.priority() == AcceptedInputQueuePriority::Ordinary)
        .map(|record| record.turn)
        .collect::<BTreeSet<_>>();
    let queued_turns = input
        .turns
        .iter()
        .filter(|record| matches!(record.state, AcceptedInputTurnSchedulingRecordState::Queued))
        .map(|record| record.turn)
        .collect::<BTreeSet<_>>();
    let queue_work = input.turns.iter().map(|record| {
        let order = if preceding_non_accepted_successors.contains_key(&record.turn) {
            AcceptedInputQueueOrder::ordinary(record.order.acceptance_position())
        } else {
            record.order
        };
        AcceptedInputQueueWork::new(record.queue_session, record.queue_turn, order)
    });
    let total_order = promote_external_interrupt_chains(
        derive_accepted_input_total_order(queue_work).map_err(|error| {
            AcceptedInputSchedulingReconstitutionFailure::InvalidQueueOrder { error }
        })?,
        external_interrupt_successors,
        &ordinary_roots,
        &queued_turns,
    );
    let execution_position_by_turn = total_order
        .iter()
        .copied()
        .enumerate()
        .map(|(position, turn)| (turn, position))
        .collect::<BTreeMap<_, _>>();
    let mut delegated_turns = BTreeMap::new();
    for fact in input.delegated_turns.iter().copied() {
        let turn = fact.turn();
        if records_by_turn.contains_key(&turn) || delegated_turns.insert(turn, fact).is_some() {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DelegatedTurnFactMismatch { turn },
            );
        }
    }
    for record in records_by_turn.values() {
        if !origin_delivery_matches_record(
            record.origin_delivery,
            record,
            &records_by_turn,
            &preceding_non_accepted_terminal_turns,
        ) {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
                    turn: record.turn,
                },
            );
        }
    }

    let mut semantic_entries = imported_session
        .into_iter()
        .flat_map(|imported| imported.semantic_entries())
        .map(|entry| (entry.reference(), entry.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut origin_by_turn = BTreeMap::new();
    let mut failure_by_turn = BTreeMap::new();
    let mut steering_by_input = BTreeMap::new();
    let mut model_identity_by_turn = BTreeMap::new();
    let mut summary_by_call = BTreeMap::new();
    let mut assistant_by_call = BTreeMap::<crate::ModelCallId, BTreeSet<_>>::new();
    let mut completion_by_turn = BTreeMap::new();
    let mut cancellation_by_turn = BTreeMap::new();
    for candidate in &input.semantic_entries {
        if candidate.source_session() != session {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::SemanticEntrySourceSessionMismatch {
                    entry: candidate.identity(),
                },
            );
        }

        let entry = SemanticTranscriptEntry::from_validated_parts(
            candidate.identity(),
            candidate.source_session(),
            candidate.payload().clone(),
        );
        let entry_reference = entry.reference();
        if semantic_entries.insert(entry_reference, entry).is_some() {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntry {
                    entry: entry_reference,
                },
            );
        }

        match candidate.payload() {
            InitialSemanticTranscriptEntryPayload::Imported { .. } => {
                return Err(
                    AcceptedInputSchedulingReconstitutionFailure::UnsupportedSemanticEntry {
                        entry: candidate.identity(),
                    },
                );
            }
            InitialSemanticTranscriptEntryPayload::ContextSummary { producing_call, .. } => {
                if summary_by_call
                    .insert(*producing_call, entry_reference)
                    .is_some()
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
                            entry: candidate.identity(),
                        },
                    );
                }
            }
            InitialSemanticTranscriptEntryPayload::OriginAcceptedInput { accepted_input } => {
                let Some(turn) = accepted_input_turns.get(accepted_input).copied() else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntrySubjectMissing {
                            entry: candidate.identity(),
                        },
                    );
                };
                let record = records_by_turn[&turn];
                if matches!(
                    &record.state,
                    AcceptedInputTurnSchedulingRecordState::Queued
                ) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntryStateMismatch {
                            entry: candidate.identity(),
                        },
                    );
                }
                if origin_by_turn.insert(turn, entry_reference).is_some() {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
                            entry: candidate.identity(),
                        },
                    );
                }
            }
            InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                accepted_input,
                source_turn,
            } => {
                if steering_by_input
                    .insert(*accepted_input, (entry_reference, *source_turn))
                    .is_some()
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
                            entry: candidate.identity(),
                        },
                    );
                }
            }
            InitialSemanticTranscriptEntryPayload::ModelIdentityChanged {
                turn,
                defaults_version,
                selected,
            } => {
                let Some(configuration_matches) = records_by_turn
                    .get(turn)
                    .map(|record| {
                        !matches!(
                            &record.state,
                            AcceptedInputTurnSchedulingRecordState::Queued
                        ) && record.origin_configuration.session_defaults_version()
                            == *defaults_version
                            && record
                                .origin_configuration
                                .effective()
                                .model()
                                .selected_direct()
                                == *selected
                    })
                    .or_else(|| {
                        delegated_turns.get(turn).map(|fact| {
                            fact.defaults_version() == *defaults_version
                                && fact.selected() == *selected
                        })
                    })
                else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntrySubjectMissing {
                            entry: candidate.identity(),
                        },
                    );
                };
                if !configuration_matches {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntryStateMismatch {
                            entry: candidate.identity(),
                        },
                    );
                }
                if model_identity_by_turn
                    .insert(*turn, entry_reference)
                    .is_some()
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
                            entry: candidate.identity(),
                        },
                    );
                }
            }
            InitialSemanticTranscriptEntryPayload::TurnFailed { turn } => {
                let Some(state_matches) = records_by_turn
                    .get(turn)
                    .map(|record| {
                        matches!(
                            &record.state,
                            AcceptedInputTurnSchedulingRecordState::TerminalFailed { .. }
                        )
                    })
                    .or_else(|| {
                        delegated_turns.get(turn).map(|fact| {
                            fact.state() == DelegatedTurnSchedulingState::TerminalFailed
                        })
                    })
                else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntrySubjectMissing {
                            entry: candidate.identity(),
                        },
                    );
                };
                if !state_matches {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntryStateMismatch {
                            entry: candidate.identity(),
                        },
                    );
                }
                if failure_by_turn.insert(*turn, entry_reference).is_some() {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
                            entry: candidate.identity(),
                        },
                    );
                }
            }
            InitialSemanticTranscriptEntryPayload::AssistantText { producing_call, .. }
            | InitialSemanticTranscriptEntryPayload::ProviderCompaction {
                producing_call, ..
            }
            | InitialSemanticTranscriptEntryPayload::AssistantToolUse { producing_call, .. } => {
                assistant_by_call
                    .entry(*producing_call)
                    .or_default()
                    .insert(entry_reference);
            }
            InitialSemanticTranscriptEntryPayload::ToolExecutionResult { .. }
            | InitialSemanticTranscriptEntryPayload::ToolDenied { .. }
            | InitialSemanticTranscriptEntryPayload::ToolClosed { .. } => {}
            InitialSemanticTranscriptEntryPayload::DelegatedTask { .. }
            | InitialSemanticTranscriptEntryPayload::DelegationMessage { .. }
            | InitialSemanticTranscriptEntryPayload::DelegationResult { .. } => {}
            InitialSemanticTranscriptEntryPayload::TurnCompleted { turn } => {
                let Some(state_matches) = records_by_turn
                    .get(turn)
                    .map(|record| {
                        matches!(
                            &record.state,
                            AcceptedInputTurnSchedulingRecordState::TerminalCompleted { .. }
                        )
                    })
                    .or_else(|| {
                        delegated_turns.get(turn).map(|fact| {
                            fact.state() == DelegatedTurnSchedulingState::TerminalCompleted
                        })
                    })
                else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntrySubjectMissing {
                            entry: candidate.identity(),
                        },
                    );
                };
                if !state_matches {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntryStateMismatch {
                            entry: candidate.identity(),
                        },
                    );
                }
                if completion_by_turn.insert(*turn, entry_reference).is_some() {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
                            entry: candidate.identity(),
                        },
                    );
                }
            }
            InitialSemanticTranscriptEntryPayload::TurnCancelled { turn } => {
                let Some(state_matches) = records_by_turn
                    .get(turn)
                    .map(|record| {
                        matches!(
                            &record.state,
                            AcceptedInputTurnSchedulingRecordState::TerminalCancelled { .. }
                        )
                    })
                    .or_else(|| {
                        delegated_turns.get(turn).map(|fact| {
                            fact.state() == DelegatedTurnSchedulingState::TerminalCancelled
                        })
                    })
                else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntrySubjectMissing {
                            entry: candidate.identity(),
                        },
                    );
                };
                if !state_matches {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::SemanticEntryStateMismatch {
                            entry: candidate.identity(),
                        },
                    );
                }
                if cancellation_by_turn
                    .insert(*turn, entry_reference)
                    .is_some()
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
                            entry: candidate.identity(),
                        },
                    );
                }
            }
        }
    }

    let initial_seed_frontier =
        imported_session.map(|imported| imported.seed_snapshot().frontier().snapshot());
    let mut snapshots = imported_session
        .into_iter()
        .map(|imported| {
            (
                imported.seed_snapshot().frontier().snapshot(),
                imported.seed_snapshot().clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut frontier_entry_validation = ContextFrontierEntryValidationCache::default();
    for candidate in &input.snapshots {
        if candidate.owning_session() != session {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::SnapshotOwningSessionMismatch {
                    snapshot: candidate.snapshot(),
                },
            );
        }
        if let Some(entry) = candidate
            .first_missing_entry(&mut frontier_entry_validation, |entry| {
                semantic_entries.contains_key(entry)
            })
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::SnapshotEntryMissing {
                    snapshot: candidate.snapshot(),
                    entry,
                },
            );
        }
        let snapshot = candidate.snapshot();
        let resolved =
            ResolvedContextFrontierSnapshot::try_from_reconstitution_input(candidate.clone())
                .map_err(|_| {
                    AcceptedInputSchedulingReconstitutionFailure::InvalidSnapshotMembership {
                        snapshot,
                    }
                })?;
        if snapshots.insert(snapshot, resolved).is_some() {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateSnapshot { snapshot },
            );
        }
    }

    let mut preceding_non_accepted_terminals = BTreeMap::new();
    for (_, turn, _, terminal_frontier, selected) in &input.preceding_non_accepted_terminals {
        let snapshot = snapshots.get(terminal_frontier).cloned().ok_or(
            AcceptedInputSchedulingReconstitutionFailure::StartingSnapshotMissing { turn: *turn },
        )?;
        preceding_non_accepted_terminals.insert(*turn, (snapshot, *selected));
    }

    let mut compaction_calls = BTreeMap::new();
    let mut compaction_snapshots = BTreeSet::new();
    for candidate in &input.compaction_calls {
        let call = candidate.id();
        let source = snapshots.get(&candidate.source_snapshot()).ok_or(
            AcceptedInputSchedulingReconstitutionFailure::CompactionCallSnapshotMissing { call },
        )?;
        let reconstituted = candidate.clone().reconstitute(source).map_err(|_| {
            AcceptedInputSchedulingReconstitutionFailure::InvalidCompactionCall { call }
        })?;
        compaction_snapshots.insert(candidate.source_snapshot());
        if compaction_calls.insert(call, reconstituted).is_some() {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateCompactionCall { call },
            );
        }
    }
    let mut compactions = BTreeMap::new();
    let mut referenced_compaction_calls = BTreeSet::new();
    for candidate in &input.compactions {
        let compaction = candidate.id();
        let source = snapshots.get(&candidate.source_snapshot()).ok_or(
            AcceptedInputSchedulingReconstitutionFailure::CompactionSnapshotMissing { compaction },
        )?;
        let result = snapshots.get(&candidate.result_snapshot()).ok_or(
            AcceptedInputSchedulingReconstitutionFailure::CompactionSnapshotMissing { compaction },
        )?;
        compaction_snapshots.insert(candidate.source_snapshot());
        compaction_snapshots.insert(candidate.result_snapshot());
        let call = compaction_calls.get(&candidate.producing_call()).ok_or(
            AcceptedInputSchedulingReconstitutionFailure::CompactionEvidenceMissing { compaction },
        )?;
        let summary_reference = summary_by_call.get(&candidate.producing_call()).ok_or(
            AcceptedInputSchedulingReconstitutionFailure::CompactionEvidenceMissing { compaction },
        )?;
        if summary_reference.entry() != candidate.summary_entry() {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::CompactionEvidenceMissing {
                    compaction,
                },
            );
        }
        let summary = semantic_entries.get(summary_reference).ok_or(
            AcceptedInputSchedulingReconstitutionFailure::CompactionEvidenceMissing { compaction },
        )?;
        let source_entries = source
            .ordered_entries()
            .map(|reference| semantic_entries[&reference].clone())
            .collect::<Vec<_>>();
        let result_entries = result
            .ordered_entries()
            .map(|reference| semantic_entries[&reference].clone())
            .collect::<Vec<_>>();
        let reconstituted = candidate
            .clone()
            .reconstitute(
                source,
                result,
                &source_entries,
                &result_entries,
                summary,
                call,
            )
            .map_err(
                |_| AcceptedInputSchedulingReconstitutionFailure::InvalidCompaction { compaction },
            )?;
        referenced_compaction_calls.insert(candidate.producing_call());
        if compactions.insert(compaction, reconstituted).is_some() {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateCompaction { compaction },
            );
        }
    }
    let mut root = None;
    let mut predecessors = BTreeSet::new();
    for compaction in compactions.values() {
        let Some(predecessor) = compaction.predecessor() else {
            if root.replace(compaction.id()).is_some() {
                return Err(
                    AcceptedInputSchedulingReconstitutionFailure::InvalidCompactionChain {
                        compaction: compaction.id(),
                    },
                );
            }
            continue;
        };
        if !predecessors.insert(predecessor) {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::InvalidCompactionChain {
                    compaction: compaction.id(),
                },
            );
        }
        let previous = compactions.get(&predecessor).ok_or(
            AcceptedInputSchedulingReconstitutionFailure::InvalidCompactionChain {
                compaction: compaction.id(),
            },
        )?;
        let previous_result = snapshots[&previous.result_frontier().snapshot()].clone();
        let source = &snapshots[&compaction.source_frontier().snapshot()];
        if !previous_result.is_semantic_prefix_of(source) {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::InvalidCompactionChain {
                    compaction: compaction.id(),
                },
            );
        }
    }
    if root.is_none()
        && let Some(compaction) = compactions.keys().next().copied()
    {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::InvalidCompactionChain { compaction },
        );
    }
    let mut leaves = compactions
        .values()
        .filter(|compaction| !predecessors.contains(&compaction.id()));
    let latest_compaction = leaves.next();
    if let Some(extra) = leaves.next() {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::InvalidCompactionChain {
                compaction: extra.id(),
            },
        );
    }
    let mut chain_members = BTreeSet::new();
    let mut chain_ids = Vec::with_capacity(compactions.len());
    let mut chain_cursor = latest_compaction.map(crate::ContextCompaction::id);
    while let Some(compaction) = chain_cursor {
        if !chain_members.insert(compaction) {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::InvalidCompactionChain { compaction },
            );
        }
        chain_ids.push(compaction);
        chain_cursor = compactions[&compaction].predecessor();
    }
    chain_ids.reverse();
    let compaction_chain = chain_ids
        .iter()
        .map(|compaction| &compactions[compaction])
        .collect::<Vec<_>>();
    if let Some(compaction) = compactions
        .keys()
        .find(|compaction| !chain_members.contains(compaction))
        .copied()
    {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::InvalidCompactionChain { compaction },
        );
    }
    let latest_compaction_result =
        latest_compaction.map(|compaction| compaction.result_frontier().snapshot());
    if let Some(call) = summary_by_call
        .keys()
        .find(|call| !referenced_compaction_calls.contains(call))
        .copied()
    {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::UnreferencedCompactionEvidence { call },
        );
    }
    if let Some(call) = compaction_calls
        .values()
        .find(|call| {
            call.state()
                == crate::ContextCompactionModelCallState::Terminal(ModelCallDisposition::Completed)
                && !referenced_compaction_calls.contains(&call.id())
        })
        .map(crate::ContextCompactionModelCall::id)
    {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::UnreferencedCompactionEvidence { call },
        );
    }
    let mut active_compaction_calls = compaction_calls.values().filter(|call| {
        matches!(
            call.state(),
            crate::ContextCompactionModelCallState::Prepared
                | crate::ContextCompactionModelCallState::InFlight
        )
    });
    let active_compaction_call = active_compaction_calls
        .next()
        .map(crate::ContextCompactionModelCall::id);
    if let Some(call) = active_compaction_calls
        .next()
        .map(crate::ContextCompactionModelCall::id)
    {
        return Err(AcceptedInputSchedulingReconstitutionFailure::InvalidCompactionCall { call });
    }

    let mut pinned_targets = BTreeMap::new();
    for candidate in &input.pinned_targets {
        let turn = candidate.turn();
        let Some(pinned) = candidate.reconstitute_for_turn(turn) else {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::UnreferencedPinnedTarget { turn },
            );
        };
        if pinned_targets.insert(turn, pinned).is_some() {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicatePinnedTarget { turn },
            );
        }
    }
    let mut referenced_pinned_targets = BTreeSet::new();
    let mut model_calls = BTreeMap::new();
    for candidate in &input.model_calls {
        let call = candidate.id();
        if compaction_calls.contains_key(&call) {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateModelCallIdentityAcrossKinds {
                    call,
                },
            );
        }
        let snapshot = snapshots.get(&candidate.frontier()).ok_or(
            AcceptedInputSchedulingReconstitutionFailure::ModelCallSnapshotMissing { call },
        )?;
        let Some(pinned) = pinned_targets.get(&candidate.turn()).copied() else {
            return Err(AcceptedInputSchedulingReconstitutionFailure::PinnedTargetMissing { call });
        };
        let reconstituted = candidate
            .reconstitute(snapshot, pinned)
            .map_err(|_| AcceptedInputSchedulingReconstitutionFailure::InvalidModelCall { call })?;
        referenced_pinned_targets.insert(candidate.turn());
        if model_calls.insert(call, reconstituted).is_some() {
            return Err(AcceptedInputSchedulingReconstitutionFailure::DuplicateModelCall { call });
        }
    }
    let model_call_inputs = input
        .model_calls
        .iter()
        .map(|call| (call.id(), call))
        .collect::<BTreeMap<_, _>>();
    let mut steering_round_evidence = BTreeMap::new();
    for round in &input.steering_continuation_rounds {
        if steering_round_evidence
            .insert(round.call(), round)
            .is_some()
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::SteeringContinuationRoundMismatch {
                    call: round.call(),
                },
            );
        }
    }
    let mut continuation_round_evidence = BTreeMap::new();
    for round in &input.continuation_rounds {
        if continuation_round_evidence
            .insert(round.call(), round)
            .is_some()
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ContinuationRoundMismatch {
                    call: round.call(),
                },
            );
        }
    }
    let mut consumed_inputs = BTreeMap::new();
    let mut consumed_by_call = BTreeMap::<
        crate::ModelCallId,
        Vec<(
            SessionInputPosition,
            SemanticTranscriptEntryRef,
            AcceptedInputId,
        )>,
    >::new();
    for consumed in &input.consumed_steering {
        let accepted_input = consumed.accepted_input.id();
        if consumed.session != session {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringSessionMismatch {
                    accepted_input,
                },
            );
        }
        if consumed_inputs.contains_key(&accepted_input) {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateConsumedSteering {
                    accepted_input,
                },
            );
        }
        let Some((entry, semantic_source_turn)) = steering_by_input.remove(&accepted_input) else {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                    accepted_input,
                },
            );
        };
        let AcceptedInputDisposition::ConsumedAsSteering { call } =
            consumed.accepted_input.disposition()
        else {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                    accepted_input,
                },
            );
        };
        consumed_inputs.insert(accepted_input, *call);
        let source_record = records_by_turn.get(&consumed.source_turn).copied();
        let source_record_matches = source_record.is_some_and(|record| {
            !matches!(record.state, AcceptedInputTurnSchedulingRecordState::Queued)
                && record.order.acceptance_position() < consumed.acceptance_position
        });
        let active_tail_matches = input.active_acceptance_tail.as_ref().is_some_and(|tail| {
            tail.entries.iter().any(|candidate| {
                candidate.session == session
                    && candidate.accepted_input == consumed.accepted_input
                    && candidate.position == consumed.acceptance_position
                    && matches!(
                        candidate.delivery,
                        DeliveryRequest::NextSafePoint {
                            expected_active_turn,
                        } if expected_active_turn == consumed.source_turn
                            && expected_active_turn == semantic_source_turn
                    )
            })
        });
        let source_is_active = source_record.is_some_and(|record| {
            matches!(
                record.state,
                AcceptedInputTurnSchedulingRecordState::Active { .. }
            )
        });
        let earlier_reclassified = records_by_turn.values().any(|record| {
            record.order.acceptance_position() < consumed.acceptance_position
                && matches!(
                    record.accepted_input.disposition(),
                    AcceptedInputDisposition::ReclassifiedAsTurnOrigin { .. }
                )
                && matches!(
                    record.origin_delivery,
                    DeliveryRequest::NextSafePoint {
                        expected_active_turn,
                    } if expected_active_turn == consumed.source_turn
                )
        });
        let model_call_matches =
            model_call_inputs
                .get(call)
                .zip(source_record)
                .is_some_and(|(model_call, record)| {
                    // A steering-consuming call that completed by proposing a
                    // tool round becomes model-visible history while its turn
                    // continues: later safe points, waits, and every terminal
                    // shape keep the consumed rows of earlier rounds. Such a
                    // consumer is correlated through its assistant entries
                    // (validated by the assistant-content law) and its exact
                    // frontier window (validated below), not through the
                    // current phase or the turn's terminal call. Only a tool
                    // proposal keeps a completed call's turn going, so a
                    // text-only completed consumer stays bound to the
                    // terminal correlation below.
                    let completed_history_consumer = !matches!(
                        &record.state,
                        AcceptedInputTurnSchedulingRecordState::Queued
                    ) && model_call.state()
                        == crate::ModelCallReconstitutionState::Terminal(
                            ModelCallDisposition::Completed,
                        )
                        && assistant_by_call
                            .get(&model_call.id())
                            .is_some_and(|entries| {
                                entries.iter().any(|entry| {
                                    matches!(
                                        semantic_entries
                                            .get(entry)
                                            .map(SemanticTranscriptEntry::payload),
                                        Some(
                                            SemanticTranscriptEntryPayload::AssistantToolUse { .. }
                                        )
                                    )
                                })
                            });
                    let lifecycle_matches = completed_history_consumer
                        || match &record.state {
                    AcceptedInputTurnSchedulingRecordState::Queued => false,
                    AcceptedInputTurnSchedulingRecordState::Active { phase, .. } => {
                        Some(model_call.attempt()) == phase.current_attempt
                            && match (&phase.state, model_call.state()) {
                                (
                                    StoredActiveTurnPhase::Prepared,
                                    crate::ModelCallReconstitutionState::Prepared,
                                )
                                | (
                                    StoredActiveTurnPhase::Running,
                                    crate::ModelCallReconstitutionState::InFlight,
                                ) => true,
                                // A continuation attempt that authorized
                                // physical tool execution is already Running
                                // when it receives its Prepared call, so this
                                // pair is legal exactly at a tool-round
                                // continuation boundary, proven by the
                                // round's result evidence and the frontier
                                // law below.
                                (
                                    StoredActiveTurnPhase::Running,
                                    crate::ModelCallReconstitutionState::Prepared,
                                ) => steering_round_evidence.contains_key(&model_call.id()),
                                (
                                    StoredActiveTurnPhase::StopRequested { call, .. },
                                    crate::ModelCallReconstitutionState::CancellationRequested,
                                ) => *call == model_call.id(),
                                (
                                    StoredActiveTurnPhase::AwaitingApproval { .. },
                                    _,
                                ) => false,
                                (
                                    StoredActiveTurnPhase::AwaitingChild { .. },
                                    _,
                                ) => false,
                                (
                                    StoredActiveTurnPhase::AwaitingToolRecovery { .. },
                                    _,
                                ) => false,
                                (
                                    StoredActiveTurnPhase::AwaitingRunnerRecovery { .. },
                                    _,
                                ) => false,
                                (
                                    StoredActiveTurnPhase::AwaitingModelCallRecovery {
                                        call, ..
                                    },
                                    crate::ModelCallReconstitutionState::Terminal(
                                        ModelCallDisposition::Ambiguous,
                                    ),
                                ) => *call == model_call.id(),
                                _ => false,
                            }
                    }
                    AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                        terminal_execution: Some(execution),
                        ..
                    } => {
                        model_call.attempt() == execution.ended_attempt
                            && execution.ended_call == Some(model_call.id())
                            && matches!(
                                model_call.state(),
                                crate::ModelCallReconstitutionState::Terminal(
                                    ModelCallDisposition::KnownFailed
                                        | ModelCallDisposition::Cancelled
                                )
                            )
                    }
                    AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                        terminal_execution: None,
                        ..
                    } => false,
                    AcceptedInputTurnSchedulingRecordState::TerminalCompleted {
                        completing_attempt,
                        completing_call,
                        ..
                    } => {
                        model_call.attempt() == *completing_attempt
                            && model_call.id() == *completing_call
                            && model_call.state()
                                == crate::ModelCallReconstitutionState::Terminal(
                                    ModelCallDisposition::Completed,
                                )
                    }
                    AcceptedInputTurnSchedulingRecordState::TerminalRefused {
                        refusing_attempt,
                        refusing_call,
                        ..
                    } => {
                        model_call.attempt() == *refusing_attempt
                            && model_call.id() == *refusing_call
                            && model_call.state()
                                == crate::ModelCallReconstitutionState::Terminal(
                                    ModelCallDisposition::Refused,
                                )
                    }
                    AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
                        terminal_execution,
                        ..
                    } => {
                        model_call.attempt() == terminal_execution.ended_attempt
                            && terminal_execution.ended_call == Some(model_call.id())
                            && model_call.state()
                                == crate::ModelCallReconstitutionState::Terminal(
                                    ModelCallDisposition::Cancelled,
                                )
                    }
                    AcceptedInputTurnSchedulingRecordState::TerminalReconciliationRequired {
                        reconciling_attempt,
                        ambiguous_call,
                        ..
                    } => {
                        model_call.attempt() == *reconciling_attempt
                            && model_call.id() == *ambiguous_call
                            && model_call.state()
                                == crate::ModelCallReconstitutionState::Terminal(
                                    ModelCallDisposition::Ambiguous,
                                )
                    }
                    AcceptedInputTurnSchedulingRecordState::TerminalToolReconciliationRequired {
                        tool_batch,
                        ..
                    } => {
                        model_call.id() == tool_batch.producing_call()
                            && model_call.state()
                                == crate::ModelCallReconstitutionState::Terminal(
                                    ModelCallDisposition::Completed,
                                )
                    }
                };
                    model_call.turn() == consumed.source_turn
                        && semantic_source_turn == consumed.source_turn
                        && model_call.selection()
                            == *record.origin_configuration.effective().model()
                        && lifecycle_matches
                });
        if accepted_input_turns.contains_key(&accepted_input)
            || !source_record_matches
            || !model_call_matches
            || earlier_reclassified
            || (source_is_active && !active_tail_matches)
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                    accepted_input,
                },
            );
        }
        consumed_by_call.entry(*call).or_default().push((
            consumed.acceptance_position,
            entry,
            accepted_input,
        ));
    }
    for consumed in &input.delegated_consumed_steering {
        let accepted_input = consumed.accepted_input.id();
        if consumed.session != session {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringSessionMismatch {
                    accepted_input,
                },
            );
        }
        if consumed_inputs.contains_key(&accepted_input) {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateConsumedSteering {
                    accepted_input,
                },
            );
        }
        let Some((_, semantic_source_turn)) = steering_by_input.remove(&accepted_input) else {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                    accepted_input,
                },
            );
        };
        let AcceptedInputDisposition::ConsumedAsSteering { call } =
            consumed.accepted_input.disposition()
        else {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                    accepted_input,
                },
            );
        };
        consumed_inputs.insert(accepted_input, *call);
        if semantic_source_turn != consumed.source_turn
            || accepted_input_turns.contains_key(&accepted_input)
            || records_by_turn.contains_key(&consumed.source_turn)
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                    accepted_input,
                },
            );
        }
    }
    if let Some((_, (entry, _))) = steering_by_input.first_key_value() {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::SteeringSemanticEntryMismatch {
                entry: entry.entry(),
            },
        );
    }
    if let Some(call) = steering_round_evidence
        .keys()
        .find(|call| !consumed_by_call.contains_key(call))
        .copied()
    {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::SteeringContinuationRoundMismatch {
                call,
            },
        );
    }
    let mut consumed_model_calls = BTreeSet::new();
    let mut consumed_snapshots = BTreeSet::new();
    for (call, mut consumed_entries) in consumed_by_call {
        consumed_entries.sort_unstable_by_key(|(position, _, _)| *position);
        let Some((_, _, first_accepted_input)) = consumed_entries.first().copied() else {
            continue;
        };
        if consumed_entries
            .windows(2)
            .any(|entries| entries[0].0 == entries[1].0)
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                    accepted_input: first_accepted_input,
                },
            );
        }
        let model_call = model_call_inputs[&call];
        let record = records_by_turn[&model_call.turn()];
        let starting_frontier = match record.state {
            AcceptedInputTurnSchedulingRecordState::Queued => None,
            AcceptedInputTurnSchedulingRecordState::Active {
                starting_frontier, ..
            }
            | AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                starting_frontier, ..
            }
            | AcceptedInputTurnSchedulingRecordState::TerminalCompleted {
                starting_frontier, ..
            }
            | AcceptedInputTurnSchedulingRecordState::TerminalRefused {
                starting_frontier, ..
            }
            | AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
                starting_frontier, ..
            }
            | AcceptedInputTurnSchedulingRecordState::TerminalReconciliationRequired {
                starting_frontier,
                ..
            }
            | AcceptedInputTurnSchedulingRecordState::TerminalToolReconciliationRequired {
                starting_frontier,
                ..
            } => Some(starting_frontier),
        };
        // Without round evidence the call was prepared at turn start, so its
        // frontier is exactly the starting frontier plus the consumed suffix.
        // With round evidence the call was prepared at a tool-round
        // continuation boundary, so everything before the consumed suffix
        // must be exactly one completed round's result projection.
        let exact_frontier_matches = starting_frontier
            .and_then(|frontier| snapshots.get(&frontier))
            .zip(snapshots.get(&model_call.frontier()))
            .is_some_and(
                |(starting, call_snapshot)| match steering_round_evidence.get(&call) {
                    None => starting
                        .ordered_entries()
                        .chain(consumed_entries.iter().map(|(_, entry, _)| *entry))
                        .eq(call_snapshot.ordered_entries()),
                    Some(round) => {
                        let Some(base_entry_count) = call_snapshot
                            .entry_count()
                            .checked_sub(consumed_entries.len())
                        else {
                            return false;
                        };
                        // The round's tools were issued by the same
                        // continuation attempt that owns the consuming call.
                        round
                            .round_tool_attempts()
                            .iter()
                            .all(|attempt| attempt.issuing_attempt() == model_call.attempt())
                            && call_snapshot
                                .ordered_entries_range(
                                    base_entry_count,
                                    call_snapshot.entry_count(),
                                )
                                .eq(consumed_entries.iter().map(|(_, entry, _)| *entry))
                            && tool_round_continuation_producing_call(
                                model_call.turn(),
                                call_snapshot,
                                base_entry_count,
                                round.round_tool_attempts(),
                                round.round_tool_denials(),
                                &model_calls,
                                &assistant_by_call,
                                &snapshots,
                                &semantic_entries,
                            )
                            .is_some()
                    }
                },
            );
        if !exact_frontier_matches {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                    accepted_input: first_accepted_input,
                },
            );
        }
        consumed_model_calls.insert(call);
        consumed_snapshots.insert(model_call.frontier());
    }
    if let Some(turn) = pinned_targets
        .keys()
        .find(|turn| !referenced_pinned_targets.contains(turn))
        .copied()
    {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::UnreferencedPinnedTarget { turn },
        );
    }
    let mut referenced_model_calls = consumed_model_calls;
    let mut assistant_call_snapshots = BTreeSet::new();
    for (call, entries) in &assistant_by_call {
        let Some(first_entry) = entries.first().copied() else {
            continue;
        };
        let Some(reconstituted) = model_calls.get(call) else {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::SemanticEntryCallMissing {
                    entry: first_entry.entry(),
                    call: *call,
                },
            );
        };
        let ReconstitutedModelCall::Ended(ended) = reconstituted else {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::SemanticEntryCallMismatch {
                    entry: first_entry.entry(),
                    call: *call,
                },
            );
        };
        let call_snapshot = snapshots.get(&ended.frontier().snapshot());
        let accepted_turn_matches =
            records_by_turn
                .get(&ended.turn())
                .copied()
                .is_some_and(|record| {
                    let starting_frontier = match record.state {
                        AcceptedInputTurnSchedulingRecordState::Queued => None,
                        AcceptedInputTurnSchedulingRecordState::Active {
                            starting_frontier, ..
                        }
                        | AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                            starting_frontier, ..
                        }
                        | AcceptedInputTurnSchedulingRecordState::TerminalCompleted {
                            starting_frontier, ..
                        }
                        | AcceptedInputTurnSchedulingRecordState::TerminalRefused {
                            starting_frontier, ..
                        }
                        | AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
                            starting_frontier, ..
                        }
                        | AcceptedInputTurnSchedulingRecordState::TerminalReconciliationRequired {
                            starting_frontier,
                            ..
                        }
                        | AcceptedInputTurnSchedulingRecordState::TerminalToolReconciliationRequired {
                            starting_frontier,
                            ..
                        } => Some(starting_frontier),
                    };
                    ended.selection() == *record.origin_configuration.effective().model()
                        && starting_frontier
                            .and_then(|starting| snapshots.get(&starting))
                            .zip(call_snapshot)
                            .is_some_and(|(starting, call_snapshot)| {
                                starting.is_semantic_prefix_of(call_snapshot)
                            })
                });
        let delegated_turn_matches = delegated_turns
            .get(&ended.turn())
            .is_some_and(|fact| ended.selection().selected_direct() == fact.selected())
            && !records_by_turn.contains_key(&ended.turn())
            && call_snapshot.is_some();
        if ended.disposition() != ModelCallDisposition::Completed
            || (!accepted_turn_matches && !delegated_turn_matches)
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::SemanticEntryCallMismatch {
                    entry: first_entry.entry(),
                    call: *call,
                },
            );
        }
        referenced_model_calls.insert(*call);
        assistant_call_snapshots.insert(ended.frontier().snapshot());
    }

    let mut turns = Vec::with_capacity(total_order.len());
    let mut previous_terminal = None;
    let mut previous_selected = None;
    let mut active = None;
    let mut active_model_call_recovery = None;
    let mut active_stop_requested_frontier = None;
    let mut active_tool_recovery_attempt = None;
    let mut active_tool_recovery_frontier = None;
    let mut active_executing_tool_batch = None;
    let mut queued_seen = false;
    let mut referenced_snapshots = consumed_snapshots;
    referenced_snapshots.extend(initial_seed_frontier);
    referenced_snapshots.extend(
        preceding_non_accepted_terminals
            .values()
            .map(|(snapshot, _)| snapshot.frontier().snapshot()),
    );
    let mut attempt_owners = BTreeMap::new();
    let mut claimed_continuation_rounds = BTreeSet::new();

    for (index, turn) in total_order.into_iter().enumerate() {
        let record = records_by_turn[&turn];
        let external_predecessor = preceding_non_accepted_successors.get(&turn).copied();
        if let Some(predecessor) = external_predecessor {
            let (snapshot, selected) = &preceding_non_accepted_terminals[&predecessor];
            previous_terminal = Some((predecessor, snapshot.clone()));
            previous_selected = Some(*selected);
        }
        let selected = record
            .origin_configuration
            .effective()
            .model()
            .selected_direct();
        let is_queued = matches!(
            &record.state,
            AcceptedInputTurnSchedulingRecordState::Queued
        );
        let model_identity_entry = if !record.model_identity_boundary_required {
            if model_identity_by_turn.contains_key(&turn) {
                return Err(
                    AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch { turn },
                );
            }
            None
        } else if is_queued {
            None
        } else {
            match previous_selected {
                Some(previous) if previous != selected => {
                    Some(model_identity_by_turn.get(&turn).copied().ok_or(
                        AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch {
                            turn,
                        },
                    )?)
                }
                _ => {
                    if model_identity_by_turn.contains_key(&turn) {
                        return Err(
                            AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch {
                                turn,
                            },
                        );
                    }
                    None
                }
            }
        };
        let state = match &record.state {
            AcceptedInputTurnSchedulingRecordState::Queued => {
                queued_seen = true;
                ReconstitutedSchedulingState::Queued
            }
            AcceptedInputTurnSchedulingRecordState::Active {
                starting_lineage,
                starting_frontier,
                phase,
            } => {
                if active.is_some() || queued_seen {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder {
                            turn,
                        },
                    );
                }
                active = Some(turn);
                if phase.owning_turn != turn {
                    return match phase.current_attempt {
                        Some(attempt) => Err(
                            AcceptedInputSchedulingReconstitutionFailure::CurrentAttemptOwnershipMismatch {
                                turn,
                                attempt,
                            },
                        ),
                        None => Err(
                            AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                turn,
                                accepted_input: record.accepted_input.id(),
                            },
                        ),
                    };
                }
                if let Some(current_attempt) = phase.current_attempt
                    && attempt_owners.insert(current_attempt, turn).is_some()
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateCurrentAttempt {
                            attempt: current_attempt,
                        },
                    );
                }
                let start = validate_start(
                    index,
                    turn,
                    *starting_lineage,
                    *starting_frontier,
                    initial_seed_frontier.and_then(|frontier| snapshots.get(&frontier)),
                    previous_terminal.as_ref(),
                    &origin_by_turn,
                    model_identity_entry,
                    &compaction_chain,
                    &snapshots,
                    &mut referenced_snapshots,
                )?;
                let canonical_phase = match &phase.state {
                    StoredActiveTurnPhase::Prepared | StoredActiveTurnPhase::Running => {
                        phase.canonical_evidence_free_phase().ok_or(
                            AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                turn,
                                accepted_input: record.accepted_input.id(),
                            },
                        )?
                    }
                    StoredActiveTurnPhase::AwaitingApproval { wait } => {
                        if wait.session() != session || wait.turn() != turn {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                    turn,
                                    accepted_input: record.accepted_input.id(),
                                },
                            );
                        }
                        phase.canonical_evidence_free_phase().ok_or(
                            AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                turn,
                                accepted_input: record.accepted_input.id(),
                            },
                        )?
                    }
                    StoredActiveTurnPhase::AwaitingChild { wait } => {
                        if phase.current_attempt.is_some() {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                    turn,
                                    accepted_input: record.accepted_input.id(),
                                },
                            );
                        }
                        ActiveTurnPhase::AwaitingChild { wait: *wait }
                    }
                    StoredActiveTurnPhase::AwaitingRunnerRecovery {
                        source_frontier,
                        ..
                    } => {
                        if let Some(source_frontier) = source_frontier {
                            let source_snapshot = snapshots.get(source_frontier).ok_or(
                                AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                    turn,
                                    accepted_input: record.accepted_input.id(),
                                },
                            )?;
                            if !snapshots[starting_frontier]
                                .is_semantic_prefix_of(source_snapshot)
                            {
                                return Err(
                                    AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                        turn,
                                        accepted_input: record.accepted_input.id(),
                                    },
                                );
                            }
                            referenced_snapshots.insert(*source_frontier);
                        }
                        phase.canonical_evidence_free_phase().ok_or(
                            AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                turn,
                                accepted_input: record.accepted_input.id(),
                            },
                        )?
                    }
                    StoredActiveTurnPhase::AwaitingToolRecovery { wait, attempt_end } => {
                        let Some(current_attempt) = phase.current_attempt else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                    turn,
                                    accepted_input: record.accepted_input.id(),
                                },
                            );
                        };
                        if wait.session() != session
                            || wait.turn() != turn
                            || wait.issuing_attempt() != current_attempt
                            || !terminal_attempt_end_matches(
                                attempt_end,
                                session,
                                turn,
                                &records_by_turn,
                                &[
                                    UnstoppedAttemptDisposition::Ambiguous,
                                    UnstoppedAttemptDisposition::Lost,
                                ],
                                &[
                                    CancellationStopDisposition::Ambiguous,
                                    CancellationStopDisposition::Lost,
                                ],
                            )
                        {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                    turn,
                                    accepted_input: record.accepted_input.id(),
                                },
                            );
                        }
                        let Ok(running_attempt) =
                            CurrentTurnAttempt::prepared(current_attempt).begin_running()
                        else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                    turn,
                                    accepted_input: record.accepted_input.id(),
                                },
                            );
                        };
                        let canonical_end = match attempt_end.end() {
                            AttemptEnd::WithoutStop { disposition } => {
                                running_attempt.end_without_stop(*disposition)
                            }
                            AttemptEnd::AfterCancellation { cause, disposition } => running_attempt
                                .request_cancellation(*cause)
                                .and_then(|attempt| {
                                    attempt.end_after_cancellation(*cause, *disposition)
                                }),
                            AttemptEnd::AfterFatalMismatch { .. } => {
                                return Err(
                                    AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                        turn,
                                        accepted_input: record.accepted_input.id(),
                                    },
                                );
                            }
                        };
                        let Ok(canonical_end) = canonical_end else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                    turn,
                                    accepted_input: record.accepted_input.id(),
                                },
                            );
                        };
                        active_tool_recovery_attempt = Some(canonical_end);
                        active_tool_recovery_frontier = Some(wait.yielded_frontier());
                        referenced_snapshots.insert(wait.yielded_frontier());
                        ActiveTurnPhase::AwaitingRecoveryDecision {
                            ambiguous_operations: NonEmptyIssuedOperationRefs::singleton(
                                crate::IssuedOperationRef::ToolAttempt(wait.attempt()),
                            ),
                            applied_interrupt: attempt_end
                                .interrupt()
                                .map(|interrupt| interrupt.proof()),
                        }
                    }
                    StoredActiveTurnPhase::StopRequested { call, interrupt } => {
                        let Some(current_attempt) = phase.current_attempt else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                    turn,
                                    accepted_input: record.accepted_input.id(),
                                },
                            );
                        };
                        let successor = records_by_turn.get(&interrupt.successor());
                        let Some(ReconstitutedModelCall::Current(current_call)) =
                            model_calls.get(call)
                        else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMissing {
                                    turn,
                                    call: *call,
                                },
                            );
                        };
                        let Some(pinned) = pinned_targets.get(&turn) else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::PinnedTargetMissing {
                                    call: *call,
                                },
                            );
                        };
                        let call_snapshot = snapshots.get(&current_call.frontier().snapshot());
                        if interrupt.session() != session
                            || interrupt.proof().predecessor() != turn
                            || successor.is_none_or(|successor| {
                                successor.stored_session != session
                                    || successor.turn != interrupt.successor()
                                    || successor.accepted_input.id()
                                        != interrupt.accepted_input()
                                    || successor.order != interrupt.successor_order()
                            })
                            || current_call.turn() != turn
                            || current_call.attempt() != current_attempt
                            || current_call.state()
                                != crate::CurrentModelCallState::CancellationRequested
                            || current_call.selection()
                                != *record.origin_configuration.effective().model()
                            || current_call.target() != pinned.target()
                            || call_snapshot.is_none_or(|snapshot| {
                                !snapshots[starting_frontier].is_semantic_prefix_of(snapshot)
                            })
                        {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                    turn,
                                    accepted_input: record.accepted_input.id(),
                                },
                            );
                        }
                        if current_call.frontier().snapshot() != *starting_frontier {
                            referenced_snapshots.insert(current_call.frontier().snapshot());
                        }
                        active_stop_requested_frontier =
                            Some(current_call.frontier().snapshot());
                        referenced_model_calls.insert(*call);
                        phase.canonical_evidence_free_phase().ok_or(
                            AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                turn,
                                accepted_input: record.accepted_input.id(),
                            },
                        )?
                    }
                    StoredActiveTurnPhase::AwaitingModelCallRecovery {
                        call,
                        attempt_end,
                    } => {
                        let Some(current_attempt) = phase.current_attempt else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMismatch {
                                    turn,
                                },
                            );
                        };
                        let Some(ReconstitutedModelCall::Ended(ended_call)) = model_calls.get(call)
                        else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMissing {
                                    turn,
                                    call: *call,
                                },
                            );
                        };
                        let source_snapshot = snapshots
                            .get(&ended_call.frontier().snapshot())
                            .cloned()
                            .ok_or(
                                AcceptedInputSchedulingReconstitutionFailure::ModelCallSnapshotMissing {
                                    call: *call,
                                },
                            )?;
                        if ended_call.turn() != turn
                            || ended_call.attempt() != current_attempt
                            || ended_call.selection()
                                != *record.origin_configuration.effective().model()
                            || !snapshots[starting_frontier]
                                .is_semantic_prefix_of(&source_snapshot)
                            || ended_call.disposition() != ModelCallDisposition::Ambiguous
                        {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMismatch {
                                    turn,
                                },
                            );
                        }
                        // An ambiguous continuation call names no
                        // starting-frontier or otherwise-referenced snapshot:
                        // its whole frontier must be the completed round's
                        // result projection, which the recovery wait extends
                        // by no entry.
                        let named_call_frontier_accounted = ended_call.frontier().snapshot()
                            == *starting_frontier
                            || referenced_model_calls.contains(call);
                        if !named_call_frontier_accounted {
                            let continuation_call_matches = continuation_round_evidence
                                .get(call)
                                .is_some_and(|round| {
                                    round.round_tool_attempts().iter().all(|tool_attempt| {
                                        tool_attempt.issuing_attempt() == current_attempt
                                    }) && tool_round_continuation_producing_call(
                                        turn,
                                        &source_snapshot,
                                        source_snapshot.entry_count(),
                                        round.round_tool_attempts(),
                                        round.round_tool_denials(),
                                        &model_calls,
                                        &assistant_by_call,
                                        &snapshots,
                                        &semantic_entries,
                                    )
                                    .is_some()
                                });
                            if !continuation_call_matches {
                                return Err(
                                    AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMismatch {
                                        turn,
                                    },
                                );
                            }
                            claimed_continuation_rounds.insert(*call);
                        }
                        let Ok(running_attempt) =
                            CurrentTurnAttempt::prepared(current_attempt).begin_running()
                        else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMismatch {
                                    turn,
                                },
                            );
                        };
                        let end_matches = terminal_attempt_end_matches(
                            attempt_end,
                            session,
                            turn,
                            &records_by_turn,
                            &[
                                UnstoppedAttemptDisposition::Ambiguous,
                                UnstoppedAttemptDisposition::Lost,
                            ],
                            &[
                                CancellationStopDisposition::Ambiguous,
                                CancellationStopDisposition::Lost,
                            ],
                        );
                        let canonical_end = match attempt_end.end() {
                            AttemptEnd::WithoutStop { disposition } => {
                                running_attempt.end_without_stop(*disposition)
                            }
                            AttemptEnd::AfterCancellation { cause, disposition } => {
                                running_attempt
                                    .request_cancellation(*cause)
                                    .and_then(|attempt| {
                                        attempt.end_after_cancellation(
                                            *cause,
                                            *disposition,
                                        )
                                    })
                            }
                            AttemptEnd::AfterFatalMismatch { .. } => {
                                return Err(
                                    AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMismatch {
                                        turn,
                                    },
                                );
                            }
                        };
                        let Ok(canonical_end) = canonical_end else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMismatch {
                                    turn,
                                },
                            );
                        };
                        if !end_matches {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMismatch {
                                    turn,
                                },
                            );
                        }
                        let ambiguous_operations =
                            NonEmptyIssuedOperationRefs::try_from_operations([
                                crate::IssuedOperationRef::ModelCall(*call),
                            ])
                            .map_err(|_| {
                                AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMismatch {
                                    turn,
                                }
                            })?;
                        referenced_model_calls.insert(*call);
                        if ended_call.frontier().snapshot() != *starting_frontier {
                            referenced_snapshots.insert(ended_call.frontier().snapshot());
                        }
                        active_model_call_recovery = Some(ActiveModelCallRecoveryWait {
                            call: ended_call.clone(),
                            attempt: canonical_end,
                            source_snapshot,
                        });
                        ActiveTurnPhase::AwaitingRecoveryDecision {
                            ambiguous_operations,
                            applied_interrupt: attempt_end
                                .interrupt()
                                .map(|interrupt| interrupt.proof()),
                        }
                    }
                };
                if let Some(tool_batch) = &phase.executing_tool_batch {
                    let yielded_frontier = tool_batch.yielded_snapshot.frontier().snapshot();
                    let stored_yielded = snapshots.get(&yielded_frontier);
                    let producing = model_calls.get(&tool_batch.producing_call);
                    let source = producing.and_then(|call| match call {
                        crate::ReconstitutedModelCall::Ended(call) => {
                            snapshots.get(&call.frontier().snapshot())
                        }
                        crate::ReconstitutedModelCall::Current(_) => None,
                    });
                    let mut observed_requests = Vec::new();
                    let suffix_valid =
                        source.is_some_and(|source| {
                            source.is_semantic_prefix_of(&tool_batch.yielded_snapshot)
                                && source.entry_count() < tool_batch.yielded_snapshot.entry_count()
                                && tool_batch
                                    .yielded_snapshot
                                    .ordered_entries()
                                    .skip(source.entry_count())
                                    .all(|reference| {
                                        let Some(entry) = semantic_entries.get(&reference) else {
                                            return false;
                                        };
                                        match entry.payload() {
                                        SemanticTranscriptEntryPayload::AssistantText {
                                            producing_call,
                                            ..
                                        } => *producing_call == tool_batch.producing_call,
                                        SemanticTranscriptEntryPayload::ProviderCompaction {
                                            producing_call,
                                            ..
                                        } => *producing_call == tool_batch.producing_call,
                                        SemanticTranscriptEntryPayload::AssistantToolUse {
                                            producing_call,
                                            request,
                                        } if *producing_call == tool_batch.producing_call => {
                                            observed_requests.push(*request);
                                            true
                                        }
                                        SemanticTranscriptEntryPayload::AssistantToolUse { .. }
                                        | SemanticTranscriptEntryPayload::DelegatedTask { .. }
                                        | SemanticTranscriptEntryPayload::DelegationMessage { .. }
                                        | SemanticTranscriptEntryPayload::DelegationResult { .. }
                                        | SemanticTranscriptEntryPayload::Imported { .. }
                                        | SemanticTranscriptEntryPayload::ModelIdentityChanged {
                                            ..
                                        }
                                        | SemanticTranscriptEntryPayload::ContextSummary { .. }
                                        | SemanticTranscriptEntryPayload::OriginAcceptedInput {
                                            ..
                                        }
                                        | SemanticTranscriptEntryPayload::SteeringAcceptedInput {
                                            ..
                                        }
                                        | SemanticTranscriptEntryPayload::ToolExecutionResult {
                                            ..
                                        }
                                        | SemanticTranscriptEntryPayload::ToolDenied { .. }
                                        | SemanticTranscriptEntryPayload::ToolClosed { .. }
                                        | SemanticTranscriptEntryPayload::TurnCompleted { .. }
                                        | SemanticTranscriptEntryPayload::TurnFailed { .. }
                                        | SemanticTranscriptEntryPayload::TurnCancelled { .. } => {
                                            false
                                        }
                                    }
                                    })
                        });
                    let producing_matches = matches!(
                        producing,
                        Some(crate::ReconstitutedModelCall::Ended(call))
                            if call.turn() == turn
                                && call.disposition() == ModelCallDisposition::Completed
                    );
                    let phase_matches = match (
                        &phase.state,
                        tool_batch.batch_attempt,
                        tool_batch.awaiting_request,
                    ) {
                        (
                            StoredActiveTurnPhase::Prepared | StoredActiveTurnPhase::Running,
                            Some(turn_attempt),
                            None,
                        ) => phase.current_attempt == Some(turn_attempt),
                        (StoredActiveTurnPhase::AwaitingApproval { wait }, None, Some(request)) => {
                            phase.current_attempt.is_none() && wait.request() == request
                        }
                        (StoredActiveTurnPhase::AwaitingChild { wait }, None, Some(request)) => {
                            phase.current_attempt.is_none() && wait.awaiting_request() == request
                        }
                        _ => false,
                    };
                    if !phase_matches
                        || tool_batch.session != session
                        || stored_yielded != Some(&tool_batch.yielded_snapshot)
                        || !producing_matches
                        || !suffix_valid
                        || observed_requests.as_slice() != tool_batch.requests.as_ref()
                    {
                        return Err(
                            AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                                turn,
                                accepted_input: record.accepted_input.id(),
                            },
                        );
                    }
                    referenced_model_calls.insert(tool_batch.producing_call);
                    referenced_snapshots.insert(yielded_frontier);
                    active_executing_tool_batch = Some(ActiveExecutingToolBatchCorrelation {
                        session,
                        turn,
                        producing_call: tool_batch.producing_call,
                        yielded_frontier,
                        turn_attempt: tool_batch.batch_attempt,
                    });
                }
                ReconstitutedSchedulingState::Active {
                    start,
                    phase: canonical_phase,
                }
            }
            AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                starting_lineage,
                starting_frontier,
                terminal_execution,
                terminal_frontier,
            } => {
                if active.is_some() || queued_seen {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder {
                            turn,
                        },
                    );
                }
                let start = validate_start(
                    index,
                    turn,
                    *starting_lineage,
                    *starting_frontier,
                    initial_seed_frontier.and_then(|frontier| snapshots.get(&frontier)),
                    previous_terminal.as_ref(),
                    &origin_by_turn,
                    model_identity_entry,
                    &compaction_chain,
                    &snapshots,
                    &mut referenced_snapshots,
                )?;
                let mut source_frontier = *starting_frontier;
                let mut named_call_frontier_accounted = true;
                if let Some(execution) = terminal_execution {
                    let attempt = execution.ended_attempt;
                    if execution.owning_turn != turn {
                        return Err(
                            AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptOwnershipMismatch {
                                turn,
                                attempt,
                            },
                        );
                    }
                    if !terminal_attempt_end_matches(
                        &execution.attempt_end,
                        session,
                        turn,
                        &records_by_turn,
                        &[
                            UnstoppedAttemptDisposition::KnownFailure,
                            UnstoppedAttemptDisposition::Lost,
                        ],
                        &[CancellationStopDisposition::KnownFailure],
                    ) {
                        return Err(
                            AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptEndMismatch {
                                turn,
                                attempt,
                            },
                        );
                    }
                    if attempt_owners.insert(attempt, turn).is_some() {
                        return Err(
                            AcceptedInputSchedulingReconstitutionFailure::DuplicateCurrentAttempt {
                                attempt,
                            },
                        );
                    }
                    if let Some(call_id) = execution.ended_call {
                        let Some(ReconstitutedModelCall::Ended(call)) = model_calls.get(&call_id)
                        else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMissing {
                                    turn,
                                    call: call_id,
                                },
                            );
                        };
                        let Some(pinned) = pinned_targets.get(&turn) else {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::PinnedTargetMissing {
                                    call: call_id,
                                },
                            );
                        };
                        let call_disposition_matches = match execution.attempt_end.end() {
                            AttemptEnd::WithoutStop {
                                disposition: UnstoppedAttemptDisposition::KnownFailure,
                            } => matches!(
                                call.disposition(),
                                ModelCallDisposition::KnownFailed | ModelCallDisposition::Cancelled
                            ),
                            AttemptEnd::WithoutStop {
                                disposition: UnstoppedAttemptDisposition::Lost,
                            }
                            | AttemptEnd::AfterCancellation {
                                disposition: CancellationStopDisposition::KnownFailure,
                                ..
                            } => call.disposition() == ModelCallDisposition::KnownFailed,
                            AttemptEnd::WithoutStop { .. }
                            | AttemptEnd::AfterCancellation { .. }
                            | AttemptEnd::AfterFatalMismatch { .. } => false,
                        };
                        if call.turn() != turn
                            || call.attempt() != attempt
                            || call.selection() != *record.origin_configuration.effective().model()
                            || call.target() != pinned.target()
                            || !call_disposition_matches
                        {
                            return Err(
                                AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                                    turn,
                                },
                            );
                        }
                        // A frontier accounted for by no other law must prove
                        // the named call stood at a tool-round continuation
                        // boundary, checked against the terminal frontier
                        // below once it is loaded.
                        named_call_frontier_accounted = call.frontier().snapshot()
                            == *starting_frontier
                            || referenced_model_calls.contains(&call_id);
                        source_frontier = call.frontier().snapshot();
                        if source_frontier != *starting_frontier {
                            referenced_snapshots.insert(source_frontier);
                        }
                        referenced_model_calls.insert(call_id);
                    }
                }
                let source = snapshots.get(&source_frontier).ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::StartingSnapshotMissing { turn },
                )?;
                if !snapshots[starting_frontier].is_semantic_prefix_of(source) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                            turn,
                        },
                    );
                }
                let terminal = snapshots.get(terminal_frontier).cloned().ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::TerminalSnapshotMissing { turn },
                )?;
                if !referenced_snapshots.insert(*terminal_frontier) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                let failed_entry = failure_by_turn.get(&turn).copied().ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::MissingFailureEntry { turn },
                )?;
                let ordinary_terminal_matches =
                    terminal.has_semantic_prefix_and_suffix(source, std::iter::once(failed_entry));
                // A failed continuation call names no starting-frontier or
                // otherwise-referenced snapshot: its whole frontier must be
                // the completed round's result projection that the terminal
                // marker extends by exactly one entry.
                if !named_call_frontier_accounted {
                    let continuation_call_matches = ordinary_terminal_matches
                        && terminal_execution.as_ref().is_some_and(|execution| {
                            execution
                                .terminal_tool_attempts()
                                .iter()
                                .all(|tool_attempt| {
                                    tool_attempt.issuing_attempt() == execution.ended_attempt
                                })
                                && tool_round_continuation_producing_call(
                                    turn,
                                    &terminal,
                                    terminal.entry_count().saturating_sub(1),
                                    execution.terminal_tool_attempts(),
                                    execution.terminal_tool_denials(),
                                    &model_calls,
                                    &assistant_by_call,
                                    &snapshots,
                                    &semantic_entries,
                                )
                                .is_some()
                        });
                    if !continuation_call_matches {
                        return Err(
                            AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                                turn,
                            },
                        );
                    }
                }
                let tool_round_terminal_matches =
                    terminal_execution.as_ref().is_some_and(|execution| {
                        execution.ended_call.is_none()
                            && tool_round_terminal_producing_call(
                                turn,
                                &terminal,
                                failed_entry,
                                execution.terminal_tool_attempts(),
                                execution.terminal_tool_denials(),
                                &model_calls,
                                &assistant_by_call,
                                &snapshots,
                                &semantic_entries,
                            )
                            .is_some()
                    });
                if !ordinary_terminal_matches && !tool_round_terminal_matches {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                previous_terminal = Some((turn, terminal.clone()));
                ReconstitutedSchedulingState::TerminalFailed {
                    start,
                    terminal_frontier: terminal,
                }
            }
            AcceptedInputTurnSchedulingRecordState::TerminalCompleted {
                starting_lineage,
                starting_frontier,
                completing_attempt,
                completing_attempt_end,
                completing_call,
                terminal_frontier,
            } => {
                if active.is_some() || queued_seen {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder {
                            turn,
                        },
                    );
                }
                if attempt_owners.insert(*completing_attempt, turn).is_some() {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateCurrentAttempt {
                            attempt: *completing_attempt,
                        },
                    );
                }
                let start = validate_start(
                    index,
                    turn,
                    *starting_lineage,
                    *starting_frontier,
                    initial_seed_frontier.and_then(|frontier| snapshots.get(&frontier)),
                    previous_terminal.as_ref(),
                    &origin_by_turn,
                    model_identity_entry,
                    &compaction_chain,
                    &snapshots,
                    &mut referenced_snapshots,
                )?;
                let Some(ReconstitutedModelCall::Ended(call)) = model_calls.get(completing_call)
                else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMissing {
                            turn,
                            call: *completing_call,
                        },
                    );
                };
                if call.turn() != turn
                    || call.attempt() != *completing_attempt
                    || !terminal_attempt_end_matches(
                        completing_attempt_end,
                        session,
                        turn,
                        &records_by_turn,
                        &[
                            UnstoppedAttemptDisposition::TurnCompleted,
                            UnstoppedAttemptDisposition::Lost,
                        ],
                        &[
                            CancellationStopDisposition::TurnCompleted,
                            CancellationStopDisposition::Lost,
                        ],
                    )
                    || call.selection() != *record.origin_configuration.effective().model()
                    || call.disposition() != ModelCallDisposition::Completed
                    || (call.frontier().snapshot() != *starting_frontier
                        && !referenced_model_calls.contains(completing_call))
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                            turn,
                        },
                    );
                }
                let source_frontier = call.frontier().snapshot();
                if source_frontier != *starting_frontier {
                    referenced_snapshots.insert(source_frontier);
                }
                let source = snapshots.get(&source_frontier).ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::StartingSnapshotMissing { turn },
                )?;
                if !snapshots[starting_frontier].is_semantic_prefix_of(source) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                            turn,
                        },
                    );
                }
                referenced_model_calls.insert(*completing_call);
                let terminal = snapshots.get(terminal_frontier).cloned().ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::TerminalSnapshotMissing { turn },
                )?;
                if !referenced_snapshots.insert(*terminal_frontier) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                let completion_entry = completion_by_turn.get(&turn).copied().ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::MissingCompletionEntry { turn },
                )?;
                let assistant_entries = assistant_by_call
                    .get(completing_call)
                    .cloned()
                    .unwrap_or_default();
                if !completed_terminal_matches(
                    source,
                    &terminal,
                    *completing_call,
                    &assistant_entries,
                    completion_entry,
                    &semantic_entries,
                ) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                previous_terminal = Some((turn, terminal.clone()));
                ReconstitutedSchedulingState::TerminalCompleted {
                    start,
                    terminal_frontier: terminal,
                }
            }
            AcceptedInputTurnSchedulingRecordState::TerminalRefused {
                starting_lineage,
                starting_frontier,
                refusing_attempt,
                refusing_attempt_end,
                refusing_call,
                terminal_frontier,
            } => {
                if active.is_some() || queued_seen {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder {
                            turn,
                        },
                    );
                }
                if attempt_owners.insert(*refusing_attempt, turn).is_some() {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::DuplicateCurrentAttempt {
                            attempt: *refusing_attempt,
                        },
                    );
                }
                let start = validate_start(
                    index,
                    turn,
                    *starting_lineage,
                    *starting_frontier,
                    initial_seed_frontier.and_then(|frontier| snapshots.get(&frontier)),
                    previous_terminal.as_ref(),
                    &origin_by_turn,
                    model_identity_entry,
                    &compaction_chain,
                    &snapshots,
                    &mut referenced_snapshots,
                )?;
                let Some(ReconstitutedModelCall::Ended(call)) = model_calls.get(refusing_call)
                else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMissing {
                            turn,
                            call: *refusing_call,
                        },
                    );
                };
                if call.turn() != turn
                    || call.attempt() != *refusing_attempt
                    || !terminal_attempt_end_matches(
                        refusing_attempt_end,
                        session,
                        turn,
                        &records_by_turn,
                        &[
                            UnstoppedAttemptDisposition::TurnRefused,
                            UnstoppedAttemptDisposition::Lost,
                        ],
                        &[
                            CancellationStopDisposition::TurnRefused,
                            CancellationStopDisposition::Lost,
                        ],
                    )
                    || call.selection() != *record.origin_configuration.effective().model()
                    || call.disposition() != ModelCallDisposition::Refused
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                            turn,
                        },
                    );
                }
                let named_call_frontier_accounted = call.frontier().snapshot()
                    == *starting_frontier
                    || referenced_model_calls.contains(refusing_call);
                let source_frontier = call.frontier().snapshot();
                if source_frontier != *starting_frontier {
                    referenced_snapshots.insert(source_frontier);
                }
                let source = snapshots.get(&source_frontier).ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::StartingSnapshotMissing { turn },
                )?;
                if !snapshots[starting_frontier].is_semantic_prefix_of(source) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                            turn,
                        },
                    );
                }
                // A refused continuation call names no starting-frontier or
                // otherwise-referenced snapshot: its whole frontier must be
                // the completed round's result projection, which the refusal
                // extends by no entry.
                if !named_call_frontier_accounted {
                    let continuation_call_matches = continuation_round_evidence
                        .get(refusing_call)
                        .is_some_and(|round| {
                            round.round_tool_attempts().iter().all(|tool_attempt| {
                                tool_attempt.issuing_attempt() == *refusing_attempt
                            }) && tool_round_continuation_producing_call(
                                turn,
                                source,
                                source.entry_count(),
                                round.round_tool_attempts(),
                                round.round_tool_denials(),
                                &model_calls,
                                &assistant_by_call,
                                &snapshots,
                                &semantic_entries,
                            )
                            .is_some()
                        });
                    if !continuation_call_matches {
                        return Err(
                            AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                                turn,
                            },
                        );
                    }
                    claimed_continuation_rounds.insert(*refusing_call);
                }
                referenced_model_calls.insert(*refusing_call);
                let terminal = snapshots.get(terminal_frontier).cloned().ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::TerminalSnapshotMissing { turn },
                )?;
                if !referenced_snapshots.insert(*terminal_frontier)
                    || !terminal.same_semantic_content(source)
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                previous_terminal = Some((turn, terminal.clone()));
                ReconstitutedSchedulingState::TerminalRefused {
                    start,
                    terminal_frontier: terminal,
                }
            }
            AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
                starting_lineage,
                starting_frontier,
                terminal_execution,
                terminal_frontier,
            } => {
                if active.is_some() || queued_seen {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder {
                            turn,
                        },
                    );
                }
                let attempt = terminal_execution.ended_attempt;
                let interrupt = terminal_execution.interrupt;
                let attempt_end = &terminal_execution.attempt_end;
                let successor = records_by_turn.get(&interrupt.successor());
                let attempt_end_matches = match attempt_end.end() {
                    AttemptEnd::AfterCancellation {
                        cause,
                        disposition: CancellationStopDisposition::Cancelled,
                    } => *cause == interrupt.proof() && attempt_end.interrupt() == Some(interrupt),
                    AttemptEnd::WithoutStop {
                        disposition: UnstoppedAttemptDisposition::YieldedToDurableWait,
                    } => attempt_end.interrupt() == Some(interrupt),
                    _ => false,
                };
                if terminal_execution.owning_turn != turn
                    || interrupt.session() != session
                    || interrupt.proof().predecessor() != turn
                    || !attempt_end_matches
                    || successor.is_none_or(|successor| {
                        successor.stored_session != session
                            || successor.accepted_input.id() != interrupt.accepted_input()
                            || successor.order != interrupt.successor_order()
                    })
                    || attempt_owners.insert(attempt, turn).is_some()
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptEndMismatch {
                            turn,
                            attempt,
                        },
                    );
                }
                let start = validate_start(
                    index,
                    turn,
                    *starting_lineage,
                    *starting_frontier,
                    initial_seed_frontier.and_then(|frontier| snapshots.get(&frontier)),
                    previous_terminal.as_ref(),
                    &origin_by_turn,
                    model_identity_entry,
                    &compaction_chain,
                    &snapshots,
                    &mut referenced_snapshots,
                )?;
                let (source_frontier, named_tool_round_producer, named_call_frontier_accounted) =
                    match terminal_execution.ended_call {
                        Some(call_id) => {
                            let Some(ReconstitutedModelCall::Ended(call)) =
                                model_calls.get(&call_id)
                            else {
                                return Err(
                                AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMissing {
                                    turn,
                                    call: call_id,
                                },
                            );
                            };
                            let Some(pinned) = pinned_targets.get(&turn) else {
                                return Err(
                                AcceptedInputSchedulingReconstitutionFailure::PinnedTargetMissing {
                                    call: call_id,
                                },
                            );
                            };
                            // Direct cancellation names its own cancelled call; a
                            // cancellation that terminalized a tool round names the
                            // batch's completed producing call instead.
                            let named_tool_round_producer = match call.disposition() {
                                ModelCallDisposition::Cancelled => None,
                                ModelCallDisposition::Completed => Some(call_id),
                                ModelCallDisposition::KnownFailed
                                | ModelCallDisposition::Refused
                                | ModelCallDisposition::Ambiguous => {
                                    return Err(
                                    AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                                        turn,
                                    },
                                );
                                }
                            };
                            if call.turn() != turn
                                || call.attempt() != attempt
                                || call.selection()
                                    != *record.origin_configuration.effective().model()
                                || call.target() != pinned.target()
                            {
                                return Err(
                                AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                                    turn,
                                },
                            );
                            }
                            // A frontier accounted for by no other law must prove
                            // the named cancelled call stood at a tool-round
                            // continuation boundary, checked against the terminal
                            // frontier below once it is loaded.
                            let named_call_frontier_accounted = call.frontier().snapshot()
                                == *starting_frontier
                                || referenced_model_calls.contains(&call_id);
                            referenced_model_calls.insert(call_id);
                            if call.frontier().snapshot() != *starting_frontier {
                                referenced_snapshots.insert(call.frontier().snapshot());
                            }
                            (
                                call.frontier().snapshot(),
                                named_tool_round_producer,
                                named_call_frontier_accounted,
                            )
                        }
                        None => (*starting_frontier, None, true),
                    };
                let source = snapshots.get(&source_frontier).ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::StartingSnapshotMissing { turn },
                )?;
                if !snapshots[starting_frontier].is_semantic_prefix_of(source) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                            turn,
                        },
                    );
                }
                let terminal = snapshots.get(terminal_frontier).cloned().ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::TerminalSnapshotMissing { turn },
                )?;
                if !referenced_snapshots.insert(*terminal_frontier) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                let cancellation_entry = cancellation_by_turn.get(&turn).copied().ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::MissingCancellationEntry { turn },
                )?;
                let ordinary_terminal_matches = named_tool_round_producer.is_none()
                    && terminal.has_semantic_prefix_and_suffix(
                        source,
                        std::iter::once(cancellation_entry),
                    );
                // A cancelled continuation call names no starting-frontier or
                // otherwise-referenced snapshot: its whole frontier must be
                // the completed round's result projection that the terminal
                // marker extends by exactly one entry.
                if !named_call_frontier_accounted {
                    let continuation_call_matches = ordinary_terminal_matches
                        && terminal_execution
                            .terminal_tool_attempts()
                            .iter()
                            .all(|tool_attempt| {
                                tool_attempt.issuing_attempt() == terminal_execution.ended_attempt
                            })
                        && tool_round_continuation_producing_call(
                            turn,
                            &terminal,
                            terminal.entry_count().saturating_sub(1),
                            terminal_execution.terminal_tool_attempts(),
                            terminal_execution.terminal_tool_denials(),
                            &model_calls,
                            &assistant_by_call,
                            &snapshots,
                            &semantic_entries,
                        )
                        .is_some();
                    if !continuation_call_matches {
                        return Err(
                            AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                                turn,
                            },
                        );
                    }
                }
                // A stored tool round either names no call, proving a batch
                // interrupt closed an executing round, or names the completed
                // producing call the round's own suffix must identify.
                let tool_round_admissible =
                    terminal_execution.ended_call.is_none() || named_tool_round_producer.is_some();
                let tool_round_terminal_matches = tool_round_admissible
                    && tool_round_terminal_producing_call(
                        turn,
                        &terminal,
                        cancellation_entry,
                        terminal_execution.terminal_tool_attempts(),
                        terminal_execution.terminal_tool_denials(),
                        &model_calls,
                        &assistant_by_call,
                        &snapshots,
                        &semantic_entries,
                    )
                    .is_some_and(|producing_call| {
                        named_tool_round_producer.is_none_or(|named| named == producing_call)
                    });
                if !ordinary_terminal_matches && !tool_round_terminal_matches {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                previous_terminal = Some((turn, terminal.clone()));
                ReconstitutedSchedulingState::TerminalCancelled {
                    start,
                    terminal_frontier: terminal,
                }
            }
            AcceptedInputTurnSchedulingRecordState::TerminalReconciliationRequired {
                starting_lineage,
                starting_frontier,
                reconciling_attempt,
                reconciling_attempt_end,
                ambiguous_call,
                authority,
                terminal_frontier,
            } => {
                if active.is_some() || queued_seen {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder {
                            turn,
                        },
                    );
                }
                let authority_matches = match authority {
                    AutomaticReconciliationAuthority::AppliedInterrupt(interrupt) => {
                        let attempt_end_matches = match reconciling_attempt_end.end() {
                            AttemptEnd::WithoutStop {
                                disposition:
                                    UnstoppedAttemptDisposition::Ambiguous
                                    | UnstoppedAttemptDisposition::Lost,
                            } => reconciling_attempt_end.interrupt().is_none(),
                            AttemptEnd::AfterCancellation {
                                cause,
                                disposition:
                                    CancellationStopDisposition::Ambiguous
                                    | CancellationStopDisposition::Lost,
                            } => {
                                *cause == interrupt.proof()
                                    && reconciling_attempt_end.interrupt() == Some(*interrupt)
                            }
                            _ => false,
                        };
                        let successor = records_by_turn.get(&interrupt.successor());
                        interrupt.session() == session
                            && interrupt.proof().predecessor() == turn
                            && attempt_end_matches
                            && successor.is_some_and(|successor| {
                                successor.stored_session == session
                                    && successor.accepted_input.id() == interrupt.accepted_input()
                                    && successor.order == interrupt.successor_order()
                            })
                    }
                    AutomaticReconciliationAuthority::AutomaticRecovery { .. } => {
                        matches!(
                            reconciling_attempt_end.end(),
                            AttemptEnd::WithoutStop {
                                disposition: UnstoppedAttemptDisposition::Ambiguous
                                    | UnstoppedAttemptDisposition::Lost,
                            }
                        ) && reconciling_attempt_end.interrupt().is_none()
                    }
                };
                if !authority_matches || attempt_owners.insert(*reconciling_attempt, turn).is_some()
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptEndMismatch {
                            turn,
                            attempt: *reconciling_attempt,
                        },
                    );
                }
                let start = validate_start(
                    index,
                    turn,
                    *starting_lineage,
                    *starting_frontier,
                    initial_seed_frontier.and_then(|frontier| snapshots.get(&frontier)),
                    previous_terminal.as_ref(),
                    &origin_by_turn,
                    model_identity_entry,
                    &compaction_chain,
                    &snapshots,
                    &mut referenced_snapshots,
                )?;
                let Some(ReconstitutedModelCall::Ended(call)) = model_calls.get(ambiguous_call)
                else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMissing {
                            turn,
                            call: *ambiguous_call,
                        },
                    );
                };
                let Some(pinned) = pinned_targets.get(&turn) else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::PinnedTargetMissing {
                            call: *ambiguous_call,
                        },
                    );
                };
                if call.turn() != turn
                    || call.attempt() != *reconciling_attempt
                    || call.selection() != *record.origin_configuration.effective().model()
                    || call.target() != pinned.target()
                    || call.disposition() != ModelCallDisposition::Ambiguous
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                            turn,
                        },
                    );
                }
                let named_call_frontier_accounted = call.frontier().snapshot()
                    == *starting_frontier
                    || referenced_model_calls.contains(ambiguous_call);
                referenced_model_calls.insert(*ambiguous_call);
                let source_frontier = call.frontier().snapshot();
                if source_frontier != *starting_frontier {
                    referenced_snapshots.insert(source_frontier);
                }
                let source = snapshots.get(&source_frontier).ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::StartingSnapshotMissing { turn },
                )?;
                if !snapshots[starting_frontier].is_semantic_prefix_of(source) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                            turn,
                        },
                    );
                }
                // A reconciliation-required continuation call names no
                // starting-frontier or otherwise-referenced snapshot: its
                // whole frontier must be the completed round's result
                // projection, which the reconciliation boundary extends by no
                // entry.
                if !named_call_frontier_accounted {
                    let continuation_call_matches = continuation_round_evidence
                        .get(ambiguous_call)
                        .is_some_and(|round| {
                            round.round_tool_attempts().iter().all(|tool_attempt| {
                                tool_attempt.issuing_attempt() == *reconciling_attempt
                            }) && tool_round_continuation_producing_call(
                                turn,
                                source,
                                source.entry_count(),
                                round.round_tool_attempts(),
                                round.round_tool_denials(),
                                &model_calls,
                                &assistant_by_call,
                                &snapshots,
                                &semantic_entries,
                            )
                            .is_some()
                        });
                    if !continuation_call_matches {
                        return Err(
                            AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                                turn,
                            },
                        );
                    }
                    claimed_continuation_rounds.insert(*ambiguous_call);
                }
                let terminal = snapshots.get(terminal_frontier).cloned().ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::TerminalSnapshotMissing { turn },
                )?;
                if !referenced_snapshots.insert(*terminal_frontier)
                    || !terminal.same_semantic_content(source)
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                previous_terminal = Some((turn, terminal.clone()));
                ReconstitutedSchedulingState::TerminalReconciliationRequired {
                    start,
                    terminal_frontier: terminal,
                }
            }
            AcceptedInputTurnSchedulingRecordState::TerminalToolReconciliationRequired {
                starting_lineage,
                starting_frontier,
                reconciling_attempt,
                reconciling_attempt_end,
                tool_batch,
                authority,
                terminal_frontier,
            } => {
                if active.is_some() || queued_seen {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder {
                            turn,
                        },
                    );
                }
                let interrupt = match *authority {
                    AutomaticReconciliationAuthority::AppliedInterrupt(interrupt) => {
                        Some(interrupt)
                    }
                    AutomaticReconciliationAuthority::AutomaticRecovery { .. } => None,
                };
                let attempt_end_matches =
                    tool_reconciliation_attempt_end_matches(reconciling_attempt_end, interrupt);
                let successor_matches = match interrupt {
                    Some(interrupt) => {
                        records_by_turn
                            .get(&interrupt.successor())
                            .is_some_and(|successor| {
                                successor.stored_session == session
                                    && successor.accepted_input.id() == interrupt.accepted_input()
                                    && successor.order == interrupt.successor_order()
                            })
                    }
                    None => true,
                };
                let Some(ambiguous_tool) = tool_batch.awaiting_recovery() else {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptEndMismatch {
                            turn,
                            attempt: *reconciling_attempt,
                        },
                    );
                };
                if interrupt.is_some_and(|interrupt| {
                    interrupt.session() != session || interrupt.proof().predecessor() != turn
                }) || !attempt_end_matches
                    || ambiguous_tool.session() != session
                    || ambiguous_tool.turn() != turn
                    || ambiguous_tool.issuing_attempt() != *reconciling_attempt
                    || !successor_matches
                    || attempt_owners.insert(*reconciling_attempt, turn).is_some()
                {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptEndMismatch {
                            turn,
                            attempt: *reconciling_attempt,
                        },
                    );
                }
                let start = validate_start(
                    index,
                    turn,
                    *starting_lineage,
                    *starting_frontier,
                    initial_seed_frontier.and_then(|frontier| snapshots.get(&frontier)),
                    previous_terminal.as_ref(),
                    &origin_by_turn,
                    model_identity_entry,
                    &compaction_chain,
                    &snapshots,
                    &mut referenced_snapshots,
                )?;
                let source_frontier = ambiguous_tool.yielded_frontier();
                if source_frontier != *starting_frontier {
                    referenced_snapshots.insert(source_frontier);
                }
                let source = snapshots.get(&source_frontier).ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::StartingSnapshotMissing { turn },
                )?;
                if !snapshots[starting_frontier].is_semantic_prefix_of(source) {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                let terminal = snapshots.get(terminal_frontier).cloned().ok_or(
                    AcceptedInputSchedulingReconstitutionFailure::TerminalSnapshotMissing { turn },
                )?;
                let entry_ids = terminal
                    .ordered_entries()
                    .skip(source.entry_count())
                    .map(|reference| reference.entry())
                    .collect();
                let expected_projection = tool_batch
                    .prepare_reconciliation_projection(entry_ids, *terminal_frontier)
                    .ok();
                let projection_matches = expected_projection.as_ref().is_some_and(|projection| {
                    projection.snapshot() == &terminal
                        && projection.entries().iter().all(|expected| {
                            semantic_entries.get(&expected.reference()) == Some(expected)
                        })
                });
                if !referenced_snapshots.insert(*terminal_frontier) || !projection_matches {
                    return Err(
                        AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                            turn,
                        },
                    );
                }
                previous_terminal = Some((turn, terminal.clone()));
                ReconstitutedSchedulingState::TerminalReconciliationRequired {
                    start,
                    terminal_frontier: terminal,
                }
            }
        };

        if !matches!(state, ReconstitutedSchedulingState::Queued)
            && !origin_by_turn.contains_key(&turn)
        {
            return Err(AcceptedInputSchedulingReconstitutionFailure::MissingOriginEntry { turn });
        }
        if matches!(state, ReconstitutedSchedulingState::Queued) {
            previous_terminal = None;
        }

        previous_selected = Some(selected);
        turns.push(AcceptedInputTurnSchedulingProjection {
            session,
            turn,
            accepted_input: record.accepted_input.clone(),
            order: record.order,
            origin_configuration: record.origin_configuration.clone(),
            configuration_provenance: record.configuration_provenance.clone(),
            state,
        });
    }

    referenced_snapshots.extend(compaction_snapshots);
    referenced_snapshots.extend(assistant_call_snapshots);
    if let Some(snapshot) = snapshots
        .keys()
        .copied()
        .find(|snapshot| !referenced_snapshots.contains(snapshot))
    {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::UnreferencedSnapshot { snapshot },
        );
    }
    if let Some(call) = model_calls
        .keys()
        .copied()
        .find(|call| !referenced_model_calls.contains(call))
    {
        return Err(AcceptedInputSchedulingReconstitutionFailure::UnreferencedModelCall { call });
    }
    if let Some(call) = continuation_round_evidence
        .keys()
        .copied()
        .find(|call| !claimed_continuation_rounds.contains(call))
    {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::ContinuationRoundMismatch { call },
        );
    }

    let active_acceptance_tail = reconstitute_active_acceptance_tail(
        session,
        active,
        input.active_acceptance_tail.as_ref(),
        ActiveAcceptanceTailReconstitutionEvidence {
            records_by_turn: &records_by_turn,
            accepted_input_turns: &accepted_input_turns,
            consumed_inputs: &consumed_inputs,
            preceding_non_accepted_terminals: &preceding_non_accepted_terminal_turns,
            execution_position_by_turn: &execution_position_by_turn,
        },
    )?;

    if let Some(call) = active_compaction_call
        && active.is_some()
    {
        return Err(AcceptedInputSchedulingReconstitutionFailure::InvalidCompactionCall { call });
    }

    Ok(AcceptedInputSchedulingProjection {
        session: input.session.clone(),
        initial_seed_frontier,
        latest_compaction_result,
        active_compaction_call,
        turns: turns.into_boxed_slice(),
        active_acceptance_tail,
        semantic_entries,
        snapshots,
        attempt_owners,
        active_model_call_recovery,
        active_stop_requested_frontier,
        active_tool_recovery_attempt,
        active_tool_recovery_frontier,
        active_executing_tool_batch,
        preceding_non_accepted_successors,
        preceding_non_accepted_terminals,
    })
}

fn promote_external_interrupt_chains(
    total_order: Vec<TurnId>,
    external_successors: BTreeSet<TurnId>,
    ordinary_roots: &BTreeSet<TurnId>,
    queued_turns: &BTreeSet<TurnId>,
) -> Vec<TurnId> {
    if external_successors.is_empty() {
        return total_order;
    }
    let roots = ordinary_roots
        .union(&external_successors)
        .copied()
        .collect::<BTreeSet<_>>();
    let Some((crossing_start, insertion)) = total_order
        .iter()
        .enumerate()
        .filter(|(_, turn)| external_successors.contains(turn) && queued_turns.contains(turn))
        .find_map(|(start, _)| {
            total_order[..start]
                .iter()
                .position(|turn| queued_turns.contains(turn) && ordinary_roots.contains(turn))
                .map(|insertion| (start, insertion))
        })
    else {
        return total_order;
    };
    let start = total_order[insertion..crossing_start]
        .iter()
        .position(|turn| external_successors.contains(turn))
        .map(|offset| insertion + offset)
        .unwrap_or(crossing_start);
    let end = total_order[crossing_start + 1..]
        .iter()
        .position(|turn| roots.contains(turn))
        .map(|offset| crossing_start + 1 + offset)
        .unwrap_or(total_order.len());
    let mut promoted = Vec::with_capacity(total_order.len());
    promoted.extend_from_slice(&total_order[..insertion]);
    promoted.extend_from_slice(&total_order[start..end]);
    promoted.extend_from_slice(&total_order[insertion..start]);
    promoted.extend_from_slice(&total_order[end..]);
    promoted
}

struct ActiveAcceptanceTailReconstitutionEvidence<'a, 'record> {
    records_by_turn: &'a BTreeMap<TurnId, &'record AcceptedInputTurnSchedulingRecord>,
    accepted_input_turns: &'a BTreeMap<AcceptedInputId, TurnId>,
    consumed_inputs: &'a BTreeMap<AcceptedInputId, crate::ModelCallId>,
    preceding_non_accepted_terminals: &'a BTreeSet<TurnId>,
    execution_position_by_turn: &'a BTreeMap<TurnId, usize>,
}

fn reconstitute_active_acceptance_tail(
    session: SessionId,
    active: Option<TurnId>,
    candidate: Option<&SessionAcceptanceTailReconstitutionInput>,
    evidence: ActiveAcceptanceTailReconstitutionEvidence<'_, '_>,
) -> Result<Option<SessionAcceptanceTail>, AcceptedInputSchedulingReconstitutionFailure> {
    let ActiveAcceptanceTailReconstitutionEvidence {
        records_by_turn,
        accepted_input_turns,
        consumed_inputs,
        preceding_non_accepted_terminals,
        execution_position_by_turn,
    } = evidence;
    let (active, candidate) = match (active, candidate) {
        (None, None) => return Ok(None),
        (None, Some(_)) => {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::UnexpectedActiveAcceptanceTail,
            );
        }
        (Some(active), None) => {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::MissingActiveAcceptanceTail {
                    turn: active,
                },
            );
        }
        (Some(active), Some(candidate)) => (active, candidate),
    };

    if candidate.session != session {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailSessionMismatch {
                expected: session,
                actual: candidate.session,
            },
        );
    }

    let active_record = records_by_turn[&active];
    let applied_interrupt = match &active_record.state {
        AcceptedInputTurnSchedulingRecordState::Active { phase, .. } => match &phase.state {
            StoredActiveTurnPhase::StopRequested { interrupt, .. } => Some(*interrupt),
            StoredActiveTurnPhase::AwaitingModelCallRecovery { attempt_end, .. } => {
                attempt_end.interrupt()
            }
            StoredActiveTurnPhase::AwaitingToolRecovery { attempt_end, .. } => {
                attempt_end.interrupt()
            }
            StoredActiveTurnPhase::Prepared
            | StoredActiveTurnPhase::Running
            | StoredActiveTurnPhase::AwaitingApproval { .. }
            | StoredActiveTurnPhase::AwaitingChild { .. }
            | StoredActiveTurnPhase::AwaitingRunnerRecovery { .. } => None,
        },
        AcceptedInputTurnSchedulingRecordState::Queued
        | AcceptedInputTurnSchedulingRecordState::TerminalFailed { .. }
        | AcceptedInputTurnSchedulingRecordState::TerminalCompleted { .. }
        | AcceptedInputTurnSchedulingRecordState::TerminalRefused { .. }
        | AcceptedInputTurnSchedulingRecordState::TerminalCancelled { .. }
        | AcceptedInputTurnSchedulingRecordState::TerminalReconciliationRequired { .. }
        | AcceptedInputTurnSchedulingRecordState::TerminalToolReconciliationRequired { .. } => None,
    };
    let expected_anchor = active_record.accepted_input.id();
    if candidate.anchor != expected_anchor {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailAnchorMismatch {
                turn: active,
                expected: expected_anchor,
                actual: candidate.anchor,
            },
        );
    }

    let latest_known_origin_position = records_by_turn
        .values()
        .map(|record| record.order.acceptance_position())
        .fold(
            active_record.order.acceptance_position(),
            SessionInputPosition::max,
        );
    if latest_known_origin_position > candidate.observed_last_position {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailLastPositionMismatch {
                expected: candidate.observed_last_position,
                actual: Some(latest_known_origin_position),
            },
        );
    }

    if let Some(first) = candidate.entries.first()
        && first.accepted_input.id() != expected_anchor
    {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailAnchorMismatch {
                turn: active,
                expected: expected_anchor,
                actual: first.accepted_input.id(),
            },
        );
    }

    let mut expected_position = active_record.order.acceptance_position();
    let mut pending_steering_seen = false;
    let origin_by_position = records_by_turn
        .values()
        .map(|record| {
            (
                record.order.acceptance_position(),
                record.accepted_input.id(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut seen = BTreeSet::new();
    let mut entries = Vec::with_capacity(candidate.entries.len());
    for (index, entry) in candidate.entries.iter().enumerate() {
        let accepted_input = entry.accepted_input.id();
        if entry.session != session {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailEntrySessionMismatch {
                    accepted_input,
                },
            );
        }
        if !seen.insert(accepted_input) {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::DuplicateAcceptanceTailEntry {
                    accepted_input,
                },
            );
        }
        if entry.position != expected_position {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailPositionMismatch {
                    accepted_input,
                    expected: expected_position,
                    actual: entry.position,
                },
            );
        }

        let disposition_valid = match entry.state {
            SessionAcceptanceTailEntryState::RetiredGoalOrigin => {
                match entry.accepted_input.disposition() {
                    AcceptedInputDisposition::OriginOf(origin) => {
                        !records_by_turn.contains_key(origin)
                            && !accepted_input_turns.contains_key(&accepted_input)
                            && match entry.delivery {
                                DeliveryRequest::StartWhenNoActiveTurn { .. } => true,
                                DeliveryRequest::Interrupt { .. }
                                | DeliveryRequest::AfterCurrentTurn { .. }
                                | DeliveryRequest::NextSafePoint { .. } => false,
                            }
                    }
                    AcceptedInputDisposition::PendingSteering { .. }
                    | AcceptedInputDisposition::ConsumedAsSteering { .. }
                    | AcceptedInputDisposition::ReclassifiedAsTurnOrigin { .. }
                    | AcceptedInputDisposition::ClosedNotDelivered => false,
                }
            }
            SessionAcceptanceTailEntryState::RuntimeRelevant => {
                match entry.accepted_input.disposition() {
                    AcceptedInputDisposition::OriginOf(origin)
                    | AcceptedInputDisposition::ReclassifiedAsTurnOrigin { turn: origin, .. } => {
                        records_by_turn.get(origin).is_some_and(|record| {
                            record.accepted_input == entry.accepted_input
                                && record.order.acceptance_position() == entry.position
                                && entry.delivery == record.origin_delivery
                                && origin_delivery_matches_record(
                                    record.origin_delivery,
                                    record,
                                    records_by_turn,
                                    preceding_non_accepted_terminals,
                                )
                        })
                    }
                    AcceptedInputDisposition::PendingSteering { binding } => {
                        pending_steering_seen = true;
                        !accepted_input_turns.contains_key(&accepted_input)
                            && !origin_by_position.contains_key(&entry.position)
                            && matches!(
                                entry.delivery,
                                DeliveryRequest::NextSafePoint {
                                    expected_active_turn,
                                } if expected_active_turn == binding.source_turn()
                                    && expected_active_turn == active
                            )
                    }
                    AcceptedInputDisposition::ConsumedAsSteering { call } => {
                        let source_precedes_active = matches!(
                            entry.delivery,
                            DeliveryRequest::NextSafePoint {
                                expected_active_turn,
                            } if records_by_turn.get(&expected_active_turn).is_some_and(|record| {
                                execution_position_by_turn.get(&expected_active_turn)
                                    .zip(execution_position_by_turn.get(&active))
                                    .is_some_and(|(source, active)| source < active)
                                    && !matches!(
                                        record.state,
                                        AcceptedInputTurnSchedulingRecordState::Queued
                                            | AcceptedInputTurnSchedulingRecordState::Active { .. }
                                    )
                            }) || preceding_non_accepted_terminals
                                .contains(&expected_active_turn)
                        );
                        consumed_inputs.get(&accepted_input) == Some(call)
                            && !pending_steering_seen
                            && !accepted_input_turns.contains_key(&accepted_input)
                            && !origin_by_position.contains_key(&entry.position)
                            && matches!(
                                entry.delivery,
                                DeliveryRequest::NextSafePoint {
                                    expected_active_turn,
                                } if expected_active_turn == active || source_precedes_active
                            )
                    }
                    AcceptedInputDisposition::ClosedNotDelivered => {
                        !accepted_input_turns.contains_key(&accepted_input)
                            && !origin_by_position.contains_key(&entry.position)
                            && matches!(
                                entry.delivery,
                                DeliveryRequest::NextSafePoint {
                                    expected_active_turn,
                                } if expected_active_turn == active
                            )
                    }
                }
            }
        };
        if !disposition_valid
            || (index == 0 && entry.accepted_input != active_record.accepted_input)
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
                    accepted_input,
                },
            );
        }

        if index > 0
            && let DeliveryRequest::Interrupt {
                expected_active_turn,
                ..
            } = entry.delivery
            && !applied_interrupt.is_some_and(|interrupt| {
                expected_active_turn == active
                    && interrupt.accepted_input() == accepted_input
                    && accepted_input_turns.get(&accepted_input) == Some(&interrupt.successor())
            })
            && !accepted_input_turns
                .get(&accepted_input)
                .and_then(|successor| records_by_turn.get(successor))
                .is_some_and(|successor| {
                    historical_interrupt_matches_terminal_proof(
                        session,
                        active,
                        expected_active_turn,
                        accepted_input,
                        successor,
                        records_by_turn,
                    )
                })
        {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                    turn: active,
                    accepted_input,
                },
            );
        }

        entries.push(SessionAcceptanceTailEntry {
            accepted_input: entry.accepted_input.clone(),
            position: entry.position,
            delivery: entry.delivery,
        });
        if index + 1 < candidate.entries.len() {
            expected_position = expected_position.checked_next().ok_or(
                AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailLastPositionMismatch {
                    expected: candidate.observed_last_position,
                    actual: Some(entry.position),
                },
            )?;
        }
    }

    let actual_last = entries.last().map(|entry| entry.position);
    if actual_last != Some(candidate.observed_last_position) {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailLastPositionMismatch {
                expected: candidate.observed_last_position,
                actual: actual_last,
            },
        );
    }

    Ok(Some(SessionAcceptanceTail {
        session,
        anchor: expected_anchor,
        observed_last_position: candidate.observed_last_position,
        entries: entries.into_boxed_slice(),
    }))
}

fn scheduling_record_is_terminal(record: &AcceptedInputTurnSchedulingRecord) -> bool {
    matches!(
        &record.state,
        AcceptedInputTurnSchedulingRecordState::TerminalFailed { .. }
            | AcceptedInputTurnSchedulingRecordState::TerminalCompleted { .. }
            | AcceptedInputTurnSchedulingRecordState::TerminalRefused { .. }
            | AcceptedInputTurnSchedulingRecordState::TerminalCancelled { .. }
            | AcceptedInputTurnSchedulingRecordState::TerminalReconciliationRequired { .. }
            | AcceptedInputTurnSchedulingRecordState::TerminalToolReconciliationRequired { .. }
    )
}

fn historical_interrupt_matches_terminal_proof(
    session: SessionId,
    active: TurnId,
    expected_active_turn: TurnId,
    accepted_input: AcceptedInputId,
    successor: &AcceptedInputTurnSchedulingRecord,
    records_by_turn: &BTreeMap<TurnId, &AcceptedInputTurnSchedulingRecord>,
) -> bool {
    expected_active_turn != active
        && scheduling_record_is_terminal(successor)
        && records_by_turn
            .get(&expected_active_turn)
            .filter(|predecessor| scheduling_record_is_terminal(predecessor))
            .and_then(|predecessor| terminal_record_interrupt(predecessor))
            .is_some_and(|interrupt| {
                interrupt.session() == session
                    && interrupt.proof().predecessor() == expected_active_turn
                    && interrupt.accepted_input() == accepted_input
                    && interrupt.successor() == successor.turn
                    && interrupt.successor_order() == successor.order
            })
}

fn terminal_record_interrupt(
    record: &AcceptedInputTurnSchedulingRecord,
) -> Option<AppliedInterruptCommandResult> {
    match &record.state {
        AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            terminal_execution: Some(execution),
            ..
        } => execution.attempt_end.interrupt(),
        AcceptedInputTurnSchedulingRecordState::TerminalCompleted {
            completing_attempt_end,
            ..
        } => completing_attempt_end.interrupt(),
        AcceptedInputTurnSchedulingRecordState::TerminalRefused {
            refusing_attempt_end,
            ..
        } => refusing_attempt_end.interrupt(),
        AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
            terminal_execution, ..
        } => Some(terminal_execution.interrupt),
        AcceptedInputTurnSchedulingRecordState::TerminalReconciliationRequired {
            authority,
            ..
        } => match authority {
            AutomaticReconciliationAuthority::AppliedInterrupt(interrupt) => Some(*interrupt),
            AutomaticReconciliationAuthority::AutomaticRecovery { .. } => None,
        },
        AcceptedInputTurnSchedulingRecordState::TerminalToolReconciliationRequired {
            authority,
            ..
        } => match authority {
            AutomaticReconciliationAuthority::AppliedInterrupt(interrupt) => Some(*interrupt),
            AutomaticReconciliationAuthority::AutomaticRecovery { .. } => None,
        },
        AcceptedInputTurnSchedulingRecordState::Queued
        | AcceptedInputTurnSchedulingRecordState::Active { .. }
        | AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            terminal_execution: None,
            ..
        } => None,
    }
}

fn tool_reconciliation_attempt_end_matches(
    attempt_end: &TerminalAttemptEndReconstitutionInput,
    interrupt: Option<AppliedInterruptCommandResult>,
) -> bool {
    match attempt_end.end() {
        AttemptEnd::WithoutStop {
            disposition: UnstoppedAttemptDisposition::Ambiguous | UnstoppedAttemptDisposition::Lost,
        } => attempt_end.interrupt().is_none(),
        AttemptEnd::AfterCancellation {
            cause,
            disposition: CancellationStopDisposition::Ambiguous | CancellationStopDisposition::Lost,
        } => {
            interrupt.is_some_and(|interrupt| *cause == interrupt.proof())
                && attempt_end.interrupt() == interrupt
        }
        AttemptEnd::WithoutStop {
            disposition: UnstoppedAttemptDisposition::YieldedToDurableWait,
        } => interrupt.is_some_and(|interrupt| attempt_end.interrupt() == Some(interrupt)),
        _ => false,
    }
}

fn origin_delivery_matches_record(
    delivery: DeliveryRequest,
    record: &AcceptedInputTurnSchedulingRecord,
    records_by_turn: &BTreeMap<TurnId, &AcceptedInputTurnSchedulingRecord>,
    preceding_non_accepted_terminals: &BTreeSet<TurnId>,
) -> bool {
    if let TurnConfigurationProvenance::InheritedForReclassifiedSteering(binding) =
        &record.configuration_provenance
    {
        let source = records_by_turn.get(&binding.source_turn());
        return matches!(
            delivery,
            DeliveryRequest::NextSafePoint {
                expected_active_turn,
            } if expected_active_turn == binding.source_turn()
        ) && record.order.priority() == AcceptedInputQueuePriority::Ordinary
            && source.is_some_and(|source| {
                source.order.acceptance_position() < record.order.acceptance_position()
                    && !matches!(
                        source.state,
                        AcceptedInputTurnSchedulingRecordState::Queued
                            | AcceptedInputTurnSchedulingRecordState::Active { .. }
                    )
                    && source.origin_configuration == record.origin_configuration
            });
    }

    if !origin_configuration_matches_delivery(delivery, &record.origin_configuration) {
        return false;
    }

    match (delivery, record.order.priority()) {
        (DeliveryRequest::StartWhenNoActiveTurn { .. }, AcceptedInputQueuePriority::Ordinary) => {
            true
        }
        (
            DeliveryRequest::AfterCurrentTurn {
                expected_active_turn,
                ..
            },
            AcceptedInputQueuePriority::Ordinary,
        ) => historical_target_precedes_origin(expected_active_turn, record, records_by_turn),
        (
            DeliveryRequest::Interrupt {
                expected_active_turn,
                ..
            },
            AcceptedInputQueuePriority::InterruptImmediatelyAfter { predecessor },
        ) => {
            expected_active_turn == predecessor
                && (historical_target_precedes_origin(
                    expected_active_turn,
                    record,
                    records_by_turn,
                ) || preceding_non_accepted_terminals.contains(&expected_active_turn))
        }
        (
            DeliveryRequest::StartWhenNoActiveTurn { .. }
            | DeliveryRequest::AfterCurrentTurn { .. },
            AcceptedInputQueuePriority::InterruptImmediatelyAfter { .. },
        )
        | (
            DeliveryRequest::Interrupt { .. } | DeliveryRequest::NextSafePoint { .. },
            AcceptedInputQueuePriority::Ordinary,
        )
        | (
            DeliveryRequest::NextSafePoint { .. },
            AcceptedInputQueuePriority::InterruptImmediatelyAfter { .. },
        ) => false,
    }
}

fn origin_configuration_matches_delivery(
    delivery: DeliveryRequest,
    origin_configuration: &OriginConfiguration,
) -> bool {
    let configuration = match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { configuration }
        | DeliveryRequest::Interrupt { configuration, .. }
        | DeliveryRequest::AfterCurrentTurn { configuration, .. } => configuration,
        DeliveryRequest::NextSafePoint { .. } => return false,
    };

    configuration.expected_session_defaults_version()
        == origin_configuration.session_defaults_version()
        && match configuration.model() {
            crate::ModelSelectionOverride::UseSessionDefault => true,
            crate::ModelSelectionOverride::ReplaceWith(requested) => {
                origin_configuration.requested().model() == requested
            }
        }
}

fn historical_target_precedes_origin(
    expected_active_turn: TurnId,
    origin: &AcceptedInputTurnSchedulingRecord,
    records_by_turn: &BTreeMap<TurnId, &AcceptedInputTurnSchedulingRecord>,
) -> bool {
    records_by_turn
        .get(&expected_active_turn)
        .is_some_and(|target| {
            target.order.acceptance_position() < origin.order.acceptance_position()
                && !matches!(target.state, AcceptedInputTurnSchedulingRecordState::Queued)
        })
}

fn validate_record_correlations(
    session: SessionId,
    record: &AcceptedInputTurnSchedulingRecord,
) -> Result<(), AcceptedInputSchedulingReconstitutionFailure> {
    if record.stored_session != session {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::TurnSessionMismatch { turn: record.turn },
        );
    }
    if record.accepted_input_session != session {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::AcceptedInputSessionMismatch {
                turn: record.turn,
            },
        );
    }
    if record.queue_session != session {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::QueueSessionMismatch {
                turn: record.turn,
            },
        );
    }
    if record.queue_turn != record.turn {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::QueueTurnMismatch { turn: record.turn },
        );
    }
    let accepted_input_matches = match &record.configuration_provenance {
        TurnConfigurationProvenance::ExplicitOrigin(configuration) => {
            configuration == &record.origin_configuration
                && record.accepted_input.disposition()
                    == &AcceptedInputDisposition::OriginOf(record.turn)
        }
        TurnConfigurationProvenance::InheritedForReclassifiedSteering(_) => matches!(
            record.accepted_input.disposition(),
            AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                turn,
                reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
            } if *turn == record.turn
        ),
    };
    if !accepted_input_matches {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::AcceptedInputOriginMismatch {
                turn: record.turn,
            },
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_start(
    index: usize,
    turn: TurnId,
    actual_lineage: AcceptedInputStartingLineage,
    starting_frontier: ContextFrontierId,
    initial_seed: Option<&ResolvedContextFrontierSnapshot>,
    previous_terminal: Option<&(TurnId, ResolvedContextFrontierSnapshot)>,
    origin_by_turn: &BTreeMap<TurnId, SemanticTranscriptEntryRef>,
    model_identity_entry: Option<SemanticTranscriptEntryRef>,
    compaction_chain: &[&crate::ContextCompaction],
    snapshots: &BTreeMap<ContextFrontierId, ResolvedContextFrontierSnapshot>,
    referenced_snapshots: &mut BTreeSet<ContextFrontierId>,
) -> Result<AcceptedInputTurnStart, AcceptedInputSchedulingReconstitutionFailure> {
    let expected_lineage = match (index, previous_terminal) {
        (0, None) => AcceptedInputStartingLineage::FirstInSession,
        (_, Some((predecessor, _))) => AcceptedInputStartingLineage::After {
            immediate_predecessor: *predecessor,
        },
        _ => {
            return Err(
                AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder { turn },
            );
        }
    };
    if actual_lineage != expected_lineage {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::StartingLineageMismatch {
                turn,
                expected: expected_lineage,
                actual: actual_lineage,
            },
        );
    }
    let snapshot = snapshots
        .get(&starting_frontier)
        .ok_or(AcceptedInputSchedulingReconstitutionFailure::StartingSnapshotMissing { turn })?;
    if !referenced_snapshots.insert(starting_frontier) {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch { turn },
        );
    }
    let origin = origin_by_turn
        .get(&turn)
        .copied()
        .ok_or(AcceptedInputSchedulingReconstitutionFailure::MissingOriginEntry { turn })?;
    let prefix = previous_terminal
        .map(|(_, frontier)| frontier)
        .or_else(|| (index == 0).then_some(initial_seed).flatten());
    let mut suffix = Vec::with_capacity(usize::from(model_identity_entry.is_some()) + 1);
    suffix.extend(model_identity_entry);
    suffix.push(origin);
    let uncompacted_matches = prefix.map_or_else(
        || {
            snapshot.entry_count() == suffix.len()
                && snapshot.ordered_entries().eq(suffix.iter().copied())
        },
        |prefix| snapshot.has_semantic_prefix_and_suffix(prefix, suffix.iter().copied()),
    );
    let mut compaction_before_start = None;
    for compaction in compaction_chain {
        let Some(source) = snapshots.get(&compaction.source_frontier().snapshot()) else {
            break;
        };
        if snapshot.is_semantic_prefix_of(source) {
            break;
        }
        compaction_before_start = snapshots.get(&compaction.result_frontier().snapshot());
    }
    let applicable_compaction = prefix.and_then(|prefix| {
        compaction_before_start.filter(|result| !result.is_semantic_prefix_of(prefix))
    });
    let membership_matches = applicable_compaction.map_or(uncompacted_matches, |result| {
        prefix.is_some_and(|prefix| {
            prefix.is_semantic_prefix_of(result)
                && snapshot.has_semantic_prefix_and_suffix(result, suffix.iter().copied())
        })
    });
    if !membership_matches {
        return Err(
            AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch { turn },
        );
    }
    Ok(AcceptedInputTurnStart::from_validated_eligibility(
        actual_lineage,
        snapshot.frontier(),
    ))
}

fn completed_terminal_matches(
    starting: &ResolvedContextFrontierSnapshot,
    terminal: &ResolvedContextFrontierSnapshot,
    completing_call: crate::ModelCallId,
    assistant_entries: &BTreeSet<SemanticTranscriptEntryRef>,
    completion_entry: SemanticTranscriptEntryRef,
    semantic_entries: &BTreeMap<SemanticTranscriptEntryRef, SemanticTranscriptEntry>,
) -> bool {
    let assistant_start = starting.entry_count();
    let assistant_end = terminal.entry_count().saturating_sub(1);
    if terminal.ordered_entries().next_back() != Some(completion_entry)
        || !starting.is_semantic_prefix_of(terminal)
        || assistant_end != assistant_start + assistant_entries.len()
    {
        return false;
    }

    terminal
        .ordered_entries_range(assistant_start, assistant_end)
        .all(|entry| {
            assistant_entries.contains(&entry)
                && matches!(
                    semantic_entries.get(&entry).map(SemanticTranscriptEntry::payload),
                    Some(InitialSemanticTranscriptEntryPayload::AssistantText {
                        producing_call,
                        ..
                    }) if *producing_call == completing_call
                )
        })
}

/// Returns the one completed call whose proposals and correlated result suffix
/// fill `terminal` up to its ending `terminal_marker`, or `None` when no call
/// or more than one call can claim that suffix.
///
/// Terminal materialization closes every request that did not complete
/// ordinary execution as `ToolClosed`, so this window admits closed stand-ins.
#[allow(clippy::too_many_arguments)]
fn tool_round_terminal_producing_call(
    turn: TurnId,
    terminal: &ResolvedContextFrontierSnapshot,
    terminal_marker: SemanticTranscriptEntryRef,
    terminal_tool_attempts: &[crate::EndedToolAttempt],
    terminal_tool_denials: &[ToolApprovalResolution],
    model_calls: &BTreeMap<crate::ModelCallId, ReconstitutedModelCall>,
    assistant_by_call: &BTreeMap<crate::ModelCallId, BTreeSet<SemanticTranscriptEntryRef>>,
    snapshots: &BTreeMap<ContextFrontierId, ResolvedContextFrontierSnapshot>,
    semantic_entries: &BTreeMap<SemanticTranscriptEntryRef, SemanticTranscriptEntry>,
) -> Option<crate::ModelCallId> {
    if terminal.ordered_entries().next_back() != Some(terminal_marker) {
        return None;
    }
    tool_round_producing_call_in_window(
        turn,
        terminal,
        terminal.entry_count().saturating_sub(1),
        ToolRoundResultWindow::TerminalClosure,
        terminal_tool_attempts,
        terminal_tool_denials,
        model_calls,
        assistant_by_call,
        snapshots,
        semantic_entries,
    )
}

/// Returns the one completed call whose proposals and correlated result suffix
/// fill `snapshot` up to `results_end`, or `None` when no call or more than
/// one call can claim that window.
///
/// Continuation happens only once every request is executed or denied, so this
/// window forbids `ToolClosed` stand-ins and admits only the attempt ends the
/// continuation writer projects.
#[allow(clippy::too_many_arguments)]
fn tool_round_continuation_producing_call(
    turn: TurnId,
    snapshot: &ResolvedContextFrontierSnapshot,
    results_end: usize,
    round_tool_attempts: &[crate::EndedToolAttempt],
    round_tool_denials: &[ToolApprovalResolution],
    model_calls: &BTreeMap<crate::ModelCallId, ReconstitutedModelCall>,
    assistant_by_call: &BTreeMap<crate::ModelCallId, BTreeSet<SemanticTranscriptEntryRef>>,
    snapshots: &BTreeMap<ContextFrontierId, ResolvedContextFrontierSnapshot>,
    semantic_entries: &BTreeMap<SemanticTranscriptEntryRef, SemanticTranscriptEntry>,
) -> Option<crate::ModelCallId> {
    tool_round_producing_call_in_window(
        turn,
        snapshot,
        results_end,
        ToolRoundResultWindow::Continuation,
        round_tool_attempts,
        round_tool_denials,
        model_calls,
        assistant_by_call,
        snapshots,
        semantic_entries,
    )
}

/// Which materialization owns a checked tool-round result window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolRoundResultWindow {
    /// Turn-end materialization: requests that did not complete ordinary
    /// execution close as `ToolClosed`, and a crash-lost known-failed attempt
    /// projects its result directly.
    TerminalClosure,
    /// The continuation transaction: every request executed or denied, so
    /// closure stand-ins and non-projectable attempt ends are forbidden.
    Continuation,
}

/// Returns the one completed call whose proposals and correlated result
/// entries fill `snapshot`'s window ending at `results_end`, or `None` when no
/// call or more than one call can claim it.
#[allow(clippy::too_many_arguments)]
fn tool_round_producing_call_in_window(
    turn: TurnId,
    terminal: &ResolvedContextFrontierSnapshot,
    before_marker_end: usize,
    window: ToolRoundResultWindow,
    terminal_tool_attempts: &[crate::EndedToolAttempt],
    terminal_tool_denials: &[ToolApprovalResolution],
    model_calls: &BTreeMap<crate::ModelCallId, ReconstitutedModelCall>,
    assistant_by_call: &BTreeMap<crate::ModelCallId, BTreeSet<SemanticTranscriptEntryRef>>,
    snapshots: &BTreeMap<ContextFrontierId, ResolvedContextFrontierSnapshot>,
    semantic_entries: &BTreeMap<SemanticTranscriptEntryRef, SemanticTranscriptEntry>,
) -> Option<crate::ModelCallId> {
    // A continuation window projects only executed results the writer admits:
    // a completed attempt or an ordinary known failure. An ambiguous or
    // crash-lost end is a turn-level failure that can never reach a
    // continuation, while terminal materialization projects the crash-lost
    // known-failed attempt directly.
    if window == ToolRoundResultWindow::Continuation
        && terminal_tool_attempts
            .iter()
            .any(|attempt| match attempt.end() {
                crate::ToolAttemptEnd::Completed { .. } => false,
                crate::ToolAttemptEnd::KnownFailed { error } => {
                    error.kind() == crate::ToolExecutionErrorKind::CrashLost
                }
                crate::ToolAttemptEnd::AwaitingChild { .. } => false,
                crate::ToolAttemptEnd::Ambiguous => true,
            })
    {
        return None;
    }
    let mut denied_requests = BTreeSet::new();
    for resolution in terminal_tool_denials {
        if !matches!(resolution.decision(), ToolApprovalDecision::Deny { .. })
            || !denied_requests.insert(resolution.request())
        {
            return None;
        }
    }

    let mut producing_calls = model_calls
        .iter()
        .filter(|(call_id, candidate)| {
            let ReconstitutedModelCall::Ended(call) = candidate else {
                return false;
            };
            if call.turn() != turn || call.disposition() != ModelCallDisposition::Completed {
                return false;
            }
            let Some(source) = snapshots.get(&call.frontier().snapshot()) else {
                return false;
            };
            if !source.is_semantic_prefix_of(terminal) {
                return false;
            }
            let Some(assistant_entries) = assistant_by_call.get(call_id) else {
                return false;
            };
            let assistant_start = source.entry_count();
            let assistant_end = assistant_start + assistant_entries.len();
            if assistant_end > before_marker_end {
                return false;
            }

            let mut requests = Vec::new();
            for entry in terminal.ordered_entries_range(assistant_start, assistant_end) {
                if !assistant_entries.contains(&entry) {
                    return false;
                }
                match semantic_entries
                    .get(&entry)
                    .map(SemanticTranscriptEntry::payload)
                {
                    Some(SemanticTranscriptEntryPayload::AssistantText {
                        producing_call, ..
                    }) if producing_call == *call_id => {}
                    Some(SemanticTranscriptEntryPayload::AssistantToolUse {
                        producing_call,
                        request,
                    }) if producing_call == *call_id => requests.push(*request),
                    _ => return false,
                }
            }
            if requests.is_empty()
                || requests.iter().copied().collect::<BTreeSet<_>>().len() != requests.len()
                || before_marker_end - assistant_end != requests.len()
            {
                return false;
            }

            let mut attempts_by_id = BTreeMap::new();
            for attempt in terminal_tool_attempts {
                if attempt.session() != terminal.frontier().owning_session()
                    || attempt.turn() != turn
                    || attempts_by_id.insert(attempt.attempt(), attempt).is_some()
                {
                    return false;
                }
            }
            let mut observed_attempts = BTreeSet::new();
            let mut observed_denials = BTreeSet::new();
            let results_match = terminal
                .ordered_entries_range(assistant_end, before_marker_end)
                .zip(requests)
                .all(|(entry, request)| {
                    match semantic_entries
                        .get(&entry)
                        .map(SemanticTranscriptEntry::payload)
                    {
                        Some(SemanticTranscriptEntryPayload::ToolExecutionResult { attempt }) => {
                            observed_attempts.insert(*attempt)
                                && attempts_by_id
                                    .get(attempt)
                                    .is_some_and(|ended| ended.request() == request)
                        }
                        Some(SemanticTranscriptEntryPayload::ToolDenied { request: actual }) => {
                            *actual == request
                                && denied_requests.contains(actual)
                                && observed_denials.insert(*actual)
                        }
                        Some(SemanticTranscriptEntryPayload::ToolClosed { request: actual }) => {
                            window == ToolRoundResultWindow::TerminalClosure && *actual == request
                        }
                        _ => false,
                    }
                });
            results_match
                && observed_attempts.len() == attempts_by_id.len()
                && observed_denials.len() == denied_requests.len()
        })
        .map(|(call_id, _)| *call_id);
    let producing_call = producing_calls.next()?;
    producing_calls.next().is_none().then_some(producing_call)
}

fn prepare_active_turn_lost_failure(
    projection: AcceptedInputSchedulingProjection,
    identities: AcceptedInputTurnFailureIdentities,
) -> Result<PreparedAcceptedInputTurnFailure, AcceptedInputTurnFailureError> {
    let fail = |projection, failure| AcceptedInputTurnFailureError {
        projection: Box::new(projection),
        identities: identities.clone(),
        failure,
    };

    let Some(active) = projection.active_turn() else {
        return Err(fail(
            projection,
            AcceptedInputTurnFailureFailure::NoActiveTurn,
        ));
    };
    let active = active.clone();

    let reclassified = match projection.active_turn_execution() {
        Some(execution) => reclassify_pending_steering_inputs(
            execution.session(),
            execution.turn(),
            execution.pending_steering(),
            identities.pending_steering_reclassifications(),
            execution.configuration().effective(),
        ),
        None => reclassify_pending_steering_inputs(
            active.session,
            active.turn,
            &[],
            identities.pending_steering_reclassifications(),
            active.origin_configuration.effective(),
        ),
    };
    let Ok(reclassified_pending_steering) = reclassified else {
        return Err(fail(
            projection,
            AcceptedInputTurnFailureFailure::PendingSteeringReclassificationMismatch,
        ));
    };

    let failure_ref =
        SemanticTranscriptEntryRef::from_source(active.session, identities.failure_entry);
    if projection.semantic_entries.contains_key(&failure_ref) {
        return Err(fail(
            projection,
            AcceptedInputTurnFailureFailure::FailureEntryIdentityAlreadyExists,
        ));
    }
    if projection
        .snapshots
        .contains_key(&identities.terminal_frontier)
    {
        return Err(fail(
            projection,
            AcceptedInputTurnFailureFailure::TerminalFrontierIdentityAlreadyExists,
        ));
    }

    let current_attempt = match active.active_phase() {
        Some(ActiveTurnPhase::Running { current_attempt }) => current_attempt.clone(),
        Some(
            ActiveTurnPhase::AwaitingApproval { .. }
            | ActiveTurnPhase::AwaitingChild { .. }
            | ActiveTurnPhase::AwaitingRecoveryDecision { .. }
            | ActiveTurnPhase::AwaitingRunnerRecovery { .. },
        )
        | None => {
            return Err(fail(
                projection,
                AcceptedInputTurnFailureFailure::ActiveAttemptCannotEndLost,
            ));
        }
    };
    let Ok(ended_attempt) = current_attempt.end_without_stop(UnstoppedAttemptDisposition::Lost)
    else {
        return Err(fail(
            projection,
            AcceptedInputTurnFailureFailure::ActiveAttemptCannotEndLost,
        ));
    };

    let Some(start) = active.start() else {
        return Err(fail(
            projection,
            AcceptedInputTurnFailureFailure::ActiveStartMissing,
        ));
    };
    let failure_entry = SemanticTranscriptEntry::from_validated_parts(
        identities.failure_entry,
        active.session,
        InitialSemanticTranscriptEntryPayload::TurnFailed { turn: active.turn },
    );
    let Some(starting_snapshot) = projection.snapshots.get(&start.frontier().snapshot()) else {
        return Err(fail(
            projection,
            AcceptedInputTurnFailureFailure::StartingSnapshotMissing,
        ));
    };
    let Ok(terminal_snapshot) = starting_snapshot.derive_appending_candidate(
        identities.terminal_frontier,
        vec![failure_entry.reference()],
    ) else {
        return Err(fail(
            projection,
            AcceptedInputTurnFailureFailure::TerminalFrontierCannotAppend,
        ));
    };
    let turn = FailedAcceptedInputTurn {
        session: active.session,
        turn: active.turn,
        accepted_input: active.accepted_input,
        order: active.order,
        start,
        ended_attempt,
        disposition: TurnDisposition::Failed,
        terminal_frontier: identities.terminal_frontier,
    };

    Ok(PreparedAcceptedInputTurnFailure {
        turn,
        failure_entry,
        terminal_snapshot,
        reclassified_pending_steering,
    })
}

fn prepare_earliest_queued_activation(
    projection: AcceptedInputSchedulingProjection,
    identities: AcceptedInputTurnActivationIdentities,
) -> Result<PreparedAcceptedInputTurnActivation, AcceptedInputEligibilityError> {
    let fail = |projection, failure| AcceptedInputEligibilityError {
        projection: Box::new(projection),
        identities,
        failure,
    };

    if projection
        .attempt_owners
        .contains_key(&identities.initial_attempt)
    {
        return Err(fail(
            projection,
            AcceptedInputEligibilityFailure::InitialAttemptIdentityAlreadyExists,
        ));
    }
    if let Some(active) = projection.active_turn() {
        let turn = active.turn();
        return Err(fail(
            projection,
            AcceptedInputEligibilityFailure::ActiveTurnPresent { turn },
        ));
    }
    if let Some(call) = projection.active_compaction_call {
        return Err(fail(
            projection,
            AcceptedInputEligibilityFailure::ContextCompactionInProgress { call },
        ));
    }
    let Some(index) = projection
        .turns
        .iter()
        .position(|turn| turn.status() == AcceptedInputTurnSchedulingStatus::Queued)
    else {
        return Err(fail(
            projection,
            AcceptedInputEligibilityFailure::NoQueuedTurn,
        ));
    };

    let source_session = projection.session.id();
    let origin_ref =
        SemanticTranscriptEntryRef::from_source(source_session, identities.origin_entry);
    if projection.semantic_entries.contains_key(&origin_ref) {
        return Err(fail(
            projection,
            AcceptedInputEligibilityFailure::OriginEntryIdentityAlreadyExists,
        ));
    }
    if projection
        .snapshots
        .contains_key(&identities.starting_frontier)
    {
        return Err(fail(
            projection,
            AcceptedInputEligibilityFailure::StartingFrontierIdentityAlreadyExists,
        ));
    }

    let queued = &projection.turns[index];
    let preceding_non_accepted_terminal = projection
        .preceding_non_accepted_successors
        .get(&queued.turn())
        .and_then(|predecessor| {
            projection
                .preceding_non_accepted_terminals
                .get(predecessor)
                .map(|(snapshot, selected)| (*predecessor, snapshot, *selected))
        });
    let origin_entry = SemanticTranscriptEntry::from_validated_parts(
        identities.origin_entry,
        source_session,
        InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
            accepted_input: queued.accepted_input.id(),
        },
    );
    let selected = queued
        .origin_configuration
        .effective()
        .model()
        .selected_direct();
    let previous_selected = preceding_non_accepted_terminal
        .as_ref()
        .map(|(_, _, selected)| *selected)
        .or_else(|| {
            index.checked_sub(1).map(|predecessor| {
                projection.turns[predecessor]
                    .origin_configuration
                    .effective()
                    .model()
                    .selected_direct()
            })
        });
    let model_identity_entry = previous_selected
        .filter(|previous| *previous != selected)
        .map(|_| {
            let reference = SemanticTranscriptEntryRef::from_source(
                source_session,
                identities.model_identity_entry,
            );
            if identities.model_identity_entry == identities.origin_entry
                || projection.semantic_entries.contains_key(&reference)
            {
                return Err(fail(
                    projection.clone(),
                    AcceptedInputEligibilityFailure::ModelIdentityEntryIdentityAlreadyExists,
                ));
            }
            Ok(SemanticTranscriptEntry::from_validated_parts(
                identities.model_identity_entry,
                source_session,
                InitialSemanticTranscriptEntryPayload::ModelIdentityChanged {
                    turn: queued.turn,
                    defaults_version: queued.origin_configuration.session_defaults_version(),
                    selected,
                },
            ))
        })
        .transpose()?;
    let starting_entries = match model_identity_entry {
        Some(model_identity_entry) => AcceptedInputTurnStartingEntries::ModelIdentityThenOrigin([
            model_identity_entry,
            origin_entry,
        ]),
        None => AcceptedInputTurnStartingEntries::Origin([origin_entry]),
    };
    let starting_references = starting_entries
        .as_slice()
        .iter()
        .map(SemanticTranscriptEntry::reference)
        .collect::<Vec<_>>();
    let (lineage, starting_snapshot) = if index == 0 && preceding_non_accepted_terminal.is_none() {
        let seed = projection
            .initial_seed_frontier
            .and_then(|frontier| projection.snapshots.get(&frontier));
        let compacted = projection
            .latest_compaction_result
            .and_then(|frontier| projection.snapshots.get(&frontier))
            .filter(|latest| seed.is_some_and(|seed| seed.is_semantic_prefix_of(latest)));
        let base = compacted.or(seed);
        let snapshot = if let Some(base) = base {
            match base.derive_appending_candidate(
                identities.starting_frontier,
                starting_references.clone(),
            ) {
                Ok(snapshot) => snapshot,
                Err(_) => {
                    return Err(fail(
                        projection,
                        AcceptedInputEligibilityFailure::InternalOriginFrontierConstructionFailed,
                    ));
                }
            }
        } else {
            match ResolvedContextFrontierSnapshot::try_from_candidate(
                source_session,
                identities.starting_frontier,
                starting_references.clone(),
            ) {
                Ok(snapshot) => snapshot,
                Err(_) => {
                    return Err(fail(
                        projection,
                        AcceptedInputEligibilityFailure::InternalOriginFrontierConstructionFailed,
                    ));
                }
            }
        };
        (AcceptedInputStartingLineage::FirstInSession, snapshot)
    } else {
        let (predecessor_turn, terminal_frontier) =
            if let Some((predecessor_turn, terminal_frontier, _)) = preceding_non_accepted_terminal
            {
                (predecessor_turn, terminal_frontier)
            } else if let Some(predecessor_index) = index.checked_sub(1) {
                let predecessor = &projection.turns[predecessor_index];
                let predecessor_turn = predecessor.turn;
                let Some(terminal_frontier) = predecessor.terminal_frontier() else {
                    return Err(fail(
                    projection,
                    AcceptedInputEligibilityFailure::InternalPredecessorTerminalFrontierMissing {
                        predecessor: predecessor_turn,
                    },
                ));
                };
                (predecessor_turn, terminal_frontier)
            } else {
                return Err(fail(
                    projection,
                    AcceptedInputEligibilityFailure::InternalStartingFrontierDerivationFailed,
                ));
            };
        let compacted = projection
            .latest_compaction_result
            .and_then(|frontier| projection.snapshots.get(&frontier))
            .filter(|latest| terminal_frontier.is_semantic_prefix_of(latest));
        let base = compacted.unwrap_or(terminal_frontier);
        let snapshot = match base
            .derive_appending_candidate(identities.starting_frontier, starting_references)
        {
            Ok(snapshot) => snapshot,
            Err(_) => {
                return Err(fail(
                    projection,
                    AcceptedInputEligibilityFailure::InternalStartingFrontierDerivationFailed,
                ));
            }
        };
        (
            AcceptedInputStartingLineage::After {
                immediate_predecessor: predecessor_turn,
            },
            snapshot,
        )
    };
    let start =
        AcceptedInputTurnStart::from_validated_eligibility(lineage, starting_snapshot.frontier());
    let turn = ActivatedAcceptedInputTurn {
        session: source_session,
        turn: queued.turn,
        accepted_input: queued.accepted_input.clone(),
        order: queued.order,
        configuration: queued.origin_configuration.clone(),
        configuration_provenance: queued.configuration_provenance.clone(),
        start,
        phase: ActiveTurnPhase::Running {
            current_attempt: CurrentTurnAttempt::prepared(identities.initial_attempt),
        },
        pending_steering: Box::new([]),
        consumed_steering: Box::new([]),
    };

    Ok(PreparedAcceptedInputTurnActivation {
        turn,
        starting_entries,
        starting_snapshot,
    })
}

#[cfg(test)]
mod tests {
    use expect_test::expect;
    use signalbox_expect_table::table;

    use super::*;
    use crate::{
        AcceptedInputDisposition, AssistantText, AttemptEnd, CreateSessionFromImportedFrontier,
        CurrentTurnAttemptState, DescendantTerminationScope, FrozenModelSelection,
        ImportedConversation, ImportedConversationFormat, ImportedRawRecordPosition,
        ImportedRawSourceRecord, ImportedRecordEntryPosition, ImportedSessionReconstitutionInput,
        ImportedSessionRelationship, ImportedSourceAttestation, ImportedSourceMetadata,
        ImportedStructuredObjectMember, ImportedStructuredValue, ImportedText,
        ImportedTranscriptContent, ImportedTranscriptEntryInput, ImportedTranscriptPosition,
        ModelCallReconstitutionInput, ModelCallReconstitutionState, ModelSelectionOverride,
        ModelSelectionRequest, NormalizedToolArguments, PerInputConfigurationChoices,
        ResolvedProviderTarget, SessionConfigurationDefaults, SessionConfigurationDefaultsVersion,
        SessionCreationCause, SessionCreationProvenance, SessionPlacement, SessionPlacementVersion,
        SessionReconstitutionInput, ToolApprovalDecision,
        ToolApprovalResolutionReconstitutionInput, ToolAttemptEnd, ToolAttemptReconstitutionInput,
        ToolAttemptReconstitutionState, ToolBatchPhaseReconstitutionInput,
        ToolBatchReconstitutionInput, ToolDispatchGeneration, ToolEffectClass, ToolExecutionError,
        ToolExecutionErrorKind, ToolName, ToolRequestOrdinal, ToolRequestReconstitutionInput,
        ToolResultContent, ToolResultText, VersionedSessionPlacement,
        test_support::{
            accepted_input_id, command_id, context_frontier_id, delegation_message_id, direct,
            imported_conversation_id, imported_transcript_entry_id, model_call_id,
            provider_model_identity, semantic_transcript_entry_id, session_id, tool_attempt_id,
            tool_request_id, transcript_frontier, turn_attempt_id, turn_id,
        },
    };

    fn current_session() -> Session {
        let session = session_id(1);
        let version = SessionConfigurationDefaultsVersion::first();
        let defaults = SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct(1)));
        SessionReconstitutionInput::new(
            session,
            session,
            SessionCreationProvenance::new(
                SessionCreationCause::Interactive,
                TranscriptAncestry::None,
            ),
            session,
            version,
            session,
            version,
            defaults,
            crate::SessionPlacementReconstitutionFacts {
                current_pointer_session: session,
                current_pointer_version: crate::SessionPlacementVersion::INITIAL,
                selected_event_session: session,
                selected_event: crate::VersionedSessionPlacement::initial(
                    crate::SessionPlacement::pathless(),
                ),
            },
        )
        .reconstitute()
        .expect("test session facts are fully correlated")
    }

    fn imported_position(value: u64) -> ImportedTranscriptPosition {
        ImportedTranscriptPosition::try_from_u64(value).expect("test position is positive")
    }

    fn imported_raw_position(value: u64) -> ImportedRawRecordPosition {
        ImportedRawRecordPosition::try_from_u64(value).expect("test position is positive")
    }

    fn imported_source_event(
        conversation: crate::ImportedConversationId,
        identity: u128,
        ordinal: u64,
        source_type: &str,
    ) -> (ImportedRawSourceRecord, ImportedTranscriptEntryInput) {
        let source_type = ImportedText::new(source_type.to_owned());
        let normalized = ImportedStructuredValue::Object(
            vec![ImportedStructuredObjectMember::new(
                ImportedText::new("type".to_owned()),
                ImportedStructuredValue::String(source_type.clone()),
            )]
            .into_boxed_slice(),
        );
        let raw = ImportedRawSourceRecord::from_converted(
            format!("synthetic-scheduling-record-{ordinal}").into_bytes(),
            normalized,
        );
        let source = ImportedSourceMetadata::new(
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
            ImportedSourceAttestation::NotAttested,
        );
        let entry = ImportedTranscriptEntryInput::new(
            imported_transcript_entry_id(identity),
            conversation,
            imported_position(ordinal),
            imported_raw_position(ordinal),
            ImportedRecordEntryPosition::first(),
            ImportedSourceAttestation::NotAttested,
            ImportedTranscriptContent::SourceEvent {
                source_type: ImportedSourceAttestation::Attested(source_type),
            },
            source,
        );
        (raw, entry)
    }

    fn imported_session() -> ReconstitutedImportedSession {
        imported_session_for(1)
    }

    fn imported_session_for(session_value: u128) -> ReconstitutedImportedSession {
        let conversation_id = imported_conversation_id(80);
        let (first_raw, first_entry) = imported_source_event(conversation_id, 81, 1, "summary");
        let (second_raw, second_entry) = imported_source_event(conversation_id, 82, 2, "system");
        let conversation = ImportedConversation::from_converted_records(
            conversation_id,
            ImportedConversationFormat::ClaudeCodeSessionJsonlV1,
            vec![first_raw, second_raw],
            vec![first_entry, second_entry],
        )
        .expect("synthetic imported scheduling history is checked");
        let command = CreateSessionFromImportedFrontier::new(
            command_id(83),
            conversation
                .frontiers()
                .last()
                .expect("fixture has two imported frontiers"),
            ImportedSessionRelationship::Resume,
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct(1))),
        );
        let mut next_entry = 84_u128;
        let prepared = command
            .clone()
            .prepare(
                &conversation,
                session_id(session_value),
                context_frontier_id(89),
                || {
                    let identity = semantic_transcript_entry_id(next_entry);
                    next_entry += 1;
                    identity
                },
            )
            .expect("matching imported history prepares a seed");
        let command_defaults = command.initial_configuration_defaults().clone();
        let seed = prepared.imported_seed();
        let snapshot = prepared.seed_snapshot();
        ImportedSessionReconstitutionInput::new(
            prepared.session().id(),
            prepared.session().id(),
            prepared.session().provenance(),
            prepared.session().id(),
            SessionConfigurationDefaultsVersion::first(),
            prepared.session().id(),
            SessionConfigurationDefaultsVersion::first(),
            command_defaults,
            crate::SessionPlacementReconstitutionFacts {
                current_pointer_session: prepared.session().id(),
                current_pointer_version: SessionPlacementVersion::INITIAL,
                selected_event_session: prepared.session().id(),
                selected_event: VersionedSessionPlacement::initial(SessionPlacement::pathless()),
            },
            conversation,
            vec![crate::ImportedSessionSeedReconstitutionInput::new(
                seed.session(),
                seed.seed_frontier(),
            )],
            vec![ResolvedContextFrontierReconstitutionInput::new(
                snapshot.frontier().owning_session(),
                snapshot.frontier().snapshot(),
                snapshot.ordered_entries().collect(),
            )],
            prepared
                .semantic_entries()
                .iter()
                .map(|entry| {
                    SemanticTranscriptEntryReconstitutionInput::new(
                        entry.identity(),
                        entry.source_session(),
                        entry.payload().clone(),
                    )
                })
                .collect(),
        )
        .reconstitute()
        .expect("complete imported scheduling fixture reconstitutes")
    }

    fn configuration(session: &Session) -> OriginConfiguration {
        let checked = session
            .current_configuration_defaults()
            .derive_request(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            )
            .expect("the test request names the current defaults");
        OriginConfiguration::freeze(checked, |_| None)
            .expect("a direct model selection does not consult aliases")
    }

    #[test]
    fn delegated_activation_preserves_task_origin_and_first_session_lineage() {
        let child = current_session();
        let spawning_request = tool_request_id(401);
        let child_turn = turn_id(402);
        let task = DelegationContent::try_new(String::from("inspect delegated work"))
            .expect("fixture task is valid");
        let task_entry = SemanticTranscriptEntryReconstitutionInput::new(
            semantic_transcript_entry_id(403),
            child.id(),
            SemanticTranscriptEntryPayload::DelegatedTask {
                spawning_request,
                parent_session: session_id(404),
                parent_turn: turn_id(405),
                content: task.clone(),
            },
        );
        let prepared = PreparedDelegatedTurnActivation::prepare(DelegatedTurnActivationInput {
            session: child.id(),
            turn: child_turn,
            spawning_request,
            task: task.clone(),
            task_entry,
            configuration: configuration(&child),
            starting_frontier: context_frontier_id(406),
            initial_attempt: turn_attempt_id(407),
        })
        .expect("exact delegated task facts prepare activation");
        let (active, origin, snapshot) = prepared.into_parts();

        assert_eq!(active.session(), child.id());
        assert_eq!(active.turn(), child_turn);
        assert_eq!(active.spawning_request(), Some(spawning_request));
        assert_eq!(active.task(), Some(&task));
        assert_eq!(
            active.start().lineage(),
            AcceptedInputStartingLineage::FirstInSession
        );
        assert_eq!(snapshot.entry_count(), 1);
        assert_eq!(origin.len(), 1);
        assert_eq!(
            origin.first().unwrap().reference(),
            snapshot.ordered_entries().next().unwrap()
        );
    }

    #[test]
    fn delegated_activation_reconstitutes_consumed_steering() {
        let child = current_session();
        let child_turn = turn_id(408);
        let task = DelegationContent::try_new(String::from("inspect delegated steering"))
            .expect("fixture task is valid");
        let task_entry = SemanticTranscriptEntryReconstitutionInput::new(
            semantic_transcript_entry_id(409),
            child.id(),
            SemanticTranscriptEntryPayload::DelegatedTask {
                spawning_request: tool_request_id(410),
                parent_session: session_id(411),
                parent_turn: turn_id(412),
                content: task.clone(),
            },
        );
        let prepared = PreparedDelegatedTurnActivation::prepare(DelegatedTurnActivationInput {
            session: child.id(),
            turn: child_turn,
            spawning_request: tool_request_id(410),
            task,
            task_entry,
            configuration: configuration(&child),
            starting_frontier: context_frontier_id(413),
            initial_attempt: turn_attempt_id(414),
        })
        .expect("exact delegated task facts prepare activation");
        let consumed_input = accepted_input_id(415);
        let consuming_call = model_call_id(416);
        let position = SessionInputPosition::try_from_u64(2).unwrap();
        let consumed = ConsumedSteeringReconstitutionInput::new(
            child.id(),
            AcceptedInputLifecycle::new(
                consumed_input,
                AcceptedInputDisposition::ConsumedAsSteering {
                    call: consuming_call,
                },
            ),
            position,
            child_turn,
        );

        let active = prepared
            .into_parts()
            .0
            .with_consumed_steering(vec![consumed])
            .expect("stored steering targets the delegated turn");

        assert_eq!(active.consumed_steering().len(), 1);
        assert_eq!(
            active.consumed_steering()[0].accepted_input(),
            consumed_input
        );
        assert_eq!(
            active.consumed_steering()[0].acceptance_position(),
            position
        );
        assert_eq!(active.consumed_steering()[0].source_turn(), child_turn);
    }

    #[test]
    fn delegated_wake_activation_preserves_delivery_range_and_predecessor_lineage() {
        let recipient = current_session();
        let predecessor = turn_id(411);
        let predecessor_entry = SemanticTranscriptEntryRef::from_source(
            recipient.id(),
            semantic_transcript_entry_id(412),
        );
        let predecessor_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
            recipient.id(),
            context_frontier_id(413),
            vec![predecessor_entry],
        )
        .expect("fixture predecessor snapshot is valid");
        let first_sequence = NonZeroU64::new(1).unwrap();
        let through_sequence = NonZeroU64::new(2).unwrap();
        let first_delivery = SemanticTranscriptEntryReconstitutionInput::new(
            semantic_transcript_entry_id(414),
            recipient.id(),
            SemanticTranscriptEntryPayload::DelegationMessage {
                spawning_request: tool_request_id(415),
                message: delegation_message_id(416),
                sender: session_id(417),
                recipient: recipient.id(),
                delivery_sequence: first_sequence,
                content: DelegationContent::try_new(String::from("first wake message")).unwrap(),
            },
        );
        let through_delivery = SemanticTranscriptEntryReconstitutionInput::new(
            semantic_transcript_entry_id(418),
            recipient.id(),
            SemanticTranscriptEntryPayload::DelegationMessage {
                spawning_request: tool_request_id(415),
                message: delegation_message_id(419),
                sender: session_id(417),
                recipient: recipient.id(),
                delivery_sequence: through_sequence,
                content: DelegationContent::try_new(String::from("second wake message")).unwrap(),
            },
        );
        let prepared =
            PreparedDelegatedTurnActivation::prepare_wake(DelegatedWakeTurnActivationInput {
                session: recipient.id(),
                turn: turn_id(420),
                first_delivery_sequence: first_sequence,
                through_delivery_sequence: through_sequence,
                deliveries: vec![first_delivery, through_delivery],
                predecessor,
                predecessor_snapshot,
                configuration: configuration(&recipient),
                starting_frontier: context_frontier_id(421),
                initial_attempt: turn_attempt_id(422),
            })
            .expect("contiguous checked deliveries prepare a wake activation");
        let (active, entries, snapshot) = prepared.into_parts();

        assert_eq!(active.spawning_request(), None);
        assert_eq!(active.task(), None);
        assert_eq!(
            active.delivery_range(),
            Some((first_sequence, through_sequence))
        );
        assert_eq!(
            active.start().lineage(),
            AcceptedInputStartingLineage::After {
                immediate_predecessor: predecessor
            }
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(snapshot.entry_count(), 3);
        assert_eq!(
            snapshot.immediate_semantic_prefix().unwrap().snapshot(),
            context_frontier_id(413)
        );
    }

    fn default_origin_delivery() -> DeliveryRequest {
        DeliveryRequest::StartWhenNoActiveTurn {
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        }
    }

    /// One accepted turn origin whose sole identity/order knob is its
    /// acceptance ordinal. Turn and accepted-input identities descend as the
    /// ordinal ascends, so identity order cannot accidentally stand in for
    /// durable acceptance order (`docs/agents/testing-style.md`, rule 4).
    #[derive(Clone, Copy)]
    struct OriginFixture {
        acceptance: u64,
    }

    fn accepted_origin(acceptance: u64) -> OriginFixture {
        OriginFixture { acceptance }
    }

    impl OriginFixture {
        fn turn(self) -> TurnId {
            turn_id(u128::from(u64::MAX - self.acceptance))
        }

        fn accepted_input(self) -> AcceptedInputId {
            accepted_input_id(u128::from(u64::MAX / 2 - self.acceptance))
        }

        fn position(self) -> SessionInputPosition {
            SessionInputPosition::try_from_u64(self.acceptance)
                .expect("test acceptance ordinals are positive")
        }

        fn ordinary_order(self) -> AcceptedInputQueueOrder {
            AcceptedInputQueueOrder::ordinary(self.position())
        }

        fn record(
            self,
            session: &Session,
            state: AcceptedInputTurnSchedulingRecordState,
        ) -> AcceptedInputTurnSchedulingRecord {
            self.record_with(
                session,
                OriginRecordFacts {
                    order: self.ordinary_order(),
                    delivery: default_origin_delivery(),
                    state,
                },
            )
        }

        fn record_with(
            self,
            session: &Session,
            facts: OriginRecordFacts,
        ) -> AcceptedInputTurnSchedulingRecord {
            let turn = self.turn();
            AcceptedInputTurnSchedulingRecord::new(
                session.id(),
                turn,
                session.id(),
                AcceptedInputLifecycle::new(
                    self.accepted_input(),
                    AcceptedInputDisposition::OriginOf(turn),
                ),
                session.id(),
                turn,
                facts.order,
                facts.delivery,
                configuration(session),
                facts.state,
            )
        }

        fn entry(
            self,
            session: &Session,
            entry: SemanticEntryFixture,
        ) -> SemanticTranscriptEntryReconstitutionInput {
            SemanticTranscriptEntryReconstitutionInput::new(
                entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                    accepted_input: self.accepted_input(),
                },
            )
        }

        fn active_tail(self, session: &Session) -> SessionAcceptanceTailReconstitutionInput {
            SessionAcceptanceTailReconstitutionInput::new(
                session.id(),
                self.accepted_input(),
                self.position(),
                vec![SessionAcceptanceTailEntryReconstitutionInput::new(
                    session.id(),
                    AcceptedInputLifecycle::new(
                        self.accepted_input(),
                        AcceptedInputDisposition::OriginOf(self.turn()),
                    ),
                    self.position(),
                    default_origin_delivery(),
                )],
            )
        }
    }

    struct OriginRecordFacts {
        order: AcceptedInputQueueOrder,
        delivery: DeliveryRequest,
        state: AcceptedInputTurnSchedulingRecordState,
    }

    #[derive(Clone, Copy)]
    struct SemanticEntryFixture {
        seed: u128,
    }

    fn semantic_entry(seed: u128) -> SemanticEntryFixture {
        SemanticEntryFixture { seed }
    }

    fn user_denial(request: ToolRequestId) -> ToolApprovalResolution {
        ToolApprovalResolutionReconstitutionInput::user_fixture(
            request,
            ToolApprovalDecision::Deny { reason: None },
        )
        .reconstitute()
        .expect("the user denial fixture is valid")
    }

    impl SemanticEntryFixture {
        fn id(self) -> SemanticTranscriptEntryId {
            semantic_transcript_entry_id(self.seed)
        }

        fn reference(self, session: &Session) -> SemanticTranscriptEntryRef {
            SemanticTranscriptEntryRef::from_source(session.id(), self.id())
        }

        fn failed_turn(
            self,
            session: &Session,
            turn: OriginFixture,
        ) -> SemanticTranscriptEntryReconstitutionInput {
            SemanticTranscriptEntryReconstitutionInput::new(
                self.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::TurnFailed { turn: turn.turn() },
            )
        }
    }

    #[derive(Clone, Copy)]
    struct FrontierFixture {
        seed: u128,
    }

    fn frontier(seed: u128) -> FrontierFixture {
        FrontierFixture { seed }
    }

    impl FrontierFixture {
        fn id(self) -> ContextFrontierId {
            context_frontier_id(self.seed)
        }

        fn snapshot(
            self,
            session: &Session,
            entries: &[SemanticEntryFixture],
        ) -> ResolvedContextFrontierReconstitutionInput {
            ResolvedContextFrontierReconstitutionInput::new(
                session.id(),
                self.id(),
                entries
                    .iter()
                    .map(|entry| entry.reference(session))
                    .collect(),
            )
        }
    }

    #[derive(Clone, Copy)]
    struct ActivationFixture {
        seed: u128,
    }

    fn activation(seed: u128) -> ActivationFixture {
        ActivationFixture { seed }
    }

    fn matching_active_attempt() -> TurnAttemptId {
        turn_attempt_id(50)
    }

    impl ActivationFixture {
        fn model_identity_entry(self) -> SemanticEntryFixture {
            semantic_entry(50 + self.seed)
        }

        fn origin_entry(self) -> SemanticEntryFixture {
            semantic_entry(100 + self.seed)
        }

        fn starting_frontier(self) -> FrontierFixture {
            frontier(200 + self.seed)
        }

        fn initial_attempt(self) -> TurnAttemptId {
            turn_attempt_id(300 + self.seed)
        }

        fn identities(self) -> AcceptedInputTurnActivationIdentities {
            AcceptedInputTurnActivationIdentities::new(
                self.model_identity_entry().id(),
                self.origin_entry().id(),
                self.starting_frontier().id(),
                self.initial_attempt(),
            )
        }

        fn identities_with_attempt(
            self,
            initial_attempt: TurnAttemptId,
        ) -> AcceptedInputTurnActivationIdentities {
            AcceptedInputTurnActivationIdentities::new(
                self.model_identity_entry().id(),
                self.origin_entry().id(),
                self.starting_frontier().id(),
                initial_attempt,
            )
        }

        fn identities_with_origin_entry(
            self,
            origin_entry: SemanticTranscriptEntryId,
        ) -> AcceptedInputTurnActivationIdentities {
            AcceptedInputTurnActivationIdentities::new(
                self.model_identity_entry().id(),
                origin_entry,
                self.starting_frontier().id(),
                self.initial_attempt(),
            )
        }

        fn identities_with_starting_frontier(
            self,
            starting_frontier: ContextFrontierId,
        ) -> AcceptedInputTurnActivationIdentities {
            AcceptedInputTurnActivationIdentities::new(
                self.model_identity_entry().id(),
                self.origin_entry().id(),
                starting_frontier,
                self.initial_attempt(),
            )
        }
    }

    #[derive(Clone)]
    struct ActiveReconstitutionFacts {
        session: Session,
        turns: Vec<AcceptedInputTurnSchedulingRecord>,
        semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
        snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
        acceptance_tail: Option<SessionAcceptanceTailReconstitutionInput>,
    }

    impl ActiveReconstitutionFacts {
        /// The origin-entry fixture the matching baseline stores for its
        /// active turn.
        fn matching_origin_entry() -> SemanticEntryFixture {
            semantic_entry(30)
        }

        /// The starting-snapshot fixture the matching baseline stores for
        /// its active turn.
        fn matching_starting_frontier() -> FrontierFixture {
            frontier(40)
        }

        fn matching(session: &Session, active: OriginFixture) -> Self {
            let origin_entry = Self::matching_origin_entry();
            let starting_frontier = Self::matching_starting_frontier();
            Self {
                session: session.clone(),
                turns: vec![active.record(
                    session,
                    AcceptedInputTurnSchedulingRecordState::Active {
                        starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                        starting_frontier: starting_frontier.id(),
                        phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                            active.turn(),
                            matching_active_attempt(),
                        ),
                    },
                )],
                semantic_entries: vec![active.entry(session, origin_entry)],
                snapshots: vec![starting_frontier.snapshot(session, &[origin_entry])],
                acceptance_tail: Some(active.active_tail(session)),
            }
        }

        /// Replaces only the behavior-relevant stored active phase while
        /// retaining every matching identity, lineage, frontier, origin,
        /// configuration, and acceptance-tail fact.
        fn replace_active_phase(&mut self, replacement: ActiveTurnSchedulingReconstitutionInput) {
            let AcceptedInputTurnSchedulingRecordState::Active { phase, .. } =
                &mut self.turns[0].state
            else {
                panic!("matching active facts retain an active scheduling record");
            };
            *phase = replacement;
        }

        /// Replaces only the stored starting lineage while retaining every
        /// other matching fact.
        fn replace_starting_lineage(&mut self, replacement: AcceptedInputStartingLineage) {
            let AcceptedInputTurnSchedulingRecordState::Active {
                starting_lineage, ..
            } = &mut self.turns[0].state
            else {
                panic!("matching active facts retain an active scheduling record");
            };
            *starting_lineage = replacement;
        }

        /// Replaces only the stored starting-snapshot identity while
        /// retaining every other matching fact.
        fn replace_starting_frontier(&mut self, replacement: ContextFrontierId) {
            let AcceptedInputTurnSchedulingRecordState::Active {
                starting_frontier, ..
            } = &mut self.turns[0].state
            else {
                panic!("matching active facts retain an active scheduling record");
            };
            *starting_frontier = replacement;
        }

        fn input(self) -> AcceptedInputSchedulingReconstitutionInput {
            AcceptedInputSchedulingReconstitutionInput::new(
                self.session,
                self.turns,
                self.semantic_entries,
                self.snapshots,
                self.acceptance_tail,
            )
        }
    }

    #[derive(Clone)]
    struct ConsumedSteeringReconstitutionFacts {
        session: Session,
        turns: Vec<AcceptedInputTurnSchedulingRecord>,
        semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
        snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
        acceptance_tail: SessionAcceptanceTailReconstitutionInput,
        pinned_targets: Vec<crate::PinnedProviderTargetReconstitutionInput>,
        model_calls: Vec<ModelCallReconstitutionInput>,
        consumed_steering: Vec<ConsumedSteeringReconstitutionInput>,
        steering_continuation_rounds: Vec<SteeringContinuationRoundReconstitutionInput>,
    }

    impl ConsumedSteeringReconstitutionFacts {
        fn matching(session: &Session, active: OriginFixture, consumed: OriginFixture) -> Self {
            let origin_entry = ActiveReconstitutionFacts::matching_origin_entry();
            let steering_entry = semantic_entry(31);
            let starting_frontier = ActiveReconstitutionFacts::matching_starting_frontier();
            let call_frontier = frontier(41);
            let call_id = model_call_id(91);
            let target = ResolvedProviderTarget::naming(provider_model_identity(51));
            let consumed_lifecycle = AcceptedInputLifecycle::new(
                consumed.accepted_input(),
                AcceptedInputDisposition::ConsumedAsSteering { call: call_id },
            );
            let mut acceptance_tail = active.active_tail(session);
            acceptance_tail.observed_last_position = consumed.position();
            acceptance_tail
                .entries
                .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                    session.id(),
                    consumed_lifecycle.clone(),
                    consumed.position(),
                    DeliveryRequest::NextSafePoint {
                        expected_active_turn: active.turn(),
                    },
                ));
            Self {
                session: session.clone(),
                turns: vec![active.record(
                    session,
                    AcceptedInputTurnSchedulingRecordState::Active {
                        starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                        starting_frontier: starting_frontier.id(),
                        phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                            active.turn(),
                            matching_active_attempt(),
                        ),
                    },
                )],
                semantic_entries: vec![
                    active.entry(session, origin_entry),
                    SemanticTranscriptEntryReconstitutionInput::new(
                        steering_entry.id(),
                        session.id(),
                        InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                            accepted_input: consumed.accepted_input(),
                            source_turn: active.turn(),
                        },
                    ),
                ],
                snapshots: vec![
                    starting_frontier.snapshot(session, &[origin_entry]),
                    call_frontier.snapshot(session, &[origin_entry, steering_entry]),
                ],
                acceptance_tail,
                pinned_targets: vec![crate::PinnedProviderTargetReconstitutionInput::new(
                    active.turn(),
                    target,
                )],
                model_calls: vec![ModelCallReconstitutionInput::new(
                    call_id,
                    active.turn(),
                    matching_active_attempt(),
                    FrozenModelSelection::Direct(direct(1)),
                    target,
                    call_frontier.id(),
                    ModelCallReconstitutionState::Prepared,
                )],
                consumed_steering: vec![ConsumedSteeringReconstitutionInput::new(
                    session.id(),
                    consumed_lifecycle,
                    consumed.position(),
                    active.turn(),
                )],
                steering_continuation_rounds: Vec::new(),
            }
        }

        /// Matching stored facts for one steering input consumed at a
        /// tool-round continuation boundary: the completed producing call's
        /// proposal, its executed result, and the consumed steering entry fill
        /// the prepared continuation call's frontier exactly, and the round's
        /// result evidence backs that window.
        fn matching_at_continuation(
            session: &Session,
            active: OriginFixture,
            consumed: OriginFixture,
        ) -> Self {
            let origin_entry = ActiveReconstitutionFacts::matching_origin_entry();
            let steering_entry = semantic_entry(31);
            let tool_use_entry = semantic_entry(34);
            let result_entry = semantic_entry(35);
            let starting_frontier = ActiveReconstitutionFacts::matching_starting_frontier();
            let call_frontier = frontier(41);
            let producing_call = Self::matching_continuation_producing_call();
            let producing_attempt = turn_attempt_id(49);
            let call_id = Self::matching_continuation_call();
            let request = Self::matching_continuation_request();
            let target = ResolvedProviderTarget::naming(provider_model_identity(51));
            let mut facts = Self::matching(session, active, consumed);
            facts.turns[0].state = AcceptedInputTurnSchedulingRecordState::Active {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                phase: ActiveTurnSchedulingReconstitutionInput::running(
                    active.turn(),
                    matching_active_attempt(),
                ),
            };
            facts.semantic_entries.extend([
                SemanticTranscriptEntryReconstitutionInput::new(
                    tool_use_entry.id(),
                    session.id(),
                    InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                        producing_call,
                        request,
                    },
                ),
                SemanticTranscriptEntryReconstitutionInput::new(
                    result_entry.id(),
                    session.id(),
                    InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                        attempt: Self::matching_continuation_tool_attempt(),
                    },
                ),
            ]);
            facts.snapshots[1] = call_frontier.snapshot(
                session,
                &[origin_entry, tool_use_entry, result_entry, steering_entry],
            );
            facts.model_calls = vec![
                ModelCallReconstitutionInput::new(
                    producing_call,
                    active.turn(),
                    producing_attempt,
                    FrozenModelSelection::Direct(direct(1)),
                    target,
                    starting_frontier.id(),
                    ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
                ),
                ModelCallReconstitutionInput::new(
                    call_id,
                    active.turn(),
                    matching_active_attempt(),
                    FrozenModelSelection::Direct(direct(1)),
                    target,
                    call_frontier.id(),
                    ModelCallReconstitutionState::Prepared,
                ),
            ];
            facts.steering_continuation_rounds =
                vec![SteeringContinuationRoundReconstitutionInput::new(
                    call_id,
                    vec![Self::matching_continuation_round_attempt(session, active)],
                    Vec::new(),
                )];
            facts
        }

        /// The completed producing call the continuation baseline stores.
        fn matching_continuation_producing_call() -> crate::ModelCallId {
            model_call_id(90)
        }

        /// The steering-consuming continuation call the baseline stores.
        fn matching_continuation_call() -> crate::ModelCallId {
            model_call_id(91)
        }

        /// The single proposed request the continuation baseline stores.
        fn matching_continuation_request() -> ToolRequestId {
            tool_request_id(92)
        }

        /// The executed tool attempt the continuation baseline stores.
        fn matching_continuation_tool_attempt() -> crate::ToolAttemptId {
            tool_attempt_id(93)
        }

        /// The ended tool attempt backing the baseline's result window.
        fn matching_continuation_round_attempt(
            session: &Session,
            active: OriginFixture,
        ) -> crate::EndedToolAttempt {
            ended_tool_attempt(
                session,
                active,
                matching_active_attempt(),
                Self::matching_continuation_tool_attempt(),
                Self::matching_continuation_request(),
            )
        }

        fn input(self) -> AcceptedInputSchedulingReconstitutionInput {
            AcceptedInputSchedulingReconstitutionInput::new(
                self.session,
                self.turns,
                self.semantic_entries,
                self.snapshots,
                Some(self.acceptance_tail),
            )
            .with_model_call_facts(self.pinned_targets, self.model_calls)
            .with_consumed_steering_facts(self.consumed_steering)
            .with_steering_continuation_rounds(self.steering_continuation_rounds)
        }
    }

    /// One ended, completed tool attempt correlated to the given request for
    /// continuation-round evidence.
    fn ended_tool_attempt(
        session: &Session,
        turn: OriginFixture,
        issuing_attempt: TurnAttemptId,
        attempt: crate::ToolAttemptId,
        request: ToolRequestId,
    ) -> crate::EndedToolAttempt {
        ended_tool_attempt_with_end(
            session,
            turn,
            issuing_attempt,
            attempt,
            request,
            ToolAttemptEnd::Completed {
                result: ToolResultContent::Text(
                    ToolResultText::try_new(String::from("ok"))
                        .expect("fixture tool result is valid"),
                ),
            },
        )
    }

    fn ended_tool_attempt_with_end(
        session: &Session,
        turn: OriginFixture,
        issuing_attempt: TurnAttemptId,
        attempt: crate::ToolAttemptId,
        request: ToolRequestId,
        end: ToolAttemptEnd,
    ) -> crate::EndedToolAttempt {
        let reconstituted = ToolAttemptReconstitutionInput::new(
            attempt,
            request,
            session.id(),
            turn.turn(),
            issuing_attempt,
            ToolEffectClass::EffectFree,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Ended(end),
        )
        .reconstitute()
        .expect("fixture tool attempt is supported");
        let crate::ReconstitutedToolAttempt::Ended(ended) = reconstituted else {
            panic!("fixture tool attempt is terminal");
        };
        ended
    }

    fn active_input(
        session: &Session,
        active: OriginFixture,
        acceptance_tail: Option<SessionAcceptanceTailReconstitutionInput>,
    ) -> AcceptedInputSchedulingReconstitutionInput {
        ActiveReconstitutionFacts {
            acceptance_tail,
            ..ActiveReconstitutionFacts::matching(session, active)
        }
        .input()
    }

    /// One-record queued scheduling input: a queued turn stores no semantic
    /// entries, snapshots, or acceptance tail, so those collections are
    /// canonically empty here.
    fn queued_input(
        session: &Session,
        queued: OriginFixture,
    ) -> AcceptedInputSchedulingReconstitutionInput {
        AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![queued.record(session, AcceptedInputTurnSchedulingRecordState::Queued)],
            Vec::new(),
            Vec::new(),
            None,
        )
    }

    /// Matching stored facts for one first-in-session failed-terminal turn:
    /// its origin entry, failed marker, starting snapshot, and terminal
    /// snapshot agree with each other, so each perturbation changes exactly
    /// one stored fact.
    #[derive(Clone)]
    struct FailedTerminalReconstitutionFacts {
        session: Session,
        turns: Vec<AcceptedInputTurnSchedulingRecord>,
        semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
        snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
        acceptance_tail: Option<SessionAcceptanceTailReconstitutionInput>,
    }

    impl FailedTerminalReconstitutionFacts {
        /// The origin-entry fixture the matching baseline stores for its
        /// failed turn.
        fn matching_origin_entry() -> SemanticEntryFixture {
            semantic_entry(30)
        }

        /// The failed-marker fixture the matching baseline stores for its
        /// failed turn.
        fn matching_failure_entry() -> SemanticEntryFixture {
            semantic_entry(31)
        }

        /// The starting-snapshot fixture the matching baseline stores for
        /// its failed turn.
        fn matching_starting_frontier() -> FrontierFixture {
            frontier(40)
        }

        /// The terminal-snapshot fixture the matching baseline stores for
        /// its failed turn.
        fn matching_terminal_frontier() -> FrontierFixture {
            frontier(41)
        }

        fn matching(session: &Session, failed: OriginFixture) -> Self {
            let origin_entry = Self::matching_origin_entry();
            let failure_entry = Self::matching_failure_entry();
            let starting_frontier = Self::matching_starting_frontier();
            let terminal_frontier = Self::matching_terminal_frontier();
            Self {
                session: session.clone(),
                turns: vec![failed.record(
                    session,
                    AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                        starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                        starting_frontier: starting_frontier.id(),
                        terminal_execution: None,
                        terminal_frontier: terminal_frontier.id(),
                    },
                )],
                semantic_entries: vec![
                    failed.entry(session, origin_entry),
                    failure_entry.failed_turn(session, failed),
                ],
                snapshots: vec![
                    starting_frontier.snapshot(session, &[origin_entry]),
                    terminal_frontier.snapshot(session, &[origin_entry, failure_entry]),
                ],
                acceptance_tail: None,
            }
        }

        /// Replaces only the stored terminal-snapshot identity while
        /// retaining every other matching fact.
        fn replace_terminal_frontier(&mut self, replacement: ContextFrontierId) {
            let AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                terminal_frontier, ..
            } = &mut self.turns[0].state
            else {
                panic!("matching failed-terminal facts retain a terminal scheduling record");
            };
            *terminal_frontier = replacement;
        }

        /// Replaces only the stored terminal execution provenance while
        /// retaining every semantic and frontier fact.
        fn replace_terminal_execution(
            &mut self,
            replacement: Option<FailedTurnExecutionReconstitutionInput>,
        ) {
            let AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                terminal_execution, ..
            } = &mut self.turns[0].state
            else {
                panic!("matching failed-terminal facts retain a terminal scheduling record");
            };
            *terminal_execution = replacement;
        }

        fn input(self) -> AcceptedInputSchedulingReconstitutionInput {
            AcceptedInputSchedulingReconstitutionInput::new(
                self.session,
                self.turns,
                self.semantic_entries,
                self.snapshots,
                self.acceptance_tail,
            )
        }
    }

    #[derive(Clone, Copy)]
    struct PostAnchorOrigins {
        active: OriginFixture,
        queued: OriginFixture,
    }

    fn active_input_with_post_anchor_origin(
        session: &Session,
        origins: PostAnchorOrigins,
        delivery: DeliveryRequest,
    ) -> AcceptedInputSchedulingReconstitutionInput {
        let mut facts = ActiveReconstitutionFacts::matching(session, origins.active);
        let tail = facts
            .acceptance_tail
            .as_mut()
            .expect("matching active facts include the acceptance tail");
        tail.observed_last_position = origins.queued.position();
        tail.entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    origins.queued.accepted_input(),
                    AcceptedInputDisposition::OriginOf(origins.queued.turn()),
                ),
                origins.queued.position(),
                delivery,
            ));
        facts.turns.push(origins.queued.record_with(
            session,
            OriginRecordFacts {
                order: origins.queued.ordinary_order(),
                delivery,
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        ));
        facts.input()
    }

    #[derive(Clone, Copy)]
    struct FailedPredecessorPostAnchorOrigins {
        predecessor: OriginFixture,
        active: OriginFixture,
        queued: OriginFixture,
    }

    fn active_input_after_failed_predecessor_with_post_anchor_origin(
        session: &Session,
        origins: FailedPredecessorPostAnchorOrigins,
        delivery: DeliveryRequest,
    ) -> AcceptedInputSchedulingReconstitutionInput {
        let predecessor_origin_entry = semantic_entry(29);
        let predecessor_failure_entry = semantic_entry(30);
        let active_origin_entry = semantic_entry(31);
        let predecessor_starting_frontier = frontier(39);
        let predecessor_terminal_frontier = frontier(40);
        let active_starting_frontier = frontier(41);
        let predecessor_record = origins.predecessor.record(
            session,
            AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: predecessor_starting_frontier.id(),
                terminal_execution: None,
                terminal_frontier: predecessor_terminal_frontier.id(),
            },
        );
        let active_delivery = DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: origins.predecessor.turn(),
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        };
        let active_record = origins.active.record_with(
            session,
            OriginRecordFacts {
                order: origins.active.ordinary_order(),
                delivery: active_delivery,
                state: AcceptedInputTurnSchedulingRecordState::Active {
                    starting_lineage: AcceptedInputStartingLineage::After {
                        immediate_predecessor: origins.predecessor.turn(),
                    },
                    starting_frontier: active_starting_frontier.id(),
                    phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                        origins.active.turn(),
                        turn_attempt_id(50),
                    ),
                },
            },
        );
        let queued_record = origins.queued.record_with(
            session,
            OriginRecordFacts {
                order: origins.queued.ordinary_order(),
                delivery,
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        );
        let tail = SessionAcceptanceTailReconstitutionInput::new(
            session.id(),
            origins.active.accepted_input(),
            origins.queued.position(),
            vec![
                SessionAcceptanceTailEntryReconstitutionInput::new(
                    session.id(),
                    AcceptedInputLifecycle::new(
                        origins.active.accepted_input(),
                        AcceptedInputDisposition::OriginOf(origins.active.turn()),
                    ),
                    origins.active.position(),
                    active_delivery,
                ),
                SessionAcceptanceTailEntryReconstitutionInput::new(
                    session.id(),
                    AcceptedInputLifecycle::new(
                        origins.queued.accepted_input(),
                        AcceptedInputDisposition::OriginOf(origins.queued.turn()),
                    ),
                    origins.queued.position(),
                    delivery,
                ),
            ],
        );
        AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![predecessor_record, active_record, queued_record],
            vec![
                origins.predecessor.entry(session, predecessor_origin_entry),
                predecessor_failure_entry.failed_turn(session, origins.predecessor),
                origins.active.entry(session, active_origin_entry),
            ],
            vec![
                predecessor_starting_frontier.snapshot(session, &[predecessor_origin_entry]),
                predecessor_terminal_frontier.snapshot(
                    session,
                    &[predecessor_origin_entry, predecessor_failure_entry],
                ),
                active_starting_frontier.snapshot(
                    session,
                    &[
                        predecessor_origin_entry,
                        predecessor_failure_entry,
                        active_origin_entry,
                    ],
                ),
            ],
            Some(tail),
        )
    }

    fn active_input_after_historical_interrupt(
        session: &Session,
        predecessor: OriginFixture,
        active: OriginFixture,
        interrupt_successor: OriginFixture,
    ) -> AcceptedInputSchedulingReconstitutionInput {
        let predecessor_origin_entry = semantic_entry(20);
        let predecessor_failure_entry = semantic_entry(21);
        let interrupt_origin_entry = semantic_entry(22);
        let interrupt_failure_entry = semantic_entry(23);
        let active_origin_entry = semantic_entry(24);
        let predecessor_starting_frontier = frontier(30);
        let predecessor_terminal_frontier = frontier(31);
        let interrupt_starting_frontier = frontier(32);
        let interrupt_terminal_frontier = frontier(33);
        let active_starting_frontier = frontier(34);
        let interrupt_order = AcceptedInputQueueOrder::interrupt_immediately_after(
            interrupt_successor.position(),
            predecessor.turn(),
        );
        let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
            command_id(40),
            session.id(),
            predecessor.turn(),
            interrupt_successor.accepted_input(),
            interrupt_successor.turn(),
            interrupt_order,
        )
        .expect("the historical interrupt is exactly correlated");
        let active_delivery = DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: predecessor.turn(),
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        };
        let interrupt_delivery = DeliveryRequest::Interrupt {
            expected_active_turn: predecessor.turn(),
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        };

        AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![
                predecessor.record(
                    session,
                    AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                        starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                        starting_frontier: predecessor_starting_frontier.id(),
                        terminal_execution: Some(
                            FailedTurnExecutionReconstitutionInput::attempt_only_after_cancellation(
                                predecessor.turn(),
                                turn_attempt_id(40),
                                CancellationStopDisposition::KnownFailure,
                                interrupt,
                            ),
                        ),
                        terminal_frontier: predecessor_terminal_frontier.id(),
                    },
                ),
                active.record_with(
                    session,
                    OriginRecordFacts {
                        order: active.ordinary_order(),
                        delivery: active_delivery,
                        state: AcceptedInputTurnSchedulingRecordState::Active {
                            starting_lineage: AcceptedInputStartingLineage::After {
                                immediate_predecessor: interrupt_successor.turn(),
                            },
                            starting_frontier: active_starting_frontier.id(),
                            phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                                active.turn(),
                                turn_attempt_id(41),
                            ),
                        },
                    },
                ),
                interrupt_successor.record_with(
                    session,
                    OriginRecordFacts {
                        order: interrupt_order,
                        delivery: interrupt_delivery,
                        state: AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                            starting_lineage: AcceptedInputStartingLineage::After {
                                immediate_predecessor: predecessor.turn(),
                            },
                            starting_frontier: interrupt_starting_frontier.id(),
                            terminal_execution: None,
                            terminal_frontier: interrupt_terminal_frontier.id(),
                        },
                    },
                ),
            ],
            vec![
                predecessor.entry(session, predecessor_origin_entry),
                predecessor_failure_entry.failed_turn(session, predecessor),
                interrupt_successor.entry(session, interrupt_origin_entry),
                interrupt_failure_entry.failed_turn(session, interrupt_successor),
                active.entry(session, active_origin_entry),
            ],
            vec![
                predecessor_starting_frontier.snapshot(session, &[predecessor_origin_entry]),
                predecessor_terminal_frontier.snapshot(
                    session,
                    &[predecessor_origin_entry, predecessor_failure_entry],
                ),
                interrupt_starting_frontier.snapshot(
                    session,
                    &[
                        predecessor_origin_entry,
                        predecessor_failure_entry,
                        interrupt_origin_entry,
                    ],
                ),
                interrupt_terminal_frontier.snapshot(
                    session,
                    &[
                        predecessor_origin_entry,
                        predecessor_failure_entry,
                        interrupt_origin_entry,
                        interrupt_failure_entry,
                    ],
                ),
                active_starting_frontier.snapshot(
                    session,
                    &[
                        predecessor_origin_entry,
                        predecessor_failure_entry,
                        interrupt_origin_entry,
                        interrupt_failure_entry,
                        active_origin_entry,
                    ],
                ),
            ],
            Some(SessionAcceptanceTailReconstitutionInput::new(
                session.id(),
                active.accepted_input(),
                interrupt_successor.position(),
                vec![
                    SessionAcceptanceTailEntryReconstitutionInput::new(
                        session.id(),
                        AcceptedInputLifecycle::new(
                            active.accepted_input(),
                            AcceptedInputDisposition::OriginOf(active.turn()),
                        ),
                        active.position(),
                        active_delivery,
                    ),
                    SessionAcceptanceTailEntryReconstitutionInput::new(
                        session.id(),
                        AcceptedInputLifecycle::new(
                            interrupt_successor.accepted_input(),
                            AcceptedInputDisposition::OriginOf(interrupt_successor.turn()),
                        ),
                        interrupt_successor.position(),
                        interrupt_delivery,
                    ),
                ],
            )),
        )
    }

    #[derive(Debug)]
    #[allow(
        dead_code,
        reason = "the table renderer reads every field through the Debug derive"
    )]
    struct ReconstitutionFailureRow {
        perturbed_stored_fact: &'static str,
        failure: String,
    }

    /// Asserts one perturbed complete input rejects while retaining every
    /// supplied fact unchanged, then returns its precise failure.
    #[track_caller]
    fn assert_input_rejects_unchanged(
        input: AcceptedInputSchedulingReconstitutionInput,
    ) -> AcceptedInputSchedulingReconstitutionFailure {
        let error = input
            .clone()
            .reconstitute()
            .expect_err("perturbed scheduling facts must fail closed");
        let failure = error.failure().clone();
        assert_eq!(error.input(), &input);
        let (returned, returned_failure) = error.into_parts();
        assert_eq!(returned, input);
        assert_eq!(returned_failure, failure);
        failure
    }

    /// Asserts one named perturbation rejects while retaining the complete
    /// unchanged input, then returns its precise failure.
    #[track_caller]
    fn assert_reconstitution_rejects_unchanged(
        facts: ActiveReconstitutionFacts,
    ) -> AcceptedInputSchedulingReconstitutionFailure {
        assert_input_rejects_unchanged(facts.input())
    }

    /// Asserts eligibility preparation rejects while retaining the complete
    /// projection and supplied identities unchanged, then returns the exact
    /// failure.
    #[track_caller]
    fn assert_eligibility_rejects_unchanged(
        projection: AcceptedInputSchedulingProjection,
        identities: AcceptedInputTurnActivationIdentities,
    ) -> AcceptedInputEligibilityFailure {
        let error = projection
            .clone()
            .prepare_earliest_queued_activation(identities)
            .expect_err("ineligible or colliding activation facts must fail closed");
        let failure = error.failure();
        assert_eq!(error.projection(), &projection);
        assert_eq!(error.identities(), identities);
        let (returned_projection, returned_identities, returned_failure) = error.into_parts();
        assert_eq!(returned_projection, projection);
        assert_eq!(returned_identities, identities);
        assert_eq!(returned_failure, failure);
        failure
    }

    /// S01 / INV-009 / INV-015: ancestry-free first eligibility fixes the
    /// origin-only frontier and enters Running with one Prepared attempt in
    /// the same sealed candidate.
    #[test]
    fn s01_first_eligibility_prepares_one_atomic_activation_candidate() {
        let session = current_session();
        let queued = accepted_origin(1);
        let activation = activation(1);
        let no_semantic_entries = Vec::new();
        let no_snapshots = Vec::new();
        let no_active_acceptance_tail = None;
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![queued.record(&session, AcceptedInputTurnSchedulingRecordState::Queued)],
            no_semantic_entries,
            no_snapshots,
            no_active_acceptance_tail,
        );

        let candidate = input
            .reconstitute()
            .expect("a complete queued projection is valid")
            .prepare_earliest_queued_activation(activation.identities())
            .expect("the sole queued turn is eligible with no active slot");

        assert_eq!(candidate.turn().turn(), queued.turn());
        assert_eq!(
            candidate.turn().accepted_input().id(),
            queued.accepted_input()
        );
        assert_eq!(
            candidate.origin_entry().payload(),
            &InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: queued.accepted_input(),
            }
        );
        assert_eq!(
            candidate.start().lineage(),
            AcceptedInputStartingLineage::FirstInSession
        );
        assert_eq!(
            candidate
                .starting_snapshot()
                .ordered_entries()
                .collect::<Vec<_>>(),
            vec![activation.origin_entry().reference(&session)]
        );
        assert!(matches!(
            candidate.turn().phase(),
            ActiveTurnPhase::Running { current_attempt }
                if current_attempt.id() == activation.initial_attempt()
                    && current_attempt.state() == &crate::CurrentTurnAttemptState::Prepared
        ));
    }

    /// S28 / INV-015 / INV-039: an imported session's first native activation
    /// appends its origin to the exact checked seed prefix without changing
    /// first-in-session lineage.
    #[test]
    fn s28_inv015_inv039_first_native_frontier_appends_to_imported_seed() {
        let imported = imported_session();
        let session = imported.session().clone();
        let seed_entries = imported
            .seed_snapshot()
            .ordered_entries()
            .collect::<Vec<_>>();
        let queued = accepted_origin(1);
        let activation = activation(1);

        let candidate = queued_input(&session, queued)
            .with_imported_session(imported)
            .reconstitute()
            .expect("the exact imported seed admits queued native work")
            .prepare_earliest_queued_activation(activation.identities())
            .expect("the first native turn appends to the imported seed");

        let mut expected = seed_entries;
        expected.push(activation.origin_entry().reference(&session));
        assert_eq!(
            candidate
                .starting_snapshot()
                .ordered_entries()
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            candidate.start().lineage(),
            AcceptedInputStartingLineage::FirstInSession
        );
    }

    /// S03 / INV-009: restart returns a queued scheduling projection with no
    /// manufactured start, and a cross-wired OriginOf fact fails closed.
    #[test]
    fn s03_checked_reconstitution_preserves_queued_state_and_exact_origin() {
        let session = current_session();
        let origin = accepted_origin(1);
        let queued = origin.record(&session, AcceptedInputTurnSchedulingRecordState::Queued);
        let no_semantic_entries = Vec::new();
        let no_snapshots = Vec::new();
        let no_active_acceptance_tail = None;
        let projection = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![queued.clone()],
            no_semantic_entries,
            no_snapshots,
            no_active_acceptance_tail,
        )
        .reconstitute()
        .expect("the complete queued record is valid");
        let reconstituted = projection
            .turn(origin.turn())
            .expect("the stored queued turn remains present");
        assert_eq!(
            reconstituted.status(),
            AcceptedInputTurnSchedulingStatus::Queued
        );
        assert_eq!(reconstituted.start(), None);

        let wrong_turn = turn_id(99);
        let cross_wired = AcceptedInputTurnSchedulingRecord::new(
            queued.stored_session(),
            queued.turn(),
            queued.accepted_input_session(),
            AcceptedInputLifecycle::new(
                queued.accepted_input().id(),
                AcceptedInputDisposition::OriginOf(wrong_turn),
            ),
            queued.queue_session(),
            queued.queue_turn(),
            queued.order(),
            queued.origin_delivery(),
            queued.origin_configuration().clone(),
            queued.state().clone(),
        );
        let no_semantic_entries = Vec::new();
        let no_snapshots = Vec::new();
        let no_active_acceptance_tail = None;
        let error = AcceptedInputSchedulingReconstitutionInput::new(
            session,
            vec![cross_wired],
            no_semantic_entries,
            no_snapshots,
            no_active_acceptance_tail,
        )
        .reconstitute()
        .expect_err("the exact OriginOf(turn) correlation is required");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::AcceptedInputOriginMismatch {
                turn: origin.turn(),
            }
        );
    }

    /// S03 / INV-009: an admitted active restart record owns its exact
    /// Prepared attempt, reconstructs Running, and makes that identity
    /// unavailable to a second activation candidate.
    #[test]
    fn s03_active_reconstitution_requires_and_exposes_exact_prepared_attempt() {
        let session = current_session();
        let active_origin = accepted_origin(1);
        let stored_attempt = matching_active_attempt();
        let facts = ActiveReconstitutionFacts::matching(&session, active_origin);
        let projection = facts
            .input()
            .reconstitute()
            .expect("the active turn has its exact prepared attempt");
        let active = projection
            .active_turn()
            .expect("the reconstructed turn owns the active slot");
        assert!(matches!(
            active.active_phase(),
            Some(ActiveTurnPhase::Running { current_attempt })
                if current_attempt.id() == stored_attempt
                    && current_attempt.state() == &CurrentTurnAttemptState::Prepared
        ));

        let colliding_activation = activation(1);
        let collision = projection
            .clone()
            .prepare_earliest_queued_activation(
                colliding_activation.identities_with_attempt(stored_attempt),
            )
            .expect_err("a current attempt identity cannot be proposed again");
        assert_eq!(
            collision.failure(),
            AcceptedInputEligibilityFailure::InitialAttemptIdentityAlreadyExists
        );
        let occupied_activation = activation(2);
        let occupied = projection
            .prepare_earliest_queued_activation(occupied_activation.identities())
            .expect_err("an active slot blocks every queued activation");
        assert_eq!(
            occupied.failure(),
            AcceptedInputEligibilityFailure::ActiveTurnPresent {
                turn: active_origin.turn(),
            }
        );
    }

    /// S03 / INV-009: inert prepared facts become a canonical attempt only
    /// inside the validated owner projection.
    #[test]
    fn active_reconstitution_derives_prepared_attempt_after_validation() {
        let session = current_session();
        let active = accepted_origin(1);
        let expected_attempt = matching_active_attempt();
        let facts = ActiveReconstitutionFacts::matching(&session, active);
        let projection = facts
            .input()
            .reconstitute()
            .expect("the complete owner projection derives the prepared attempt");
        let phase = projection
            .active_turn()
            .expect("the turn owns the active slot")
            .active_phase();
        assert!(matches!(
            phase,
            Some(ActiveTurnPhase::Running { current_attempt })
                if current_attempt.id() == expected_attempt
                    && current_attempt.state() == &CurrentTurnAttemptState::Prepared
        ));
    }

    /// S03 / INV-009: inert running facts traverse the sealed
    /// prepared-to-running transition only inside the validated owner
    /// projection.
    #[test]
    fn active_reconstitution_derives_running_attempt_after_validation() {
        let session = current_session();
        let active = accepted_origin(1);
        let expected_attempt = turn_attempt_id(51);
        let mut facts = ActiveReconstitutionFacts::matching(&session, active);
        facts.replace_active_phase(ActiveTurnSchedulingReconstitutionInput::running(
            active.turn(),
            expected_attempt,
        ));
        let projection = facts
            .input()
            .reconstitute()
            .expect("the complete owner projection derives the running attempt");
        let execution = projection
            .active_turn_execution()
            .expect("active scheduling facts seal execution ownership");
        assert_eq!(execution.turn(), active.turn());
        assert!(matches!(
            execution.phase(),
            ActiveTurnPhase::Running { current_attempt }
                if current_attempt.id() == expected_attempt
                    && current_attempt.state() == &CurrentTurnAttemptState::Running
        ));
    }

    /// S07 / INV-006 / INV-011: a running continuation retains the exact
    /// independently checked tool batch correlation needed by interruption.
    #[test]
    fn s07_inv006_inv011_running_tool_batch_correlation_is_reconstituted() {
        let session = current_session();
        let active = accepted_origin(1);
        let origin_entry = ActiveReconstitutionFacts::matching_origin_entry();
        let starting_frontier = ActiveReconstitutionFacts::matching_starting_frontier();
        let producing_call = model_call_id(50);
        let continuation_attempt = turn_attempt_id(60);
        let request_id = tool_request_id(70);
        let assistant_tool_entry = semantic_entry(31);
        let yielded_frontier = frontier(41);
        let request = ToolRequestReconstitutionInput::new(
            request_id,
            session.id(),
            active.turn(),
            producing_call,
            ToolRequestOrdinal::from_u32(0),
            ToolName::try_new(String::from("current_time")).expect("fixture name is canonical"),
            NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("fixture arguments are canonical"),
        )
        .into_request();
        let approval = ToolApprovalResolutionReconstitutionInput::user_fixture(
            request_id,
            ToolApprovalDecision::Approve,
        )
        .reconstitute()
        .expect("user approval is implemented");
        let yielded = ResolvedContextFrontierSnapshot::try_from_candidate(
            session.id(),
            yielded_frontier.id(),
            vec![
                origin_entry.reference(&session),
                assistant_tool_entry.reference(&session),
            ],
        )
        .expect("the tool response extends the starting frontier");
        let batch = ToolBatchReconstitutionInput::new(
            session.id(),
            active.turn(),
            producing_call,
            yielded,
            vec![request],
            vec![approval],
            vec![],
            ToolBatchPhaseReconstitutionInput::Executing {
                turn_attempt: continuation_attempt,
            },
        )
        .reconstitute()
        .expect("the complete approved batch is executing");
        let mut facts = ActiveReconstitutionFacts::matching(&session, active);
        facts.replace_active_phase(
            ActiveTurnSchedulingReconstitutionInput::prepared(active.turn(), continuation_attempt)
                .with_executing_tool_batch(&batch),
        );
        facts
            .semantic_entries
            .push(SemanticTranscriptEntryReconstitutionInput::new(
                assistant_tool_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call,
                    request: request_id,
                },
            ));
        facts
            .snapshots
            .push(yielded_frontier.snapshot(&session, &[origin_entry, assistant_tool_entry]));
        let model_call = ModelCallReconstitutionInput::new(
            producing_call,
            active.turn(),
            turn_attempt_id(59),
            FrozenModelSelection::Direct(direct(1)),
            ResolvedProviderTarget::naming(provider_model_identity(51)),
            starting_frontier.id(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
        );

        let projection = facts
            .input()
            .with_model_call_facts(
                vec![crate::PinnedProviderTargetReconstitutionInput::new(
                    active.turn(),
                    model_call.target(),
                )],
                vec![model_call],
            )
            .reconstitute()
            .expect("the running batch is bound to its exact call and yielded frontier");

        assert_eq!(
            projection.active_executing_tool_batch,
            Some(ActiveExecutingToolBatchCorrelation {
                session: session.id(),
                turn: active.turn(),
                producing_call,
                yielded_frontier: yielded_frontier.id(),
                turn_attempt: Some(continuation_attempt),
            })
        );
        let active_execution = projection
            .active_turn_execution()
            .expect("the correlated continuation owns the active slot");
        let ActiveTurnPhase::Running { current_attempt } = active_execution.phase() else {
            panic!("the correlated continuation is the running phase");
        };
        assert_eq!(current_attempt.id(), continuation_attempt);
        assert_eq!(current_attempt.state(), &CurrentTurnAttemptState::Prepared);
    }

    /// S03 / INV-034: startup recovery consumes the complete active
    /// projection, ends its exact evidence-free attempt as Lost, and appends
    /// one `TurnFailed` marker to the starting frontier.
    #[test]
    fn s03_inv034_prepares_atomic_lost_failed_terminal_candidate() {
        let session = current_session();
        let active = accepted_origin(1);
        let failure_entry = semantic_entry(500);
        let terminal_frontier = frontier(600);
        let identities =
            AcceptedInputTurnFailureIdentities::new(failure_entry.id(), terminal_frontier.id());
        let projection = ActiveReconstitutionFacts::matching(&session, active)
            .input()
            .reconstitute()
            .expect("the complete active projection is valid");

        let candidate = projection
            .prepare_active_turn_lost_failure(identities)
            .expect("evidence-free prior-process work can end Lost");

        assert_eq!(candidate.turn().turn(), active.turn());
        assert_eq!(
            candidate.turn().ended_attempt().id(),
            matching_active_attempt()
        );
        assert_eq!(
            candidate.turn().ended_attempt().end(),
            &AttemptEnd::WithoutStop {
                disposition: UnstoppedAttemptDisposition::Lost,
            }
        );
        assert_eq!(candidate.turn().disposition(), &TurnDisposition::Failed);
        assert_eq!(
            candidate.failure_entry().payload(),
            &InitialSemanticTranscriptEntryPayload::TurnFailed {
                turn: active.turn(),
            }
        );
        assert_eq!(
            candidate
                .terminal_snapshot()
                .ordered_entries()
                .collect::<Vec<_>>(),
            vec![
                ActiveReconstitutionFacts::matching_origin_entry().reference(&session),
                failure_entry.reference(&session),
            ]
        );
        assert_eq!(
            candidate.terminal_snapshot().frontier().snapshot(),
            terminal_frontier.id()
        );
    }

    /// INV-034: the same Lost failure transition is valid for a stored
    /// Running attempt, without inventing a stop cause.
    #[test]
    fn inv034_running_attempt_also_prepares_without_stop_lost() {
        let session = current_session();
        let active = accepted_origin(1);
        let running_attempt = turn_attempt_id(51);
        let mut facts = ActiveReconstitutionFacts::matching(&session, active);
        facts.replace_active_phase(ActiveTurnSchedulingReconstitutionInput::running(
            active.turn(),
            running_attempt,
        ));

        let candidate = facts
            .input()
            .reconstitute()
            .expect("the complete running projection is valid")
            .prepare_active_turn_lost_failure(AcceptedInputTurnFailureIdentities::new(
                semantic_entry(500).id(),
                frontier(600).id(),
            ))
            .expect("running prior-process work can end Lost");

        assert_eq!(candidate.turn().ended_attempt().id(), running_attempt);
        assert_eq!(
            candidate.turn().ended_attempt().end(),
            &AttemptEnd::WithoutStop {
                disposition: UnstoppedAttemptDisposition::Lost,
            }
        );
    }

    /// INV-016 / INV-034: pending steering is not a stop cause; the lost
    /// failure reclassifies it into a queued successor, and identities that do
    /// not match the pending inventory leave the projection unchanged.
    #[test]
    fn inv016_inv034_lost_failure_reclassifies_pending_steering() {
        let session = current_session();
        let active = accepted_origin(1);
        let pending = accepted_origin(2);
        let mut facts = ActiveReconstitutionFacts::matching(&session, active);
        let tail = facts
            .acceptance_tail
            .as_mut()
            .expect("matching facts contain the active tail");
        tail.observed_last_position = pending.position();
        tail.entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    pending.accepted_input(),
                    AcceptedInputDisposition::PendingSteering {
                        binding: crate::SteeringBinding::new(active.turn()),
                    },
                ),
                pending.position(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: active.turn(),
                },
            ));
        let projection = facts
            .input()
            .reconstitute()
            .expect("the pending-steering tail is complete");
        let identities =
            AcceptedInputTurnFailureIdentities::new(semantic_entry(500).id(), frontier(600).id());

        let error = projection
            .clone()
            .prepare_active_turn_lost_failure(identities.clone())
            .expect_err("pending steering needs a successor identity");
        assert_eq!(error.projection(), &projection);
        assert_eq!(error.identities(), &identities);
        assert_eq!(
            error.failure(),
            AcceptedInputTurnFailureFailure::PendingSteeringReclassificationMismatch
        );

        let successor = turn_id(700);
        let candidate = projection
            .prepare_active_turn_lost_failure(identities.with_pending_steering_reclassifications(
                vec![PendingSteeringReclassificationIdentity::new(
                    pending.accepted_input(),
                    successor,
                )],
            ))
            .expect("pending steering is reclassified rather than refused");
        let [reclassified] = candidate.reclassified_pending_steering() else {
            panic!("exactly one successor is reclassified");
        };
        assert_eq!(reclassified.turn(), successor);
        assert_eq!(reclassified.source_turn(), active.turn());
        assert_eq!(
            reclassified.accepted_input(),
            &AcceptedInputLifecycle::new(
                pending.accepted_input(),
                AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                    turn: successor,
                    reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
                },
            )
        );
        assert_eq!(
            reclassified.order(),
            AcceptedInputQueueOrder::ordinary(pending.position())
        );
    }

    /// INV-089: pending steering remains outside the active rendered frontier
    /// until a safe-point continuation incorporates it.
    #[test]
    fn inv089_pending_steering_is_not_an_active_rendered_frontier_origin() {
        let session = current_session();
        let active = accepted_origin(1);
        let pending = accepted_origin(2);
        let mut facts = ActiveReconstitutionFacts::matching(&session, active);
        let tail = facts
            .acceptance_tail
            .as_mut()
            .expect("matching facts contain the active tail");
        tail.observed_last_position = pending.position();
        tail.entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    pending.accepted_input(),
                    AcceptedInputDisposition::PendingSteering {
                        binding: crate::SteeringBinding::new(active.turn()),
                    },
                ),
                pending.position(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: active.turn(),
                },
            ));
        let projection = facts
            .input()
            .reconstitute()
            .expect("the pending-steering tail is complete");

        assert_eq!(
            projection.active_rendered_frontier_origins(),
            Some(vec![active.accepted_input()])
        );
    }

    /// INV-001 / INV-034: startup failure preparation rejects each committed
    /// identity before constructing a candidate.
    #[test]
    fn inv001_inv034_rejects_committed_failure_identities() {
        let session = current_session();
        let active = accepted_origin(1);
        let projection = ActiveReconstitutionFacts::matching(&session, active)
            .input()
            .reconstitute()
            .expect("the complete active projection is valid");

        let entry_collision = projection
            .clone()
            .prepare_active_turn_lost_failure(AcceptedInputTurnFailureIdentities::new(
                ActiveReconstitutionFacts::matching_origin_entry().id(),
                frontier(600).id(),
            ))
            .expect_err("the semantic identity is already committed");
        assert_eq!(
            entry_collision.failure(),
            AcceptedInputTurnFailureFailure::FailureEntryIdentityAlreadyExists
        );

        let frontier_collision = projection
            .prepare_active_turn_lost_failure(AcceptedInputTurnFailureIdentities::new(
                semantic_entry(500).id(),
                ActiveReconstitutionFacts::matching_starting_frontier().id(),
            ))
            .expect_err("the frontier identity is already committed");
        assert_eq!(
            frontier_collision.failure(),
            AcceptedInputTurnFailureFailure::TerminalFrontierIdentityAlreadyExists
        );
    }

    /// S02 / S07 / S11 / INV-005 / INV-006 / INV-037: scheduling
    /// reconstitution accepts the exact terminal shape written when an
    /// interrupt closes a yielded tool round.
    #[test]
    fn s02_s07_s11_inv005_inv006_inv037_cancelled_tool_round_reconstitutes() {
        let session = current_session();
        let cancelled = accepted_origin(1);
        let successor = accepted_origin(2);
        let origin_entry = semantic_entry(30);
        let tool_use_entry = semantic_entry(31);
        let result_entry = semantic_entry(32);
        let cancellation_entry = semantic_entry(33);
        let starting_frontier = frontier(40);
        let terminal_frontier = frontier(41);
        let producing_call = model_call_id(50);
        let producing_attempt = turn_attempt_id(51);
        let terminal_attempt = turn_attempt_id(52);
        let request = tool_request_id(60);
        let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
            successor.position(),
            cancelled.turn(),
        );
        let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
            command_id(70),
            session.id(),
            cancelled.turn(),
            successor.accepted_input(),
            successor.turn(),
            successor_order,
        )
        .expect("the terminal interrupt is exactly correlated");
        let cancelled_record = cancelled.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                    cancelled.turn(),
                    terminal_attempt,
                    TerminalAttemptEndReconstitutionInput::after_cancellation(
                        CancellationStopDisposition::Cancelled,
                        interrupt,
                    ),
                    None,
                    interrupt,
                )
                .with_terminal_tool_denials(vec![user_denial(request)]),
                terminal_frontier: terminal_frontier.id(),
            },
        );
        let successor_record = successor.record_with(
            &session,
            OriginRecordFacts {
                order: successor_order,
                delivery: DeliveryRequest::Interrupt {
                    expected_active_turn: cancelled.turn(),
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: PerInputConfigurationChoices::new(
                        SessionConfigurationDefaultsVersion::first(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        );
        let semantic_entries = vec![
            cancelled.entry(&session, origin_entry),
            SemanticTranscriptEntryReconstitutionInput::new(
                tool_use_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call,
                    request,
                },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                result_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::ToolDenied { request },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                cancellation_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::TurnCancelled {
                    turn: cancelled.turn(),
                },
            ),
        ];
        let target = ResolvedProviderTarget::naming(provider_model_identity(80));
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![cancelled_record, successor_record],
            semantic_entries,
            vec![
                starting_frontier.snapshot(&session, &[origin_entry]),
                terminal_frontier.snapshot(
                    &session,
                    &[
                        origin_entry,
                        tool_use_entry,
                        result_entry,
                        cancellation_entry,
                    ],
                ),
            ],
            None,
        )
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                cancelled.turn(),
                target,
            )],
            vec![ModelCallReconstitutionInput::new(
                producing_call,
                cancelled.turn(),
                producing_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                starting_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            )],
        );

        let projection = input
            .reconstitute()
            .expect("the writer-produced cancelled tool-round shape reconstitutes");

        assert_eq!(
            projection
                .turn(cancelled.turn())
                .expect("the cancelled turn remains present")
                .status(),
            AcceptedInputTurnSchedulingStatus::TerminalCancelled
        );
        assert_eq!(
            projection
                .earliest_queued_turn()
                .expect("the interrupt successor remains queued")
                .turn(),
            successor.turn()
        );
    }

    /// S02 / S07 / S11 / INV-005 / INV-006 / INV-037: scheduling
    /// reconstitution accepts the exact terminal shape written when a stop
    /// request races a tool-using response, which names the batch's completed
    /// producing call.
    #[test]
    fn s02_s07_s11_inv005_inv006_inv037_stopped_tool_round_reconstitutes_from_named_call() {
        let session = current_session();
        let cancelled = accepted_origin(1);
        let successor = accepted_origin(2);
        let origin_entry = semantic_entry(30);
        let tool_use_entry = semantic_entry(31);
        let closed_result_entry = semantic_entry(32);
        let cancellation_entry = semantic_entry(33);
        let starting_frontier = frontier(40);
        let terminal_frontier = frontier(41);
        let producing_call = model_call_id(50);
        // The stop-requested attempt that issued the racing call is the exact
        // attempt the cancellation ends, so one identity names both.
        let stopped_attempt = turn_attempt_id(51);
        let request = tool_request_id(60);
        let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
            successor.position(),
            cancelled.turn(),
        );
        let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
            command_id(70),
            session.id(),
            cancelled.turn(),
            successor.accepted_input(),
            successor.turn(),
            successor_order,
        )
        .expect("the terminal interrupt is exactly correlated");
        let cancelled_record = cancelled.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                    cancelled.turn(),
                    stopped_attempt,
                    TerminalAttemptEndReconstitutionInput::after_cancellation(
                        CancellationStopDisposition::Cancelled,
                        interrupt,
                    ),
                    Some(producing_call),
                    interrupt,
                ),
                terminal_frontier: terminal_frontier.id(),
            },
        );
        let successor_record = successor.record_with(
            &session,
            OriginRecordFacts {
                order: successor_order,
                delivery: DeliveryRequest::Interrupt {
                    expected_active_turn: cancelled.turn(),
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: PerInputConfigurationChoices::new(
                        SessionConfigurationDefaultsVersion::first(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        );
        let semantic_entries = vec![
            cancelled.entry(&session, origin_entry),
            SemanticTranscriptEntryReconstitutionInput::new(
                tool_use_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call,
                    request,
                },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                closed_result_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::ToolClosed { request },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                cancellation_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::TurnCancelled {
                    turn: cancelled.turn(),
                },
            ),
        ];
        let target = ResolvedProviderTarget::naming(provider_model_identity(80));
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![cancelled_record, successor_record],
            semantic_entries,
            vec![
                starting_frontier.snapshot(&session, &[origin_entry]),
                terminal_frontier.snapshot(
                    &session,
                    &[
                        origin_entry,
                        tool_use_entry,
                        closed_result_entry,
                        cancellation_entry,
                    ],
                ),
            ],
            None,
        )
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                cancelled.turn(),
                target,
            )],
            vec![ModelCallReconstitutionInput::new(
                producing_call,
                cancelled.turn(),
                stopped_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                starting_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            )],
        );

        let projection = input
            .reconstitute()
            .expect("the writer-produced stopped tool-round shape reconstitutes");

        assert_eq!(
            projection
                .turn(cancelled.turn())
                .expect("the cancelled turn remains present")
                .status(),
            AcceptedInputTurnSchedulingStatus::TerminalCancelled
        );
        assert_eq!(
            projection
                .earliest_queued_turn()
                .expect("the interrupt successor remains queued")
                .turn(),
            successor.turn()
        );
    }

    /// S02 / S07 / S11 / INV-005 / INV-006 / INV-037: a cancelled terminal
    /// turn naming a completed call that is not the tool round's producing
    /// call fails closed.
    #[test]
    fn s02_s07_s11_inv005_inv006_inv037_cancelled_tool_round_rejects_unrelated_named_call() {
        let session = current_session();
        let cancelled = accepted_origin(1);
        let successor = accepted_origin(2);
        let origin_entry = semantic_entry(30);
        let tool_use_entry = semantic_entry(31);
        let closed_result_entry = semantic_entry(32);
        let cancellation_entry = semantic_entry(33);
        let starting_frontier = frontier(40);
        let terminal_frontier = frontier(41);
        let producing_call = model_call_id(50);
        // The named call is a completed call of the same turn and attempt that
        // proposed nothing in the terminal round: naming it is the behavior
        // under test.
        let unrelated_call = model_call_id(51);
        let stopped_attempt = turn_attempt_id(52);
        let request = tool_request_id(60);
        let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
            successor.position(),
            cancelled.turn(),
        );
        let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
            command_id(70),
            session.id(),
            cancelled.turn(),
            successor.accepted_input(),
            successor.turn(),
            successor_order,
        )
        .expect("the terminal interrupt is exactly correlated");
        let cancelled_record = cancelled.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                    cancelled.turn(),
                    stopped_attempt,
                    TerminalAttemptEndReconstitutionInput::after_cancellation(
                        CancellationStopDisposition::Cancelled,
                        interrupt,
                    ),
                    Some(unrelated_call),
                    interrupt,
                ),
                terminal_frontier: terminal_frontier.id(),
            },
        );
        let successor_record = successor.record_with(
            &session,
            OriginRecordFacts {
                order: successor_order,
                delivery: DeliveryRequest::Interrupt {
                    expected_active_turn: cancelled.turn(),
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: PerInputConfigurationChoices::new(
                        SessionConfigurationDefaultsVersion::first(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        );
        let semantic_entries = vec![
            cancelled.entry(&session, origin_entry),
            SemanticTranscriptEntryReconstitutionInput::new(
                tool_use_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call,
                    request,
                },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                closed_result_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::ToolClosed { request },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                cancellation_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::TurnCancelled {
                    turn: cancelled.turn(),
                },
            ),
        ];
        let target = ResolvedProviderTarget::naming(provider_model_identity(80));
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![cancelled_record, successor_record],
            semantic_entries,
            vec![
                starting_frontier.snapshot(&session, &[origin_entry]),
                terminal_frontier.snapshot(
                    &session,
                    &[
                        origin_entry,
                        tool_use_entry,
                        closed_result_entry,
                        cancellation_entry,
                    ],
                ),
            ],
            None,
        )
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                cancelled.turn(),
                target,
            )],
            vec![
                ModelCallReconstitutionInput::new(
                    producing_call,
                    cancelled.turn(),
                    stopped_attempt,
                    FrozenModelSelection::Direct(direct(1)),
                    target,
                    starting_frontier.id(),
                    ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
                ),
                ModelCallReconstitutionInput::new(
                    unrelated_call,
                    cancelled.turn(),
                    stopped_attempt,
                    FrozenModelSelection::Direct(direct(1)),
                    target,
                    starting_frontier.id(),
                    ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
                ),
            ],
        );

        let failure = assert_input_rejects_unchanged(input);

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                turn: cancelled.turn(),
            }
        );
    }

    /// S02 / S07 / S11 / INV-005 / INV-006 / INV-037: a cancelled terminal
    /// tool round whose `ToolDenied` result entry names no user denial
    /// resolution fails closed.
    #[test]
    fn s02_s07_s11_inv005_inv006_inv037_cancelled_tool_round_rejects_missing_denial_resolution() {
        let session = current_session();
        let cancelled = accepted_origin(1);
        let successor = accepted_origin(2);
        let origin_entry = semantic_entry(30);
        let tool_use_entry = semantic_entry(31);
        let result_entry = semantic_entry(32);
        let cancellation_entry = semantic_entry(33);
        let starting_frontier = frontier(40);
        let terminal_frontier = frontier(41);
        let producing_call = model_call_id(50);
        let producing_attempt = turn_attempt_id(51);
        let terminal_attempt = turn_attempt_id(52);
        let request = tool_request_id(60);
        let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
            successor.position(),
            cancelled.turn(),
        );
        let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
            command_id(70),
            session.id(),
            cancelled.turn(),
            successor.accepted_input(),
            successor.turn(),
            successor_order,
        )
        .expect("the terminal interrupt is exactly correlated");
        let cancelled_record = cancelled.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                    cancelled.turn(),
                    terminal_attempt,
                    TerminalAttemptEndReconstitutionInput::after_cancellation(
                        CancellationStopDisposition::Cancelled,
                        interrupt,
                    ),
                    None,
                    interrupt,
                )
                // The denial entry's backing user resolution is deliberately
                // absent: this emptiness is the behavior under test.
                .with_terminal_tool_denials(Vec::new()),
                terminal_frontier: terminal_frontier.id(),
            },
        );
        let successor_record = successor.record_with(
            &session,
            OriginRecordFacts {
                order: successor_order,
                delivery: DeliveryRequest::Interrupt {
                    expected_active_turn: cancelled.turn(),
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: PerInputConfigurationChoices::new(
                        SessionConfigurationDefaultsVersion::first(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        );
        let semantic_entries = vec![
            cancelled.entry(&session, origin_entry),
            SemanticTranscriptEntryReconstitutionInput::new(
                tool_use_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call,
                    request,
                },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                result_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::ToolDenied { request },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                cancellation_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::TurnCancelled {
                    turn: cancelled.turn(),
                },
            ),
        ];
        let target = ResolvedProviderTarget::naming(provider_model_identity(80));
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![cancelled_record, successor_record],
            semantic_entries,
            vec![
                starting_frontier.snapshot(&session, &[origin_entry]),
                terminal_frontier.snapshot(
                    &session,
                    &[
                        origin_entry,
                        tool_use_entry,
                        result_entry,
                        cancellation_entry,
                    ],
                ),
            ],
            None,
        )
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                cancelled.turn(),
                target,
            )],
            vec![ModelCallReconstitutionInput::new(
                producing_call,
                cancelled.turn(),
                producing_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                starting_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            )],
        );

        let failure = assert_input_rejects_unchanged(input);

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                turn: cancelled.turn(),
            }
        );
    }

    /// S02 / S07 / S11 / INV-005 / INV-006 / INV-037: an approving user
    /// resolution cannot back a cancelled terminal tool round's `ToolDenied`
    /// result entry; the round fails closed.
    #[test]
    fn s02_s07_s11_inv005_inv006_inv037_cancelled_tool_round_rejects_approving_resolution() {
        let session = current_session();
        let cancelled = accepted_origin(1);
        let successor = accepted_origin(2);
        let origin_entry = semantic_entry(30);
        let tool_use_entry = semantic_entry(31);
        let result_entry = semantic_entry(32);
        let cancellation_entry = semantic_entry(33);
        let starting_frontier = frontier(40);
        let terminal_frontier = frontier(41);
        let producing_call = model_call_id(50);
        let producing_attempt = turn_attempt_id(51);
        let terminal_attempt = turn_attempt_id(52);
        let request = tool_request_id(60);
        let approving_resolution = ToolApprovalResolutionReconstitutionInput::user_fixture(
            request,
            ToolApprovalDecision::Approve,
        )
        .reconstitute()
        .expect("the approving resolution fixture is valid");
        let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
            successor.position(),
            cancelled.turn(),
        );
        let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
            command_id(70),
            session.id(),
            cancelled.turn(),
            successor.accepted_input(),
            successor.turn(),
            successor_order,
        )
        .expect("the terminal interrupt is exactly correlated");
        let cancelled_record = cancelled.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                    cancelled.turn(),
                    terminal_attempt,
                    TerminalAttemptEndReconstitutionInput::after_cancellation(
                        CancellationStopDisposition::Cancelled,
                        interrupt,
                    ),
                    None,
                    interrupt,
                )
                .with_terminal_tool_denials(vec![approving_resolution]),
                terminal_frontier: terminal_frontier.id(),
            },
        );
        let successor_record = successor.record_with(
            &session,
            OriginRecordFacts {
                order: successor_order,
                delivery: DeliveryRequest::Interrupt {
                    expected_active_turn: cancelled.turn(),
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: PerInputConfigurationChoices::new(
                        SessionConfigurationDefaultsVersion::first(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        );
        let semantic_entries = vec![
            cancelled.entry(&session, origin_entry),
            SemanticTranscriptEntryReconstitutionInput::new(
                tool_use_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call,
                    request,
                },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                result_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::ToolDenied { request },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                cancellation_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::TurnCancelled {
                    turn: cancelled.turn(),
                },
            ),
        ];
        let target = ResolvedProviderTarget::naming(provider_model_identity(80));
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![cancelled_record, successor_record],
            semantic_entries,
            vec![
                starting_frontier.snapshot(&session, &[origin_entry]),
                terminal_frontier.snapshot(
                    &session,
                    &[
                        origin_entry,
                        tool_use_entry,
                        result_entry,
                        cancellation_entry,
                    ],
                ),
            ],
            None,
        )
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                cancelled.turn(),
                target,
            )],
            vec![ModelCallReconstitutionInput::new(
                producing_call,
                cancelled.turn(),
                producing_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                starting_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            )],
        );

        let failure = assert_input_rejects_unchanged(input);

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                turn: cancelled.turn(),
            }
        );
    }

    /// S02 / S03 / S11 / INV-005 / INV-006 / INV-034: scheduling
    /// reconstitution accepts the exact terminal shape written when a
    /// crash-lost tool round closes the turn as failed.
    #[test]
    fn s02_s03_s11_inv005_inv006_inv034_failed_tool_round_reconstitutes() {
        let session = current_session();
        let failed = accepted_origin(1);
        let origin_entry = semantic_entry(30);
        let first_tool_use = semantic_entry(31);
        let second_tool_use = semantic_entry(32);
        let first_result = semantic_entry(33);
        let second_result = semantic_entry(34);
        let failure_entry = semantic_entry(35);
        let starting_frontier = frontier(40);
        let terminal_frontier = frontier(41);
        let producing_call = model_call_id(50);
        let producing_attempt = turn_attempt_id(51);
        let terminal_attempt = turn_attempt_id(52);
        let first_request = tool_request_id(60);
        let second_request = tool_request_id(61);
        let executed_attempt = ToolAttemptReconstitutionInput::new(
            tool_attempt_id(70),
            first_request,
            session.id(),
            failed.turn(),
            terminal_attempt,
            ToolEffectClass::EffectFree,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Completed {
                result: ToolResultContent::Text(
                    ToolResultText::try_new(String::from("ok"))
                        .expect("fixture tool result is valid"),
                ),
            }),
        )
        .reconstitute()
        .expect("fixture tool attempt is supported");
        let crate::ReconstitutedToolAttempt::Ended(executed_attempt) = executed_attempt else {
            panic!("fixture tool attempt is terminal");
        };
        let failed_record = failed.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                terminal_execution: Some(
                    FailedTurnExecutionReconstitutionInput::attempt_only(
                        failed.turn(),
                        terminal_attempt,
                        UnstoppedAttemptDisposition::KnownFailure,
                    )
                    .with_terminal_tool_attempts(vec![executed_attempt]),
                ),
                terminal_frontier: terminal_frontier.id(),
            },
        );
        let semantic_entries = vec![
            failed.entry(&session, origin_entry),
            SemanticTranscriptEntryReconstitutionInput::new(
                first_tool_use.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call,
                    request: first_request,
                },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                second_tool_use.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call,
                    request: second_request,
                },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                first_result.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                    attempt: tool_attempt_id(70),
                },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                second_result.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::ToolClosed {
                    request: second_request,
                },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                failure_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::TurnFailed {
                    turn: failed.turn(),
                },
            ),
        ];
        let target = ResolvedProviderTarget::naming(provider_model_identity(80));
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![failed_record],
            semantic_entries,
            vec![
                starting_frontier.snapshot(&session, &[origin_entry]),
                terminal_frontier.snapshot(
                    &session,
                    &[
                        origin_entry,
                        first_tool_use,
                        second_tool_use,
                        first_result,
                        second_result,
                        failure_entry,
                    ],
                ),
            ],
            None,
        )
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                failed.turn(),
                target,
            )],
            vec![ModelCallReconstitutionInput::new(
                producing_call,
                failed.turn(),
                producing_attempt,
                FrozenModelSelection::Direct(direct(1)),
                target,
                starting_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            )],
        );

        let mismatched_attempt = ToolAttemptReconstitutionInput::new(
            tool_attempt_id(70),
            second_request,
            session.id(),
            failed.turn(),
            terminal_attempt,
            ToolEffectClass::EffectFree,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Completed {
                result: ToolResultContent::Text(
                    ToolResultText::try_new(String::from("wrong request"))
                        .expect("fixture tool result is valid"),
                ),
            }),
        )
        .reconstitute()
        .expect("fixture tool attempt is supported");
        let crate::ReconstitutedToolAttempt::Ended(mismatched_attempt) = mismatched_attempt else {
            panic!("fixture tool attempt is terminal");
        };
        let mut mismatched_input = input.clone();
        let AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            terminal_execution: Some(execution),
            ..
        } = &mut mismatched_input.turns[0].state
        else {
            panic!("fixture is a failed terminal");
        };
        execution.terminal_tool_attempts = vec![mismatched_attempt];
        assert_eq!(
            mismatched_input
                .reconstitute()
                .expect_err("a result attempt must execute its paired request")
                .failure()
                .to_owned(),
            AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                turn: failed.turn(),
            }
        );

        let projection = input
            .reconstitute()
            .expect("the writer-produced failed tool-round shape reconstitutes");

        assert_eq!(
            projection
                .turn(failed.turn())
                .expect("the failed turn remains present")
                .status(),
            AcceptedInputTurnSchedulingStatus::TerminalFailed
        );
    }

    /// S02 / S11 / INV-005: complete scheduling reconstitution admits every
    /// reference-only tool entry while retaining completed-call provenance
    /// for assistant tool use from an earlier intra-turn round.
    #[test]
    fn s02_s11_inv005_scheduling_reconstitutes_tool_round_history() {
        let session = current_session();
        let active = accepted_origin(1);
        let producing_call = model_call_id(90);
        let request = tool_request_id(91);
        let attempt = tool_attempt_id(92);
        let denied_request = tool_request_id(93);
        let closed_request = tool_request_id(94);
        let tool_use = semantic_entry(31);
        let execution_result = semantic_entry(32);
        let denied = semantic_entry(33);
        let closed = semantic_entry(34);
        let mut facts = ActiveReconstitutionFacts::matching(&session, active);
        facts.semantic_entries.extend([
            SemanticTranscriptEntryReconstitutionInput::new(
                tool_use.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call,
                    request,
                },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                execution_result.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::ToolExecutionResult { attempt },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                denied.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::ToolDenied {
                    request: denied_request,
                },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                closed.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::ToolClosed {
                    request: closed_request,
                },
            ),
        ]);
        let target = ResolvedProviderTarget::naming(provider_model_identity(51));
        let projection = facts
            .input()
            .with_model_call_facts(
                vec![crate::PinnedProviderTargetReconstitutionInput::new(
                    active.turn(),
                    target,
                )],
                vec![ModelCallReconstitutionInput::new(
                    producing_call,
                    active.turn(),
                    turn_attempt_id(49),
                    FrozenModelSelection::Direct(direct(1)),
                    target,
                    ActiveReconstitutionFacts::matching_starting_frontier().id(),
                    ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
                )],
            )
            .reconstitute()
            .expect("tool-round history and its completed producing call agree");

        assert!(matches!(
            projection
                .semantic_entry(tool_use.reference(&session))
                .map(SemanticTranscriptEntry::payload),
            Some(InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                producing_call: actual_call,
                request: actual_request,
            }) if *actual_call == producing_call && *actual_request == request
        ));
        assert!(matches!(
            projection
                .semantic_entry(execution_result.reference(&session))
                .map(SemanticTranscriptEntry::payload),
            Some(InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                attempt: actual,
            }) if *actual == attempt
        ));
        assert!(matches!(
            projection
                .semantic_entry(denied.reference(&session))
                .map(SemanticTranscriptEntry::payload),
            Some(InitialSemanticTranscriptEntryPayload::ToolDenied { request: actual })
                if *actual == denied_request
        ));
        assert!(matches!(
            projection
                .semantic_entry(closed.reference(&session))
                .map(SemanticTranscriptEntry::payload),
            Some(InitialSemanticTranscriptEntryPayload::ToolClosed { request: actual })
                if *actual == closed_request
        ));
    }

    /// S02 / S08 / S09 / INV-012 / INV-016 / INV-036: scheduling
    /// reconstitution admits consumed steering only when its semantic subject,
    /// accepted lifecycle, source turn, call frontier, and acceptance order
    /// agree exactly.
    #[test]
    fn s02_s08_s09_inv012_inv016_inv036_reconstitution_validates_steering_subjects() {
        let session = current_session();
        let active = accepted_origin(1);
        let consumed = accepted_origin(2);
        ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed)
            .input()
            .reconstitute()
            .expect("matching consumed steering reconstructs");

        let mut nonfollowing_position =
            ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
        nonfollowing_position.consumed_steering[0].acceptance_position = active.position();
        nonfollowing_position.acceptance_tail.entries[1].position = active.position();
        nonfollowing_position.acceptance_tail.observed_last_position = active.position();
        assert_eq!(
            assert_input_rejects_unchanged(nonfollowing_position.input()),
            AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                accepted_input: consumed.accepted_input(),
            }
        );

        let mut skipped_reclassified =
            ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
        skipped_reclassified.turns[0].state =
            AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: ActiveReconstitutionFacts::matching_starting_frontier().id(),
                terminal_execution: None,
                terminal_frontier: frontier(42).id(),
            };
        let reclassified = accepted_origin(2);
        skipped_reclassified
            .turns
            .push(AcceptedInputTurnSchedulingRecord::reclassified(
                session.id(),
                reclassified.turn(),
                session.id(),
                AcceptedInputLifecycle::new(
                    reclassified.accepted_input(),
                    AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                        turn: reclassified.turn(),
                        reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
                    },
                ),
                session.id(),
                reclassified.turn(),
                reclassified.ordinary_order(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: active.turn(),
                },
                crate::SteeringBinding::new(active.turn()),
                configuration(&session),
                AcceptedInputTurnSchedulingRecordState::Queued,
            ));
        assert_eq!(
            assert_input_rejects_unchanged(skipped_reclassified.input()),
            AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                accepted_input: consumed.accepted_input(),
            }
        );

        let mut nonexistent =
            ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
        nonexistent.semantic_entries[1] = SemanticTranscriptEntryReconstitutionInput::new(
            semantic_entry(31).id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                accepted_input: accepted_input_id(99),
                source_turn: active.turn(),
            },
        );
        assert_eq!(
            assert_input_rejects_unchanged(nonexistent.input()),
            AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                accepted_input: consumed.accepted_input(),
            }
        );

        let mut wrong_source =
            ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
        wrong_source.semantic_entries[1] = SemanticTranscriptEntryReconstitutionInput::new(
            semantic_entry(31).id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                accepted_input: consumed.accepted_input(),
                source_turn: turn_id(99),
            },
        );
        assert_eq!(
            assert_input_rejects_unchanged(wrong_source.input()),
            AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                accepted_input: consumed.accepted_input(),
            }
        );

        let mut missing_lifecycle =
            ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
        missing_lifecycle.consumed_steering.clear();
        assert_eq!(
            assert_input_rejects_unchanged(missing_lifecycle.input()),
            AcceptedInputSchedulingReconstitutionFailure::SteeringSemanticEntryMismatch {
                entry: semantic_entry(31).id(),
            }
        );

        let mut duplicate_subject =
            ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
        duplicate_subject
            .semantic_entries
            .push(SemanticTranscriptEntryReconstitutionInput::new(
                semantic_entry(32).id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                    accepted_input: consumed.accepted_input(),
                    source_turn: active.turn(),
                },
            ));
        assert_eq!(
            assert_input_rejects_unchanged(duplicate_subject.input()),
            AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
                entry: semantic_entry(32).id(),
            }
        );

        let mut duplicate_lifecycle =
            ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
        let duplicate = duplicate_lifecycle
            .consumed_steering
            .first()
            .cloned()
            .expect("the matching fixture contains one consumed subject");
        duplicate_lifecycle.consumed_steering.push(duplicate);
        assert_eq!(
            assert_input_rejects_unchanged(duplicate_lifecycle.input()),
            AcceptedInputSchedulingReconstitutionFailure::DuplicateConsumedSteering {
                accepted_input: consumed.accepted_input(),
            }
        );
    }

    /// S02 / S08 / S10 / INV-016 / INV-036: scheduling reconstitution admits
    /// the durable shape the continuation transaction commits — a running
    /// continuation attempt owning a prepared steering-consuming call whose
    /// frontier is the round's exact result projection plus the consumed
    /// suffix.
    #[test]
    fn s02_s08_s10_inv016_inv036_steering_consumed_at_continuation_reconstitutes() {
        let session = current_session();
        let active = accepted_origin(1);
        let consumed = accepted_origin(2);
        ConsumedSteeringReconstitutionFacts::matching_at_continuation(&session, active, consumed)
            .input()
            .reconstitute()
            .expect("continuation-consumed steering reconstructs");
    }

    /// S02 / S08 / INV-016 / INV-036: a running attempt owning a prepared
    /// steering-consuming call is legal only with the round's result
    /// evidence.
    #[test]
    fn s02_s08_inv016_inv036_continuation_pair_requires_round_evidence() {
        let session = current_session();
        let active = accepted_origin(1);
        let consumed = accepted_origin(2);
        let mut missing_evidence = ConsumedSteeringReconstitutionFacts::matching_at_continuation(
            &session, active, consumed,
        );
        missing_evidence.steering_continuation_rounds.clear();
        assert_eq!(
            assert_input_rejects_unchanged(missing_evidence.input()),
            AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                accepted_input: consumed.accepted_input(),
            }
        );
    }

    /// S02 / S08 / INV-016 / INV-036: continuation-round evidence must name a
    /// steering-consuming call.
    #[test]
    fn s02_s08_inv016_inv036_round_evidence_requires_a_consuming_call() {
        let session = current_session();
        let active = accepted_origin(1);
        let consumed = accepted_origin(2);
        let mut dangling_evidence = ConsumedSteeringReconstitutionFacts::matching_at_continuation(
            &session, active, consumed,
        );
        dangling_evidence.steering_continuation_rounds.push(
            SteeringContinuationRoundReconstitutionInput::new(
                ConsumedSteeringReconstitutionFacts::matching_continuation_producing_call(),
                vec![
                    ConsumedSteeringReconstitutionFacts::matching_continuation_round_attempt(
                        &session, active,
                    ),
                ],
                Vec::new(),
            ),
        );
        assert_eq!(
            assert_input_rejects_unchanged(dangling_evidence.input()),
            AcceptedInputSchedulingReconstitutionFailure::SteeringContinuationRoundMismatch {
                call: ConsumedSteeringReconstitutionFacts::matching_continuation_producing_call(),
            }
        );
    }

    /// S02 / S08 / INV-016 / INV-036: continuation-round evidence names each
    /// consuming call at most once.
    #[test]
    fn s02_s08_inv016_inv036_round_evidence_names_each_consumer_once() {
        let session = current_session();
        let active = accepted_origin(1);
        let consumed = accepted_origin(2);
        let mut duplicate_evidence = ConsumedSteeringReconstitutionFacts::matching_at_continuation(
            &session, active, consumed,
        );
        let duplicated = duplicate_evidence.steering_continuation_rounds[0].clone();
        duplicate_evidence
            .steering_continuation_rounds
            .push(duplicated);
        assert_eq!(
            assert_input_rejects_unchanged(duplicate_evidence.input()),
            AcceptedInputSchedulingReconstitutionFailure::SteeringContinuationRoundMismatch {
                call: ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
            }
        );
    }

    /// S02 / S08 / INV-016 / INV-036: the consumed steering entries must be
    /// the exact trailing suffix after the round's result window.
    #[test]
    fn s02_s08_inv016_inv036_consumed_steering_is_the_continuation_trailing_suffix() {
        let session = current_session();
        let active = accepted_origin(1);
        let consumed = accepted_origin(2);
        let mut interposed_steering = ConsumedSteeringReconstitutionFacts::matching_at_continuation(
            &session, active, consumed,
        );
        interposed_steering.snapshots[1] = frontier(41).snapshot(
            &session,
            &[
                ActiveReconstitutionFacts::matching_origin_entry(),
                semantic_entry(34),
                semantic_entry(31),
                semantic_entry(35),
            ],
        );
        assert_eq!(
            assert_input_rejects_unchanged(interposed_steering.input()),
            AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                accepted_input: consumed.accepted_input(),
            }
        );
    }

    /// S02 / S08 / S10 / INV-016 / INV-036: each result entry in the
    /// continuation window must correlate to its proposal-ordered request.
    #[test]
    fn s02_s08_s10_inv016_inv036_continuation_results_correlate_to_proposal_order() {
        let session = current_session();
        let active = accepted_origin(1);
        let consumed = accepted_origin(2);
        let mut miscorrelated_result =
            ConsumedSteeringReconstitutionFacts::matching_at_continuation(
                &session, active, consumed,
            );
        miscorrelated_result.steering_continuation_rounds =
            vec![SteeringContinuationRoundReconstitutionInput::new(
                ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
                vec![ended_tool_attempt(
                    &session,
                    active,
                    matching_active_attempt(),
                    ConsumedSteeringReconstitutionFacts::matching_continuation_tool_attempt(),
                    tool_request_id(96),
                )],
                Vec::new(),
            )];
        assert_eq!(
            assert_input_rejects_unchanged(miscorrelated_result.input()),
            AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                accepted_input: consumed.accepted_input(),
            }
        );
    }

    /// S02 / S08 / S10 / INV-016 / INV-036: the round's tools were issued by
    /// the same continuation attempt that owns the consuming call; evidence
    /// issued by a foreign attempt fails closed.
    #[test]
    fn s02_s08_s10_inv016_inv036_continuation_results_bind_to_the_consuming_attempt() {
        let session = current_session();
        let active = accepted_origin(1);
        let consumed = accepted_origin(2);
        let mut foreign_issuing_attempt =
            ConsumedSteeringReconstitutionFacts::matching_at_continuation(
                &session, active, consumed,
            );
        foreign_issuing_attempt.steering_continuation_rounds =
            vec![SteeringContinuationRoundReconstitutionInput::new(
                ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
                vec![ended_tool_attempt(
                    &session,
                    active,
                    turn_attempt_id(49),
                    ConsumedSteeringReconstitutionFacts::matching_continuation_tool_attempt(),
                    ConsumedSteeringReconstitutionFacts::matching_continuation_request(),
                )],
                Vec::new(),
            )];
        assert_eq!(
            assert_input_rejects_unchanged(foreign_issuing_attempt.input()),
            AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                accepted_input: consumed.accepted_input(),
            }
        );
    }

    /// S02 / S08 / S10 / INV-016 / INV-036: a continuation window forbids
    /// turn-end closures, which exist only in terminal materialization.
    #[test]
    fn s02_s08_s10_inv016_inv036_continuation_window_forbids_turn_end_closures() {
        let session = current_session();
        let active = accepted_origin(1);
        let consumed = accepted_origin(2);
        let mut closed_request = ConsumedSteeringReconstitutionFacts::matching_at_continuation(
            &session, active, consumed,
        );
        closed_request.semantic_entries[3] = SemanticTranscriptEntryReconstitutionInput::new(
            semantic_entry(35).id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolClosed {
                request: ConsumedSteeringReconstitutionFacts::matching_continuation_request(),
            },
        );
        closed_request.steering_continuation_rounds =
            vec![SteeringContinuationRoundReconstitutionInput::new(
                ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
                Vec::new(),
                Vec::new(),
            )];
        assert_eq!(
            assert_input_rejects_unchanged(closed_request.input()),
            AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                accepted_input: consumed.accepted_input(),
            }
        );
    }

    /// S02 / S08 / S10 / INV-016 / INV-036: an ambiguous attempt end is a
    /// turn-level failure and never reaches a continuation window.
    #[test]
    fn s02_s08_s10_inv016_inv036_continuation_window_rejects_an_ambiguous_attempt_end() {
        let session = current_session();
        let active = accepted_origin(1);
        let consumed = accepted_origin(2);
        let mut ambiguous_attempt = ConsumedSteeringReconstitutionFacts::matching_at_continuation(
            &session, active, consumed,
        );
        ambiguous_attempt.steering_continuation_rounds =
            vec![SteeringContinuationRoundReconstitutionInput::new(
                ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
                vec![ended_tool_attempt_with_end(
                    &session,
                    active,
                    matching_active_attempt(),
                    ConsumedSteeringReconstitutionFacts::matching_continuation_tool_attempt(),
                    ConsumedSteeringReconstitutionFacts::matching_continuation_request(),
                    ToolAttemptEnd::Ambiguous,
                )],
                Vec::new(),
            )];
        assert_eq!(
            assert_input_rejects_unchanged(ambiguous_attempt.input()),
            AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                accepted_input: consumed.accepted_input(),
            }
        );
    }

    /// S02 / S08 / S10 / INV-016 / INV-036: a crash-lost attempt end is a
    /// turn-level failure and never reaches a continuation window.
    #[test]
    fn s02_s08_s10_inv016_inv036_continuation_window_rejects_a_crash_lost_attempt_end() {
        let session = current_session();
        let active = accepted_origin(1);
        let consumed = accepted_origin(2);
        let mut crash_lost_attempt = ConsumedSteeringReconstitutionFacts::matching_at_continuation(
            &session, active, consumed,
        );
        crash_lost_attempt.steering_continuation_rounds =
            vec![SteeringContinuationRoundReconstitutionInput::new(
                ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
                vec![ended_tool_attempt_with_end(
                    &session,
                    active,
                    matching_active_attempt(),
                    ConsumedSteeringReconstitutionFacts::matching_continuation_tool_attempt(),
                    ConsumedSteeringReconstitutionFacts::matching_continuation_request(),
                    ToolAttemptEnd::KnownFailed {
                        error: ToolExecutionError::new(ToolExecutionErrorKind::CrashLost, None),
                    },
                )],
                Vec::new(),
            )];
        assert_eq!(
            assert_input_rejects_unchanged(crash_lost_attempt.input()),
            AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                accepted_input: consumed.accepted_input(),
            }
        );
    }

    /// S02 / S08 / INV-016 / INV-036: only a tool proposal keeps a completed
    /// consumer's turn going, so a text-only completed consumer inside an
    /// active turn cannot claim the historical-consumer correlation.
    #[test]
    fn s02_s08_inv016_inv036_text_only_completed_consumer_fails_closed() {
        let session = current_session();
        let active = accepted_origin(1);
        let consumed = accepted_origin(2);
        let mut text_only_consumer =
            ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
        text_only_consumer.model_calls[0] = ModelCallReconstitutionInput::new(
            ConsumedSteeringReconstitutionFacts::matching_continuation_call(),
            active.turn(),
            matching_active_attempt(),
            FrozenModelSelection::Direct(direct(1)),
            ResolvedProviderTarget::naming(provider_model_identity(51)),
            frontier(41).id(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
        );
        text_only_consumer
            .semantic_entries
            .push(SemanticTranscriptEntryReconstitutionInput::new(
                semantic_entry(36).id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::AssistantText {
                    producing_call: ConsumedSteeringReconstitutionFacts::matching_continuation_call(
                    ),
                    value: AssistantText::try_new(String::from("text-only response"))
                        .expect("fixture assistant text is valid"),
                },
            ));
        assert_eq!(
            assert_input_rejects_unchanged(text_only_consumer.input()),
            AcceptedInputSchedulingReconstitutionFailure::ConsumedSteeringMismatch {
                accepted_input: consumed.accepted_input(),
            }
        );
    }

    /// S02 / S08 / S10 / INV-016 / INV-036: a steering-consuming call that
    /// completed by proposing a tool round stays reconstitutable while the
    /// round is parked awaiting approval — the consumer is correlated through
    /// its assistant history and exact frontier window, not the current
    /// phase's attempt.
    #[test]
    fn s02_s08_s10_inv016_inv036_parked_tool_round_retains_consumed_steering() {
        let session = current_session();
        let active = accepted_origin(1);
        let consumed = accepted_origin(2);
        let origin_entry = ActiveReconstitutionFacts::matching_origin_entry();
        let steering_entry = semantic_entry(31);
        let tool_use_entry = semantic_entry(34);
        let call_frontier = frontier(41);
        let yielded_frontier = frontier(42);
        let consuming_call = ConsumedSteeringReconstitutionFacts::matching_continuation_call();
        let request_id = ConsumedSteeringReconstitutionFacts::matching_continuation_request();
        let request = ToolRequestReconstitutionInput::new(
            request_id,
            session.id(),
            active.turn(),
            consuming_call,
            ToolRequestOrdinal::from_u32(0),
            ToolName::try_new(String::from("current_time")).expect("fixture name is canonical"),
            NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("fixture arguments are canonical"),
        )
        .into_request();
        let yielded = ResolvedContextFrontierSnapshot::try_from_candidate(
            session.id(),
            yielded_frontier.id(),
            vec![
                origin_entry.reference(&session),
                steering_entry.reference(&session),
                tool_use_entry.reference(&session),
            ],
        )
        .expect("the tool response extends the steering-bearing call frontier");
        let batch = ToolBatchReconstitutionInput::new(
            session.id(),
            active.turn(),
            consuming_call,
            yielded,
            vec![request],
            vec![],
            vec![],
            ToolBatchPhaseReconstitutionInput::AwaitingApproval {
                request: request_id,
            },
        )
        .reconstitute()
        .expect("the undecided batch is awaiting approval");
        let mut facts = ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
        let AcceptedInputTurnSchedulingRecordState::Active { phase, .. } =
            &mut facts.turns[0].state
        else {
            panic!("matching consumed-steering facts retain an active scheduling record");
        };
        *phase = ActiveTurnSchedulingReconstitutionInput::awaiting_approval(active.turn(), &batch)
            .expect("the approval wait names the parked batch");
        facts
            .semantic_entries
            .push(SemanticTranscriptEntryReconstitutionInput::new(
                tool_use_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call: consuming_call,
                    request: request_id,
                },
            ));
        facts.snapshots.push(
            yielded_frontier.snapshot(&session, &[origin_entry, steering_entry, tool_use_entry]),
        );
        facts.model_calls = vec![ModelCallReconstitutionInput::new(
            consuming_call,
            active.turn(),
            matching_active_attempt(),
            FrozenModelSelection::Direct(direct(1)),
            ResolvedProviderTarget::naming(provider_model_identity(51)),
            call_frontier.id(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
        )];

        facts
            .input()
            .reconstitute()
            .expect("a parked tool round retains its consumed steering");
    }

    /// Matching stored facts for one failed terminal turn naming its
    /// round-two continuation call: the call's whole frontier is the
    /// completed round's result projection and the terminal frontier extends
    /// it by exactly the failure marker.
    fn failed_continuation_call_input(
        session: &Session,
        failed: OriginFixture,
    ) -> AcceptedInputSchedulingReconstitutionInput {
        let session = session.clone();
        let origin_entry = semantic_entry(30);
        let tool_use_entry = semantic_entry(31);
        let result_entry = semantic_entry(32);
        let failure_entry = semantic_entry(33);
        let starting_frontier = frontier(40);
        let call_frontier = frontier(41);
        let terminal_frontier = frontier(42);
        let producing_call = model_call_id(50);
        let producing_attempt = turn_attempt_id(51);
        let terminal_attempt = turn_attempt_id(52);
        let continuation_call = model_call_id(53);
        let request = tool_request_id(60);
        let executed_attempt = ended_tool_attempt(
            &session,
            failed,
            terminal_attempt,
            tool_attempt_id(70),
            request,
        );
        let failed_record = failed.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                terminal_execution: Some(
                    FailedTurnExecutionReconstitutionInput::with_call(
                        failed.turn(),
                        terminal_attempt,
                        UnstoppedAttemptDisposition::KnownFailure,
                        continuation_call,
                    )
                    .with_terminal_tool_attempts(vec![executed_attempt]),
                ),
                terminal_frontier: terminal_frontier.id(),
            },
        );
        let semantic_entries = vec![
            failed.entry(&session, origin_entry),
            SemanticTranscriptEntryReconstitutionInput::new(
                tool_use_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call,
                    request,
                },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                result_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                    attempt: tool_attempt_id(70),
                },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                failure_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::TurnFailed {
                    turn: failed.turn(),
                },
            ),
        ];
        let target = ResolvedProviderTarget::naming(provider_model_identity(80));
        AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![failed_record],
            semantic_entries,
            vec![
                starting_frontier.snapshot(&session, &[origin_entry]),
                call_frontier.snapshot(&session, &[origin_entry, tool_use_entry, result_entry]),
                terminal_frontier.snapshot(
                    &session,
                    &[origin_entry, tool_use_entry, result_entry, failure_entry],
                ),
            ],
            None,
        )
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                failed.turn(),
                target,
            )],
            vec![
                ModelCallReconstitutionInput::new(
                    producing_call,
                    failed.turn(),
                    producing_attempt,
                    FrozenModelSelection::Direct(direct(1)),
                    target,
                    starting_frontier.id(),
                    ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
                ),
                ModelCallReconstitutionInput::new(
                    continuation_call,
                    failed.turn(),
                    terminal_attempt,
                    FrozenModelSelection::Direct(direct(1)),
                    target,
                    call_frontier.id(),
                    ModelCallReconstitutionState::Terminal(ModelCallDisposition::KnownFailed),
                ),
            ],
        )
    }

    /// S02 / S10 / S11 / INV-006: a failed terminal turn naming its round-two
    /// continuation call reconstitutes when that call's whole frontier is the
    /// completed round's result projection the terminal marker extends.
    #[test]
    fn s02_s10_s11_inv006_failed_continuation_call_reconstitutes() {
        let session = current_session();
        let failed = accepted_origin(1);
        failed_continuation_call_input(&session, failed)
            .reconstitute()
            .expect("the failed continuation-call terminal shape reconstructs");
    }

    /// S02 / S10 / S11 / INV-006: a failed terminal turn naming a
    /// continuation call is accepted only with its round's result evidence.
    #[test]
    fn s02_s10_s11_inv006_failed_continuation_call_requires_round_evidence() {
        let session = current_session();
        let failed = accepted_origin(1);
        let mut missing_evidence = failed_continuation_call_input(&session, failed);
        let AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            terminal_execution: Some(execution),
            ..
        } = &mut missing_evidence.turns[0].state
        else {
            panic!("fixture is a failed terminal");
        };
        execution.terminal_tool_attempts = Vec::new();
        assert_eq!(
            assert_input_rejects_unchanged(missing_evidence),
            AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                turn: failed.turn(),
            }
        );
    }

    /// S02 / S10 / S11 / INV-006: a named continuation call's round
    /// completed, so its window forbids turn-end closures.
    #[test]
    fn s02_s10_s11_inv006_failed_continuation_call_window_forbids_turn_end_closures() {
        let session = current_session();
        let failed = accepted_origin(1);
        let mut closed_request = failed_continuation_call_input(&session, failed);
        closed_request.semantic_entries[2] = SemanticTranscriptEntryReconstitutionInput::new(
            semantic_entry(32).id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolClosed {
                request: tool_request_id(60),
            },
        );
        let AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            terminal_execution: Some(execution),
            ..
        } = &mut closed_request.turns[0].state
        else {
            panic!("fixture is a failed terminal");
        };
        execution.terminal_tool_attempts = Vec::new();
        assert_eq!(
            assert_input_rejects_unchanged(closed_request),
            AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                turn: failed.turn(),
            }
        );
    }

    /// Matching stored facts for one cancelled terminal turn naming its
    /// unsent round-two continuation call: the call's whole frontier is the
    /// completed round's result projection and the terminal frontier extends
    /// it by exactly the cancellation marker.
    fn cancelled_continuation_call_input(
        session: &Session,
        cancelled: OriginFixture,
        successor: OriginFixture,
    ) -> AcceptedInputSchedulingReconstitutionInput {
        let session = session.clone();
        let origin_entry = semantic_entry(30);
        let tool_use_entry = semantic_entry(31);
        let result_entry = semantic_entry(32);
        let cancellation_entry = semantic_entry(33);
        let starting_frontier = frontier(40);
        let call_frontier = frontier(41);
        let terminal_frontier = frontier(42);
        let producing_call = model_call_id(50);
        let producing_attempt = turn_attempt_id(51);
        let terminal_attempt = turn_attempt_id(52);
        let continuation_call = model_call_id(53);
        let request = tool_request_id(60);
        let executed_attempt = ended_tool_attempt(
            &session,
            cancelled,
            terminal_attempt,
            tool_attempt_id(70),
            request,
        );
        let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
            successor.position(),
            cancelled.turn(),
        );
        let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
            command_id(71),
            session.id(),
            cancelled.turn(),
            successor.accepted_input(),
            successor.turn(),
            successor_order,
        )
        .expect("the terminal interrupt is exactly correlated");
        let cancelled_record = cancelled.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                    cancelled.turn(),
                    terminal_attempt,
                    TerminalAttemptEndReconstitutionInput::after_cancellation(
                        CancellationStopDisposition::Cancelled,
                        interrupt,
                    ),
                    Some(continuation_call),
                    interrupt,
                )
                .with_terminal_tool_attempts(vec![executed_attempt]),
                terminal_frontier: terminal_frontier.id(),
            },
        );
        let successor_record = successor.record_with(
            &session,
            OriginRecordFacts {
                order: successor_order,
                delivery: DeliveryRequest::Interrupt {
                    expected_active_turn: cancelled.turn(),
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: PerInputConfigurationChoices::new(
                        SessionConfigurationDefaultsVersion::first(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        );
        let semantic_entries = vec![
            cancelled.entry(&session, origin_entry),
            SemanticTranscriptEntryReconstitutionInput::new(
                tool_use_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call,
                    request,
                },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                result_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                    attempt: tool_attempt_id(70),
                },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                cancellation_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::TurnCancelled {
                    turn: cancelled.turn(),
                },
            ),
        ];
        let target = ResolvedProviderTarget::naming(provider_model_identity(80));
        AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![cancelled_record, successor_record],
            semantic_entries,
            vec![
                starting_frontier.snapshot(&session, &[origin_entry]),
                call_frontier.snapshot(&session, &[origin_entry, tool_use_entry, result_entry]),
                terminal_frontier.snapshot(
                    &session,
                    &[
                        origin_entry,
                        tool_use_entry,
                        result_entry,
                        cancellation_entry,
                    ],
                ),
            ],
            None,
        )
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                cancelled.turn(),
                target,
            )],
            vec![
                ModelCallReconstitutionInput::new(
                    producing_call,
                    cancelled.turn(),
                    producing_attempt,
                    FrozenModelSelection::Direct(direct(1)),
                    target,
                    starting_frontier.id(),
                    ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
                ),
                ModelCallReconstitutionInput::new(
                    continuation_call,
                    cancelled.turn(),
                    terminal_attempt,
                    FrozenModelSelection::Direct(direct(1)),
                    target,
                    call_frontier.id(),
                    ModelCallReconstitutionState::Terminal(ModelCallDisposition::Cancelled),
                ),
            ],
        )
    }

    /// S02 / S07 / S10 / INV-006 / INV-037: a cancelled terminal turn naming
    /// its unsent round-two continuation call reconstitutes when that call's
    /// whole frontier is the completed round's result projection the
    /// cancellation marker extends.
    #[test]
    fn s02_s07_s10_inv006_inv037_cancelled_continuation_call_reconstitutes() {
        let session = current_session();
        let cancelled = accepted_origin(1);
        let successor = accepted_origin(2);
        cancelled_continuation_call_input(&session, cancelled, successor)
            .reconstitute()
            .expect("the cancelled continuation-call terminal shape reconstructs");
    }

    /// S02 / S07 / S10 / INV-006 / INV-037: a cancelled terminal turn naming
    /// a continuation call is accepted only with its round's result evidence.
    #[test]
    fn s02_s07_s10_inv006_inv037_cancelled_continuation_call_requires_round_evidence() {
        let session = current_session();
        let cancelled = accepted_origin(1);
        let successor = accepted_origin(2);
        let mut missing_evidence =
            cancelled_continuation_call_input(&session, cancelled, successor);
        let AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
            terminal_execution, ..
        } = &mut missing_evidence.turns[0].state
        else {
            panic!("fixture is a cancelled terminal");
        };
        terminal_execution.terminal_tool_attempts = Vec::new();
        assert_eq!(
            assert_input_rejects_unchanged(missing_evidence),
            AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                turn: cancelled.turn(),
            }
        );
    }

    /// Matching stored facts for one refused terminal turn naming its
    /// round-two continuation call: the call's whole frontier is the
    /// completed round's result projection and the equal-content terminal
    /// frontier extends it by no entry.
    fn refused_continuation_call_input(
        session: &Session,
        refused: OriginFixture,
    ) -> AcceptedInputSchedulingReconstitutionInput {
        let session = session.clone();
        let origin_entry = semantic_entry(30);
        let tool_use_entry = semantic_entry(31);
        let result_entry = semantic_entry(32);
        let starting_frontier = frontier(40);
        let call_frontier = frontier(41);
        let terminal_frontier = frontier(42);
        let producing_call = model_call_id(50);
        let producing_attempt = turn_attempt_id(51);
        let terminal_attempt = turn_attempt_id(52);
        let continuation_call = model_call_id(53);
        let request = tool_request_id(60);
        let executed_attempt = ended_tool_attempt(
            &session,
            refused,
            terminal_attempt,
            tool_attempt_id(70),
            request,
        );
        let refused_record = refused.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalRefused {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                refusing_attempt: terminal_attempt,
                refusing_attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(
                    UnstoppedAttemptDisposition::TurnRefused,
                ),
                refusing_call: continuation_call,
                terminal_frontier: terminal_frontier.id(),
            },
        );
        let semantic_entries = vec![
            refused.entry(&session, origin_entry),
            SemanticTranscriptEntryReconstitutionInput::new(
                tool_use_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call,
                    request,
                },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                result_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                    attempt: tool_attempt_id(70),
                },
            ),
        ];
        let target = ResolvedProviderTarget::naming(provider_model_identity(80));
        AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![refused_record],
            semantic_entries,
            vec![
                starting_frontier.snapshot(&session, &[origin_entry]),
                call_frontier.snapshot(&session, &[origin_entry, tool_use_entry, result_entry]),
                terminal_frontier.snapshot(&session, &[origin_entry, tool_use_entry, result_entry]),
            ],
            None,
        )
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                refused.turn(),
                target,
            )],
            vec![
                ModelCallReconstitutionInput::new(
                    producing_call,
                    refused.turn(),
                    producing_attempt,
                    FrozenModelSelection::Direct(direct(1)),
                    target,
                    starting_frontier.id(),
                    ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
                ),
                ModelCallReconstitutionInput::new(
                    continuation_call,
                    refused.turn(),
                    terminal_attempt,
                    FrozenModelSelection::Direct(direct(1)),
                    target,
                    call_frontier.id(),
                    ModelCallReconstitutionState::Terminal(ModelCallDisposition::Refused),
                ),
            ],
        )
        .with_continuation_rounds(vec![ContinuationRoundReconstitutionInput::new(
            continuation_call,
            vec![executed_attempt],
            Vec::new(),
        )])
    }

    /// S02 / S10 / INV-006: a refused terminal turn naming its round-two
    /// continuation call reconstitutes when that call's whole frontier is the
    /// completed round's result projection the equal-content terminal
    /// frontier repeats.
    #[test]
    fn s02_s10_inv006_refused_continuation_call_reconstitutes() {
        let session = current_session();
        let refused = accepted_origin(1);
        refused_continuation_call_input(&session, refused)
            .reconstitute()
            .expect("the refused continuation-call terminal shape reconstructs");
    }

    /// S02 / S10 / INV-006: a refused terminal turn naming a continuation
    /// call is accepted only with its round's result evidence.
    #[test]
    fn s02_s10_inv006_refused_continuation_call_requires_round_evidence() {
        let session = current_session();
        let refused = accepted_origin(1);
        let mut missing_evidence = refused_continuation_call_input(&session, refused);
        missing_evidence.continuation_rounds.clear();
        assert_eq!(
            assert_input_rejects_unchanged(missing_evidence),
            AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                turn: refused.turn(),
            }
        );
    }

    /// S02 / S10 / INV-006: a named refused continuation call's round
    /// completed, so its window forbids turn-end closures.
    #[test]
    fn s02_s10_inv006_refused_continuation_call_window_forbids_turn_end_closures() {
        let session = current_session();
        let refused = accepted_origin(1);
        let mut closed_request = refused_continuation_call_input(&session, refused);
        closed_request.semantic_entries[2] = SemanticTranscriptEntryReconstitutionInput::new(
            semantic_entry(32).id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ToolClosed {
                request: tool_request_id(60),
            },
        );
        closed_request.continuation_rounds = vec![ContinuationRoundReconstitutionInput::new(
            model_call_id(53),
            Vec::new(),
            Vec::new(),
        )];
        assert_eq!(
            assert_input_rejects_unchanged(closed_request),
            AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                turn: refused.turn(),
            }
        );
    }

    /// S02 / S10 / INV-006: gate-named continuation-round evidence names each
    /// call at most once.
    #[test]
    fn s02_s10_inv006_continuation_round_evidence_names_each_call_once() {
        let session = current_session();
        let refused = accepted_origin(1);
        let mut duplicate_evidence = refused_continuation_call_input(&session, refused);
        let duplicated = duplicate_evidence.continuation_rounds[0].clone();
        duplicate_evidence.continuation_rounds.push(duplicated);
        assert_eq!(
            assert_input_rejects_unchanged(duplicate_evidence),
            AcceptedInputSchedulingReconstitutionFailure::ContinuationRoundMismatch {
                call: model_call_id(53),
            }
        );
    }

    /// S02 / S10 / INV-006: gate-named continuation-round evidence must name
    /// a call a terminal or recovery gate proves against it.
    #[test]
    fn s02_s10_inv006_continuation_round_evidence_requires_a_naming_gate() {
        let session = current_session();
        let refused = accepted_origin(1);
        let mut dangling_evidence = refused_continuation_call_input(&session, refused);
        dangling_evidence
            .continuation_rounds
            .push(ContinuationRoundReconstitutionInput::new(
                model_call_id(50),
                Vec::new(),
                Vec::new(),
            ));
        assert_eq!(
            assert_input_rejects_unchanged(dangling_evidence),
            AcceptedInputSchedulingReconstitutionFailure::ContinuationRoundMismatch {
                call: model_call_id(50),
            }
        );
    }

    /// Matching stored facts for one reconciliation-required terminal turn
    /// naming its interrupted round-two continuation call: the ambiguous
    /// call's whole frontier is the completed round's result projection and
    /// the equal-content terminal frontier extends it by no entry.
    fn reconciliation_required_continuation_call_input(
        session: &Session,
        reconciled: OriginFixture,
        successor: OriginFixture,
    ) -> AcceptedInputSchedulingReconstitutionInput {
        let session = session.clone();
        let origin_entry = semantic_entry(30);
        let tool_use_entry = semantic_entry(31);
        let result_entry = semantic_entry(32);
        let starting_frontier = frontier(40);
        let call_frontier = frontier(41);
        let terminal_frontier = frontier(42);
        let producing_call = model_call_id(50);
        let producing_attempt = turn_attempt_id(51);
        let terminal_attempt = turn_attempt_id(52);
        let continuation_call = model_call_id(53);
        let request = tool_request_id(60);
        let executed_attempt = ended_tool_attempt(
            &session,
            reconciled,
            terminal_attempt,
            tool_attempt_id(70),
            request,
        );
        let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
            successor.position(),
            reconciled.turn(),
        );
        let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
            command_id(71),
            session.id(),
            reconciled.turn(),
            successor.accepted_input(),
            successor.turn(),
            successor_order,
        )
        .expect("the reconciling interrupt is exactly correlated");
        let reconciled_record = reconciled.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalReconciliationRequired {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                reconciling_attempt: terminal_attempt,
                reconciling_attempt_end: TerminalAttemptEndReconstitutionInput::after_cancellation(
                    CancellationStopDisposition::Lost,
                    interrupt,
                ),
                ambiguous_call: continuation_call,
                authority: AutomaticReconciliationAuthority::AppliedInterrupt(interrupt),
                terminal_frontier: terminal_frontier.id(),
            },
        );
        let successor_record = successor.record_with(
            &session,
            OriginRecordFacts {
                order: successor_order,
                delivery: DeliveryRequest::Interrupt {
                    expected_active_turn: reconciled.turn(),
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: PerInputConfigurationChoices::new(
                        SessionConfigurationDefaultsVersion::first(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        );
        let semantic_entries = vec![
            reconciled.entry(&session, origin_entry),
            SemanticTranscriptEntryReconstitutionInput::new(
                tool_use_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call,
                    request,
                },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                result_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                    attempt: tool_attempt_id(70),
                },
            ),
        ];
        let target = ResolvedProviderTarget::naming(provider_model_identity(80));
        AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![reconciled_record, successor_record],
            semantic_entries,
            vec![
                starting_frontier.snapshot(&session, &[origin_entry]),
                call_frontier.snapshot(&session, &[origin_entry, tool_use_entry, result_entry]),
                terminal_frontier.snapshot(&session, &[origin_entry, tool_use_entry, result_entry]),
            ],
            None,
        )
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                reconciled.turn(),
                target,
            )],
            vec![
                ModelCallReconstitutionInput::new(
                    producing_call,
                    reconciled.turn(),
                    producing_attempt,
                    FrozenModelSelection::Direct(direct(1)),
                    target,
                    starting_frontier.id(),
                    ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
                ),
                ModelCallReconstitutionInput::new(
                    continuation_call,
                    reconciled.turn(),
                    terminal_attempt,
                    FrozenModelSelection::Direct(direct(1)),
                    target,
                    call_frontier.id(),
                    ModelCallReconstitutionState::Terminal(ModelCallDisposition::Ambiguous),
                ),
            ],
        )
        .with_continuation_rounds(vec![ContinuationRoundReconstitutionInput::new(
            continuation_call,
            vec![executed_attempt],
            Vec::new(),
        )])
    }

    /// S04 / S07 / INV-006 / INV-037: a reconciliation-required terminal turn
    /// naming its interrupted round-two continuation call reconstitutes when
    /// that call's whole frontier is the completed round's result projection
    /// the equal-content terminal frontier repeats.
    #[test]
    fn s04_s07_inv006_inv037_reconciliation_required_continuation_call_reconstitutes() {
        let session = current_session();
        let reconciled = accepted_origin(1);
        let successor = accepted_origin(2);
        reconciliation_required_continuation_call_input(&session, reconciled, successor)
            .reconstitute()
            .expect("the reconciliation-required continuation-call terminal shape reconstructs");
    }

    /// S04 / S07 / INV-006 / INV-037: a reconciliation-required terminal turn
    /// naming a continuation call is accepted only with its round's result
    /// evidence.
    #[test]
    fn s04_s07_inv006_inv037_reconciliation_required_continuation_call_requires_round_evidence() {
        let session = current_session();
        let reconciled = accepted_origin(1);
        let successor = accepted_origin(2);
        let mut missing_evidence =
            reconciliation_required_continuation_call_input(&session, reconciled, successor);
        missing_evidence.continuation_rounds.clear();
        assert_eq!(
            assert_input_rejects_unchanged(missing_evidence),
            AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                turn: reconciled.turn(),
            }
        );
    }

    /// Matching stored facts for one active turn parked on the ambiguous
    /// round-two continuation call of a completed tool round: the call's
    /// whole frontier is the completed round's result projection and the
    /// recovery wait extends it by no entry.
    fn recovery_wait_continuation_call_input(
        session: &Session,
        active: OriginFixture,
    ) -> AcceptedInputSchedulingReconstitutionInput {
        let session = session.clone();
        let origin_entry = semantic_entry(30);
        let tool_use_entry = semantic_entry(31);
        let result_entry = semantic_entry(32);
        let starting_frontier = frontier(40);
        let call_frontier = frontier(41);
        let producing_call = model_call_id(50);
        let producing_attempt = turn_attempt_id(51);
        let recovery_attempt = turn_attempt_id(52);
        let continuation_call = model_call_id(53);
        let request = tool_request_id(60);
        let executed_attempt = ended_tool_attempt(
            &session,
            active,
            recovery_attempt,
            tool_attempt_id(70),
            request,
        );
        let active_record = active.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::Active {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                phase: ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery_after_restart(
                    active.turn(),
                    recovery_attempt,
                    continuation_call,
                ),
            },
        );
        let semantic_entries = vec![
            active.entry(&session, origin_entry),
            SemanticTranscriptEntryReconstitutionInput::new(
                tool_use_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                    producing_call,
                    request,
                },
            ),
            SemanticTranscriptEntryReconstitutionInput::new(
                result_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::ToolExecutionResult {
                    attempt: tool_attempt_id(70),
                },
            ),
        ];
        let target = ResolvedProviderTarget::naming(provider_model_identity(80));
        AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![active_record],
            semantic_entries,
            vec![
                starting_frontier.snapshot(&session, &[origin_entry]),
                call_frontier.snapshot(&session, &[origin_entry, tool_use_entry, result_entry]),
            ],
            Some(active.active_tail(&session)),
        )
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                active.turn(),
                target,
            )],
            vec![
                ModelCallReconstitutionInput::new(
                    producing_call,
                    active.turn(),
                    producing_attempt,
                    FrozenModelSelection::Direct(direct(1)),
                    target,
                    starting_frontier.id(),
                    ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
                ),
                ModelCallReconstitutionInput::new(
                    continuation_call,
                    active.turn(),
                    recovery_attempt,
                    FrozenModelSelection::Direct(direct(1)),
                    target,
                    call_frontier.id(),
                    ModelCallReconstitutionState::Terminal(ModelCallDisposition::Ambiguous),
                ),
            ],
        )
        .with_continuation_rounds(vec![ContinuationRoundReconstitutionInput::new(
            continuation_call,
            vec![executed_attempt],
            Vec::new(),
        )])
    }

    /// S04 / INV-025 / INV-026: an active turn parked on the ambiguous
    /// round-two continuation call of a completed tool round reconstitutes
    /// the exact recovery wait when that call's whole frontier is the
    /// completed round's result projection.
    #[test]
    fn s04_inv025_inv026_recovery_wait_continuation_call_reconstitutes() {
        let session = current_session();
        let active = accepted_origin(1);
        let projection = recovery_wait_continuation_call_input(&session, active)
            .reconstitute()
            .expect("the parked continuation-call recovery wait reconstructs");
        let waiting = projection
            .active_turn()
            .expect("the recovery wait retains the progressing slot");

        assert!(matches!(
            waiting.active_phase(),
            Some(ActiveTurnPhase::AwaitingRecoveryDecision {
                ambiguous_operations,
                ..
            }) if ambiguous_operations
                .contains(crate::IssuedOperationRef::ModelCall(model_call_id(53)))
        ));
    }

    /// S04 / INV-025 / INV-026: a recovery wait naming a continuation call is
    /// accepted only with its round's result evidence.
    #[test]
    fn s04_inv025_inv026_recovery_wait_continuation_call_requires_round_evidence() {
        let session = current_session();
        let active = accepted_origin(1);
        let mut missing_evidence = recovery_wait_continuation_call_input(&session, active);
        missing_evidence.continuation_rounds.clear();
        assert_eq!(
            assert_input_rejects_unchanged(missing_evidence),
            AcceptedInputSchedulingReconstitutionFailure::RecoveryModelCallMismatch {
                turn: active.turn(),
            }
        );
    }

    /// S03 / S08 / INV-009 / INV-016: an active scheduling projection
    /// requires the exact session-scoped interval anchored at its origin; a
    /// missing, cross-session, or cross-wired interval fails closed.
    #[test]
    fn active_reconstitution_requires_exact_session_acceptance_tail_identity() {
        let session = current_session();
        let active = accepted_origin(1);

        let missing = assert_reconstitution_rejects_unchanged(ActiveReconstitutionFacts {
            acceptance_tail: None,
            ..ActiveReconstitutionFacts::matching(&session, active)
        });
        assert_eq!(
            missing,
            AcceptedInputSchedulingReconstitutionFailure::MissingActiveAcceptanceTail {
                turn: active.turn(),
            }
        );

        let other_session = session_id(2);
        let mut wrong_session_facts = ActiveReconstitutionFacts::matching(&session, active);
        wrong_session_facts
            .acceptance_tail
            .as_mut()
            .expect("matching facts include the acceptance tail")
            .session = other_session;
        let wrong_session = assert_reconstitution_rejects_unchanged(wrong_session_facts);
        assert_eq!(
            wrong_session,
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailSessionMismatch {
                expected: session.id(),
                actual: other_session,
            }
        );

        let other_anchor = accepted_input_id(99);
        let mut wrong_anchor_facts = ActiveReconstitutionFacts::matching(&session, active);
        wrong_anchor_facts
            .acceptance_tail
            .as_mut()
            .expect("matching facts include the acceptance tail")
            .anchor = other_anchor;
        let wrong_anchor = assert_reconstitution_rejects_unchanged(wrong_anchor_facts);
        assert_eq!(
            wrong_anchor,
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailAnchorMismatch {
                turn: active.turn(),
                expected: active.accepted_input(),
                actual: other_anchor,
            }
        );

        expect![[r#"
            ┌──────────────────────────┬─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact    │ failure                                                                                                                                                                                                             │
            ├──────────────────────────┼─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
            │ active tail omitted      │ MissingActiveAcceptanceTail { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) }                                                                                                                                  │
            │ tail session cross-wired │ AcceptanceTailSessionMismatch { expected: SessionId(00000000-0000-0000-0000-000000000001), actual: SessionId(00000000-0000-0000-0000-000000000002) }                                                                │
            │ tail anchor cross-wired  │ AcceptanceTailAnchorMismatch { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe), expected: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffe), actual: AcceptedInputId(00000000-0000-0000-0000-000000000063) } │
            └──────────────────────────┴─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&table([
            ReconstitutionFailureRow {
                perturbed_stored_fact: "active tail omitted",
                failure: format!("{missing:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "tail session cross-wired",
                failure: format!("{wrong_session:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "tail anchor cross-wired",
                failure: format!("{wrong_anchor:?}"),
            },
        ]));
    }

    /// S03 / S08 / INV-016: every position from the active origin through
    /// the observed session tail is present exactly once and every
    /// pending-steering disposition remains bound to that active turn.
    #[test]
    fn active_reconstitution_rejects_gapped_or_misbound_acceptance_tail() {
        let session = current_session();
        let active = accepted_origin(1);
        let second = accepted_origin(2);
        let third = accepted_origin(3);

        let mut gapped_facts = ActiveReconstitutionFacts::matching(&session, active);
        let gapped_tail = gapped_facts
            .acceptance_tail
            .as_mut()
            .expect("matching facts include the acceptance tail");
        gapped_tail.observed_last_position = third.position();
        gapped_tail
            .entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    second.accepted_input(),
                    AcceptedInputDisposition::PendingSteering {
                        binding: crate::SteeringBinding::new(active.turn()),
                    },
                ),
                third.position(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: active.turn(),
                },
            ));
        let gapped = assert_reconstitution_rejects_unchanged(gapped_facts);
        assert_eq!(
            gapped,
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailPositionMismatch {
                accepted_input: second.accepted_input(),
                expected: second.position(),
                actual: third.position(),
            }
        );

        let other_turn = turn_id(99);
        let mut misbound_facts = ActiveReconstitutionFacts::matching(&session, active);
        let misbound_tail = misbound_facts
            .acceptance_tail
            .as_mut()
            .expect("matching facts include the acceptance tail");
        misbound_tail.observed_last_position = second.position();
        misbound_tail
            .entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    second.accepted_input(),
                    AcceptedInputDisposition::PendingSteering {
                        binding: crate::SteeringBinding::new(other_turn),
                    },
                ),
                second.position(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: other_turn,
                },
            ));
        let misbound = assert_reconstitution_rejects_unchanged(misbound_facts);
        assert_eq!(
            misbound,
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
                accepted_input: second.accepted_input(),
            }
        );

        let after_active_delivery = DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: active.turn(),
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        };
        let mut cross_wired_facts = ActiveReconstitutionFacts::matching(&session, active);
        let cross_wired_tail = cross_wired_facts
            .acceptance_tail
            .as_mut()
            .expect("matching facts include the acceptance tail");
        cross_wired_tail.observed_last_position = third.position();
        cross_wired_tail
            .entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    second.accepted_input(),
                    AcceptedInputDisposition::OriginOf(second.turn()),
                ),
                second.position(),
                after_active_delivery,
            ));
        cross_wired_tail
            .entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    third.accepted_input(),
                    AcceptedInputDisposition::PendingSteering {
                        binding: crate::SteeringBinding::new(active.turn()),
                    },
                ),
                third.position(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: active.turn(),
                },
            ));
        cross_wired_facts.turns.push(second.record_with(
            &session,
            OriginRecordFacts {
                order: AcceptedInputQueueOrder::ordinary(third.position()),
                delivery: after_active_delivery,
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        ));
        let cross_wired = assert_reconstitution_rejects_unchanged(cross_wired_facts);
        assert_eq!(
            cross_wired,
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
                accepted_input: second.accepted_input(),
            }
        );

        expect![[r#"
            ┌────────────────────────────────────┬──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact              │ failure                                                                                                                                                                      │
            ├────────────────────────────────────┼──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
            │ interior position omitted          │ AcceptanceTailPositionMismatch { accepted_input: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffd), expected: SessionInputPosition(2), actual: SessionInputPosition(3) } │
            │ pending steering owner cross-wired │ AcceptanceTailDispositionMismatch { accepted_input: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffd) }                                                                  │
            │ origin position cross-wired        │ AcceptanceTailDispositionMismatch { accepted_input: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffd) }                                                                  │
            └────────────────────────────────────┴──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&table([
            ReconstitutionFailureRow {
                perturbed_stored_fact: "interior position omitted",
                failure: format!("{gapped:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "pending steering owner cross-wired",
                failure: format!("{misbound:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "origin position cross-wired",
                failure: format!("{cross_wired:?}"),
            },
        ]));
    }

    /// S03 / INV-016: a newly active queued origin retains later acceptance
    /// positions already consumed by its terminal predecessor, while only its
    /// own consumed steering reaches the active execution aggregate.
    #[test]
    fn s03_inv016_active_tail_retains_predecessor_consumed_steering() {
        let session = current_session();
        let predecessor = accepted_origin(1);
        let active = accepted_origin(2);
        let predecessor_consumed = accepted_origin(3);
        let active_consumed = accepted_origin(4);
        let predecessor_record = predecessor.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: frontier(70).id(),
                terminal_execution: None,
                terminal_frontier: frontier(71).id(),
            },
        );
        let active_record = active.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::Active {
                starting_lineage: AcceptedInputStartingLineage::After {
                    immediate_predecessor: predecessor.turn(),
                },
                starting_frontier: frontier(72).id(),
                phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                    active.turn(),
                    turn_attempt_id(73),
                ),
            },
        );
        let records = BTreeMap::from([
            (predecessor.turn(), &predecessor_record),
            (active.turn(), &active_record),
        ]);
        let accepted_input_turns = BTreeMap::from([
            (predecessor.accepted_input(), predecessor.turn()),
            (active.accepted_input(), active.turn()),
        ]);
        let execution_position_by_turn =
            BTreeMap::from([(predecessor.turn(), 0), (active.turn(), 1)]);
        let tail_input = SessionAcceptanceTailReconstitutionInput::new(
            session.id(),
            active.accepted_input(),
            active_consumed.position(),
            vec![
                SessionAcceptanceTailEntryReconstitutionInput::new(
                    session.id(),
                    AcceptedInputLifecycle::new(
                        active.accepted_input(),
                        AcceptedInputDisposition::OriginOf(active.turn()),
                    ),
                    active.position(),
                    default_origin_delivery(),
                ),
                SessionAcceptanceTailEntryReconstitutionInput::new(
                    session.id(),
                    AcceptedInputLifecycle::new(
                        predecessor_consumed.accepted_input(),
                        AcceptedInputDisposition::ConsumedAsSteering {
                            call: model_call_id(74),
                        },
                    ),
                    predecessor_consumed.position(),
                    DeliveryRequest::NextSafePoint {
                        expected_active_turn: predecessor.turn(),
                    },
                ),
                SessionAcceptanceTailEntryReconstitutionInput::new(
                    session.id(),
                    AcceptedInputLifecycle::new(
                        active_consumed.accepted_input(),
                        AcceptedInputDisposition::ConsumedAsSteering {
                            call: model_call_id(75),
                        },
                    ),
                    active_consumed.position(),
                    DeliveryRequest::NextSafePoint {
                        expected_active_turn: active.turn(),
                    },
                ),
            ],
        );
        let tail = reconstitute_active_acceptance_tail(
            session.id(),
            Some(active.turn()),
            Some(&tail_input),
            ActiveAcceptanceTailReconstitutionEvidence {
                records_by_turn: &records,
                accepted_input_turns: &accepted_input_turns,
                consumed_inputs: &BTreeMap::from([
                    (predecessor_consumed.accepted_input(), model_call_id(74)),
                    (active_consumed.accepted_input(), model_call_id(75)),
                ]),
                preceding_non_accepted_terminals: &BTreeSet::new(),
                execution_position_by_turn: &execution_position_by_turn,
            },
        )
        .expect("the terminal predecessor's consumed steering remains valid history")
        .expect("an active turn retains its complete acceptance tail");
        let (pending, consumed) = active_execution_steering_inputs(active.turn(), &tail);

        assert!(pending.is_empty());
        assert_eq!(consumed.len(), 1);
        assert_eq!(
            consumed[0].accepted_input(),
            active_consumed.accepted_input()
        );
        assert_eq!(consumed[0].source_turn(), active.turn());

        let mut cross_wired_tail = tail_input.clone();
        cross_wired_tail.entries[1] = SessionAcceptanceTailEntryReconstitutionInput::new(
            session.id(),
            AcceptedInputLifecycle::new(
                predecessor_consumed.accepted_input(),
                AcceptedInputDisposition::ConsumedAsSteering {
                    call: model_call_id(76),
                },
            ),
            predecessor_consumed.position(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: predecessor.turn(),
            },
        );
        let failure = reconstitute_active_acceptance_tail(
            session.id(),
            Some(active.turn()),
            Some(&cross_wired_tail),
            ActiveAcceptanceTailReconstitutionEvidence {
                records_by_turn: &records,
                accepted_input_turns: &accepted_input_turns,
                consumed_inputs: &BTreeMap::from([
                    (predecessor_consumed.accepted_input(), model_call_id(74)),
                    (active_consumed.accepted_input(), model_call_id(75)),
                ]),
                preceding_non_accepted_terminals: &BTreeSet::new(),
                execution_position_by_turn: &execution_position_by_turn,
            },
        )
        .expect_err("cross-wired historical steering must fail closed");

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
                accepted_input: predecessor_consumed.accepted_input(),
            }
        );
    }

    /// S03 / INV-016: later-accepted interrupt work executes before the
    /// ordinary origin it displaced, so steering consumed by that interrupt
    /// remains historical rather than becoming active execution input.
    #[test]
    fn s03_inv016_active_tail_rejects_unproven_historical_consumed_steering() {
        let session = current_session();
        let predecessor = accepted_origin(1);
        let active = accepted_origin(2);
        let interrupt_successor = accepted_origin(3);
        let interrupt_consumed = accepted_origin(4);
        let mut input = active_input_after_historical_interrupt(
            &session,
            predecessor,
            active,
            interrupt_successor,
        );
        let tail = input
            .active_acceptance_tail
            .as_mut()
            .expect("the historical-interrupt helper supplies an active tail");
        tail.observed_last_position = interrupt_consumed.position();
        tail.entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    interrupt_consumed.accepted_input(),
                    AcceptedInputDisposition::ConsumedAsSteering {
                        call: model_call_id(76),
                    },
                ),
                interrupt_consumed.position(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: interrupt_successor.turn(),
                },
            ));
        let failure = assert_input_rejects_unchanged(input);

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
                accepted_input: interrupt_consumed.accepted_input(),
            }
        );
    }

    /// S03 / S09 / INV-009 / INV-016: a scheduler-gap start remains
    /// a valid ordinary origin after an earlier queued turn becomes active.
    #[test]
    fn active_reconstitution_preserves_post_anchor_scheduler_gap_start() {
        let session = current_session();
        let origins = FailedPredecessorPostAnchorOrigins {
            predecessor: accepted_origin(1),
            active: accepted_origin(2),
            queued: accepted_origin(3),
        };
        active_input_after_failed_predecessor_with_post_anchor_origin(
            &session,
            origins,
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        )
        .reconstitute()
        .expect("the later origin was accepted during a valid scheduler gap");
    }

    /// S03 / S09 / INV-009 / INV-016: an ordinary queued origin
    /// retains the historical active target named at acceptance.
    #[test]
    fn active_reconstitution_preserves_post_anchor_historical_target() {
        let session = current_session();
        let origins = FailedPredecessorPostAnchorOrigins {
            predecessor: accepted_origin(1),
            active: accepted_origin(2),
            queued: accepted_origin(3),
        };
        active_input_after_failed_predecessor_with_post_anchor_origin(
            &session,
            origins,
            DeliveryRequest::AfterCurrentTurn {
                expected_active_turn: origins.predecessor.turn(),
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        )
        .reconstitute()
        .expect("the later origin retains its exact previously active target");
    }

    /// S03 / S09 / INV-009 / INV-016: after-current delivery must
    /// name an earlier nonqueued target in the complete turn inventory.
    #[test]
    fn active_reconstitution_rejects_missing_historical_delivery_target() {
        let session = current_session();
        let origins = PostAnchorOrigins {
            active: accepted_origin(1),
            queued: accepted_origin(2),
        };
        let missing_target_turn = turn_id(99);
        let missing_target = active_input_with_post_anchor_origin(
            &session,
            origins,
            DeliveryRequest::AfterCurrentTurn {
                expected_active_turn: missing_target_turn,
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        )
        .reconstitute()
        .expect_err("after-current delivery requires its historical target record");
        assert_eq!(
            missing_target.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
                turn: origins.queued.turn(),
            }
        );
    }

    /// S03 / S07 / INV-009 / INV-016: an interrupt delivery must
    /// agree with the origin record's durable interrupt-priority relation.
    #[test]
    fn active_reconstitution_rejects_delivery_priority_mismatch() {
        let session = current_session();
        let origins = PostAnchorOrigins {
            active: accepted_origin(1),
            queued: accepted_origin(2),
        };
        let wrong_priority = active_input_with_post_anchor_origin(
            &session,
            origins,
            DeliveryRequest::Interrupt {
                expected_active_turn: origins.active.turn(),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        )
        .reconstitute()
        .expect_err("interrupt delivery cannot carry ordinary queue priority");
        assert_eq!(
            wrong_priority.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
                turn: origins.queued.turn(),
            }
        );
    }

    /// S01 / INV-009 / INV-016: origin delivery and queue facts
    /// are validated even when no active turn requires an acceptance tail.
    #[test]
    fn s01_inv009_inv016_queued_reconstitution_rejects_delivery_order_mismatch() {
        let session = current_session();
        let queued = accepted_origin(1);
        let no_semantic_entries = Vec::new();
        let no_snapshots = Vec::new();
        let no_active_acceptance_tail = None;
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![queued.record_with(
                &session,
                OriginRecordFacts {
                    order: queued.ordinary_order(),
                    delivery: DeliveryRequest::NextSafePoint {
                        expected_active_turn: turn_id(99),
                    },
                    state: AcceptedInputTurnSchedulingRecordState::Queued,
                },
            )],
            no_semantic_entries,
            no_snapshots,
            no_active_acceptance_tail,
        );

        let error = input
            .reconstitute()
            .expect_err("steering-only delivery cannot reconstruct queued turn work");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
                turn: queued.turn(),
            }
        );
    }

    /// S01 / INV-008 / INV-009 / INV-016: a configured origin's
    /// accepted defaults version must equal its frozen provenance version.
    #[test]
    fn s01_inv008_inv009_inv016_queued_origin_rejects_defaults_version_mismatch() {
        let session = current_session();
        let queued = accepted_origin(1);
        let mismatched_version = SessionConfigurationDefaultsVersion::try_from_u64(2)
            .expect("the mismatched test version is positive");
        let no_semantic_entries = Vec::new();
        let no_snapshots = Vec::new();
        let no_active_acceptance_tail = None;
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![queued.record_with(
                &session,
                OriginRecordFacts {
                    order: queued.ordinary_order(),
                    delivery: DeliveryRequest::StartWhenNoActiveTurn {
                        configuration: PerInputConfigurationChoices::new(
                            mismatched_version,
                            ModelSelectionOverride::UseSessionDefault,
                        ),
                    },
                    state: AcceptedInputTurnSchedulingRecordState::Queued,
                },
            )],
            no_semantic_entries,
            no_snapshots,
            no_active_acceptance_tail,
        );

        let error = input
            .reconstitute()
            .expect_err("accepted delivery and frozen provenance versions must agree");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
                turn: queued.turn(),
            }
        );
    }

    /// S01 / INV-008 / INV-009 / INV-016: an explicit accepted
    /// model request must equal the request retained by frozen provenance.
    #[test]
    fn s01_inv008_inv009_inv016_queued_origin_rejects_explicit_request_mismatch() {
        let session = current_session();
        let queued = accepted_origin(1);
        let requested = ModelSelectionRequest::Direct(direct(99));
        let no_semantic_entries = Vec::new();
        let no_snapshots = Vec::new();
        let no_active_acceptance_tail = None;
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![queued.record_with(
                &session,
                OriginRecordFacts {
                    order: queued.ordinary_order(),
                    delivery: DeliveryRequest::StartWhenNoActiveTurn {
                        configuration: PerInputConfigurationChoices::new(
                            SessionConfigurationDefaultsVersion::first(),
                            ModelSelectionOverride::ReplaceWith(requested),
                        ),
                    },
                    state: AcceptedInputTurnSchedulingRecordState::Queued,
                },
            )],
            no_semantic_entries,
            no_snapshots,
            no_active_acceptance_tail,
        );

        let error = input
            .reconstitute()
            .expect_err("explicit delivery request and frozen provenance must agree");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
                turn: queued.turn(),
            }
        );
    }

    /// S03 / INV-008 / INV-016: the tail repeats the exact
    /// immutable versioned delivery stored for its origin rather than
    /// supplying an independently plausible configuration choice.
    #[test]
    fn active_reconstitution_rejects_origin_delivery_configuration_mismatch() {
        let session = current_session();
        let active = accepted_origin(1);
        let mut facts = ActiveReconstitutionFacts::matching(&session, active);
        facts
            .acceptance_tail
            .as_mut()
            .expect("matching facts include the active tail")
            .entries[0]
            .delivery = DeliveryRequest::StartWhenNoActiveTurn {
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::try_from_u64(2)
                    .expect("the mismatched test version is positive"),
                ModelSelectionOverride::UseSessionDefault,
            ),
        };

        let error = assert_reconstitution_rejects_unchanged(facts);
        assert_eq!(
            error,
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
                accepted_input: active.accepted_input(),
            }
        );
    }

    /// S03 / S07 / INV-001 / INV-009: an accepted interrupt
    /// against the current owner prevents evidence-free phase reconstruction.
    #[test]
    fn active_reconstitution_rejects_interrupt_evidence_for_evidence_free_phase() {
        let session = current_session();
        let origins = PostAnchorOrigins {
            active: accepted_origin(1),
            queued: accepted_origin(2),
        };
        let delivery = DeliveryRequest::Interrupt {
            expected_active_turn: origins.active.turn(),
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        };
        let mut input = active_input_with_post_anchor_origin(&session, origins, delivery);
        input.turns[1] = origins.queued.record_with(
            &session,
            OriginRecordFacts {
                order: AcceptedInputQueueOrder::interrupt_immediately_after(
                    origins.queued.position(),
                    origins.active.turn(),
                ),
                delivery,
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        );

        let error = input
            .reconstitute()
            .expect_err("applied interrupt evidence requires a proof-bearing phase projection");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                turn: origins.active.turn(),
                accepted_input: origins.queued.accepted_input(),
            }
        );
    }

    /// S03 / S07 / INV-029 / INV-037: a historical interrupt in the active
    /// acceptance tail retains the target terminal's exact stop proof.
    #[test]
    fn s03_s07_inv029_inv037_historical_interrupt_requires_target_stop_proof() {
        let session = current_session();
        let predecessor = accepted_origin(1);
        let active = accepted_origin(2);
        let interrupt_successor = accepted_origin(3);
        let matching = active_input_after_historical_interrupt(
            &session,
            predecessor,
            active,
            interrupt_successor,
        );
        matching
            .clone()
            .reconstitute()
            .expect("the exact historical interrupt proof remains admissible");

        let mut missing_proof = matching;
        let AcceptedInputTurnSchedulingRecordState::TerminalFailed {
            terminal_execution, ..
        } = &mut missing_proof.turns[0].state
        else {
            panic!("the historical target fixture is terminal failed");
        };
        *terminal_execution = None;
        assert_eq!(
            assert_input_rejects_unchanged(missing_proof),
            AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
                turn: active.turn(),
                accepted_input: interrupt_successor.accepted_input(),
            }
        );
    }

    /// S03 / S08 / INV-009 / INV-016: one accepted input cannot
    /// be both pending steering and a turn origin in the scheduling inventory.
    #[test]
    fn active_reconstitution_rejects_pending_identity_that_is_also_an_origin() {
        let session = current_session();
        let active = accepted_origin(1);
        let pending = accepted_origin(2);
        let mut tail = active.active_tail(&session);
        tail.observed_last_position = pending.position();
        tail.entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    pending.accepted_input(),
                    AcceptedInputDisposition::PendingSteering {
                        binding: crate::SteeringBinding::new(active.turn()),
                    },
                ),
                pending.position(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: active.turn(),
                },
            ));

        active_input(&session, active, Some(tail.clone()))
            .reconstitute()
            .expect("pending steering remains distinct from every origin");

        let mut aliased = active_input(&session, active, Some(tail));
        aliased
            .turns
            .push(pending.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
        let aliased = aliased
            .reconstitute()
            .expect_err("pending steering cannot reuse a turn-origin identity");
        assert_eq!(
            aliased.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
                accepted_input: pending.accepted_input(),
            }
        );
    }

    /// S02 / S08 / INV-012 / INV-036: a prepared call consumes the complete
    /// pending prefix; durable history cannot claim that it skipped an earlier
    /// pending input and consumed a later one.
    #[test]
    fn s02_s08_inv012_inv036_active_tail_rejects_consumed_after_pending() {
        let session = current_session();
        let active = accepted_origin(1);
        let pending = accepted_origin(2);
        let consumed = accepted_origin(3);
        let mut tail = active.active_tail(&session);
        tail.observed_last_position = consumed.position();
        tail.entries.extend([
            SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    pending.accepted_input(),
                    AcceptedInputDisposition::PendingSteering {
                        binding: crate::SteeringBinding::new(active.turn()),
                    },
                ),
                pending.position(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: active.turn(),
                },
            ),
            SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    consumed.accepted_input(),
                    AcceptedInputDisposition::ConsumedAsSteering {
                        call: model_call_id(91),
                    },
                ),
                consumed.position(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: active.turn(),
                },
            ),
        ]);

        let error = active_input(&session, active, Some(tail))
            .reconstitute()
            .expect_err("a later consumed receipt cannot skip earlier pending steering");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
                accepted_input: consumed.accepted_input(),
            }
        );
    }

    /// S03 / S08 / INV-007 / INV-016: a pending tail entry cannot
    /// replace a different origin that owns the same acceptance position.
    #[test]
    fn active_reconstitution_rejects_pending_position_owned_by_an_origin() {
        let session = current_session();
        let active = accepted_origin(1);
        let origin = accepted_origin(2);
        let pending = accepted_origin(3);
        let mut tail = active.active_tail(&session);
        tail.observed_last_position = origin.position();
        tail.entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    pending.accepted_input(),
                    AcceptedInputDisposition::PendingSteering {
                        binding: crate::SteeringBinding::new(active.turn()),
                    },
                ),
                origin.position(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: active.turn(),
                },
            ));
        let mut input = active_input(&session, active, Some(tail));
        input
            .turns
            .push(origin.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));

        let error = input
            .reconstitute()
            .expect_err("the complete tail cannot replace an origin at the same position");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailDispositionMismatch {
                accepted_input: pending.accepted_input(),
            }
        );
    }

    /// S03 / INV-016: the last represented position must equal
    /// the authoritative session tail observed by the same read.
    #[test]
    fn active_reconstitution_rejects_incomplete_claimed_acceptance_tail() {
        let session = current_session();
        let active = accepted_origin(1);
        let next = accepted_origin(2);
        let mut incomplete = active.active_tail(&session);
        incomplete.observed_last_position = next.position();
        let incomplete = active_input(&session, active, Some(incomplete))
            .reconstitute()
            .expect_err("the represented interval must reach the claimed session tail");
        assert_eq!(
            incomplete.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailLastPositionMismatch {
                expected: next.position(),
                actual: Some(active.position()),
            }
        );
    }

    /// S03 / INV-009 / INV-016: the claimed session observation
    /// cannot end before a later origin supplied by the same scheduling read.
    #[test]
    fn s03_inv009_inv016_active_tail_reaches_every_known_origin() {
        let session = current_session();
        let origins = PostAnchorOrigins {
            active: accepted_origin(1),
            queued: accepted_origin(2),
        };
        let mut input = active_input_with_post_anchor_origin(
            &session,
            origins,
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        );
        let tail = input
            .active_acceptance_tail
            .as_mut()
            .expect("the helper supplies an active tail");
        tail.observed_last_position = origins.active.position();
        tail.entries.truncate(1);

        let error = input
            .reconstitute()
            .expect_err("a known later origin disproves the claimed tail observation");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailLastPositionMismatch {
                expected: origins.active.position(),
                actual: Some(origins.queued.position()),
            }
        );
    }

    /// S03 / INV-009: a current attempt owned by another turn cannot
    /// reconstruct an active aggregate.
    #[test]
    fn s03_active_reconstitution_rejects_cross_wired_attempt_owner() {
        let session = current_session();
        let active = accepted_origin(1);
        let other_turn = turn_id(99);
        let attempt = matching_active_attempt();
        let mut facts = ActiveReconstitutionFacts::matching(&session, active);
        facts.replace_active_phase(ActiveTurnSchedulingReconstitutionInput::prepared(
            other_turn, attempt,
        ));
        let error = assert_reconstitution_rejects_unchanged(facts);
        assert_eq!(
            error,
            AcceptedInputSchedulingReconstitutionFailure::CurrentAttemptOwnershipMismatch {
                turn: active.turn(),
                attempt,
            }
        );
    }

    /// S03 / INV-009: eligibility derives the target from complete durable
    /// order and cannot be directed to skip earlier queued work.
    #[test]
    fn s03_eligibility_consumes_the_earliest_queued_origin() {
        let session = current_session();
        let later = accepted_origin(2);
        let earlier = accepted_origin(1);
        let later_record = later.record(&session, AcceptedInputTurnSchedulingRecordState::Queued);
        let earlier_record =
            earlier.record(&session, AcceptedInputTurnSchedulingRecordState::Queued);
        let no_semantic_entries = Vec::new();
        let no_snapshots = Vec::new();
        let activation = activation(1);
        let no_active_acceptance_tail = None;
        let candidate = AcceptedInputSchedulingReconstitutionInput::new(
            session,
            vec![later_record, earlier_record],
            no_semantic_entries,
            no_snapshots,
            no_active_acceptance_tail,
        )
        .reconstitute()
        .expect("the complete queue order is valid")
        .prepare_earliest_queued_activation(activation.identities())
        .expect("no active slot blocks the earliest queued work");

        assert_eq!(candidate.turn().turn(), earlier.turn());
        assert_eq!(
            candidate.turn().accepted_input().id(),
            earlier.accepted_input()
        );
    }

    /// S09 / INV-009 / INV-015: the earliest queued successor starts only
    /// after the exact immediately preceding failed turn and retains its
    /// complete origin-then-failure terminal prefix before appending its own
    /// origin.
    #[test]
    fn s09_successor_uses_exact_failed_predecessor_terminal_frontier() {
        let session = current_session();
        let predecessor = accepted_origin(1);
        let successor = accepted_origin(2);
        let predecessor_origin_entry = semantic_entry(30);
        let predecessor_failure_entry = semantic_entry(31);
        let predecessor_starting_frontier = frontier(40);
        let predecessor_terminal_frontier = frontier(41);
        let activation = activation(1);
        let no_active_acceptance_tail = None;
        let predecessor_record = predecessor.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: predecessor_starting_frontier.id(),
                terminal_execution: None,
                terminal_frontier: predecessor_terminal_frontier.id(),
            },
        );
        let successor_record =
            successor.record(&session, AcceptedInputTurnSchedulingRecordState::Queued);
        let projection = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![successor_record, predecessor_record],
            vec![
                predecessor_failure_entry.failed_turn(&session, predecessor),
                predecessor.entry(&session, predecessor_origin_entry),
            ],
            vec![
                predecessor_terminal_frontier.snapshot(
                    &session,
                    &[predecessor_origin_entry, predecessor_failure_entry],
                ),
                predecessor_starting_frontier.snapshot(&session, &[predecessor_origin_entry]),
            ],
            no_active_acceptance_tail,
        )
        .reconstitute()
        .expect("the failed predecessor has a complete validated frontier");

        let candidate = projection
            .prepare_earliest_queued_activation(activation.identities())
            .expect("the successor is the earliest queued turn with no active slot");

        assert_eq!(candidate.turn().turn(), successor.turn());
        assert_eq!(
            candidate.start().lineage(),
            AcceptedInputStartingLineage::After {
                immediate_predecessor: predecessor.turn(),
            }
        );
        assert_eq!(
            candidate
                .starting_snapshot()
                .ordered_entries()
                .collect::<Vec<_>>(),
            vec![
                predecessor_origin_entry.reference(&session),
                predecessor_failure_entry.reference(&session),
                activation.origin_entry().reference(&session),
            ]
        );
    }

    /// S33 / INV-015 / INV-046: an actual frozen direct-model transition
    /// inserts exactly one typed identity boundary between the predecessor
    /// terminal frontier and the successor origin.
    #[test]
    fn s33_inv015_inv046_model_transition_extends_frontier_before_origin() {
        let session = current_session();
        let predecessor = accepted_origin(1);
        let successor = accepted_origin(2);
        let successor_selection = direct(2);
        let predecessor_origin_entry = semantic_entry(30);
        let predecessor_failure_entry = semantic_entry(31);
        let predecessor_starting_frontier = frontier(40);
        let predecessor_terminal_frontier = frontier(41);
        let activation = activation(1);
        let successor_choices = PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Direct(successor_selection)),
        );
        let successor_configuration = OriginConfiguration::freeze(
            session
                .current_configuration_defaults()
                .derive_request(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Direct(
                        successor_selection,
                    )),
                )
                .expect("the override is derived from current defaults"),
            |_| None,
        )
        .expect("the direct selection needs no alias resolution");
        let predecessor_record = predecessor.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: predecessor_starting_frontier.id(),
                terminal_execution: None,
                terminal_frontier: predecessor_terminal_frontier.id(),
            },
        );
        let successor_record = AcceptedInputTurnSchedulingRecord::new(
            session.id(),
            successor.turn(),
            session.id(),
            AcceptedInputLifecycle::new(
                successor.accepted_input(),
                AcceptedInputDisposition::OriginOf(successor.turn()),
            ),
            session.id(),
            successor.turn(),
            successor.ordinary_order(),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: successor_choices,
            },
            successor_configuration,
            AcceptedInputTurnSchedulingRecordState::Queued,
        );
        let projection = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![successor_record, predecessor_record],
            vec![
                predecessor_failure_entry.failed_turn(&session, predecessor),
                predecessor.entry(&session, predecessor_origin_entry),
            ],
            vec![
                predecessor_terminal_frontier.snapshot(
                    &session,
                    &[predecessor_origin_entry, predecessor_failure_entry],
                ),
                predecessor_starting_frontier.snapshot(&session, &[predecessor_origin_entry]),
            ],
            None,
        )
        .reconstitute()
        .expect("the predecessor and changed successor are fully correlated");

        let candidate = projection
            .prepare_earliest_queued_activation(activation.identities())
            .expect("the changed successor is eligible");

        assert_eq!(candidate.starting_entries().len(), 2);
        assert_eq!(
            candidate.starting_entries()[0].identity(),
            activation.model_identity_entry().id()
        );
        assert_eq!(
            candidate.starting_entries()[0].payload(),
            &SemanticTranscriptEntryPayload::ModelIdentityChanged {
                turn: successor.turn(),
                defaults_version: SessionConfigurationDefaultsVersion::first(),
                selected: successor_selection,
            }
        );
        assert_eq!(
            candidate
                .starting_snapshot()
                .ordered_entries()
                .collect::<Vec<_>>(),
            vec![
                predecessor_origin_entry.reference(&session),
                predecessor_failure_entry.reference(&session),
                activation.model_identity_entry().reference(&session),
                activation.origin_entry().reference(&session),
            ]
        );
    }

    /// INV-015 / INV-046: a durable legacy marker admits only a start whose
    /// frontier was committed before model-identity boundaries existed.
    #[test]
    fn inv015_inv046_legacy_start_grandfathers_its_historical_frontier() {
        let session = current_session();
        let predecessor = accepted_origin(1);
        let active = accepted_origin(2);
        let queued = accepted_origin(3);
        let predecessor_selection = direct(2);
        let predecessor_choices = PerInputConfigurationChoices::new(
            SessionConfigurationDefaultsVersion::first(),
            ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Direct(
                predecessor_selection,
            )),
        );
        let predecessor_configuration = OriginConfiguration::freeze(
            session
                .current_configuration_defaults()
                .derive_request(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Direct(
                        predecessor_selection,
                    )),
                )
                .expect("the legacy override is derived from current defaults"),
            |_| None,
        )
        .expect("the direct selection needs no alias resolution");
        let mut input = active_input_after_failed_predecessor_with_post_anchor_origin(
            &session,
            FailedPredecessorPostAnchorOrigins {
                predecessor,
                active,
                queued,
            },
            DeliveryRequest::AfterCurrentTurn {
                expected_active_turn: active.turn(),
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        );
        input.turns[0].origin_delivery = DeliveryRequest::StartWhenNoActiveTurn {
            configuration: predecessor_choices,
        };
        input.turns[0].origin_configuration = predecessor_configuration.clone();
        input.turns[0].configuration_provenance =
            TurnConfigurationProvenance::ExplicitOrigin(predecessor_configuration);

        let strict = input
            .clone()
            .reconstitute()
            .expect_err("a post-migration start cannot omit its changed-model boundary");
        assert_eq!(
            strict.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch {
                turn: active.turn(),
            }
        );

        input.turns[1] = input.turns[1]
            .clone()
            .without_legacy_model_identity_boundary();
        input
            .reconstitute()
            .expect("the durable legacy bit retains the historical marker-free frontier");
    }

    /// S08 / S09 / INV-008 / INV-009 / INV-016: terminally reclassified
    /// steering becomes ordinary queued work at its original position and
    /// inherits the source turn's canonical configuration.
    #[test]
    fn s08_s09_inv008_inv009_inv016_reclassified_steering_becomes_eligible_work() {
        let session = current_session();
        let predecessor = accepted_origin(1);
        let successor = accepted_origin(2);
        let predecessor_origin_entry = semantic_entry(30);
        let predecessor_failure_entry = semantic_entry(31);
        let predecessor_starting_frontier = frontier(40);
        let predecessor_terminal_frontier = frontier(41);
        let activation = activation(1);
        let source_configuration = configuration(&session);
        let binding = crate::SteeringBinding::new(predecessor.turn());
        let predecessor_record = predecessor.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: predecessor_starting_frontier.id(),
                terminal_execution: None,
                terminal_frontier: predecessor_terminal_frontier.id(),
            },
        );
        let successor_record = AcceptedInputTurnSchedulingRecord::reclassified(
            session.id(),
            successor.turn(),
            session.id(),
            AcceptedInputLifecycle::new(
                successor.accepted_input(),
                AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                    turn: successor.turn(),
                    reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
                },
            ),
            session.id(),
            successor.turn(),
            successor.ordinary_order(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: predecessor.turn(),
            },
            binding,
            source_configuration.clone(),
            AcceptedInputTurnSchedulingRecordState::Queued,
        );
        let mismatched_delivery_record = AcceptedInputTurnSchedulingRecord::reclassified(
            session.id(),
            successor.turn(),
            session.id(),
            AcceptedInputLifecycle::new(
                successor.accepted_input(),
                AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                    turn: successor.turn(),
                    reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
                },
            ),
            session.id(),
            successor.turn(),
            successor.ordinary_order(),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: turn_id(99),
            },
            binding,
            source_configuration.clone(),
            AcceptedInputTurnSchedulingRecordState::Queued,
        );
        let semantic_entries = vec![
            predecessor_failure_entry.failed_turn(&session, predecessor),
            predecessor.entry(&session, predecessor_origin_entry),
        ];
        let snapshots = vec![
            predecessor_terminal_frontier.snapshot(
                &session,
                &[predecessor_origin_entry, predecessor_failure_entry],
            ),
            predecessor_starting_frontier.snapshot(&session, &[predecessor_origin_entry]),
        ];
        let error = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![mismatched_delivery_record, predecessor_record.clone()],
            semantic_entries.clone(),
            snapshots.clone(),
            None,
        )
        .reconstitute()
        .expect_err("stored reclassified delivery must agree with its exact source binding");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::OriginDeliveryMismatch {
                turn: successor.turn(),
            }
        );
        let projection = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![successor_record, predecessor_record],
            semantic_entries,
            snapshots,
            None,
        )
        .reconstitute()
        .expect("reclassified steering is correlated to its terminal source");

        let candidate = projection
            .prepare_earliest_queued_activation(activation.identities())
            .expect("the reclassified successor is eligible after its source");

        assert_eq!(candidate.turn().turn(), successor.turn());
        assert_eq!(candidate.turn().order(), successor.ordinary_order());
        assert_eq!(candidate.turn().configuration(), &source_configuration);
        assert_eq!(
            candidate.turn().configuration_provenance(),
            &TurnConfigurationProvenance::InheritedForReclassifiedSteering(binding)
        );
    }

    #[track_caller]
    fn assert_failed_terminal_call_provenance_is_complete(
        session: &Session,
        failed: OriginFixture,
        attempt: TurnAttemptId,
        call_disposition: ModelCallDisposition,
    ) {
        let origin_entry = FailedTerminalReconstitutionFacts::matching_origin_entry();
        let failure_entry = FailedTerminalReconstitutionFacts::matching_failure_entry();
        let steering_entry = semantic_entry(32);
        let consumed = accepted_origin(2);
        let call_frontier = frontier(42);
        let terminal_frontier = FailedTerminalReconstitutionFacts::matching_terminal_frontier();
        let call_id = model_call_id(50);
        let mut facts = FailedTerminalReconstitutionFacts::matching(session, failed);
        facts
            .semantic_entries
            .push(SemanticTranscriptEntryReconstitutionInput::new(
                steering_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                    accepted_input: consumed.accepted_input(),
                    source_turn: failed.turn(),
                },
            ));
        facts
            .snapshots
            .retain(|snapshot| snapshot.snapshot() != terminal_frontier.id());
        facts.snapshots.extend([
            call_frontier.snapshot(session, &[origin_entry, steering_entry]),
            terminal_frontier.snapshot(session, &[origin_entry, steering_entry, failure_entry]),
        ]);
        facts.replace_terminal_execution(Some(FailedTurnExecutionReconstitutionInput::with_call(
            failed.turn(),
            attempt,
            UnstoppedAttemptDisposition::KnownFailure,
            call_id,
        )));
        let call = ModelCallReconstitutionInput::new(
            call_id,
            failed.turn(),
            attempt,
            FrozenModelSelection::Direct(direct(1)),
            ResolvedProviderTarget::naming(provider_model_identity(51)),
            call_frontier.id(),
            ModelCallReconstitutionState::Terminal(call_disposition),
        );
        let projection = facts
            .input()
            .with_model_call_facts(
                vec![crate::PinnedProviderTargetReconstitutionInput::new(
                    failed.turn(),
                    call.target(),
                )],
                vec![call],
            )
            .with_consumed_steering_facts(vec![ConsumedSteeringReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    consumed.accepted_input(),
                    AcceptedInputDisposition::ConsumedAsSteering { call: call_id },
                ),
                consumed.position(),
                failed.turn(),
            )])
            .reconstitute()
            .expect("failed terminal call provenance is fully correlated");
        assert_eq!(
            projection.attempt_owners.get(&attempt),
            Some(&failed.turn())
        );
        assert!(
            projection
                .semantic_entries
                .contains_key(&origin_entry.reference(session))
        );
    }

    /// S02 / S03 / INV-006 / INV-016: failed-terminal reconstitution
    /// preserves all three accepted execution shapes and any steering already
    /// committed in an ended call's source frontier.
    #[test]
    fn s02_s03_inv006_failed_terminal_execution_provenance_is_complete() {
        let session = current_session();
        let failed = accepted_origin(1);
        let attempt = turn_attempt_id(60);

        let direct_failure = FailedTerminalReconstitutionFacts::matching(&session, failed)
            .input()
            .reconstitute()
            .expect("a direct static failure has no execution provenance");
        assert!(direct_failure.attempt_owners.is_empty());

        let mut attempt_only_facts = FailedTerminalReconstitutionFacts::matching(&session, failed);
        attempt_only_facts.replace_terminal_execution(Some(
            FailedTurnExecutionReconstitutionInput::attempt_only(
                failed.turn(),
                attempt,
                UnstoppedAttemptDisposition::Lost,
            ),
        ));
        let attempt_only = attempt_only_facts
            .input()
            .reconstitute()
            .expect("startup loss retains its exact ended attempt");
        assert_eq!(
            attempt_only.attempt_owners.get(&attempt),
            Some(&failed.turn())
        );

        assert_failed_terminal_call_provenance_is_complete(
            &session,
            failed,
            attempt,
            ModelCallDisposition::KnownFailed,
        );
        assert_failed_terminal_call_provenance_is_complete(
            &session,
            failed,
            attempt,
            ModelCallDisposition::Cancelled,
        );
    }

    /// S02 / S07 / INV-006 / INV-037: a proof-bearing known-failure attempt
    /// can only correlate a physically known-failed call. Confirmed physical
    /// cancellation remains the cancelled terminal outcome.
    #[test]
    fn s02_s07_inv006_inv037_stopped_failure_rejects_cancelled_call() {
        let session = current_session();
        let failed = accepted_origin(1);
        let successor = accepted_origin(2);
        let attempt = turn_attempt_id(60);
        let call_id = model_call_id(50);
        let starting_frontier = FailedTerminalReconstitutionFacts::matching_starting_frontier();
        let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
            successor.position(),
            failed.turn(),
        );
        let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
            command_id(60),
            session.id(),
            failed.turn(),
            successor.accepted_input(),
            successor.turn(),
            successor_order,
        )
        .expect("the fixture interrupt is exactly correlated");
        let mut facts = FailedTerminalReconstitutionFacts::matching(&session, failed);
        facts.replace_terminal_execution(Some(
            FailedTurnExecutionReconstitutionInput::with_call_after_cancellation(
                failed.turn(),
                attempt,
                CancellationStopDisposition::KnownFailure,
                interrupt,
                call_id,
            ),
        ));
        facts.turns.push(successor.record_with(
            &session,
            OriginRecordFacts {
                order: successor_order,
                delivery: DeliveryRequest::Interrupt {
                    expected_active_turn: failed.turn(),
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: PerInputConfigurationChoices::new(
                        SessionConfigurationDefaultsVersion::first(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        ));
        let input_for = |disposition| {
            let call = ModelCallReconstitutionInput::new(
                call_id,
                failed.turn(),
                attempt,
                FrozenModelSelection::Direct(direct(1)),
                ResolvedProviderTarget::naming(provider_model_identity(51)),
                starting_frontier.id(),
                ModelCallReconstitutionState::Terminal(disposition),
            );
            facts.clone().input().with_model_call_facts(
                vec![crate::PinnedProviderTargetReconstitutionInput::new(
                    failed.turn(),
                    call.target(),
                )],
                vec![call],
            )
        };

        input_for(ModelCallDisposition::KnownFailed)
            .reconstitute()
            .expect("stopped known failure retains its known-failed call");
        assert_eq!(
            assert_input_rejects_unchanged(input_for(ModelCallDisposition::Cancelled)),
            AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                turn: failed.turn(),
            }
        );
    }

    /// S02 / S03 / INV-006: failed-terminal attempt provenance fails closed
    /// when either ownership or the allowed terminal end is contradicted.
    #[test]
    fn s02_s03_inv006_failed_terminal_attempt_provenance_fails_closed() {
        let session = current_session();
        let failed = accepted_origin(1);
        let attempt = turn_attempt_id(60);

        let mut wrong_owner = FailedTerminalReconstitutionFacts::matching(&session, failed);
        wrong_owner.replace_terminal_execution(Some(
            FailedTurnExecutionReconstitutionInput::attempt_only(
                turn_id(99),
                attempt,
                UnstoppedAttemptDisposition::KnownFailure,
            ),
        ));
        assert_eq!(
            assert_input_rejects_unchanged(wrong_owner.input()),
            AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptOwnershipMismatch {
                turn: failed.turn(),
                attempt,
            }
        );

        let mut wrong_end = FailedTerminalReconstitutionFacts::matching(&session, failed);
        wrong_end.replace_terminal_execution(Some(
            FailedTurnExecutionReconstitutionInput::attempt_only(
                failed.turn(),
                attempt,
                UnstoppedAttemptDisposition::TurnCompleted,
            ),
        ));
        assert_eq!(
            assert_input_rejects_unchanged(wrong_end.input()),
            AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptEndMismatch {
                turn: failed.turn(),
                attempt,
            }
        );

        let successor = accepted_origin(2);
        let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
            successor.position(),
            failed.turn(),
        );
        let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
            command_id(60),
            session.id(),
            failed.turn(),
            successor.accepted_input(),
            successor.turn(),
            successor_order,
        )
        .expect("the fixture interrupt is exactly correlated");
        let mut lost_after_cancellation =
            FailedTerminalReconstitutionFacts::matching(&session, failed);
        lost_after_cancellation.replace_terminal_execution(Some(
            FailedTurnExecutionReconstitutionInput::attempt_only_after_cancellation(
                failed.turn(),
                attempt,
                CancellationStopDisposition::Lost,
                interrupt,
            ),
        ));
        lost_after_cancellation.turns.push(successor.record_with(
            &session,
            OriginRecordFacts {
                order: successor_order,
                delivery: DeliveryRequest::Interrupt {
                    expected_active_turn: failed.turn(),
                    descendant_scope: DescendantTerminationScope::ParentAlone,
                    configuration: PerInputConfigurationChoices::new(
                        SessionConfigurationDefaultsVersion::first(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        ));
        assert_eq!(
            assert_input_rejects_unchanged(lost_after_cancellation.input()),
            AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptEndMismatch {
                turn: failed.turn(),
                attempt,
            }
        );
    }

    /// S02 / INV-006 / INV-014: a failed terminal call must match the ended
    /// attempt and the turn's selection, target, starting frontier, and
    /// KnownFailed-or-Cancelled physical disposition.
    #[test]
    fn s02_inv006_inv014_failed_terminal_call_provenance_fails_closed() {
        let session = current_session();
        let failed = accepted_origin(1);
        let attempt = turn_attempt_id(60);
        let call_id = model_call_id(50);
        let starting_frontier = FailedTerminalReconstitutionFacts::matching_starting_frontier();
        let mut facts = FailedTerminalReconstitutionFacts::matching(&session, failed);
        facts.replace_terminal_execution(Some(FailedTurnExecutionReconstitutionInput::with_call(
            failed.turn(),
            attempt,
            UnstoppedAttemptDisposition::KnownFailure,
            call_id,
        )));
        let mismatched_call = ModelCallReconstitutionInput::new(
            call_id,
            failed.turn(),
            turn_attempt_id(61),
            FrozenModelSelection::Direct(direct(1)),
            ResolvedProviderTarget::naming(provider_model_identity(51)),
            starting_frontier.id(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::KnownFailed),
        );
        let input = facts.input().with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                failed.turn(),
                mismatched_call.target(),
            )],
            vec![mismatched_call],
        );
        assert_eq!(
            assert_input_rejects_unchanged(input),
            AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                turn: failed.turn(),
            }
        );
    }

    /// S02 / S04 / S07 / S09 / INV-005 / INV-009 / INV-015 / INV-016 /
    /// INV-037: a live or startup-recovered completed response validates the
    /// producing call's steering-extended source, stop provenance, and final
    /// marker before the exact terminal frontier becomes the successor's
    /// starting prefix.
    #[test]
    fn s02_s04_s09_inv005_inv009_inv015_completed_frontier_becomes_successor_prefix() {
        let session = current_session();
        let predecessor = accepted_origin(1);
        let consumed = accepted_origin(2);
        let successor = accepted_origin(3);
        let origin_entry = semantic_entry(30);
        let steering_entry = semantic_entry(31);
        let assistant_entry = semantic_entry(32);
        let completion_entry = semantic_entry(33);
        let starting_frontier = frontier(40);
        let call_frontier = frontier(41);
        let terminal_frontier = frontier(42);
        let completing_call = model_call_id(50);
        let activation = activation(2);
        let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
            successor.position(),
            predecessor.turn(),
        );
        let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
            command_id(60),
            session.id(),
            predecessor.turn(),
            successor.accepted_input(),
            successor.turn(),
            successor_order,
        )
        .expect("the fixture interrupt is exactly correlated");
        let interrupt_delivery = DeliveryRequest::Interrupt {
            expected_active_turn: predecessor.turn(),
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        };
        let assert_case = |completing_attempt_end, queued_record| {
            let terminal_record = predecessor.record(
                &session,
                AcceptedInputTurnSchedulingRecordState::TerminalCompleted {
                    starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                    starting_frontier: starting_frontier.id(),
                    completing_attempt: turn_attempt_id(60),
                    completing_attempt_end,
                    completing_call,
                    terminal_frontier: terminal_frontier.id(),
                },
            );
            let steering = SemanticTranscriptEntryReconstitutionInput::new(
                steering_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                    accepted_input: consumed.accepted_input(),
                    source_turn: predecessor.turn(),
                },
            );
            let assistant = SemanticTranscriptEntryReconstitutionInput::new(
                assistant_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::AssistantText {
                    producing_call: completing_call,
                    value: AssistantText::try_new(String::from("reply"))
                        .expect("test assistant text is nonempty"),
                },
            );
            let completion = SemanticTranscriptEntryReconstitutionInput::new(
                completion_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::TurnCompleted {
                    turn: predecessor.turn(),
                },
            );
            let call = ModelCallReconstitutionInput::new(
                completing_call,
                predecessor.turn(),
                turn_attempt_id(60),
                FrozenModelSelection::Direct(direct(1)),
                ResolvedProviderTarget::naming(provider_model_identity(51)),
                call_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            );
            let projection = AcceptedInputSchedulingReconstitutionInput::new(
                session.clone(),
                vec![queued_record, terminal_record],
                vec![
                    assistant,
                    completion,
                    steering,
                    predecessor.entry(&session, origin_entry),
                ],
                vec![
                    terminal_frontier.snapshot(
                        &session,
                        &[
                            origin_entry,
                            steering_entry,
                            assistant_entry,
                            completion_entry,
                        ],
                    ),
                    call_frontier.snapshot(&session, &[origin_entry, steering_entry]),
                    starting_frontier.snapshot(&session, &[origin_entry]),
                ],
                None,
            )
            .with_model_call_facts(
                vec![crate::PinnedProviderTargetReconstitutionInput::new(
                    call.turn(),
                    call.target(),
                )],
                vec![call],
            )
            .with_consumed_steering_facts(vec![ConsumedSteeringReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    consumed.accepted_input(),
                    AcceptedInputDisposition::ConsumedAsSteering {
                        call: completing_call,
                    },
                ),
                consumed.position(),
                predecessor.turn(),
            )])
            .reconstitute()
            .expect("the completed predecessor is fully correlated");

            let collision = projection
                .clone()
                .prepare_earliest_queued_activation(
                    activation.identities_with_attempt(turn_attempt_id(60)),
                )
                .expect_err("a terminal attempt identity cannot be minted again");
            assert_eq!(
                collision.failure(),
                AcceptedInputEligibilityFailure::InitialAttemptIdentityAlreadyExists
            );

            let candidate = projection
                .prepare_earliest_queued_activation(activation.identities())
                .expect("the completed predecessor releases the progressing slot");

            assert_eq!(candidate.turn().turn(), successor.turn());
            assert_eq!(
                candidate
                    .starting_snapshot()
                    .ordered_entries()
                    .collect::<Vec<_>>(),
                vec![
                    origin_entry.reference(&session),
                    steering_entry.reference(&session),
                    assistant_entry.reference(&session),
                    completion_entry.reference(&session),
                    activation.origin_entry().reference(&session),
                ]
            );
        };

        assert_case(
            TerminalAttemptEndReconstitutionInput::without_stop(
                UnstoppedAttemptDisposition::TurnCompleted,
            ),
            successor.record(&session, AcceptedInputTurnSchedulingRecordState::Queued),
        );
        assert_case(
            TerminalAttemptEndReconstitutionInput::without_stop(UnstoppedAttemptDisposition::Lost),
            successor.record(&session, AcceptedInputTurnSchedulingRecordState::Queued),
        );
        assert_case(
            TerminalAttemptEndReconstitutionInput::after_cancellation(
                CancellationStopDisposition::TurnCompleted,
                interrupt,
            ),
            successor.record_with(
                &session,
                OriginRecordFacts {
                    order: successor_order,
                    delivery: interrupt_delivery,
                    state: AcceptedInputTurnSchedulingRecordState::Queued,
                },
            ),
        );
        assert_case(
            TerminalAttemptEndReconstitutionInput::after_cancellation(
                CancellationStopDisposition::Lost,
                interrupt,
            ),
            successor.record_with(
                &session,
                OriginRecordFacts {
                    order: successor_order,
                    delivery: interrupt_delivery,
                    state: AcceptedInputTurnSchedulingRecordState::Queued,
                },
            ),
        );
    }

    /// S02 / S04 / INV-006 / INV-009: one physical attempt identity cannot
    /// back terminal outcomes for two different turns.
    #[test]
    fn s02_s04_inv006_inv009_terminal_turns_reject_shared_attempt_identity() {
        let session = current_session();
        let completed = accepted_origin(1);
        let refused = accepted_origin(2);
        let completed_origin = semantic_entry(30);
        let assistant = semantic_entry(31);
        let completion = semantic_entry(32);
        let refused_origin = semantic_entry(33);
        let completed_start = frontier(40);
        let completed_terminal = frontier(41);
        let refused_start = frontier(42);
        let refused_terminal = frontier(43);
        let shared_attempt = turn_attempt_id(60);
        let completed_call = model_call_id(50);
        let refused_call = model_call_id(51);
        let completed_start_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
            session.id(),
            completed_start.id(),
            vec![completed_origin.reference(&session)],
        )
        .expect("the completed call frontier has unique membership");
        let refused_start_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
            session.id(),
            refused_start.id(),
            vec![
                completed_origin.reference(&session),
                assistant.reference(&session),
                completion.reference(&session),
                refused_origin.reference(&session),
            ],
        )
        .expect("the refused call frontier has unique membership");
        let completed_record = completed.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalCompleted {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: completed_start.id(),
                completing_attempt: shared_attempt,
                completing_attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(
                    UnstoppedAttemptDisposition::TurnCompleted,
                ),
                completing_call: completed_call,
                terminal_frontier: completed_terminal.id(),
            },
        );
        let refused_record = refused.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalRefused {
                starting_lineage: AcceptedInputStartingLineage::After {
                    immediate_predecessor: completed.turn(),
                },
                starting_frontier: refused_start.id(),
                refusing_attempt: shared_attempt,
                refusing_attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(
                    UnstoppedAttemptDisposition::TurnRefused,
                ),
                refusing_call: refused_call,
                terminal_frontier: refused_terminal.id(),
            },
        );
        let assistant_entry = SemanticTranscriptEntryReconstitutionInput::new(
            assistant.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantText {
                producing_call: completed_call,
                value: AssistantText::try_new("reply".to_owned())
                    .expect("test assistant text is nonempty"),
            },
        );
        let completion_entry = SemanticTranscriptEntryReconstitutionInput::new(
            completion.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnCompleted {
                turn: completed.turn(),
            },
        );
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![refused_record, completed_record],
            vec![
                completed.entry(&session, completed_origin),
                assistant_entry,
                completion_entry,
                refused.entry(&session, refused_origin),
            ],
            vec![
                completed_start.snapshot(&session, &[completed_origin]),
                completed_terminal.snapshot(&session, &[completed_origin, assistant, completion]),
                refused_start.snapshot(
                    &session,
                    &[completed_origin, assistant, completion, refused_origin],
                ),
                refused_terminal.snapshot(
                    &session,
                    &[completed_origin, assistant, completion, refused_origin],
                ),
            ],
            None,
        )
        .with_model_call_facts(
            vec![
                crate::PinnedProviderTargetReconstitutionInput::new(
                    completed.turn(),
                    ResolvedProviderTarget::naming(provider_model_identity(51)),
                ),
                crate::PinnedProviderTargetReconstitutionInput::new(
                    refused.turn(),
                    ResolvedProviderTarget::naming(provider_model_identity(51)),
                ),
            ],
            vec![
                ModelCallReconstitutionInput::new(
                    completed_call,
                    completed.turn(),
                    shared_attempt,
                    FrozenModelSelection::Direct(direct(1)),
                    ResolvedProviderTarget::naming(provider_model_identity(51)),
                    completed_start_snapshot.frontier().snapshot(),
                    ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
                ),
                ModelCallReconstitutionInput::new(
                    refused_call,
                    refused.turn(),
                    shared_attempt,
                    FrozenModelSelection::Direct(direct(1)),
                    ResolvedProviderTarget::naming(provider_model_identity(51)),
                    refused_start_snapshot.frontier().snapshot(),
                    ModelCallReconstitutionState::Terminal(ModelCallDisposition::Refused),
                ),
            ],
        );

        let error = input
            .reconstitute()
            .expect_err("one attempt cannot terminalize two turns");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::DuplicateCurrentAttempt {
                attempt: shared_attempt,
            }
        );
    }

    /// S02 / S04 / S07 / S09 / INV-005 / INV-009 / INV-015 / INV-016 /
    /// INV-037: a live or startup-recovered refusal validates the producing
    /// call's steering-extended source and stop provenance, releases the slot,
    /// and preserves its equal-content terminal frontier as the successor's
    /// exact prefix.
    #[test]
    fn s02_s04_s09_inv005_inv009_inv015_refused_frontier_becomes_successor_prefix() {
        let session = current_session();
        let predecessor = accepted_origin(1);
        let consumed = accepted_origin(2);
        let successor = accepted_origin(3);
        let origin_entry = semantic_entry(30);
        let steering_entry = semantic_entry(31);
        let starting_frontier = frontier(40);
        let call_frontier = frontier(41);
        let terminal_frontier = frontier(42);
        let refusing_call = model_call_id(50);
        let refusing_attempt = turn_attempt_id(60);
        let activation = activation(2);
        let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
            successor.position(),
            predecessor.turn(),
        );
        let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
            command_id(60),
            session.id(),
            predecessor.turn(),
            successor.accepted_input(),
            successor.turn(),
            successor_order,
        )
        .expect("the fixture interrupt is exactly correlated");
        let interrupt_delivery = DeliveryRequest::Interrupt {
            expected_active_turn: predecessor.turn(),
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        };
        let assert_case = |refusing_attempt_end, queued_record| {
            let terminal_record = predecessor.record(
                &session,
                AcceptedInputTurnSchedulingRecordState::TerminalRefused {
                    starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                    starting_frontier: starting_frontier.id(),
                    refusing_attempt,
                    refusing_attempt_end,
                    refusing_call,
                    terminal_frontier: terminal_frontier.id(),
                },
            );
            let steering = SemanticTranscriptEntryReconstitutionInput::new(
                steering_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::SteeringAcceptedInput {
                    accepted_input: consumed.accepted_input(),
                    source_turn: predecessor.turn(),
                },
            );
            let call = ModelCallReconstitutionInput::new(
                refusing_call,
                predecessor.turn(),
                refusing_attempt,
                FrozenModelSelection::Direct(direct(1)),
                ResolvedProviderTarget::naming(provider_model_identity(51)),
                call_frontier.id(),
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Refused),
            );
            let projection = AcceptedInputSchedulingReconstitutionInput::new(
                session.clone(),
                vec![queued_record, terminal_record],
                vec![predecessor.entry(&session, origin_entry), steering],
                vec![
                    terminal_frontier.snapshot(&session, &[origin_entry, steering_entry]),
                    call_frontier.snapshot(&session, &[origin_entry, steering_entry]),
                    starting_frontier.snapshot(&session, &[origin_entry]),
                ],
                None,
            )
            .with_model_call_facts(
                vec![crate::PinnedProviderTargetReconstitutionInput::new(
                    call.turn(),
                    call.target(),
                )],
                vec![call],
            )
            .with_consumed_steering_facts(vec![ConsumedSteeringReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    consumed.accepted_input(),
                    AcceptedInputDisposition::ConsumedAsSteering {
                        call: refusing_call,
                    },
                ),
                consumed.position(),
                predecessor.turn(),
            )])
            .reconstitute()
            .expect("the refused predecessor is fully correlated");

            let candidate = projection
                .prepare_earliest_queued_activation(activation.identities())
                .expect("the refused predecessor releases the progressing slot");

            assert_eq!(candidate.turn().turn(), successor.turn());
            assert_eq!(
                candidate
                    .starting_snapshot()
                    .ordered_entries()
                    .collect::<Vec<_>>(),
                vec![
                    origin_entry.reference(&session),
                    steering_entry.reference(&session),
                    activation.origin_entry().reference(&session),
                ]
            );
        };

        assert_case(
            TerminalAttemptEndReconstitutionInput::without_stop(
                UnstoppedAttemptDisposition::TurnRefused,
            ),
            successor.record(&session, AcceptedInputTurnSchedulingRecordState::Queued),
        );
        assert_case(
            TerminalAttemptEndReconstitutionInput::without_stop(UnstoppedAttemptDisposition::Lost),
            successor.record(&session, AcceptedInputTurnSchedulingRecordState::Queued),
        );
        assert_case(
            TerminalAttemptEndReconstitutionInput::after_cancellation(
                CancellationStopDisposition::TurnRefused,
                interrupt,
            ),
            successor.record_with(
                &session,
                OriginRecordFacts {
                    order: successor_order,
                    delivery: interrupt_delivery,
                    state: AcceptedInputTurnSchedulingRecordState::Queued,
                },
            ),
        );
        assert_case(
            TerminalAttemptEndReconstitutionInput::after_cancellation(
                CancellationStopDisposition::Lost,
                interrupt,
            ),
            successor.record_with(
                &session,
                OriginRecordFacts {
                    order: successor_order,
                    delivery: interrupt_delivery,
                    state: AcceptedInputTurnSchedulingRecordState::Queued,
                },
            ),
        );
    }

    /// S02 / INV-005: assistant text cannot name a refused call because only
    /// completed physical calls can produce semantic assistant content.
    #[test]
    fn s02_inv005_refused_call_rejects_assistant_content() {
        let session = current_session();
        let origin = accepted_origin(1);
        let origin_entry = semantic_entry(30);
        let assistant_entry = semantic_entry(31);
        let starting_frontier = frontier(40);
        let terminal_frontier = frontier(41);
        let refusing_call = model_call_id(50);
        let refusing_attempt = turn_attempt_id(60);
        let resolved_starting = ResolvedContextFrontierSnapshot::try_from_candidate(
            session.id(),
            starting_frontier.id(),
            vec![origin_entry.reference(&session)],
        )
        .expect("the call frontier has unique membership");
        let terminal_record = origin.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalRefused {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                refusing_attempt,
                refusing_attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(
                    UnstoppedAttemptDisposition::TurnRefused,
                ),
                refusing_call,
                terminal_frontier: terminal_frontier.id(),
            },
        );
        let assistant = SemanticTranscriptEntryReconstitutionInput::new(
            assistant_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::AssistantText {
                producing_call: refusing_call,
                value: AssistantText::try_new(String::from("not a refusal"))
                    .expect("test assistant text is nonempty"),
            },
        );
        let call = ModelCallReconstitutionInput::new(
            refusing_call,
            origin.turn(),
            refusing_attempt,
            FrozenModelSelection::Direct(direct(1)),
            ResolvedProviderTarget::naming(provider_model_identity(51)),
            resolved_starting.frontier().snapshot(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Refused),
        );
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![terminal_record],
            vec![assistant, origin.entry(&session, origin_entry)],
            vec![
                terminal_frontier.snapshot(&session, &[origin_entry]),
                starting_frontier.snapshot(&session, &[origin_entry]),
            ],
            None,
        )
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                call.turn(),
                call.target(),
            )],
            vec![call],
        );

        let error = input
            .reconstitute()
            .expect_err("refused calls cannot produce assistant content");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::SemanticEntryCallMismatch {
                entry: assistant_entry.id(),
                call: refusing_call,
            }
        );
    }

    /// S02 / INV-006 / INV-009: a terminal refusal must be backed by the
    /// stored ended-attempt refusal disposition, not only a matching identity.
    #[test]
    fn s02_inv006_inv009_refused_turn_rejects_attempt_disposition_mismatch() {
        let session = current_session();
        let origin = accepted_origin(1);
        let origin_entry = semantic_entry(30);
        let starting_frontier = frontier(40);
        let terminal_frontier = frontier(41);
        let refusing_call = model_call_id(50);
        let refusing_attempt = turn_attempt_id(60);
        let resolved_starting = ResolvedContextFrontierSnapshot::try_from_candidate(
            session.id(),
            starting_frontier.id(),
            vec![origin_entry.reference(&session)],
        )
        .expect("the call frontier has unique membership");
        let terminal_record = origin.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalRefused {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                refusing_attempt,
                refusing_attempt_end: TerminalAttemptEndReconstitutionInput::without_stop(
                    UnstoppedAttemptDisposition::KnownFailure,
                ),
                refusing_call,
                terminal_frontier: terminal_frontier.id(),
            },
        );
        let call = ModelCallReconstitutionInput::new(
            refusing_call,
            origin.turn(),
            refusing_attempt,
            FrozenModelSelection::Direct(direct(1)),
            ResolvedProviderTarget::naming(provider_model_identity(51)),
            resolved_starting.frontier().snapshot(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Refused),
        );
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![terminal_record],
            vec![origin.entry(&session, origin_entry)],
            vec![
                terminal_frontier.snapshot(&session, &[origin_entry]),
                starting_frontier.snapshot(&session, &[origin_entry]),
            ],
            None,
        )
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                call.turn(),
                call.target(),
            )],
            vec![call],
        );

        let error = input
            .reconstitute()
            .expect_err("a refusal cannot be inferred from attempt identity alone");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                turn: origin.turn(),
            }
        );
    }

    /// S07 / INV-006 / INV-029 / INV-037: a terminal-cancelled projection
    /// validates the stored attempt end rather than inferring it from the
    /// separately supplied interrupt result.
    #[test]
    fn s07_inv006_inv029_inv037_cancelled_turn_rejects_attempt_end_mismatch() {
        let session = current_session();
        let cancelled = accepted_origin(1);
        let successor = accepted_origin(2);
        let origin_entry = semantic_entry(30);
        let cancellation_entry = semantic_entry(31);
        let starting_frontier = frontier(40);
        let terminal_frontier = frontier(41);
        let attempt = turn_attempt_id(50);
        let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
            successor.position(),
            cancelled.turn(),
        );
        let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
            command_id(60),
            session.id(),
            cancelled.turn(),
            successor.accepted_input(),
            successor.turn(),
            successor_order,
        )
        .expect("the fixture interrupt is exactly correlated");
        let terminal_record = cancelled.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                    cancelled.turn(),
                    attempt,
                    TerminalAttemptEndReconstitutionInput::without_stop(
                        UnstoppedAttemptDisposition::Ambiguous,
                    ),
                    None,
                    interrupt,
                ),
                terminal_frontier: terminal_frontier.id(),
            },
        );
        let successor_delivery = DeliveryRequest::Interrupt {
            expected_active_turn: cancelled.turn(),
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        };
        let successor_record = successor.record_with(
            &session,
            OriginRecordFacts {
                order: successor_order,
                delivery: successor_delivery,
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        );
        let cancellation_entry = SemanticTranscriptEntryReconstitutionInput::new(
            cancellation_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnCancelled {
                turn: cancelled.turn(),
            },
        );
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![terminal_record, successor_record],
            vec![cancelled.entry(&session, origin_entry), cancellation_entry],
            vec![
                starting_frontier.snapshot(&session, &[origin_entry]),
                terminal_frontier.snapshot(&session, &[origin_entry, semantic_entry(31)]),
            ],
            None,
        );

        let error = input
            .reconstitute()
            .expect_err("cancelled turn authority cannot substitute for its attempt end");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::TerminalAttemptEndMismatch {
                turn: cancelled.turn(),
                attempt,
            }
        );
    }

    /// S07 / INV-005 / INV-006 / INV-037: a cancelled call frontier must
    /// preserve the starting frontier rather than substituting unrelated
    /// semantic history before the cancellation marker.
    #[test]
    fn s07_inv005_inv006_inv037_cancelled_turn_rejects_unrelated_call_frontier() {
        let session = current_session();
        let cancelled = accepted_origin(1);
        let successor = accepted_origin(2);
        let origin_entry = semantic_entry(30);
        let cancellation_entry = semantic_entry(31);
        let starting_frontier = frontier(40);
        let terminal_frontier = frontier(41);
        let unrelated_call_frontier = frontier(42);
        let call = model_call_id(49);
        let attempt = turn_attempt_id(50);
        let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
            successor.position(),
            cancelled.turn(),
        );
        let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
            command_id(60),
            session.id(),
            cancelled.turn(),
            successor.accepted_input(),
            successor.turn(),
            successor_order,
        )
        .expect("the fixture interrupt is exactly correlated");
        let terminal_record = cancelled.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalCancelled {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                terminal_execution: CancelledTurnExecutionReconstitutionInput::new(
                    cancelled.turn(),
                    attempt,
                    TerminalAttemptEndReconstitutionInput::after_cancellation(
                        CancellationStopDisposition::Cancelled,
                        interrupt,
                    ),
                    Some(call),
                    interrupt,
                ),
                terminal_frontier: terminal_frontier.id(),
            },
        );
        let successor_delivery = DeliveryRequest::Interrupt {
            expected_active_turn: cancelled.turn(),
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        };
        let successor_record = successor.record_with(
            &session,
            OriginRecordFacts {
                order: successor_order,
                delivery: successor_delivery,
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        );
        let cancellation_entry = SemanticTranscriptEntryReconstitutionInput::new(
            cancellation_entry.id(),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnCancelled {
                turn: cancelled.turn(),
            },
        );
        let stored_call = ModelCallReconstitutionInput::new(
            call,
            cancelled.turn(),
            attempt,
            FrozenModelSelection::Direct(direct(1)),
            ResolvedProviderTarget::naming(provider_model_identity(51)),
            unrelated_call_frontier.id(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Cancelled),
        );
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![terminal_record, successor_record],
            vec![cancelled.entry(&session, origin_entry), cancellation_entry],
            vec![
                starting_frontier.snapshot(&session, &[origin_entry]),
                unrelated_call_frontier.snapshot(&session, &[]),
                terminal_frontier.snapshot(&session, &[semantic_entry(31)]),
            ],
            None,
        )
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                cancelled.turn(),
                ResolvedProviderTarget::naming(provider_model_identity(51)),
            )],
            vec![stored_call],
        );

        let error = input
            .reconstitute()
            .expect_err("a cancelled call cannot replace its turn's starting history");
        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::TerminalModelCallMismatch {
                turn: cancelled.turn(),
            }
        );
    }

    /// S04 / INV-025 / INV-026: complete ambiguous-call facts reconstruct the
    /// exact recovery wait and preserve the active progressing slot.
    #[test]
    fn s04_inv025_inv026_ambiguous_call_reconstructs_recovery_wait() {
        let session = current_session();
        let active = accepted_origin(1);
        let origin_entry = semantic_entry(30);
        let starting_frontier = frontier(40);
        let ambiguous_call = model_call_id(50);
        let resolved_starting = ResolvedContextFrontierSnapshot::try_from_candidate(
            session.id(),
            starting_frontier.id(),
            vec![origin_entry.reference(&session)],
        )
        .expect("the call frontier has unique membership");
        let active_record = active.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::Active {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                phase: ActiveTurnSchedulingReconstitutionInput::awaiting_model_call_recovery(
                    active.turn(),
                    turn_attempt_id(60),
                    ambiguous_call,
                ),
            },
        );
        let call = ModelCallReconstitutionInput::new(
            ambiguous_call,
            active.turn(),
            turn_attempt_id(60),
            FrozenModelSelection::Direct(direct(1)),
            ResolvedProviderTarget::naming(provider_model_identity(51)),
            resolved_starting.frontier().snapshot(),
            ModelCallReconstitutionState::Terminal(ModelCallDisposition::Ambiguous),
        );
        let projection = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![active_record],
            vec![active.entry(&session, origin_entry)],
            vec![starting_frontier.snapshot(&session, &[origin_entry])],
            Some(active.active_tail(&session)),
        )
        .with_model_call_facts(
            vec![crate::PinnedProviderTargetReconstitutionInput::new(
                call.turn(),
                call.target(),
            )],
            vec![call],
        )
        .reconstitute()
        .expect("the ambiguous call and wait are fully correlated");
        let waiting = projection
            .active_turn()
            .expect("the recovery wait retains the progressing slot");

        assert!(matches!(
            waiting.active_phase(),
            Some(ActiveTurnPhase::AwaitingRecoveryDecision {
                ambiguous_operations,
                ..
            }) if ambiguous_operations.contains(crate::IssuedOperationRef::ModelCall(ambiguous_call))
        ));
    }

    /// S06 / S07 / INV-025 / INV-026 / INV-029 / INV-037: an opaque wait from
    /// a completely validated ambiguous tool batch reconstructs the exact
    /// typed recovery subject and preserves it through interruption.
    #[test]
    fn s06_s07_inv025_inv026_inv029_inv037_ambiguous_tool_recovery_and_interrupt() {
        let session = current_session();
        let active = accepted_origin(1);
        let origin_entry = semantic_entry(30);
        let starting_frontier = frontier(40);
        let producing_call = model_call_id(50);
        let issuing_attempt = turn_attempt_id(60);
        let request = ToolRequestReconstitutionInput::new(
            tool_request_id(70),
            session.id(),
            active.turn(),
            producing_call,
            ToolRequestOrdinal::from_u32(0),
            ToolName::try_new(String::from("external_tool")).expect("fixture name is canonical"),
            NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("fixture arguments are canonical"),
        )
        .into_request();
        let approval = ToolApprovalResolutionReconstitutionInput::user_fixture(
            request.id(),
            ToolApprovalDecision::Approve,
        )
        .reconstitute()
        .expect("user approval is implemented");
        let expected_tool_attempt = tool_attempt_id(80);
        let tool_attempt = ToolAttemptReconstitutionInput::new(
            expected_tool_attempt,
            request.id(),
            session.id(),
            active.turn(),
            issuing_attempt,
            ToolEffectClass::ExternalEffect,
            ToolDispatchGeneration::first(),
            ToolAttemptReconstitutionState::Ended(ToolAttemptEnd::Ambiguous),
        )
        .reconstitute()
        .expect("the first tool dispatch generation is supported");
        let yielded = ResolvedContextFrontierSnapshot::try_from_candidate(
            session.id(),
            starting_frontier.id(),
            vec![origin_entry.reference(&session)],
        )
        .expect("the yielded snapshot is valid");
        let expected_request = request.id();
        let batch = ToolBatchReconstitutionInput::new(
            session.id(),
            active.turn(),
            producing_call,
            yielded,
            vec![request],
            vec![approval],
            vec![tool_attempt],
            ToolBatchPhaseReconstitutionInput::AwaitingRecovery {
                attempt: expected_tool_attempt,
            },
        )
        .reconstitute()
        .expect("the complete tool batch is exactly ambiguous");
        let wait = batch
            .awaiting_recovery()
            .expect("the validated batch exposes opaque wait evidence");
        let cross_wired_record = active.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::Active {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                phase: ActiveTurnSchedulingReconstitutionInput::awaiting_tool_recovery(
                    active.turn(),
                    turn_attempt_id(61),
                    wait,
                ),
            },
        );
        let error = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![cross_wired_record],
            vec![active.entry(&session, origin_entry)],
            vec![starting_frontier.snapshot(&session, &[origin_entry])],
            Some(active.active_tail(&session)),
        )
        .reconstitute()
        .expect_err("the wait cannot be attached to another turn attempt");
        let AcceptedInputSchedulingReconstitutionFailure::ActivePhaseEvidenceMismatch {
            turn, ..
        } = error.failure()
        else {
            panic!("the cross-wired wait fails as an active-phase mismatch");
        };
        assert_eq!(*turn, active.turn());
        let successor = accepted_origin(2);
        let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
            successor.position(),
            active.turn(),
        );
        let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
            command_id(90),
            session.id(),
            active.turn(),
            successor.accepted_input(),
            successor.turn(),
            successor_order,
        )
        .expect("the fixture interrupt is exactly correlated");
        let terminal_tool_reconciliation = active.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalToolReconciliationRequired {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                reconciling_attempt: issuing_attempt,
                reconciling_attempt_end: TerminalAttemptEndReconstitutionInput::after_cancellation(
                    CancellationStopDisposition::Lost,
                    interrupt,
                ),
                tool_batch: batch.clone(),
                authority: AutomaticReconciliationAuthority::AppliedInterrupt(interrupt),
                terminal_frontier: starting_frontier.id(),
            },
        );
        assert!(
            scheduling_record_is_terminal(&terminal_tool_reconciliation),
            "tool reconciliation is terminal historical proof"
        );
        let active_record = active.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::Active {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: starting_frontier.id(),
                phase:
                    ActiveTurnSchedulingReconstitutionInput::awaiting_tool_recovery_after_cancellation_restart(
                    active.turn(),
                    issuing_attempt,
                    wait,
                    interrupt,
                ),
            },
        );
        let successor_delivery = DeliveryRequest::Interrupt {
            expected_active_turn: active.turn(),
            descendant_scope: DescendantTerminationScope::ParentAlone,
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        };
        let successor_record = successor.record_with(
            &session,
            OriginRecordFacts {
                order: successor_order,
                delivery: successor_delivery,
                state: AcceptedInputTurnSchedulingRecordState::Queued,
            },
        );
        let acceptance_tail = SessionAcceptanceTailReconstitutionInput::new(
            session.id(),
            active.accepted_input(),
            successor.position(),
            vec![
                SessionAcceptanceTailEntryReconstitutionInput::new(
                    session.id(),
                    AcceptedInputLifecycle::new(
                        active.accepted_input(),
                        AcceptedInputDisposition::OriginOf(active.turn()),
                    ),
                    active.position(),
                    default_origin_delivery(),
                ),
                SessionAcceptanceTailEntryReconstitutionInput::new(
                    session.id(),
                    AcceptedInputLifecycle::new(
                        successor.accepted_input(),
                        AcceptedInputDisposition::OriginOf(successor.turn()),
                    ),
                    successor.position(),
                    successor_delivery,
                ),
            ],
        );
        let projection = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![active_record, successor_record],
            vec![active.entry(&session, origin_entry)],
            vec![starting_frontier.snapshot(&session, &[origin_entry])],
            Some(acceptance_tail),
        )
        .reconstitute()
        .expect("the opaque tool wait and ended turn attempt are correlated");
        let waiting = projection
            .active_turn()
            .expect("the recovery wait retains the progressing slot");

        let Some(ActiveTurnPhase::AwaitingRecoveryDecision {
            ambiguous_operations,
            ..
        }) = waiting.active_phase()
        else {
            panic!("the opaque tool wait remains an active recovery decision");
        };
        assert!(
            ambiguous_operations.contains(crate::IssuedOperationRef::ToolAttempt(
                expected_tool_attempt
            ))
        );

        let retained_request = batch
            .requests()
            .first()
            .expect("the one-request batch retains its request");
        assert_eq!(retained_request.id(), expected_request);
        let Some(crate::ReconstitutedToolAttempt::Ended(ended_tool)) =
            batch.attempt(retained_request.id())
        else {
            panic!("the batch retains its ended ambiguous attempt");
        };
        assert_eq!(ended_tool.attempt(), expected_tool_attempt);
        let ended_tool = ended_tool.clone();
        let result_entry = semantic_entry(31);
        let result_projection = batch
            .prepare_reconciliation_projection(vec![result_entry.id()], frontier(41).id())
            .expect("the terminal batch closes its logical request");
        let reconciled = projection
            .apply_interrupt_to_tool_recovery(
                wait,
                ended_tool,
                result_projection,
                interrupt,
                crate::AmbiguousModelCallTurnIdentities::new(frontier(41).id()),
            )
            .expect("the interrupt retains exact tool ambiguity");
        assert_eq!(reconciled.tool_attempt().attempt(), expected_tool_attempt);
        assert_eq!(
            reconciled.attempt().end(),
            &AttemptEnd::AfterCancellation {
                cause: interrupt.proof(),
                disposition: CancellationStopDisposition::Lost,
            }
        );
        assert_eq!(
            reconciled
                .terminal_snapshot()
                .ordered_entries()
                .collect::<Vec<_>>(),
            vec![
                origin_entry.reference(&session),
                result_entry.reference(&session)
            ]
        );
        assert_eq!(
            reconciled.tool_result_entries()[0].payload(),
            &crate::SemanticTranscriptEntryPayload::ToolClosed {
                request: expected_request,
            }
        );
        let crate::TurnDisposition::ReconciliationRequired { marker } = reconciled.disposition()
        else {
            panic!("the interrupted ambiguity requires reconciliation");
        };
        assert!(
            marker
                .ambiguous_operations()
                .contains(crate::IssuedOperationRef::ToolAttempt(
                    expected_tool_attempt
                ))
        );
    }

    /// S07 / INV-006 / INV-037: a later interrupt supplies terminal authority
    /// without being rewritten into an already ambiguous attempt end.
    #[test]
    fn s07_inv006_inv037_tool_reconciliation_retains_without_stop_attempt_end() {
        let session = current_session();
        let predecessor = accepted_origin(1);
        let successor = accepted_origin(2);
        let successor_order = AcceptedInputQueueOrder::interrupt_immediately_after(
            successor.position(),
            predecessor.turn(),
        );
        let interrupt = AppliedInterruptCommandResult::from_correlated_submit(
            command_id(90),
            session.id(),
            predecessor.turn(),
            successor.accepted_input(),
            successor.turn(),
            successor_order,
        )
        .expect("the fixture interrupt is exactly correlated");
        let attempt_end = TerminalAttemptEndReconstitutionInput::without_stop(
            UnstoppedAttemptDisposition::Ambiguous,
        );

        assert!(tool_reconciliation_attempt_end_matches(
            &attempt_end,
            Some(interrupt),
        ));
    }

    /// S09 / INV-015: a predecessor snapshot that omits its required failed
    /// marker is not a terminal frontier and cannot authorize a successor.
    #[test]
    fn s09_incomplete_failed_terminal_frontier_fails_closed() {
        let session = current_session();
        let predecessor = accepted_origin(1);
        let origin_entry = semantic_entry(30);
        let failure_entry = semantic_entry(31);
        let starting_frontier = frontier(40);
        let terminal_frontier = frontier(41);
        let no_active_acceptance_tail = None;
        let error = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            vec![predecessor.record(
                &session,
                AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                    starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                    starting_frontier: starting_frontier.id(),
                    terminal_execution: None,
                    terminal_frontier: terminal_frontier.id(),
                },
            )],
            vec![
                predecessor.entry(&session, origin_entry),
                failure_entry.failed_turn(&session, predecessor),
            ],
            vec![
                starting_frontier.snapshot(&session, &[origin_entry]),
                terminal_frontier.snapshot(&session, &[origin_entry]),
            ],
            no_active_acceptance_tail,
        )
        .reconstitute()
        .expect_err("the failed marker must follow the exact starting prefix");

        assert_eq!(
            error.failure(),
            &AcceptedInputSchedulingReconstitutionFailure::TerminalFrontierMismatch {
                turn: predecessor.turn(),
            }
        );
    }

    /// S28 / INV-002 / INV-039: imported ancestry is admitted only together
    /// with its exact complete independently checked seed projection.
    #[test]
    fn s28_inv002_inv039_imported_scheduling_requires_exact_seed_projection() {
        let imported = imported_session();
        let session = imported.session().clone();
        let queued = accepted_origin(1);

        let missing = assert_input_rejects_unchanged(queued_input(&session, queued));
        assert_eq!(
            missing,
            AcceptedInputSchedulingReconstitutionFailure::MissingImportedSession
        );

        let mismatched = assert_input_rejects_unchanged(
            queued_input(&session, queued).with_imported_session(imported_session_for(2)),
        );
        assert_eq!(
            mismatched,
            AcceptedInputSchedulingReconstitutionFailure::ImportedSessionMismatch
        );

        let unexpected = assert_input_rejects_unchanged(
            queued_input(&current_session(), queued).with_imported_session(imported),
        );
        assert_eq!(
            unexpected,
            AcceptedInputSchedulingReconstitutionFailure::UnexpectedImportedSession
        );
    }

    /// S03 / INV-009: this closed slice still cannot resolve a first frontier
    /// from native session ancestry, so an otherwise-valid queued projection
    /// for a native ancestral session fails closed.
    #[test]
    fn s03_inv009_reconstitution_rejects_ancestral_session() {
        let ancestral = session_id(1);
        let version = SessionConfigurationDefaultsVersion::first();
        let defaults = SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(direct(1)));
        let session = SessionReconstitutionInput::new(
            ancestral,
            ancestral,
            SessionCreationProvenance::new(
                SessionCreationCause::Interactive,
                TranscriptAncestry::SingleSource {
                    source_session: session_id(9),
                    source_frontier: transcript_frontier(9),
                },
            ),
            ancestral,
            version,
            ancestral,
            version,
            defaults,
            crate::SessionPlacementReconstitutionFacts {
                current_pointer_session: ancestral,
                current_pointer_version: crate::SessionPlacementVersion::INITIAL,
                selected_event_session: ancestral,
                selected_event: crate::VersionedSessionPlacement::initial(
                    crate::SessionPlacement::pathless(),
                ),
            },
        )
        .reconstitute()
        .expect("ancestral session facts are fully correlated");
        let queued = accepted_origin(1);

        let failure = assert_input_rejects_unchanged(queued_input(&session, queued));

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::UnsupportedSessionAncestry
        );
    }

    /// S03 / INV-009: every stored session and turn correlation on one
    /// scheduling record must repeat the owning identities exactly; each
    /// cross-wired stored identity fails closed with its own failure.
    #[test]
    fn s03_inv009_reconstitution_rejects_cross_wired_record_identities() {
        let session = current_session();
        let queued = accepted_origin(1);
        let other_session = session_id(2);
        let other_turn = turn_id(99);

        let mut turn_session_facts = queued_input(&session, queued);
        turn_session_facts.turns[0].stored_session = other_session;
        let turn_session = assert_input_rejects_unchanged(turn_session_facts);
        assert_eq!(
            turn_session,
            AcceptedInputSchedulingReconstitutionFailure::TurnSessionMismatch {
                turn: queued.turn(),
            }
        );

        let mut accepted_input_session_facts = queued_input(&session, queued);
        accepted_input_session_facts.turns[0].accepted_input_session = other_session;
        let accepted_input_session = assert_input_rejects_unchanged(accepted_input_session_facts);
        assert_eq!(
            accepted_input_session,
            AcceptedInputSchedulingReconstitutionFailure::AcceptedInputSessionMismatch {
                turn: queued.turn(),
            }
        );

        let mut queue_session_facts = queued_input(&session, queued);
        queue_session_facts.turns[0].queue_session = other_session;
        let queue_session = assert_input_rejects_unchanged(queue_session_facts);
        assert_eq!(
            queue_session,
            AcceptedInputSchedulingReconstitutionFailure::QueueSessionMismatch {
                turn: queued.turn(),
            }
        );

        let mut queue_turn_facts = queued_input(&session, queued);
        queue_turn_facts.turns[0].queue_turn = other_turn;
        let queue_turn = assert_input_rejects_unchanged(queue_turn_facts);
        assert_eq!(
            queue_turn,
            AcceptedInputSchedulingReconstitutionFailure::QueueTurnMismatch {
                turn: queued.turn(),
            }
        );

        expect![[r#"
            ┌───────────────────────────────────────────┬─────────────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact                     │ failure                                                                             │
            ├───────────────────────────────────────────┼─────────────────────────────────────────────────────────────────────────────────────┤
            │ turn record session cross-wired           │ TurnSessionMismatch { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) }          │
            │ accepted-input record session cross-wired │ AcceptedInputSessionMismatch { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) } │
            │ queue record session cross-wired          │ QueueSessionMismatch { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) }         │
            │ queue record turn cross-wired             │ QueueTurnMismatch { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) }            │
            └───────────────────────────────────────────┴─────────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&table([
            ReconstitutionFailureRow {
                perturbed_stored_fact: "turn record session cross-wired",
                failure: format!("{turn_session:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "accepted-input record session cross-wired",
                failure: format!("{accepted_input_session:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "queue record session cross-wired",
                failure: format!("{queue_session:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "queue record turn cross-wired",
                failure: format!("{queue_turn:?}"),
            },
        ]));
    }

    /// S03 / INV-004 / INV-009: two turn records cannot both claim one
    /// accepted input as their typed durable origin.
    #[test]
    fn s03_inv009_reconstitution_rejects_shared_accepted_input_identity() {
        let session = current_session();
        let first = accepted_origin(1);
        let second = accepted_origin(2);
        let mut input = queued_input(&session, first);
        input
            .turns
            .push(second.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
        input.turns[1].accepted_input = AcceptedInputLifecycle::new(
            first.accepted_input(),
            AcceptedInputDisposition::OriginOf(second.turn()),
        );

        let failure = assert_input_rejects_unchanged(input);

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::DuplicateAcceptedInput {
                accepted_input: first.accepted_input(),
            }
        );
    }

    /// S03 / INV-009: a delegation-origin turn fact cannot also be represented
    /// by an accepted-input lifecycle record.
    #[test]
    fn s03_inv009_reconstitution_rejects_delegated_accepted_turn_fact() {
        let session = current_session();
        let queued = accepted_origin(1);
        let input = queued_input(&session, queued).with_delegated_turn_facts(vec![
            DelegatedTurnSchedulingFact::new(
                queued.turn(),
                SessionConfigurationDefaultsVersion::first(),
                direct(1),
                DelegatedTurnSchedulingState::Active,
            ),
        ]);

        let failure = assert_input_rejects_unchanged(input);

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::DelegatedTurnFactMismatch {
                turn: queued.turn(),
            }
        );
    }

    /// S03 / INV-009: complete delegation-origin turn facts cannot duplicate
    /// the same stored turn identity.
    #[test]
    fn s03_inv009_reconstitution_rejects_duplicate_delegated_turn_fact() {
        let session = current_session();
        let queued = accepted_origin(1);
        let delegated = turn_id(99);
        let fact = DelegatedTurnSchedulingFact::new(
            delegated,
            SessionConfigurationDefaultsVersion::first(),
            direct(1),
            DelegatedTurnSchedulingState::Active,
        );
        let input = queued_input(&session, queued).with_delegated_turn_facts(vec![fact, fact]);

        let failure = assert_input_rejects_unchanged(input);

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::DelegatedTurnFactMismatch {
                turn: delegated,
            }
        );
    }

    /// S18 / INV-008 / INV-009: a delegated model-identity entry must match
    /// the exact configuration frozen by its stored turn origin.
    #[test]
    fn s18_inv008_inv009_delegated_model_identity_requires_stored_configuration() {
        let session = current_session();
        let queued = accepted_origin(1);
        let delegated = turn_id(99);
        let identity_entry = semantic_entry(99);
        let mut input = queued_input(&session, queued).with_delegated_turn_facts(vec![
            DelegatedTurnSchedulingFact::new(
                delegated,
                SessionConfigurationDefaultsVersion::first(),
                direct(1),
                DelegatedTurnSchedulingState::Active,
            ),
        ]);
        input
            .semantic_entries
            .push(SemanticTranscriptEntryReconstitutionInput::new(
                identity_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::ModelIdentityChanged {
                    turn: delegated,
                    defaults_version: SessionConfigurationDefaultsVersion::first(),
                    selected: direct(2),
                },
            ));

        let failure = assert_input_rejects_unchanged(input);

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::SemanticEntryStateMismatch {
                entry: identity_entry.id(),
            }
        );
    }

    /// S18 / INV-009: a delegated terminal semantic entry must match the
    /// independently stored delegated lifecycle state.
    #[test]
    fn s18_inv009_delegated_terminal_entry_requires_stored_lifecycle() {
        let session = current_session();
        let queued = accepted_origin(1);
        let delegated = turn_id(99);
        let failure_entry = semantic_entry(99);
        let mut input = queued_input(&session, queued).with_delegated_turn_facts(vec![
            DelegatedTurnSchedulingFact::new(
                delegated,
                SessionConfigurationDefaultsVersion::first(),
                direct(1),
                DelegatedTurnSchedulingState::Active,
            ),
        ]);
        input
            .semantic_entries
            .push(SemanticTranscriptEntryReconstitutionInput::new(
                failure_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::TurnFailed { turn: delegated },
            ));

        let failure = assert_input_rejects_unchanged(input);

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::SemanticEntryStateMismatch {
                entry: failure_entry.id(),
            }
        );
    }

    /// S03 / INV-007 / INV-009: immutable queue facts that cannot form one
    /// durable total order fail closed with the exact derivation error.
    #[test]
    fn s03_inv009_reconstitution_rejects_underivable_queue_order() {
        let session = current_session();
        let first = accepted_origin(1);
        let second = accepted_origin(2);
        let mut input = queued_input(&session, first);
        input
            .turns
            .push(second.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
        input.turns[1].order = AcceptedInputQueueOrder::ordinary(first.position());

        let failure = assert_input_rejects_unchanged(input);

        // Turn identities descend as acceptance ordinals ascend, so the
        // second fixture holds the lower canonical turn identity.
        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::InvalidQueueOrder {
                error: AcceptedInputQueueOrderError::DuplicateAcceptancePosition {
                    position: first.position(),
                    first_turn: second.turn(),
                    second_turn: first.turn(),
                },
            }
        );
    }

    /// S03 / INV-009: a stored semantic entry must name the scheduling
    /// session as its source session.
    #[test]
    fn s03_inv009_reconstitution_rejects_cross_session_semantic_entry() {
        let session = current_session();
        let active = accepted_origin(1);
        let other_session = session_id(2);
        let origin_entry = ActiveReconstitutionFacts::matching_origin_entry();
        let mut facts = ActiveReconstitutionFacts::matching(&session, active);
        facts.semantic_entries[0] = SemanticTranscriptEntryReconstitutionInput::new(
            origin_entry.id(),
            other_session,
            InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: active.accepted_input(),
            },
        );

        let failure = assert_reconstitution_rejects_unchanged(facts);

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::SemanticEntrySourceSessionMismatch {
                entry: origin_entry.id(),
            }
        );
    }

    /// S03 / INV-009: the same source-qualified semantic entry cannot appear
    /// twice in the complete entry collection.
    #[test]
    fn s03_inv009_reconstitution_rejects_duplicate_semantic_entry() {
        let session = current_session();
        let active = accepted_origin(1);
        let origin_entry = ActiveReconstitutionFacts::matching_origin_entry();
        let mut facts = ActiveReconstitutionFacts::matching(&session, active);
        facts
            .semantic_entries
            .push(active.entry(&session, origin_entry));

        let failure = assert_reconstitution_rejects_unchanged(facts);

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntry {
                entry: origin_entry.reference(&session),
            }
        );
    }

    /// S03 / INV-009: a failed marker naming a turn absent from the complete
    /// scheduling inventory fails closed.
    #[test]
    fn s03_inv009_reconstitution_rejects_semantic_entry_without_subject() {
        let session = current_session();
        let queued = accepted_origin(1);
        let unknown_turn = turn_id(99);
        let stray_entry = semantic_entry(31);
        let mut input = queued_input(&session, queued);
        input
            .semantic_entries
            .push(SemanticTranscriptEntryReconstitutionInput::new(
                stray_entry.id(),
                session.id(),
                InitialSemanticTranscriptEntryPayload::TurnFailed { turn: unknown_turn },
            ));

        let failure = assert_input_rejects_unchanged(input);

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::SemanticEntrySubjectMissing {
                entry: stray_entry.id(),
            }
        );
    }

    /// S03 / INV-009: an origin entry for a turn whose stored lifecycle is
    /// still queued contradicts that turn's state and fails closed.
    #[test]
    fn s03_inv009_reconstitution_rejects_origin_entry_for_queued_turn() {
        let session = current_session();
        let queued = accepted_origin(1);
        let origin_entry = semantic_entry(30);
        let mut input = queued_input(&session, queued);
        input
            .semantic_entries
            .push(queued.entry(&session, origin_entry));

        let failure = assert_input_rejects_unchanged(input);

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::SemanticEntryStateMismatch {
                entry: origin_entry.id(),
            }
        );
    }

    /// S03 / INV-009: one started turn owns exactly one origin entry; a
    /// second origin entry naming the same accepted input fails closed.
    #[test]
    fn s03_inv009_reconstitution_rejects_second_origin_entry_for_one_turn() {
        let session = current_session();
        let active = accepted_origin(1);
        let second_origin_entry = semantic_entry(31);
        let mut facts = ActiveReconstitutionFacts::matching(&session, active);
        facts
            .semantic_entries
            .push(active.entry(&session, second_origin_entry));

        let failure = assert_reconstitution_rejects_unchanged(facts);

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::DuplicateSemanticEntryForSubject {
                entry: second_origin_entry.id(),
            }
        );
    }

    /// S03 / INV-009: a started turn requires its exact origin entry; an
    /// absent origin fails closed instead of deriving a start without one.
    #[test]
    fn s03_inv009_reconstitution_rejects_started_turn_without_origin_entry() {
        let session = current_session();
        let active = accepted_origin(1);
        let mut facts = ActiveReconstitutionFacts::matching(&session, active);
        // The starting snapshot must stop referencing the removed entry, or
        // the snapshot-reference check would mask the origin-entry check.
        facts.semantic_entries.clear();
        facts.snapshots =
            vec![ActiveReconstitutionFacts::matching_starting_frontier().snapshot(&session, &[])];

        let failure = assert_reconstitution_rejects_unchanged(facts);

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::MissingOriginEntry {
                turn: active.turn(),
            }
        );
    }

    /// S09 / INV-009 / INV-015: a failed turn requires its exact failed
    /// marker; an absent marker fails closed instead of accepting the
    /// stored terminal frontier on faith.
    #[test]
    fn s09_reconstitution_rejects_failed_turn_without_failure_marker() {
        let session = current_session();
        let failed = accepted_origin(1);
        let origin_entry = FailedTerminalReconstitutionFacts::matching_origin_entry();
        let mut facts = FailedTerminalReconstitutionFacts::matching(&session, failed);
        // The terminal snapshot must stop referencing the removed marker, or
        // the snapshot-reference check would mask the failed-marker check.
        facts.semantic_entries = vec![failed.entry(&session, origin_entry)];
        facts.snapshots = vec![
            FailedTerminalReconstitutionFacts::matching_starting_frontier()
                .snapshot(&session, &[origin_entry]),
            FailedTerminalReconstitutionFacts::matching_terminal_frontier()
                .snapshot(&session, &[origin_entry]),
        ];

        let failure = assert_input_rejects_unchanged(facts.input());

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::MissingFailureEntry {
                turn: failed.turn(),
            }
        );
    }

    /// S03 / INV-016: a supplied acceptance tail requires an active turn; a
    /// tail alongside a queued-only projection fails closed.
    #[test]
    fn s03_inv016_reconstitution_rejects_tail_without_active_turn() {
        let session = current_session();
        let queued = accepted_origin(1);
        let mut input = queued_input(&session, queued);
        input.active_acceptance_tail = Some(queued.active_tail(&session));

        let failure = assert_input_rejects_unchanged(input);

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::UnexpectedActiveAcceptanceTail
        );
    }

    /// S03 / S08 / INV-016: every tail entry belongs to the
    /// scheduling session and appears exactly once; a cross-session entry or
    /// a repeated accepted-input identity fails closed.
    #[test]
    fn active_reconstitution_rejects_cross_session_or_repeated_tail_entries() {
        let session = current_session();
        let active = accepted_origin(1);
        let second = accepted_origin(2);

        let other_session = session_id(2);
        let mut cross_session_facts = ActiveReconstitutionFacts::matching(&session, active);
        cross_session_facts
            .acceptance_tail
            .as_mut()
            .expect("matching facts include the acceptance tail")
            .entries[0]
            .session = other_session;
        let cross_session = assert_reconstitution_rejects_unchanged(cross_session_facts);
        assert_eq!(
            cross_session,
            AcceptedInputSchedulingReconstitutionFailure::AcceptanceTailEntrySessionMismatch {
                accepted_input: active.accepted_input(),
            }
        );

        let mut repeated_facts = ActiveReconstitutionFacts::matching(&session, active);
        let repeated_tail = repeated_facts
            .acceptance_tail
            .as_mut()
            .expect("matching facts include the acceptance tail");
        repeated_tail.observed_last_position = second.position();
        repeated_tail
            .entries
            .push(SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    active.accepted_input(),
                    AcceptedInputDisposition::PendingSteering {
                        binding: crate::SteeringBinding::new(active.turn()),
                    },
                ),
                second.position(),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: active.turn(),
                },
            ));
        let repeated = assert_reconstitution_rejects_unchanged(repeated_facts);
        assert_eq!(
            repeated,
            AcceptedInputSchedulingReconstitutionFailure::DuplicateAcceptanceTailEntry {
                accepted_input: active.accepted_input(),
            }
        );

        expect![[r#"
            ┌────────────────────────────────┬──────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact          │ failure                                                                                                      │
            ├────────────────────────────────┼──────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
            │ tail entry session cross-wired │ AcceptanceTailEntrySessionMismatch { accepted_input: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffe) } │
            │ tail entry identity repeated   │ DuplicateAcceptanceTailEntry { accepted_input: AcceptedInputId(00000000-0000-0000-7fff-fffffffffffe) }       │
            └────────────────────────────────┴──────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&table([
            ReconstitutionFailureRow {
                perturbed_stored_fact: "tail entry session cross-wired",
                failure: format!("{cross_session:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "tail entry identity repeated",
                failure: format!("{repeated:?}"),
            },
        ]));
    }

    /// S03 / INV-009 / INV-015: every stored snapshot is owned by the
    /// scheduling session, unique, duplicate-free, and backed by supplied
    /// entries; each malformed snapshot collection fails closed.
    #[test]
    fn s03_inv015_reconstitution_rejects_malformed_snapshot_collection() {
        let session = current_session();
        let active = accepted_origin(1);
        let origin_entry = ActiveReconstitutionFacts::matching_origin_entry();
        let starting_frontier = ActiveReconstitutionFacts::matching_starting_frontier();

        let other_session = session_id(2);
        let mut cross_session_facts = ActiveReconstitutionFacts::matching(&session, active);
        cross_session_facts.snapshots[0] = ResolvedContextFrontierReconstitutionInput::new(
            other_session,
            starting_frontier.id(),
            vec![origin_entry.reference(&session)],
        );
        let cross_session = assert_reconstitution_rejects_unchanged(cross_session_facts);
        assert_eq!(
            cross_session,
            AcceptedInputSchedulingReconstitutionFailure::SnapshotOwningSessionMismatch {
                snapshot: starting_frontier.id(),
            }
        );

        let mut duplicate_facts = ActiveReconstitutionFacts::matching(&session, active);
        duplicate_facts
            .snapshots
            .push(starting_frontier.snapshot(&session, &[origin_entry]));
        let duplicate = assert_reconstitution_rejects_unchanged(duplicate_facts);
        assert_eq!(
            duplicate,
            AcceptedInputSchedulingReconstitutionFailure::DuplicateSnapshot {
                snapshot: starting_frontier.id(),
            }
        );

        let mut membership_facts = ActiveReconstitutionFacts::matching(&session, active);
        membership_facts.snapshots[0] =
            starting_frontier.snapshot(&session, &[origin_entry, origin_entry]);
        let membership = assert_reconstitution_rejects_unchanged(membership_facts);
        assert_eq!(
            membership,
            AcceptedInputSchedulingReconstitutionFailure::InvalidSnapshotMembership {
                snapshot: starting_frontier.id(),
            }
        );

        let absent_entry = semantic_entry(99);
        let mut unbacked_facts = ActiveReconstitutionFacts::matching(&session, active);
        unbacked_facts.snapshots[0] = starting_frontier.snapshot(&session, &[absent_entry]);
        let unbacked = assert_reconstitution_rejects_unchanged(unbacked_facts);
        assert_eq!(
            unbacked,
            AcceptedInputSchedulingReconstitutionFailure::SnapshotEntryMissing {
                snapshot: starting_frontier.id(),
                entry: absent_entry.reference(&session),
            }
        );

        expect![[r#"
            ┌────────────────────────────────────┬───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact              │ failure                                                                                                                                                                                                                                                                   │
            ├────────────────────────────────────┼───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
            │ snapshot owner cross-wired         │ SnapshotOwningSessionMismatch { snapshot: ContextFrontierId(00000000-0000-0000-0000-000000000028) }                                                                                                                                                                       │
            │ snapshot identity repeated         │ DuplicateSnapshot { snapshot: ContextFrontierId(00000000-0000-0000-0000-000000000028) }                                                                                                                                                                                   │
            │ snapshot membership entry repeated │ InvalidSnapshotMembership { snapshot: ContextFrontierId(00000000-0000-0000-0000-000000000028) }                                                                                                                                                                           │
            │ snapshot entry unsupplied          │ SnapshotEntryMissing { snapshot: ContextFrontierId(00000000-0000-0000-0000-000000000028), entry: SemanticTranscriptEntryRef { source_session: SessionId(00000000-0000-0000-0000-000000000001), entry: SemanticTranscriptEntryId(00000000-0000-0000-0000-000000000063) } } │
            └────────────────────────────────────┴───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&table([
            ReconstitutionFailureRow {
                perturbed_stored_fact: "snapshot owner cross-wired",
                failure: format!("{cross_session:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "snapshot identity repeated",
                failure: format!("{duplicate:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "snapshot membership entry repeated",
                failure: format!("{membership:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "snapshot entry unsupplied",
                failure: format!("{unbacked:?}"),
            },
        ]));
    }

    /// S03 / S09 / INV-009 / INV-015: a stored start or failed terminal must
    /// name a snapshot present in the complete supplied set; an absent
    /// snapshot fails closed. Together with the frontier-exactness
    /// rejections, this validated precondition backs eligibility's
    /// failed-terminal-prefix expectation when preparing a successor.
    #[test]
    fn reconstitution_rejects_absent_starting_or_terminal_snapshot() {
        let session = current_session();
        let absent_frontier = frontier(99);

        let active = accepted_origin(1);
        let mut starting_facts = ActiveReconstitutionFacts::matching(&session, active);
        starting_facts.replace_starting_frontier(absent_frontier.id());
        let starting = assert_reconstitution_rejects_unchanged(starting_facts);
        assert_eq!(
            starting,
            AcceptedInputSchedulingReconstitutionFailure::StartingSnapshotMissing {
                turn: active.turn(),
            }
        );

        let failed = accepted_origin(1);
        let mut terminal_facts = FailedTerminalReconstitutionFacts::matching(&session, failed);
        terminal_facts.replace_terminal_frontier(absent_frontier.id());
        let terminal = assert_input_rejects_unchanged(terminal_facts.input());
        assert_eq!(
            terminal,
            AcceptedInputSchedulingReconstitutionFailure::TerminalSnapshotMissing {
                turn: failed.turn(),
            }
        );

        expect![[r#"
            ┌─────────────────────────────────┬────────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact           │ failure                                                                        │
            ├─────────────────────────────────┼────────────────────────────────────────────────────────────────────────────────┤
            │ stored starting snapshot absent │ StartingSnapshotMissing { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) } │
            │ stored terminal snapshot absent │ TerminalSnapshotMissing { turn: TurnId(00000000-0000-0000-ffff-fffffffffffe) } │
            └─────────────────────────────────┴────────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&table([
            ReconstitutionFailureRow {
                perturbed_stored_fact: "stored starting snapshot absent",
                failure: format!("{starting:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "stored terminal snapshot absent",
                failure: format!("{terminal:?}"),
            },
        ]));
    }

    /// S03 / INV-009 / INV-015: a supplied snapshot that no stored lifecycle
    /// fact references cannot ride along; the complete collection fails
    /// closed. This is the read-side rejection recorded for orphan committed
    /// snapshot headers.
    #[test]
    fn s03_inv015_reconstitution_rejects_unreferenced_snapshot() {
        let session = current_session();
        let active = accepted_origin(1);
        let stray_frontier = frontier(90);
        let mut facts = ActiveReconstitutionFacts::matching(&session, active);
        facts.snapshots.push(stray_frontier.snapshot(
            &session,
            &[ActiveReconstitutionFacts::matching_origin_entry()],
        ));

        let failure = assert_reconstitution_rejects_unchanged(facts);

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::UnreferencedSnapshot {
                snapshot: stray_frontier.id(),
            }
        );
    }

    /// S03 / INV-009: durable total order admits only a failed-terminal
    /// prefix, at most one active slot, and a queued suffix; every
    /// out-of-order stored lifecycle fails closed on the first offending
    /// turn.
    #[test]
    fn s03_inv009_reconstitution_rejects_out_of_order_lifecycle_states() {
        let session = current_session();
        let earlier = accepted_origin(1);
        let later = accepted_origin(2);

        let mut active_after_queued_facts = ActiveReconstitutionFacts::matching(&session, later);
        active_after_queued_facts
            .turns
            .push(earlier.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
        let active_after_queued =
            assert_reconstitution_rejects_unchanged(active_after_queued_facts);
        assert_eq!(
            active_after_queued,
            AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder {
                turn: later.turn(),
            }
        );

        let mut terminal_after_queued_facts =
            FailedTerminalReconstitutionFacts::matching(&session, later);
        terminal_after_queued_facts
            .turns
            .push(earlier.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
        let terminal_after_queued =
            assert_input_rejects_unchanged(terminal_after_queued_facts.input());
        assert_eq!(
            terminal_after_queued,
            AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder {
                turn: later.turn(),
            }
        );

        // The ordering check rejects a second active slot before any
        // duplicate current-attempt bookkeeping, which is why the stored
        // attempt identity may repeat here: DuplicateCurrentAttempt is
        // unreachable behind this rejection.
        let mut second_active_facts = ActiveReconstitutionFacts::matching(&session, earlier);
        second_active_facts.turns.push(later.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::Active {
                starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                starting_frontier: ActiveReconstitutionFacts::matching_starting_frontier().id(),
                phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                    later.turn(),
                    matching_active_attempt(),
                ),
            },
        ));
        let second_active = assert_reconstitution_rejects_unchanged(second_active_facts);
        assert_eq!(
            second_active,
            AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder {
                turn: later.turn(),
            }
        );

        // The ordering check rejects the record before consulting its
        // frontier facts, so the claimed snapshots need not be supplied.
        let mut terminal_after_active_facts =
            ActiveReconstitutionFacts::matching(&session, earlier);
        terminal_after_active_facts.turns.push(later.record(
            &session,
            AcceptedInputTurnSchedulingRecordState::TerminalFailed {
                starting_lineage: AcceptedInputStartingLineage::After {
                    immediate_predecessor: earlier.turn(),
                },
                starting_frontier: frontier(98).id(),
                terminal_execution: None,
                terminal_frontier: frontier(99).id(),
            },
        ));
        let terminal_after_active =
            assert_reconstitution_rejects_unchanged(terminal_after_active_facts);
        assert_eq!(
            terminal_after_active,
            AcceptedInputSchedulingReconstitutionFailure::InvalidLifecycleOrder {
                turn: later.turn(),
            }
        );

        expect![[r#"
            ┌───────────────────────────────────────┬──────────────────────────────────────────────────────────────────────────────┐
            │ perturbed_stored_fact                 │ failure                                                                      │
            ├───────────────────────────────────────┼──────────────────────────────────────────────────────────────────────────────┤
            │ active slot after queued work         │ InvalidLifecycleOrder { turn: TurnId(00000000-0000-0000-ffff-fffffffffffd) } │
            │ failed terminal after queued work     │ InvalidLifecycleOrder { turn: TurnId(00000000-0000-0000-ffff-fffffffffffd) } │
            │ second active slot                    │ InvalidLifecycleOrder { turn: TurnId(00000000-0000-0000-ffff-fffffffffffd) } │
            │ failed terminal after the active slot │ InvalidLifecycleOrder { turn: TurnId(00000000-0000-0000-ffff-fffffffffffd) } │
            └───────────────────────────────────────┴──────────────────────────────────────────────────────────────────────────────┘
        "#]]
        .assert_eq(&table([
            ReconstitutionFailureRow {
                perturbed_stored_fact: "active slot after queued work",
                failure: format!("{active_after_queued:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "failed terminal after queued work",
                failure: format!("{terminal_after_queued:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "second active slot",
                failure: format!("{second_active:?}"),
            },
            ReconstitutionFailureRow {
                perturbed_stored_fact: "failed terminal after the active slot",
                failure: format!("{terminal_after_active:?}"),
            },
        ]));
    }

    /// S03 / INV-009 / INV-015: the stored starting lineage must equal the
    /// lineage derived from durable total order; a first-in-session active
    /// turn cannot claim a predecessor.
    #[test]
    fn s03_inv009_reconstitution_rejects_stored_lineage_disagreeing_with_order() {
        let session = current_session();
        let active = accepted_origin(1);
        let claimed_lineage = AcceptedInputStartingLineage::After {
            immediate_predecessor: turn_id(99),
        };
        let mut facts = ActiveReconstitutionFacts::matching(&session, active);
        facts.replace_starting_lineage(claimed_lineage);

        let failure = assert_reconstitution_rejects_unchanged(facts);

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::StartingLineageMismatch {
                turn: active.turn(),
                expected: AcceptedInputStartingLineage::FirstInSession,
                actual: claimed_lineage,
            }
        );
    }

    /// INV-015 / INV-089: attachment origins hidden by completed context
    /// compaction do not contribute to the rendered frontier bound.
    #[test]
    fn inv015_inv089_rendered_frontier_origins_exclude_compacted_input() {
        let session = current_session();
        let hidden_input = accepted_input_id(1);
        let hidden_origin = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(1),
            session.id(),
            InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: hidden_input,
            },
        );
        let terminal = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(2),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnCompleted { turn: turn_id(1) },
        );
        let summary = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(3),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ContextSummary {
                producing_call: model_call_id(4),
                summarized: crate::ContextCompactionRange::inclusive(
                    hidden_origin.reference(),
                    terminal.reference(),
                ),
                value: AssistantText::try_new(String::from("summary"))
                    .expect("fixture summary is nonempty"),
            },
        );
        let snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
            session.id(),
            context_frontier_id(5),
            vec![
                hidden_origin.reference(),
                terminal.reference(),
                summary.reference(),
            ],
        )
        .expect("the complete frontier retains compacted entries");
        let semantic_entries = BTreeMap::from([
            (hidden_origin.reference(), hidden_origin),
            (terminal.reference(), terminal),
            (summary.reference(), summary),
        ]);

        assert_eq!(
            AcceptedInputSchedulingProjection::rendered_frontier_origins(
                Some(&snapshot),
                &semantic_entries,
            ),
            Some(Vec::new())
        );
    }

    /// S03 / INV-009 / INV-015: the stored starting snapshot must be exactly
    /// the predecessor prefix plus the turn's origin entry; a snapshot
    /// omitting the origin fails closed.
    #[test]
    fn s03_inv015_reconstitution_rejects_starting_snapshot_omitting_origin() {
        let session = current_session();
        let active = accepted_origin(1);
        let mut facts = ActiveReconstitutionFacts::matching(&session, active);
        facts.snapshots =
            vec![ActiveReconstitutionFacts::matching_starting_frontier().snapshot(&session, &[])];

        let failure = assert_reconstitution_rejects_unchanged(facts);

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch {
                turn: active.turn(),
            }
        );
    }

    /// S03 / INV-015: after a completed compaction, the exact compacted
    /// result followed by the next turn's origin is a valid starting
    /// frontier even though the predecessor frontier remains complete.
    #[test]
    fn s03_inv015_reconstitution_accepts_exact_compaction_result_then_origin() {
        let session = current_session();
        let predecessor_turn = turn_id(1);
        let active_turn = turn_id(2);
        let predecessor_entry = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(1),
            session.id(),
            InitialSemanticTranscriptEntryPayload::TurnCompleted {
                turn: predecessor_turn,
            },
        );
        let origin_entry = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(3),
            session.id(),
            InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                accepted_input: accepted_input_id(2),
            },
        );
        let range = crate::ContextCompactionRange::inclusive(
            predecessor_entry.reference(),
            predecessor_entry.reference(),
        );
        let compaction_call = model_call_id(4);
        let summary_entry = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(5),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ContextSummary {
                producing_call: compaction_call,
                summarized: range,
                value: AssistantText::try_new(String::from("summary"))
                    .expect("fixture summary is nonempty"),
            },
        );
        let predecessor_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
            session.id(),
            context_frontier_id(6),
            vec![predecessor_entry.reference()],
        )
        .expect("the predecessor fixture is a unique complete frontier");
        let compacted_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
            session.id(),
            context_frontier_id(7),
            vec![predecessor_entry.reference(), summary_entry.reference()],
        )
        .expect("the compaction result appends the summary");
        let starting_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
            session.id(),
            context_frontier_id(8),
            vec![
                predecessor_entry.reference(),
                summary_entry.reference(),
                origin_entry.reference(),
            ],
        )
        .expect("the next start appends its origin to the compaction result");
        let call = crate::ContextCompactionModelCallReconstitutionInput::new(
            compaction_call,
            session.id(),
            direct(9),
            ResolvedProviderTarget::naming(provider_model_identity(10)),
            predecessor_snapshot.frontier().snapshot(),
            crate::ContextCompactionModelCallState::Terminal(
                crate::ModelCallDisposition::Completed,
            ),
            crate::ContextCompactionTokenUsage::unreported(),
        )
        .reconstitute(&predecessor_snapshot)
        .expect("the dedicated call exactly names the predecessor frontier");
        let compaction = crate::ContextCompactionReconstitutionInput::new(
            crate::ContextCompactionId::from_uuid(uuid::Uuid::from_u128(11)),
            session.id(),
            None,
            predecessor_snapshot.frontier().snapshot(),
            compacted_snapshot.frontier().snapshot(),
            compaction_call,
            range,
            summary_entry.identity(),
        )
        .reconstitute(
            &predecessor_snapshot,
            &compacted_snapshot,
            std::slice::from_ref(&predecessor_entry),
            &[predecessor_entry.clone(), summary_entry.clone()],
            &summary_entry,
            &call,
        )
        .expect("the exact compaction facts reconstruct");
        let mut compactions = BTreeMap::from([(compaction.id(), compaction)]);
        let mut snapshots = BTreeMap::from([
            (
                predecessor_snapshot.frontier().snapshot(),
                predecessor_snapshot.clone(),
            ),
            (
                compacted_snapshot.frontier().snapshot(),
                compacted_snapshot.clone(),
            ),
            (
                starting_snapshot.frontier().snapshot(),
                starting_snapshot.clone(),
            ),
        ]);
        let origins = BTreeMap::from([(active_turn, origin_entry.reference())]);
        let mut referenced_snapshots = BTreeSet::new();
        let compaction_chain = compactions.values().collect::<Vec<_>>();

        let start = validate_start(
            1,
            active_turn,
            AcceptedInputStartingLineage::After {
                immediate_predecessor: predecessor_turn,
            },
            starting_snapshot.frontier().snapshot(),
            None,
            Some(&(predecessor_turn, predecessor_snapshot.clone())),
            &origins,
            None,
            &compaction_chain,
            &snapshots,
            &mut referenced_snapshots,
        )
        .expect("the validated compacted frontier remains an exact start");

        assert_eq!(start.frontier(), starting_snapshot.frontier());

        let successor_range = crate::ContextCompactionRange::inclusive(
            summary_entry.reference(),
            origin_entry.reference(),
        );
        let successor_call = model_call_id(13);
        let successor_summary = SemanticTranscriptEntry::from_validated_parts(
            semantic_transcript_entry_id(14),
            session.id(),
            InitialSemanticTranscriptEntryPayload::ContextSummary {
                producing_call: successor_call,
                summarized: successor_range,
                value: AssistantText::try_new(String::from("newer summary"))
                    .expect("fixture summary is nonempty"),
            },
        );
        let successor_result = ResolvedContextFrontierSnapshot::try_from_candidate(
            session.id(),
            context_frontier_id(15),
            vec![
                predecessor_entry.reference(),
                summary_entry.reference(),
                origin_entry.reference(),
                successor_summary.reference(),
            ],
        )
        .expect("the successor compaction appends its summary");
        let successor_call_record = crate::ContextCompactionModelCallReconstitutionInput::new(
            successor_call,
            session.id(),
            direct(9),
            ResolvedProviderTarget::naming(provider_model_identity(10)),
            starting_snapshot.frontier().snapshot(),
            crate::ContextCompactionModelCallState::Terminal(
                crate::ModelCallDisposition::Completed,
            ),
            crate::ContextCompactionTokenUsage::unreported(),
        )
        .reconstitute(&starting_snapshot)
        .expect("the successor call names its exact source");
        let predecessor_compaction = compactions
            .values()
            .next()
            .expect("the first compaction is present");
        let successor_compaction = crate::ContextCompactionReconstitutionInput::new(
            crate::ContextCompactionId::from_uuid(uuid::Uuid::from_u128(16)),
            session.id(),
            Some(predecessor_compaction.id()),
            starting_snapshot.frontier().snapshot(),
            successor_result.frontier().snapshot(),
            successor_call,
            successor_range,
            successor_summary.identity(),
        )
        .reconstitute(
            &starting_snapshot,
            &successor_result,
            &[
                predecessor_entry.clone(),
                summary_entry.clone(),
                origin_entry.clone(),
            ],
            &[
                predecessor_entry.clone(),
                summary_entry.clone(),
                origin_entry.clone(),
                successor_summary.clone(),
            ],
            &successor_summary,
            &successor_call_record,
        )
        .expect("the successor compaction reconstructs");
        compactions.insert(successor_compaction.id(), successor_compaction);
        snapshots.insert(successor_result.frontier().snapshot(), successor_result);
        let compaction_chain = [
            compactions
                .values()
                .find(|compaction| compaction.predecessor().is_none())
                .expect("the root compaction is present"),
            compactions
                .values()
                .find(|compaction| compaction.predecessor().is_some())
                .expect("the successor compaction is present"),
        ];
        let mut historical_references = BTreeSet::new();

        let historical_start = validate_start(
            1,
            active_turn,
            AcceptedInputStartingLineage::After {
                immediate_predecessor: predecessor_turn,
            },
            starting_snapshot.frontier().snapshot(),
            None,
            Some(&(predecessor_turn, predecessor_snapshot.clone())),
            &origins,
            None,
            &compaction_chain,
            &snapshots,
            &mut historical_references,
        )
        .expect("the intervening historical start retains the earlier summary");

        assert_eq!(historical_start.frontier(), starting_snapshot.frontier());

        let stale_snapshot = ResolvedContextFrontierSnapshot::try_from_candidate(
            session.id(),
            context_frontier_id(12),
            vec![predecessor_entry.reference(), origin_entry.reference()],
        )
        .expect("the stale fixture omits only the required summary append");
        snapshots.insert(stale_snapshot.frontier().snapshot(), stale_snapshot.clone());
        let mut stale_references = BTreeSet::new();

        let stale_failure = validate_start(
            1,
            active_turn,
            AcceptedInputStartingLineage::After {
                immediate_predecessor: predecessor_turn,
            },
            stale_snapshot.frontier().snapshot(),
            None,
            Some(&(predecessor_turn, predecessor_snapshot)),
            &origins,
            None,
            &compaction_chain,
            &snapshots,
            &mut stale_references,
        )
        .expect_err("a post-compaction start cannot omit the summary result");

        assert_eq!(
            stale_failure,
            AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch {
                turn: active_turn,
            }
        );
    }

    /// S03 / S09 / INV-009 / INV-015: each start owns a distinct snapshot; a
    /// successor start naming its predecessor's already-referenced starting
    /// snapshot fails closed. With the content-exactness rejection, this
    /// backs eligibility's expectation that fresh snapshot identities
    /// preserve the validated prefix.
    #[test]
    fn s09_reconstitution_rejects_starting_frontier_reused_from_predecessor() {
        let session = current_session();
        let predecessor = accepted_origin(1);
        let active = accepted_origin(2);
        let active_origin_entry = semantic_entry(32);
        let active_delivery = DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: predecessor.turn(),
            configuration: PerInputConfigurationChoices::new(
                SessionConfigurationDefaultsVersion::first(),
                ModelSelectionOverride::UseSessionDefault,
            ),
        };
        let mut facts = FailedTerminalReconstitutionFacts::matching(&session, predecessor);
        facts.turns.push(active.record_with(
            &session,
            OriginRecordFacts {
                order: active.ordinary_order(),
                delivery: active_delivery,
                state: AcceptedInputTurnSchedulingRecordState::Active {
                    starting_lineage: AcceptedInputStartingLineage::After {
                        immediate_predecessor: predecessor.turn(),
                    },
                    // The perturbation: the successor claims its
                    // predecessor's starting snapshot instead of a distinct
                    // successor prefix snapshot.
                    starting_frontier:
                        FailedTerminalReconstitutionFacts::matching_starting_frontier().id(),
                    phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                        active.turn(),
                        matching_active_attempt(),
                    ),
                },
            },
        ));
        facts
            .semantic_entries
            .push(active.entry(&session, active_origin_entry));
        facts.acceptance_tail = Some(SessionAcceptanceTailReconstitutionInput::new(
            session.id(),
            active.accepted_input(),
            active.position(),
            vec![SessionAcceptanceTailEntryReconstitutionInput::new(
                session.id(),
                AcceptedInputLifecycle::new(
                    active.accepted_input(),
                    AcceptedInputDisposition::OriginOf(active.turn()),
                ),
                active.position(),
                active_delivery,
            )],
        ));

        let failure = assert_input_rejects_unchanged(facts.input());

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::StartingFrontierMismatch {
                turn: active.turn(),
            }
        );
    }

    /// S09 / INV-009: an all-terminal projection holds no queued work;
    /// eligibility rejects instead of manufacturing a candidate.
    #[test]
    fn s09_eligibility_rejects_projection_without_queued_work() {
        let session = current_session();
        let failed = accepted_origin(1);
        let activation = activation(1);
        let projection = FailedTerminalReconstitutionFacts::matching(&session, failed)
            .input()
            .reconstitute()
            .expect("the complete failed-terminal record is valid");

        let failure = assert_eligibility_rejects_unchanged(projection, activation.identities());

        assert_eq!(failure, AcceptedInputEligibilityFailure::NoQueuedTurn);
    }

    /// S01 / S09 / INV-009: a proposed origin-entry identity colliding with
    /// a committed semantic entry fails closed before any candidate is
    /// prepared.
    #[test]
    fn eligibility_rejects_committed_origin_entry_identity() {
        let session = current_session();
        let failed = accepted_origin(1);
        let queued = accepted_origin(2);
        let activation = activation(1);
        let mut facts = FailedTerminalReconstitutionFacts::matching(&session, failed);
        facts
            .turns
            .push(queued.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
        let projection = facts
            .input()
            .reconstitute()
            .expect("a failed-terminal prefix with one queued successor is valid");
        let committed_origin_entry = FailedTerminalReconstitutionFacts::matching_origin_entry();

        let failure = assert_eligibility_rejects_unchanged(
            projection,
            activation.identities_with_origin_entry(committed_origin_entry.id()),
        );

        assert_eq!(
            failure,
            AcceptedInputEligibilityFailure::OriginEntryIdentityAlreadyExists
        );
    }

    /// S01 / S09 / INV-009 / INV-015: a proposed starting-snapshot identity
    /// colliding with a committed session-scoped snapshot fails closed
    /// before any candidate is prepared.
    #[test]
    fn eligibility_rejects_committed_starting_frontier_identity() {
        let session = current_session();
        let failed = accepted_origin(1);
        let queued = accepted_origin(2);
        let activation = activation(1);
        let mut facts = FailedTerminalReconstitutionFacts::matching(&session, failed);
        facts
            .turns
            .push(queued.record(&session, AcceptedInputTurnSchedulingRecordState::Queued));
        let projection = facts
            .input()
            .reconstitute()
            .expect("a failed-terminal prefix with one queued successor is valid");
        let committed_frontier = FailedTerminalReconstitutionFacts::matching_terminal_frontier();

        let failure = assert_eligibility_rejects_unchanged(
            projection,
            activation.identities_with_starting_frontier(committed_frontier.id()),
        );

        assert_eq!(
            failure,
            AcceptedInputEligibilityFailure::StartingFrontierIdentityAlreadyExists
        );
    }

    /// S03 / INV-015: a prepared standalone compaction call survives complete
    /// reconstitution and prevents queued-turn activation until recovery.
    #[test]
    fn s03_inv015_prepared_compaction_call_blocks_activation_after_reconstitution() {
        let session = current_session();
        let source = ResolvedContextFrontierReconstitutionInput::new(
            session.id(),
            context_frontier_id(701),
            Vec::new(),
        );
        let call = model_call_id(702);
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            Vec::new(),
            Vec::new(),
            vec![source],
            None,
        )
        .with_context_compaction_facts(
            vec![crate::ContextCompactionModelCallReconstitutionInput::new(
                call,
                session.id(),
                direct(703),
                ResolvedProviderTarget::naming(provider_model_identity(704)),
                context_frontier_id(701),
                crate::ContextCompactionModelCallState::Prepared,
                crate::ContextCompactionTokenUsage::unreported(),
            )],
            Vec::new(),
        );
        let projection = input
            .reconstitute()
            .expect("prepared compaction evidence remains recoverable");
        let error = projection
            .prepare_earliest_queued_activation(activation(705).identities())
            .expect_err("unfinished compaction owns the execution slot");

        assert_eq!(
            error.failure(),
            AcceptedInputEligibilityFailure::ContextCompactionInProgress { call }
        );
    }

    /// S03 / INV-015: an authorized standalone compaction call remains
    /// recoverable and owns the execution slot after restart reconstitution.
    #[test]
    fn s03_inv015_in_flight_compaction_call_blocks_activation_after_reconstitution() {
        let session = current_session();
        let source = ResolvedContextFrontierReconstitutionInput::new(
            session.id(),
            context_frontier_id(706),
            Vec::new(),
        );
        let call = model_call_id(707);
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session.clone(),
            Vec::new(),
            Vec::new(),
            vec![source],
            None,
        )
        .with_context_compaction_facts(
            vec![crate::ContextCompactionModelCallReconstitutionInput::new(
                call,
                session.id(),
                direct(708),
                ResolvedProviderTarget::naming(provider_model_identity(709)),
                context_frontier_id(706),
                crate::ContextCompactionModelCallState::InFlight,
                crate::ContextCompactionTokenUsage::unreported(),
            )],
            Vec::new(),
        );
        let projection = input
            .reconstitute()
            .expect("in-flight compaction evidence remains recoverable");
        let error = projection
            .prepare_earliest_queued_activation(activation(710).identities())
            .expect_err("authorized compaction owns the execution slot");

        assert_eq!(
            error.failure(),
            AcceptedInputEligibilityFailure::ContextCompactionInProgress { call }
        );
    }

    /// S03 / INV-015: a terminal non-completed dedicated call is retained as
    /// historical recovery evidence without requiring a compaction result.
    #[test]
    fn s03_inv015_known_failed_compaction_call_is_legal_standalone_evidence() {
        let session = current_session();
        let source = ResolvedContextFrontierReconstitutionInput::new(
            session.id(),
            context_frontier_id(711),
            Vec::new(),
        );
        let call = crate::ContextCompactionModelCallReconstitutionInput::new(
            model_call_id(712),
            session.id(),
            direct(713),
            ResolvedProviderTarget::naming(provider_model_identity(714)),
            context_frontier_id(711),
            crate::ContextCompactionModelCallState::Terminal(ModelCallDisposition::KnownFailed),
            crate::ContextCompactionTokenUsage::unreported(),
        );
        let input = AcceptedInputSchedulingReconstitutionInput::new(
            session,
            Vec::new(),
            Vec::new(),
            vec![source],
            None,
        )
        .with_context_compaction_facts(vec![call], Vec::new());

        let projection = input
            .reconstitute()
            .expect("known-failed compaction evidence is complete without a summary");
        let error = projection
            .prepare_earliest_queued_activation(activation(715).identities())
            .expect_err("the fixture contains no queued turn");

        assert_eq!(
            error.failure(),
            AcceptedInputEligibilityFailure::NoQueuedTurn
        );
    }

    /// INV-001 / INV-015: ordinary and compaction call maps cannot claim the
    /// same identity even when both purpose-specific records are valid alone.
    #[test]
    fn inv001_inv015_reconstitution_rejects_cross_kind_model_call_identity() {
        let session = current_session();
        let active = accepted_origin(1);
        let consumed = accepted_origin(2);
        let facts = ConsumedSteeringReconstitutionFacts::matching(&session, active, consumed);
        let call = facts.model_calls[0].id();
        let source = facts.model_calls[0].frontier();
        let collision = crate::ContextCompactionModelCallReconstitutionInput::new(
            call,
            session.id(),
            direct(1),
            ResolvedProviderTarget::naming(provider_model_identity(51)),
            source,
            crate::ContextCompactionModelCallState::Prepared,
            crate::ContextCompactionTokenUsage::unreported(),
        );
        let input = facts
            .input()
            .with_context_compaction_facts(vec![collision], Vec::new());

        let failure = assert_input_rejects_unchanged(input);

        assert_eq!(
            failure,
            AcceptedInputSchedulingReconstitutionFailure::DuplicateModelCallIdentityAcrossKinds {
                call,
            }
        );
    }

    /// INV-009 / INV-044: checked relational runner-loss facts reconstitute the
    /// exact closed active phase without a live turn attempt.
    #[test]
    fn inv009_inv044_runner_recovery_phase_reconstitutes_exact_loss_subject() {
        let owning_turn = turn_id(801);
        let runner = crate::RunnerId::from_uuid(uuid::Uuid::from_u128(802));
        let revision = crate::RunnerGeneration::try_from_u64(3)
            .expect("the fixture placement revision is positive");
        let interrupted_tool_attempt = Some(tool_attempt_id(803));
        let input = ActiveTurnSchedulingReconstitutionInput::awaiting_runner_recovery(
            owning_turn,
            runner,
            revision,
            interrupted_tool_attempt,
            None,
        );

        assert_eq!(
            input.canonical_evidence_free_phase(),
            Some(ActiveTurnPhase::AwaitingRunnerRecovery {
                runner,
                placement_revision: revision,
                optional_tool_attempt: interrupted_tool_attempt,
            })
        );
    }

    /// INV-009: an interrupt successor authenticated against an external
    /// terminal predecessor remains ahead of older ordinary queued work.
    #[test]
    fn inv009_external_interrupt_chain_is_the_first_accepted_order_root() {
        let older_ordinary = turn_id(811);
        let external_successor = turn_id(812);
        let interrupt_descendant = turn_id(813);
        let later_ordinary = turn_id(814);
        let ordinary_roots = BTreeSet::from([older_ordinary, later_ordinary]);
        let queued_turns = BTreeSet::from([
            older_ordinary,
            external_successor,
            interrupt_descendant,
            later_ordinary,
        ]);

        let promoted = super::promote_external_interrupt_chains(
            vec![
                older_ordinary,
                external_successor,
                interrupt_descendant,
                later_ordinary,
            ],
            BTreeSet::from([external_successor]),
            &ordinary_roots,
            &queued_turns,
        );

        assert_eq!(
            promoted,
            vec![
                external_successor,
                interrupt_descendant,
                older_ordinary,
                later_ordinary,
            ]
        );
    }

    /// INV-009: later external chains retain their historical placement once
    /// the oldest crossing chain is promoted ahead of queued work.
    #[test]
    fn inv009_multiple_external_interrupt_chains_are_retained_in_order() {
        let older_ordinary = turn_id(821);
        let first_external_successor = turn_id(822);
        let first_descendant = turn_id(823);
        let second_external_successor = turn_id(824);
        let second_descendant = turn_id(825);
        let later_ordinary = turn_id(826);
        let ordinary_roots = BTreeSet::from([older_ordinary, later_ordinary]);
        let queued_turns = BTreeSet::from([
            older_ordinary,
            first_external_successor,
            first_descendant,
            second_external_successor,
            second_descendant,
            later_ordinary,
        ]);

        let promoted = super::promote_external_interrupt_chains(
            vec![
                older_ordinary,
                first_external_successor,
                first_descendant,
                second_external_successor,
                second_descendant,
                later_ordinary,
            ],
            BTreeSet::from([first_external_successor, second_external_successor]),
            &ordinary_roots,
            &queued_turns,
        );

        assert_eq!(
            promoted,
            vec![
                first_external_successor,
                first_descendant,
                older_ordinary,
                second_external_successor,
                second_descendant,
                later_ordinary,
            ]
        );
    }

    /// INV-009: an external interrupt chain does not cross a completed
    /// accepted-input terminal prefix.
    #[test]
    fn inv009_external_interrupt_chain_retains_terminal_prefix() {
        let terminal = turn_id(831);
        let external_successor = turn_id(832);
        let ordinary_roots = BTreeSet::from([terminal]);

        let promoted = super::promote_external_interrupt_chains(
            vec![terminal, external_successor],
            BTreeSet::from([external_successor]),
            &ordinary_roots,
            &BTreeSet::from([external_successor]),
        );

        assert_eq!(promoted, vec![terminal, external_successor]);
    }

    /// INV-009: an external interrupt chain crosses queued ordinary work but
    /// retains the completed accepted-input terminal prefix.
    #[test]
    fn inv009_external_interrupt_chain_precedes_only_queued_prefix() {
        let terminal = turn_id(841);
        let older_queued = turn_id(842);
        let external_successor = turn_id(843);
        let ordinary_roots = BTreeSet::from([terminal, older_queued]);
        let queued_turns = BTreeSet::from([older_queued, external_successor]);

        let promoted = super::promote_external_interrupt_chains(
            vec![terminal, older_queued, external_successor],
            BTreeSet::from([external_successor]),
            &ordinary_roots,
            &queued_turns,
        );

        assert_eq!(promoted, vec![terminal, external_successor, older_queued]);
    }

    /// INV-009: a historical external terminal does not hide the later
    /// external interrupt chain that actually crosses queued ordinary work.
    #[test]
    fn inv009_later_external_interrupt_chain_crosses_queued_work() {
        let historical_external = turn_id(851);
        let older_queued = turn_id(852);
        let crossing_external = turn_id(853);
        let ordinary_roots = BTreeSet::from([older_queued]);
        let queued_turns = BTreeSet::from([older_queued, crossing_external]);

        let promoted = super::promote_external_interrupt_chains(
            vec![older_queued, historical_external, crossing_external],
            BTreeSet::from([historical_external, crossing_external]),
            &ordinary_roots,
            &queued_turns,
        );

        assert_eq!(
            promoted,
            vec![historical_external, crossing_external, older_queued]
        );
    }
}
