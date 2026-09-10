//! Caller-supplied submit-input reconstruction and origin inputs for
//! `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::SubmitInput;
use super::reconstituted::ReconstitutedSubmitInput;
use super::validation::validate_turn_origin_reconstitution_input;
use crate::AcceptedInputDisposition;
use crate::AcceptedInputId;
use crate::AcceptedInputLifecycle;
use crate::AcceptedInputQueueOrder;
use crate::Actor;
use crate::AppliedInterruptCommandResult;
use crate::BlobDigest;
use crate::DeliveryRequest;
use crate::DurableCommandId;
use crate::FrozenModelSelection;
use crate::GoalGeneration;
use crate::GoalTurnSource;
use crate::ModelAlias;
use crate::ModelChangeAdjustment;
use crate::ModelSelectionRequest;
use crate::SessionConfigurationDefaults;
use crate::SessionConfigurationDefaultsVersion;
use crate::SessionId;
use crate::SessionInputPosition;
use crate::TurnDisposition;
use crate::TurnId;
use crate::UserContent;
use crate::ValidatedModelSettings;

/// Complete purpose-specific facts for one accepted-input turn origin used by
/// another command's replay.
///
/// The immutable command receipt alone is insufficient because pending
/// steering can later become visible origin work without rewriting its
/// original `PendingSteering` result. Checked submission reconstitution
/// correlates this receipt with the accepted input's current lifecycle, the
/// accepted-input-keyed immutable queue association, and—for reclassification—
/// the canonical terminal source turn before treating it as a predecessor or
/// active source.
#[derive(Clone, Debug)]
pub struct SubmitInputTurnOriginReconstitutionInput {
    pub(super) chain: Vec<SubmitInputTurnOriginReconstitutionFacts>,
}

#[derive(Clone, Debug)]
pub(super) struct SubmitInputTurnOriginReconstitutionFacts {
    pub(super) provenance: TurnOriginProvenance,
    pub(super) lifecycle: AcceptedInputLifecycle,
    pub(super) queue_accepted_input: AcceptedInputId,
    pub(super) queue_session: SessionId,
    pub(super) queue_turn: TurnId,
    pub(super) queue_order: AcceptedInputQueueOrder,
    pub(super) source_terminal: Option<SubmitInputTerminalFacts>,
}

#[derive(Clone, Debug)]
pub(super) enum TurnOriginProvenance {
    Submit(Box<ReconstitutedSubmitInput>),
    Goal(GoalTurnOriginFacts),
}

#[derive(Clone, Debug)]
pub(super) struct GoalTurnOriginFacts {
    _generation: GoalGeneration,
    pub(super) source: GoalTurnSource,
    pub(super) session: SessionId,
    pub(super) accepted_input: AcceptedInputId,
    pub(super) turn: TurnId,
    pub(super) acceptance_position: SessionInputPosition,
    pub(super) content: UserContent,
}

/// Complete purpose-specific facts proving that a reclassified origin's
/// source turn is terminal.
///
/// The source's canonical origin retains a flat chain so directly created and
/// previously reclassified turns use the same checked boundary without
/// recursive validation or destruction. The terminal disposition admits
/// every terminal outcome in docs/spec/turn-lifecycle-and-scheduling.md and
/// is correlated with its explicit owning turn during submission
/// reconstitution.
#[derive(Clone, Debug)]
pub struct SubmitInputTerminalSourceReconstitutionInput {
    origin: SubmitInputTurnOriginReconstitutionInput,
    turn: TurnId,
    disposition: TurnDisposition,
}

#[derive(Clone, Debug)]
pub(super) struct SubmitInputTerminalFacts {
    pub(super) turn: TurnId,
    pub(super) disposition: TurnDisposition,
}

/// Named facts for one canonical terminal source turn.
#[derive(Clone, Debug)]
pub struct SubmitInputTerminalSourceConstructionInput {
    /// The canonical origin facts owned by the terminal source turn.
    pub origin: SubmitInputTurnOriginReconstitutionInput,
    /// The terminal source turn identity.
    pub turn: TurnId,
    /// The authoritative terminal disposition.
    pub disposition: TurnDisposition,
}

