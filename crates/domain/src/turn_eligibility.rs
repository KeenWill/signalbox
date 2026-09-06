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
        let response_matches_disposition = match ended.disposition() {
            ModelCallDisposition::Completed => true,
            ModelCallDisposition::Refused => entries.iter().all(|entry| {
                matches!(
                    semantic_entries
                        .get(entry)
                        .map(SemanticTranscriptEntry::payload),
                    Some(InitialSemanticTranscriptEntryPayload::ProviderCompaction {
                        producing_call,
                        ..
                    }) if *producing_call == *call
                )
            }),
            ModelCallDisposition::KnownFailed
            | ModelCallDisposition::Cancelled
            | ModelCallDisposition::Ambiguous => false,
        };
        if !response_matches_disposition || (!accepted_turn_matches && !delegated_turn_matches) {
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
                let assistant_entries = assistant_by_call
                    .get(refusing_call)
                    .cloned()
                    .unwrap_or_default();
                if !referenced_snapshots.insert(*terminal_frontier)
                    || !refused_terminal_matches(
                        source,
                        &terminal,
                        *refusing_call,
                        &assistant_entries,
                        &semantic_entries,
                    )
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
                    Some(
                        InitialSemanticTranscriptEntryPayload::AssistantText {
                            producing_call,
                            ..
                        }
                        | InitialSemanticTranscriptEntryPayload::ProviderCompaction {
                            producing_call,
                            ..
                        }
                    ) if *producing_call == completing_call
                )
        })
}

fn refused_terminal_matches(
    source: &ResolvedContextFrontierSnapshot,
    terminal: &ResolvedContextFrontierSnapshot,
    refusing_call: crate::ModelCallId,
    assistant_entries: &BTreeSet<SemanticTranscriptEntryRef>,
    semantic_entries: &BTreeMap<SemanticTranscriptEntryRef, SemanticTranscriptEntry>,
) -> bool {
    let suffix_start = source.entry_count();
    if !source.is_semantic_prefix_of(terminal)
        || terminal.entry_count() != suffix_start + assistant_entries.len()
    {
        return false;
    }
    terminal
        .ordered_entries_range(suffix_start, terminal.entry_count())
        .all(|entry| {
            assistant_entries.contains(&entry)
                && matches!(
                    semantic_entries.get(&entry).map(SemanticTranscriptEntry::payload),
                    Some(InitialSemanticTranscriptEntryPayload::ProviderCompaction {
                        producing_call,
                        ..
                    }) if *producing_call == refusing_call
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
                    Some(SemanticTranscriptEntryPayload::ProviderCompaction {
                        producing_call,
                        ..
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
mod tests;