/// Named facts for an interrupted ambiguous model-call reconciliation source.
#[derive(Clone, Debug)]
pub struct SubmitInputInterruptedModelCallReconciliationConstructionInput {
    /// The canonical origin facts owned by the terminal source turn.
    pub origin: SubmitInputTurnOriginReconstitutionInput,
    /// The terminal source turn identity.
    pub turn: TurnId,
    /// The unresolved model call requiring reconciliation.
    pub ambiguous_call: crate::ModelCallId,
    /// The applied interrupt proof that stopped the turn.
    pub interrupt: crate::AppliedInterruptProof,
}

/// Named facts for an automatically reconciled ambiguous-operation source.
#[derive(Clone, Debug)]
pub struct SubmitInputAutomaticReconciliationConstructionInput {
    /// The canonical origin facts owned by the terminal source turn.
    pub origin: SubmitInputTurnOriginReconstitutionInput,
    /// The terminal source turn identity.
    pub turn: TurnId,
    /// The unresolved physical operation requiring reconciliation.
    pub ambiguous_operation: crate::IssuedOperationRef,
    /// The one-based durable automatic recovery attempt.
    pub attempt: std::num::NonZeroU32,
}

/// Named facts for an interrupted ambiguous tool-attempt reconciliation source.
#[derive(Clone, Debug)]
pub struct SubmitInputInterruptedToolReconciliationConstructionInput {
    /// The canonical origin facts owned by the terminal source turn.
    pub origin: SubmitInputTurnOriginReconstitutionInput,
    /// The terminal source turn identity.
    pub turn: TurnId,
    /// The unresolved tool attempt requiring reconciliation.
    pub ambiguous_attempt: crate::ToolAttemptId,
    /// The applied interrupt proof that stopped the turn.
    pub interrupt: crate::AppliedInterruptProof,
}

impl SubmitInputTerminalSourceReconstitutionInput {
    /// Supplies the source turn canonical origin facts, terminal-record owner,
    /// and disposition.
    pub fn new(input: SubmitInputTerminalSourceConstructionInput) -> Self {
        let SubmitInputTerminalSourceConstructionInput {
            origin,
            turn,
            disposition,
        } = input;
        Self {
            origin,
            turn,
            disposition,
        }
    }

    /// Supplies a terminal source whose exact ambiguous model call remained
    /// unresolved after an applied interrupt.
    pub fn interrupted_model_call_reconciliation(
        input: SubmitInputInterruptedModelCallReconciliationConstructionInput,
    ) -> Self {
        let SubmitInputInterruptedModelCallReconciliationConstructionInput {
            origin,
            turn,
            ambiguous_call,
            interrupt,
        } = input;
        let ambiguous_operations = crate::NonEmptyIssuedOperationRefs::singleton(
            crate::IssuedOperationRef::ModelCall(ambiguous_call),
        );
        Self::new(SubmitInputTerminalSourceConstructionInput {
            origin,
            turn,
            disposition: TurnDisposition::ReconciliationRequired {
                marker: crate::ReconciliationMarker::from_interrupt_ambiguity(
                    ambiguous_operations,
                    interrupt,
                ),
            },
        })
    }

    /// Supplies a terminal source whose exact ambiguous operation remained
    /// unresolved after one daemon-owned durable recovery attempt.
    pub fn automatic_reconciliation(
        input: SubmitInputAutomaticReconciliationConstructionInput,
    ) -> Self {
        let SubmitInputAutomaticReconciliationConstructionInput {
            origin,
            turn,
            ambiguous_operation,
            attempt,
        } = input;
        let ambiguous_operations =
            crate::NonEmptyIssuedOperationRefs::singleton(ambiguous_operation);
        Self::new(SubmitInputTerminalSourceConstructionInput {
            origin,
            turn,
            disposition: TurnDisposition::ReconciliationRequired {
                marker: crate::ReconciliationMarker::from_automatic_recovery(
                    ambiguous_operations,
                    attempt,
                ),
            },
        })
    }

    /// Supplies a terminal source whose exact ambiguous tool attempt remained
    /// unresolved after an applied interrupt.
    pub fn interrupted_tool_reconciliation(
        input: SubmitInputInterruptedToolReconciliationConstructionInput,
    ) -> Self {
        let SubmitInputInterruptedToolReconciliationConstructionInput {
            origin,
            turn,
            ambiguous_attempt,
            interrupt,
        } = input;
        let ambiguous_operations = crate::NonEmptyIssuedOperationRefs::singleton(
            crate::IssuedOperationRef::ToolAttempt(ambiguous_attempt),
        );
        Self::new(SubmitInputTerminalSourceConstructionInput {
            origin,
            turn,
            disposition: TurnDisposition::ReconciliationRequired {
                marker: crate::ReconciliationMarker::from_interrupt_ambiguity(
                    ambiguous_operations,
                    interrupt,
                ),
            },
        })
    }
}

/// Named durable facts for one goal-owned autonomous turn origin.
#[derive(Clone, Debug)]
pub struct GoalTurnOriginConstructionInput {
    /// Immutable statement generation pursued by the turn.
    pub generation: GoalGeneration,
    /// Event or successful predecessor that caused this turn.
    pub source: GoalTurnSource,
    /// Owning session.
    pub session: SessionId,
    /// Accepted input identity.
    pub accepted_input: AcceptedInputId,
    /// Logical turn identity.
    pub turn: TurnId,
    /// Immutable session acceptance position.
    pub acceptance_position: SessionInputPosition,
    /// Exact statement or resume guidance delivered to the model.
    pub content: UserContent,
    /// Accepted input's current lifecycle.
    pub lifecycle: AcceptedInputLifecycle,
    /// Accepted-input identity keyed by the queue association.
    pub queue_accepted_input: AcceptedInputId,
    /// Session identity stored with the queue association.
    pub queue_session: SessionId,
    /// Turn identity stored with the queue association.
    pub queue_turn: TurnId,
    /// Immutable queue order stored for the origin turn.
    pub queue_order: AcceptedInputQueueOrder,
}

/// Named facts for one directly created accepted-input turn origin.
#[derive(Clone, Debug)]
pub struct SubmitInputDirectTurnOriginConstructionInput {
    /// The immutable command receipt that created the accepted input.
    pub receipt: ReconstitutedSubmitInput,
    /// The accepted input current lifecycle.
    pub lifecycle: AcceptedInputLifecycle,
    /// The accepted-input identity keyed by the queue association.
    pub queue_accepted_input: AcceptedInputId,
    /// The session identity stored with the queue association.
    pub queue_session: SessionId,
    /// The turn identity stored with the queue association.
    pub queue_turn: TurnId,
    /// The immutable queue order stored for the origin turn.
    pub queue_order: AcceptedInputQueueOrder,
}

/// Named facts for steering reclassified into accepted-input origin work.
#[derive(Clone, Debug)]
pub struct SubmitInputReclassifiedTurnOriginConstructionInput {
    /// The immutable command receipt that created the accepted input.
    pub receipt: ReconstitutedSubmitInput,
    /// The accepted input current lifecycle.
    pub lifecycle: AcceptedInputLifecycle,
    /// The accepted-input identity keyed by the queue association.
    pub queue_accepted_input: AcceptedInputId,
    /// The session identity stored with the queue association.
    pub queue_session: SessionId,
    /// The turn identity stored with the queue association.
    pub queue_turn: TurnId,
    /// The immutable queue order stored for the reclassified origin turn.
    pub queue_order: AcceptedInputQueueOrder,
    /// The canonical terminal source turn that released the steering input.
    pub source_terminal: SubmitInputTerminalSourceReconstitutionInput,
}

impl SubmitInputTurnOriginReconstitutionInput {
    /// Supplies a directly created origin immutable receipt, current
    /// accepted-input lifecycle, and accepted-input-keyed queue facts.
    pub fn new(input: SubmitInputDirectTurnOriginConstructionInput) -> Self {
        let SubmitInputDirectTurnOriginConstructionInput {
            receipt,
            lifecycle,
            queue_accepted_input,
            queue_session,
            queue_turn,
            queue_order,
        } = input;
        Self {
            chain: vec![SubmitInputTurnOriginReconstitutionFacts {
                provenance: TurnOriginProvenance::Submit(Box::new(receipt)),
                lifecycle,
                queue_accepted_input,
                queue_session,
                queue_turn,
                queue_order,
                source_terminal: None,
            }],
        }
    }

    /// Supplies a goal-owned origin with its event-stream provenance and
    /// accepted-input-keyed lifecycle and queue facts.
    pub fn from_goal(input: GoalTurnOriginConstructionInput) -> Self {
        let GoalTurnOriginConstructionInput {
            generation,
            source,
            session,
            accepted_input,
            turn,
            acceptance_position,
            content,
            lifecycle,
            queue_accepted_input,
            queue_session,
            queue_turn,
            queue_order,
        } = input;
        Self {
            chain: vec![SubmitInputTurnOriginReconstitutionFacts {
                provenance: TurnOriginProvenance::Goal(GoalTurnOriginFacts {
                    _generation: generation,
                    source,
                    session,
                    accepted_input,
                    turn,
                    acceptance_position,
                    content,
                }),
                lifecycle,
                queue_accepted_input,
                queue_session,
                queue_turn,
                queue_order,
                source_terminal: None,
            }],
        }
    }

    /// Supplies reclassified steering immutable receipt, current lifecycle,
    /// accepted-input-keyed queue facts, and canonical terminal source turn.
    pub fn reclassified(input: SubmitInputReclassifiedTurnOriginConstructionInput) -> Self {
        let SubmitInputReclassifiedTurnOriginConstructionInput {
            receipt,
            lifecycle,
            queue_accepted_input,
            queue_session,
            queue_turn,
            queue_order,
            source_terminal,
        } = input;
        let SubmitInputTerminalSourceReconstitutionInput {
            mut origin,
            turn,
            disposition,
        } = source_terminal;
        origin.chain.push(SubmitInputTurnOriginReconstitutionFacts {
            provenance: TurnOriginProvenance::Submit(Box::new(receipt)),
            lifecycle,
            queue_accepted_input,
            queue_session,
            queue_turn,
            queue_order,
            source_terminal: Some(SubmitInputTerminalFacts { turn, disposition }),
        });
        origin
    }

    pub(crate) fn validated_origin_content(&self) -> Option<(AcceptedInputId, UserContent)> {
        let validated = validate_turn_origin_reconstitution_input(self)?;
        Some((validated.accepted_input, validated.content))
    }
}
/// Exact terminal predecessor facts for an origin that did not come from an
/// accepted input, such as a delegated turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NonAcceptedTurnPredecessorReconstitutionInput {
    /// The session owning the predecessor.
    pub session: SessionId,
    /// The terminal predecessor turn.
    pub turn: TurnId,
}
/// Named facts for reconstructing an applied turn-origin submission.
#[derive(Clone, Debug)]
pub struct SubmitInputAppliedTurnOriginReconstitutionInput {
    /// The canonical durable command.
    pub command: SubmitInput,
    /// The actor spelling stored with the command.
    pub stored_actor: Actor,
    /// The session identity stored in the recorded result.
    pub result_session: SessionId,
    /// The accepted-input identity stored in the recorded result.
    pub result_accepted_input: AcceptedInputId,
    /// The origin turn identity stored in the recorded result.
    pub result_turn: TurnId,
    /// The canonical predecessor origin required by after-current delivery.
    pub predecessor_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
    /// The exact non-accepted terminal predecessor, admitted only for an
    /// interrupt origin when no accepted-input predecessor exists.
    pub non_accepted_predecessor: Option<NonAcceptedTurnPredecessorReconstitutionInput>,
    /// The command identity stored with the accepted input.
    pub accepted_command: DurableCommandId,
    /// The accepted-input identity stored with accepted content.
    pub accepted_input: AcceptedInputId,
    /// The session identity stored with the accepted input.
    pub accepted_session: SessionId,
    /// The exact content stored with the accepted input.
    pub accepted_content: UserContent,
    /// The exact delivery request stored with the accepted input.
    pub accepted_delivery: DeliveryRequest,
    /// The immutable session acceptance position.
    pub accepted_position: SessionInputPosition,
    /// The accepted input current durable disposition.
    pub accepted_disposition: AcceptedInputDisposition,
    /// The session identity stored with the queue association.
    pub queue_session: SessionId,
    /// The turn identity stored with the queue association.
    pub queue_turn: TurnId,
    /// The immutable queue order stored for the origin turn.
    pub queue_order: AcceptedInputQueueOrder,
    /// The session identity owning the selected defaults.
    pub defaults_session: SessionId,
    /// The selected defaults version.
    pub defaults_version: SessionConfigurationDefaultsVersion,
    /// The exact selected session defaults.
    pub defaults: SessionConfigurationDefaults,
    /// The requested model selection stored with the origin.
    pub stored_requested_model: ModelSelectionRequest,
    /// The frozen model selection stored with the origin.
    pub stored_frozen_model: FrozenModelSelection,
    /// The complete resolved model settings stored for the origin.
    pub stored_model_settings: Option<ValidatedModelSettings>,
    /// Ordered automatic model-change adjustments stored for the origin.
    pub stored_model_settings_adjustments: Vec<ModelChangeAdjustment>,
}

/// Named facts for reconstructing an applied pending-steering submission.
#[derive(Clone, Debug)]
pub struct SubmitInputAppliedPendingSteeringReconstitutionInput {
    /// The canonical durable command.
    pub command: SubmitInput,
    /// The actor spelling stored with the command.
    pub stored_actor: Actor,
    /// The session identity stored in the recorded result.
    pub result_session: SessionId,
    /// The accepted-input identity stored in the recorded result.
    pub result_accepted_input: AcceptedInputId,
    /// The source turn identity stored in the recorded result.
    pub result_source_turn: TurnId,
    /// The canonical origin facts for the steering source turn.
    pub source_turn_origin: SubmitInputTurnOriginReconstitutionInput,
    /// The command identity stored with the accepted input.
    pub accepted_command: DurableCommandId,
    /// The accepted-input identity stored with accepted content.
    pub accepted_input: AcceptedInputId,
    /// The session identity stored with the accepted input.
    pub accepted_session: SessionId,
    /// The exact content stored with the accepted input.
    pub accepted_content: UserContent,
    /// The exact delivery request stored with the accepted input.
    pub accepted_delivery: DeliveryRequest,
    /// The immutable session acceptance position.
    pub accepted_position: SessionInputPosition,
}

/// Named facts for reconstructing a missing attachment-blob rejection.
#[derive(Clone, Debug)]
pub struct SubmitInputRejectedAttachmentBlobNotFoundReconstitutionInput {
    /// The canonical durable command.
    pub command: SubmitInput,
    /// The actor spelling stored with the command.
    pub stored_actor: Actor,
    /// The target session identity stored in the result.
    pub result_session: SessionId,
    /// The unavailable attachment digest stored in the result.
    pub result_digest: BlobDigest,
    /// Canonically ordered attachment digests verified before the first unavailable digest.
    pub verified_prefix: Option<Box<[BlobDigest]>>,
}

/// Named facts for reconstructing an attachment-byte-budget rejection.
#[derive(Clone, Debug)]
pub struct SubmitInputRejectedAttachmentByteBudgetExceededReconstitutionInput {
    /// The canonical durable command.
    pub command: SubmitInput,
    /// The actor spelling stored with the command.
    pub stored_actor: Actor,
    /// The target session identity stored in the result.
    pub result_session: SessionId,
    /// The configured maximum per-blob byte count stored in the result.
    pub result_maximum_bytes: u64,
}

/// Named facts for reconstructing a missing-session rejection.
#[derive(Clone, Debug)]
pub struct SubmitInputRejectedSessionNotFoundReconstitutionInput {
    /// The canonical durable command.
    pub command: SubmitInput,
    /// The actor spelling stored with the command.
    pub stored_actor: Actor,
    /// The absent session identity stored in the result.
    pub result_session: SessionId,
}

/// Named facts for reconstructing a no-active-turn rejection.
#[derive(Clone, Debug)]
pub struct SubmitInputRejectedNoActiveTurnReconstitutionInput {
    /// The canonical durable command.
    pub command: SubmitInput,
    /// The actor spelling stored with the command.
    pub stored_actor: Actor,
    /// The target session identity stored in the result.
    pub result_session: SessionId,
    /// The active turn expected by the command.
    pub result_expected_active_turn: TurnId,
}

/// Named facts for reconstructing an active-turn-present rejection.
#[derive(Clone, Debug)]
pub struct SubmitInputRejectedActiveTurnPresentReconstitutionInput {
    /// The canonical durable command.
    pub command: SubmitInput,
    /// The actor spelling stored with the command.
    pub stored_actor: Actor,
    /// The target session identity stored in the result.
    pub result_session: SessionId,
    /// The authoritative active turn stored in the result.
    pub result_active_turn: TurnId,
    /// The canonical origin facts for the active turn.
    pub active_turn_origin: SubmitInputTurnOriginReconstitutionInput,
}

/// Named facts for reconstructing an active-turn-mismatch rejection.
#[derive(Clone, Debug)]
pub struct SubmitInputRejectedActiveTurnMismatchReconstitutionInput {
    /// The canonical durable command.
    pub command: SubmitInput,
    /// The actor spelling stored with the command.
    pub stored_actor: Actor,
    /// The target session identity stored in the result.
    pub result_session: SessionId,
    /// The active turn expected by the command.
    pub result_expected_active_turn: TurnId,
    /// The authoritative active turn stored in the result.
    pub result_actual_active_turn: TurnId,
    /// The canonical origin facts for the authoritative active turn.
    pub actual_turn_origin: SubmitInputTurnOriginReconstitutionInput,
}

/// Named facts for reconstructing a defaults-version-mismatch rejection.
#[derive(Clone, Debug)]
pub struct SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
    /// The canonical durable command.
    pub command: SubmitInput,
    /// The actor spelling stored with the command.
    pub stored_actor: Actor,
    /// The target session identity stored in the result.
    pub result_session: SessionId,
    /// The defaults version expected by the command.
    pub result_expected: SessionConfigurationDefaultsVersion,
    /// The authoritative current defaults version.
    pub result_current: SessionConfigurationDefaultsVersion,
    /// The canonical active-turn origin when the session had active work.
    pub active_turn_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
}

/// Named facts for reconstructing an unknown-model-alias rejection.
#[derive(Clone, Debug)]
pub struct SubmitInputRejectedUnknownModelAliasReconstitutionInput {
    /// The canonical durable command.
    pub command: SubmitInput,
    /// The actor spelling stored with the command.
    pub stored_actor: Actor,
    /// The target session identity stored in the result.
    pub result_session: SessionId,
    /// The unresolved alias stored in the result.
    pub result_alias: ModelAlias,
    /// The session identity owning the selected defaults.
    pub defaults_session: SessionId,
    /// The selected defaults version.
    pub defaults_version: SessionConfigurationDefaultsVersion,
    /// The exact selected session defaults.
    pub defaults: SessionConfigurationDefaults,
    /// The canonical active-turn origin when the session had active work.
    pub active_turn_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
}

/// Named facts for reconstructing an exhausted-acceptance-position rejection.
#[derive(Clone, Debug)]
pub struct SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput {
    /// The canonical durable command.
    pub command: SubmitInput,
    /// The actor spelling stored with the command.
    pub stored_actor: Actor,
    /// The target session identity stored in the result.
    pub result_session: SessionId,
    /// The last representable acceptance position stored in the result.
    pub result_last_position: SessionInputPosition,
    /// The canonical active-turn origin when the session had active work.
    pub active_turn_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
}

/// Named facts for reconstructing a safe-point-unavailable rejection.
#[derive(Clone, Debug)]
pub struct SubmitInputRejectedSafePointUnavailableWhileStoppingReconstitutionInput {
    /// The canonical durable command.
    pub command: SubmitInput,
    /// The actor spelling stored with the command.
    pub stored_actor: Actor,
    /// The target session identity stored in the result.
    pub result_session: SessionId,
    /// The authoritative active turn stored in the result.
    pub result_active_turn: TurnId,
    /// The canonical origin facts for the active turn.
    pub active_turn_origin: SubmitInputTurnOriginReconstitutionInput,
    /// The applied interrupt already stopping the active turn.
    pub existing_interrupt: AppliedInterruptCommandResult,
}

/// Named facts for reconstructing an interrupt-already-applied rejection.
#[derive(Clone, Debug)]
pub struct SubmitInputRejectedInterruptAlreadyAppliedReconstitutionInput {
    /// The canonical durable command.
    pub command: SubmitInput,
    /// The actor spelling stored with the command.
    pub stored_actor: Actor,
    /// The target session identity stored in the result.
    pub result_session: SessionId,
    /// The authoritative active turn stored in the result.
    pub result_active_turn: TurnId,
    /// The earlier interrupt command identity stored in the result.
    pub result_existing_command: DurableCommandId,
    /// The canonical origin facts for the active turn.
    pub active_turn_origin: SubmitInputTurnOriginReconstitutionInput,
    /// The earlier applied interrupt that retains cancellation authority.
    pub existing_interrupt: AppliedInterruptCommandResult,
}

/// Named facts for reconstructing a parked-approval interrupt rejection.
#[derive(Clone, Debug)]
pub struct SubmitInputRejectedInterruptUnavailableWhileAwaitingApprovalReconstitutionInput {
    /// The canonical durable command.
    pub command: SubmitInput,
    /// The actor spelling stored with the command.
    pub stored_actor: Actor,
    /// The target session identity stored in the result.
    pub result_session: SessionId,
    /// The authoritative active turn stored in the result.
    pub result_active_turn: TurnId,
    /// The canonical origin facts for the active turn.
    pub active_turn_origin: SubmitInputTurnOriginReconstitutionInput,
}
