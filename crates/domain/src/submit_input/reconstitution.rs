//! Submit-input stored facts and checked reconstruction for
//! `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::SubmitInput;
use super::SubmitInputAppliedResult;
use super::SubmitInputPendingSteeringAppliedResult;
use super::SubmitInputRejectedResult;
use super::SubmitInputResult;
use super::SubmitInputTurnOriginAppliedResult;
use super::reconstituted::ReconstitutedSubmitInput;
use super::reconstituted::SubmitInputReconstitutionError;
use super::reconstituted::SubmitInputReconstitutionFailure;
use super::reconstitution_input::NonAcceptedTurnPredecessorReconstitutionInput;
use super::reconstitution_input::SubmitInputAppliedPendingSteeringReconstitutionInput;
use super::reconstitution_input::SubmitInputAppliedTurnOriginReconstitutionInput;
use super::reconstitution_input::SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput;
use super::reconstitution_input::SubmitInputRejectedActiveTurnMismatchReconstitutionInput;
use super::reconstitution_input::SubmitInputRejectedActiveTurnPresentReconstitutionInput;
use super::reconstitution_input::SubmitInputRejectedAttachmentBlobNotFoundReconstitutionInput;
use super::reconstitution_input::SubmitInputRejectedAttachmentByteBudgetExceededReconstitutionInput;
use super::reconstitution_input::SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput;
use super::reconstitution_input::SubmitInputRejectedInterruptAlreadyAppliedReconstitutionInput;
use super::reconstitution_input::SubmitInputRejectedInterruptUnavailableWhileAwaitingApprovalReconstitutionInput;
use super::reconstitution_input::SubmitInputRejectedNoActiveTurnReconstitutionInput;
use super::reconstitution_input::SubmitInputRejectedSafePointUnavailableWhileStoppingReconstitutionInput;
use super::reconstitution_input::SubmitInputRejectedSessionNotFoundReconstitutionInput;
use super::reconstitution_input::SubmitInputRejectedUnknownModelAliasReconstitutionInput;
use super::reconstitution_input::SubmitInputTurnOriginReconstitutionInput;
use super::validation::StoredOriginConfigurationReconstitutionFacts;
use super::validation::expected_active_turn;
use super::validation::position_exhaustion_origin;
use super::validation::reconstruct_origin_configuration;
use super::validation::rejection_configuration;
use super::validation::validate_existing_interrupt;
use super::validation::validate_rejection_active_turn_origin;
use super::validation::validate_turn_origin_reconstitution_input;
use crate::AcceptedInputDisposition;
use crate::AcceptedInputId;
use crate::AcceptedInputQueueOrder;
use crate::AcceptedInputQueuePriority;
use crate::Actor;
use crate::AppliedInterruptCommandResult;
use crate::BlobDigest;
use crate::DeliveryRequest;
use crate::DurableCommandId;
use crate::FrozenModelSelection;
use crate::ModelAlias;
use crate::ModelChangeAdjustment;
use crate::ModelSelectionRequest;
use crate::SessionConfigurationDefaults;
use crate::SessionConfigurationDefaultsVersion;
use crate::SessionId;
use crate::SessionInputPosition;
use crate::SteeringBinding;
use crate::TurnId;
use crate::UserContent;
use crate::ValidatedModelSettings;
use crate::VersionedSessionConfigurationDefaults;

#[derive(Clone, Debug)]
pub(super) struct SubmitInputTurnOriginAppliedReconstitutionFacts {
    pub(super) result_session: SessionId,
    result_accepted_input: AcceptedInputId,
    pub(super) result_turn: TurnId,
    pub(super) predecessor_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
    pub(super) non_accepted_predecessor: Option<NonAcceptedTurnPredecessorReconstitutionInput>,
    pub(super) accepted_command: DurableCommandId,
    pub(super) accepted_input: AcceptedInputId,
    pub(super) accepted_session: SessionId,
    pub(super) accepted_content: UserContent,
    pub(super) accepted_delivery: DeliveryRequest,
    pub(super) accepted_position: SessionInputPosition,
    pub(super) accepted_disposition: AcceptedInputDisposition,
    pub(super) queue_session: SessionId,
    pub(super) queue_turn: TurnId,
    pub(super) queue_order: AcceptedInputQueueOrder,
    pub(super) defaults_session: SessionId,
    pub(super) defaults_version: SessionConfigurationDefaultsVersion,
    defaults: SessionConfigurationDefaults,
    pub(super) stored_requested_model: ModelSelectionRequest,
    pub(super) stored_frozen_model: FrozenModelSelection,
    stored_model_settings: Option<ValidatedModelSettings>,
    stored_model_settings_adjustments: Box<[ModelChangeAdjustment]>,
}
#[derive(Clone, Debug)]
pub(super) struct SubmitInputPendingSteeringAppliedReconstitutionFacts {
    pub(super) result_session: SessionId,
    result_accepted_input: AcceptedInputId,
    pub(super) result_source_turn: TurnId,
    pub(super) source_turn_origin: SubmitInputTurnOriginReconstitutionInput,
    pub(super) accepted_command: DurableCommandId,
    pub(super) accepted_input: AcceptedInputId,
    pub(super) accepted_session: SessionId,
    pub(super) accepted_content: UserContent,
    pub(super) accepted_delivery: DeliveryRequest,
    pub(super) accepted_position: SessionInputPosition,
}

#[derive(Clone, Debug)]
pub(super) enum SubmitInputReconstitutionFacts {
    AppliedTurnOrigin(Box<SubmitInputTurnOriginAppliedReconstitutionFacts>),
    AppliedPendingSteering(Box<SubmitInputPendingSteeringAppliedReconstitutionFacts>),
    RejectedAttachmentBlobNotFound {
        result_session: SessionId,
        result_digest: BlobDigest,
        verified_prefix: Option<Box<[BlobDigest]>>,
    },
    RejectedAttachmentByteBudgetExceeded {
        result_session: SessionId,
        result_maximum_bytes: u64,
    },
    RejectedSessionNotFound {
        result_session: SessionId,
    },
    RejectedNoActiveTurn {
        result_session: SessionId,
        result_expected_active_turn: TurnId,
    },
    RejectedActiveTurnPresent {
        result_session: SessionId,
        result_active_turn: TurnId,
        active_turn_origin: SubmitInputTurnOriginReconstitutionInput,
    },
    RejectedActiveTurnMismatch {
        result_session: SessionId,
        result_expected_active_turn: TurnId,
        result_actual_active_turn: TurnId,
        actual_turn_origin: SubmitInputTurnOriginReconstitutionInput,
    },
    RejectedDefaultsVersionMismatch {
        result_session: SessionId,
        result_expected: SessionConfigurationDefaultsVersion,
        result_current: SessionConfigurationDefaultsVersion,
        active_turn_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
    },
    RejectedUnknownModelAlias {
        result_session: SessionId,
        result_alias: ModelAlias,
        defaults_session: SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
        active_turn_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
    },
    RejectedAcceptancePositionExhausted {
        result_session: SessionId,
        result_last_position: SessionInputPosition,
        active_turn_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
    },
    RejectedSafePointUnavailableWhileStopping {
        result_session: SessionId,
        result_active_turn: TurnId,
        active_turn_origin: SubmitInputTurnOriginReconstitutionInput,
        existing_interrupt: AppliedInterruptCommandResult,
    },
    RejectedInterruptAlreadyApplied {
        result_session: SessionId,
        result_active_turn: TurnId,
        result_existing_command: DurableCommandId,
        active_turn_origin: SubmitInputTurnOriginReconstitutionInput,
        existing_interrupt: AppliedInterruptCommandResult,
    },
    RejectedInterruptUnavailableWhileAwaitingApproval {
        result_session: SessionId,
        result_active_turn: TurnId,
        active_turn_origin: SubmitInputTurnOriginReconstitutionInput,
    },
}
/// Complete checked domain inputs for reconstructing one recorded submission.
///
/// The stored actor is the durable spelling of the command's attributed
/// agency and is supplied separately for the domain-owned comparison.
#[derive(Clone, Debug)]
pub struct SubmitInputReconstitutionInput {
    pub(super) command: SubmitInput,
    pub(super) stored_actor: Actor,
    pub(super) facts: SubmitInputReconstitutionFacts,
}

impl SubmitInputReconstitutionInput {
    /// Supplies every recorded turn-origin result and durable effect
    /// correlation.
    pub fn applied_turn_origin(input: SubmitInputAppliedTurnOriginReconstitutionInput) -> Self {
        let SubmitInputAppliedTurnOriginReconstitutionInput {
            command,
            stored_actor,
            result_session,
            result_accepted_input,
            result_turn,
            predecessor_origin,
            non_accepted_predecessor,
            accepted_command,
            accepted_input,
            accepted_session,
            accepted_content,
            accepted_delivery,
            accepted_position,
            accepted_disposition,
            queue_session,
            queue_turn,
            queue_order,
            defaults_session,
            defaults_version,
            defaults,
            stored_requested_model,
            stored_frozen_model,
            stored_model_settings,
            stored_model_settings_adjustments,
        } = input;
        Self {
            command,
            stored_actor,
            facts: SubmitInputReconstitutionFacts::AppliedTurnOrigin(Box::new(
                SubmitInputTurnOriginAppliedReconstitutionFacts {
                    result_session,
                    result_accepted_input,
                    result_turn,
                    predecessor_origin,
                    non_accepted_predecessor,
                    accepted_command,
                    accepted_input,
                    accepted_session,
                    accepted_content,
                    accepted_delivery,
                    accepted_position,
                    accepted_disposition,
                    queue_session,
                    queue_turn,
                    queue_order,
                    defaults_session,
                    defaults_version,
                    defaults,
                    stored_requested_model,
                    stored_frozen_model,
                    stored_model_settings,
                    stored_model_settings_adjustments: stored_model_settings_adjustments
                        .into_boxed_slice(),
                },
            )),
        }
    }

    /// Supplies the immutable receipt facts for one accepted safe-point input.
    ///
    /// The accepted input mutable current disposition is deliberately not an
    /// input: normal steering consumption or reclassification cannot rewrite
    /// the original command result.
    pub fn applied_pending_steering(
        input: SubmitInputAppliedPendingSteeringReconstitutionInput,
    ) -> Self {
        let SubmitInputAppliedPendingSteeringReconstitutionInput {
            command,
            stored_actor,
            result_session,
            result_accepted_input,
            result_source_turn,
            source_turn_origin,
            accepted_command,
            accepted_input,
            accepted_session,
            accepted_content,
            accepted_delivery,
            accepted_position,
        } = input;
        Self {
            command,
            stored_actor,
            facts: SubmitInputReconstitutionFacts::AppliedPendingSteering(Box::new(
                SubmitInputPendingSteeringAppliedReconstitutionFacts {
                    result_session,
                    result_accepted_input,
                    result_source_turn,
                    source_turn_origin,
                    accepted_command,
                    accepted_input,
                    accepted_session,
                    accepted_content,
                    accepted_delivery,
                    accepted_position,
                },
            )),
        }
    }

    /// Supplies a recorded missing attachment-blob result.
    pub fn rejected_attachment_blob_not_found(
        input: SubmitInputRejectedAttachmentBlobNotFoundReconstitutionInput,
    ) -> Self {
        let SubmitInputRejectedAttachmentBlobNotFoundReconstitutionInput {
            command,
            stored_actor,
            result_session,
            result_digest,
            verified_prefix,
        } = input;
        Self {
            command,
            stored_actor,
            facts: SubmitInputReconstitutionFacts::RejectedAttachmentBlobNotFound {
                result_session,
                result_digest,
                verified_prefix,
            },
        }
    }

    /// Supplies a recorded attachment-byte-budget result.
    pub fn rejected_attachment_byte_budget_exceeded(
        input: SubmitInputRejectedAttachmentByteBudgetExceededReconstitutionInput,
    ) -> Self {
        let SubmitInputRejectedAttachmentByteBudgetExceededReconstitutionInput {
            command,
            stored_actor,
            result_session,
            result_maximum_bytes,
        } = input;
        Self {
            command,
            stored_actor,
            facts: SubmitInputReconstitutionFacts::RejectedAttachmentByteBudgetExceeded {
                result_session,
                result_maximum_bytes,
            },
        }
    }

    /// Supplies a recorded missing-session result.
    pub fn rejected_session_not_found(
        input: SubmitInputRejectedSessionNotFoundReconstitutionInput,
    ) -> Self {
        let SubmitInputRejectedSessionNotFoundReconstitutionInput {
            command,
            stored_actor,
            result_session,
        } = input;
        Self {
            command,
            stored_actor,
            facts: SubmitInputReconstitutionFacts::RejectedSessionNotFound { result_session },
        }
    }

    /// Supplies a recorded no-active-turn result.
    pub fn rejected_no_active_turn(
        input: SubmitInputRejectedNoActiveTurnReconstitutionInput,
    ) -> Self {
        let SubmitInputRejectedNoActiveTurnReconstitutionInput {
            command,
            stored_actor,
            result_session,
            result_expected_active_turn,
        } = input;
        Self {
            command,
            stored_actor,
            facts: SubmitInputReconstitutionFacts::RejectedNoActiveTurn {
                result_session,
                result_expected_active_turn,
            },
        }
    }

    /// Supplies a recorded start rejection and the canonical origin of the
    /// turn that owned the slot.
    pub fn rejected_active_turn_present(
        input: SubmitInputRejectedActiveTurnPresentReconstitutionInput,
    ) -> Self {
        let SubmitInputRejectedActiveTurnPresentReconstitutionInput {
            command,
            stored_actor,
            result_session,
            result_active_turn,
            active_turn_origin,
        } = input;
        Self {
            command,
            stored_actor,
            facts: SubmitInputReconstitutionFacts::RejectedActiveTurnPresent {
                result_session,
                result_active_turn,
                active_turn_origin,
            },
        }
    }

    /// Supplies a recorded stale-target rejection and the canonical origin of
    /// the actual turn that owned the slot.
    pub fn rejected_active_turn_mismatch(
        input: SubmitInputRejectedActiveTurnMismatchReconstitutionInput,
    ) -> Self {
        let SubmitInputRejectedActiveTurnMismatchReconstitutionInput {
            command,
            stored_actor,
            result_session,
            result_expected_active_turn,
            result_actual_active_turn,
            actual_turn_origin,
        } = input;
        Self {
            command,
            stored_actor,
            facts: SubmitInputReconstitutionFacts::RejectedActiveTurnMismatch {
                result_session,
                result_expected_active_turn,
                result_actual_active_turn,
                actual_turn_origin,
            },
        }
    }

    /// Supplies a recorded defaults-version mismatch.
    pub fn rejected_defaults_version_mismatch(
        input: SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput,
    ) -> Self {
        let SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
            command,
            stored_actor,
            result_session,
            result_expected,
            result_current,
            active_turn_origin,
        } = input;
        Self {
            command,
            stored_actor,
            facts: SubmitInputReconstitutionFacts::RejectedDefaultsVersionMismatch {
                result_session,
                result_expected,
                result_current,
                active_turn_origin,
            },
        }
    }

    /// Supplies a recorded unknown-alias result and its exact selected
    /// defaults version.
    pub fn rejected_unknown_model_alias(
        input: SubmitInputRejectedUnknownModelAliasReconstitutionInput,
    ) -> Self {
        let SubmitInputRejectedUnknownModelAliasReconstitutionInput {
            command,
            stored_actor,
            result_session,
            result_alias,
            defaults_session,
            defaults_version,
            defaults,
            active_turn_origin,
        } = input;
        Self {
            command,
            stored_actor,
            facts: SubmitInputReconstitutionFacts::RejectedUnknownModelAlias {
                result_session,
                result_alias,
                defaults_session,
                defaults_version,
                defaults,
                active_turn_origin,
            },
        }
    }

    /// Supplies a recorded exhausted-position result.
    pub fn rejected_acceptance_position_exhausted(
        input: SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput,
    ) -> Self {
        let SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput {
            command,
            stored_actor,
            result_session,
            result_last_position,
            active_turn_origin,
        } = input;
        Self {
            command,
            stored_actor,
            facts: SubmitInputReconstitutionFacts::RejectedAcceptancePositionExhausted {
                result_session,
                result_last_position,
                active_turn_origin,
            },
        }
    }

    /// Supplies a safe-point rejection and the exact applied interrupt that
    /// has already stopped its authoritative active turn.
    pub fn rejected_safe_point_unavailable_while_stopping(
        input: SubmitInputRejectedSafePointUnavailableWhileStoppingReconstitutionInput,
    ) -> Self {
        let SubmitInputRejectedSafePointUnavailableWhileStoppingReconstitutionInput {
            command,
            stored_actor,
            result_session,
            result_active_turn,
            active_turn_origin,
            existing_interrupt,
        } = input;
        Self {
            command,
            stored_actor,
            facts: SubmitInputReconstitutionFacts::RejectedSafePointUnavailableWhileStopping {
                result_session,
                result_active_turn,
                active_turn_origin,
                existing_interrupt,
            },
        }
    }

    /// Supplies a later-interrupt rejection and the exact earlier applied
    /// interrupt whose cancellation authority remains binding.
    pub fn rejected_interrupt_already_applied(
        input: SubmitInputRejectedInterruptAlreadyAppliedReconstitutionInput,
    ) -> Self {
        let SubmitInputRejectedInterruptAlreadyAppliedReconstitutionInput {
            command,
            stored_actor,
            result_session,
            result_active_turn,
            result_existing_command,
            active_turn_origin,
            existing_interrupt,
        } = input;
        Self {
            command,
            stored_actor,
            facts: SubmitInputReconstitutionFacts::RejectedInterruptAlreadyApplied {
                result_session,
                result_active_turn,
                result_existing_command,
                active_turn_origin,
                existing_interrupt,
            },
        }
    }

    /// Supplies a parked-approval interrupt rejection and the canonical
    /// origin of the active turn retaining the slot on its approval wait.
    pub fn rejected_interrupt_unavailable_while_awaiting_approval(
        input: SubmitInputRejectedInterruptUnavailableWhileAwaitingApprovalReconstitutionInput,
    ) -> Self {
        let SubmitInputRejectedInterruptUnavailableWhileAwaitingApprovalReconstitutionInput {
            command,
            stored_actor,
            result_session,
            result_active_turn,
            active_turn_origin,
        } = input;
        Self {
            command,
            stored_actor,
            facts:
                SubmitInputReconstitutionFacts::RejectedInterruptUnavailableWhileAwaitingApproval {
                    result_session,
                    result_active_turn,
                    active_turn_origin,
                },
        }
    }

    /// Borrows the reconstructed canonical command.
    pub const fn command(&self) -> &SubmitInput {
        &self.command
    }

    /// Reconstructs the complete recorded handling without authorizing an
    /// effect or claiming that a transaction committed.
    pub fn reconstitute(self) -> Result<ReconstitutedSubmitInput, SubmitInputReconstitutionError> {
        let fail = |failure| SubmitInputReconstitutionError {
            input: Box::new(self.clone()),
            failure,
        };

        if self.stored_actor != self.command.actor {
            return Err(fail(SubmitInputReconstitutionFailure::StoredActorMismatch));
        }

        let result = match self.facts.clone() {
            SubmitInputReconstitutionFacts::AppliedTurnOrigin(facts) => {
                let SubmitInputTurnOriginAppliedReconstitutionFacts {
                    result_session,
                    result_accepted_input,
                    result_turn,
                    predecessor_origin,
                    non_accepted_predecessor,
                    accepted_command,
                    accepted_input,
                    accepted_session,
                    accepted_content,
                    accepted_delivery,
                    accepted_position,
                    accepted_disposition,
                    queue_session,
                    queue_turn,
                    queue_order,
                    defaults_session,
                    defaults_version,
                    defaults,
                    stored_requested_model,
                    stored_frozen_model,
                    stored_model_settings,
                    stored_model_settings_adjustments,
                } = *facts;
                let (expected_predecessor, expected_priority, interrupt_predecessor) = match self
                    .command
                    .delivery
                {
                    DeliveryRequest::StartWhenNoActiveTurn { .. } => {
                        (None, AcceptedInputQueuePriority::Ordinary, None)
                    }
                    DeliveryRequest::AfterCurrentTurn {
                        expected_active_turn,
                        ..
                    } => {
                        if expected_active_turn == result_turn {
                            return Err(fail(SubmitInputReconstitutionFailure::QueueTurnMismatch));
                        }
                        (
                            Some(expected_active_turn),
                            AcceptedInputQueuePriority::Ordinary,
                            None,
                        )
                    }
                    DeliveryRequest::Interrupt {
                        expected_active_turn,
                        ..
                    } => {
                        if expected_active_turn == result_turn {
                            return Err(fail(SubmitInputReconstitutionFailure::QueueTurnMismatch));
                        }
                        (
                            Some(expected_active_turn),
                            AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                                predecessor: expected_active_turn,
                            },
                            Some(expected_active_turn),
                        )
                    }
                    DeliveryRequest::NextSafePoint { .. } => {
                        return Err(fail(
                            SubmitInputReconstitutionFailure::AppliedDeliveryIsNotTurnOrigin,
                        ));
                    }
                };
                if result_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ResultSessionMismatch,
                    ));
                }
                if accepted_command != self.command.command_id {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::AcceptedCommandMismatch,
                    ));
                }
                if accepted_input != result_accepted_input {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::AcceptedInputMismatch,
                    ));
                }
                if accepted_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::AcceptedSessionMismatch,
                    ));
                }
                if accepted_content != self.command.content {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::AcceptedContentMismatch,
                    ));
                }
                if accepted_delivery != self.command.delivery {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::AcceptedDeliveryMismatch,
                    ));
                }
                if accepted_disposition != AcceptedInputDisposition::OriginOf(result_turn) {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::AcceptedDispositionMismatch,
                    ));
                }
                if queue_session != self.command.session {
                    return Err(fail(SubmitInputReconstitutionFailure::QueueSessionMismatch));
                }
                if queue_turn != result_turn {
                    return Err(fail(SubmitInputReconstitutionFailure::QueueTurnMismatch));
                }
                if queue_order.acceptance_position() != accepted_position {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::QueuePositionMismatch,
                    ));
                }
                if queue_order.priority() != expected_priority {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::QueuePriorityMismatch,
                    ));
                }
                match (
                    expected_predecessor,
                    predecessor_origin,
                    non_accepted_predecessor,
                    interrupt_predecessor,
                ) {
                    (None, None, None, None) => {}
                    (Some(expected_predecessor), Some(predecessor_origin), None, _) => {
                        let Some(predecessor) =
                            validate_turn_origin_reconstitution_input(&predecessor_origin)
                        else {
                            return Err(fail(
                                SubmitInputReconstitutionFailure::AfterCurrentPredecessorOriginMismatch,
                            ));
                        };
                        if predecessor.session != self.command.session
                            || predecessor.turn != expected_predecessor
                        {
                            return Err(fail(
                                SubmitInputReconstitutionFailure::AfterCurrentPredecessorOriginMismatch,
                            ));
                        }
                        if predecessor.accepted_inputs.contains(&accepted_input) {
                            return Err(fail(
                                SubmitInputReconstitutionFailure::AfterCurrentPredecessorAcceptedInputReused,
                            ));
                        }
                        if predecessor.command_ids.contains(&accepted_command) {
                            return Err(fail(
                                SubmitInputReconstitutionFailure::AfterCurrentPredecessorCommandReused,
                            ));
                        }
                        if predecessor.turns.contains(&result_turn) {
                            return Err(fail(SubmitInputReconstitutionFailure::QueueTurnMismatch));
                        }
                        if accepted_position <= predecessor.acceptance_position {
                            return Err(fail(
                                SubmitInputReconstitutionFailure::AfterCurrentAcceptanceDoesNotFollowPredecessorOrigin,
                            ));
                        }
                    }
                    (
                        Some(expected_predecessor),
                        None,
                        Some(non_accepted_predecessor),
                        Some(interrupt_predecessor),
                    ) if non_accepted_predecessor.session == self.command.session
                        && non_accepted_predecessor.turn == expected_predecessor
                        && non_accepted_predecessor.turn == interrupt_predecessor => {}
                    _ => {
                        return Err(fail(
                            SubmitInputReconstitutionFailure::AfterCurrentPredecessorOriginMismatch,
                        ));
                    }
                }

                let origin_configuration = reconstruct_origin_configuration(
                    &self.command,
                    StoredOriginConfigurationReconstitutionFacts {
                        defaults_session,
                        defaults_version,
                        defaults,
                        stored_requested_model,
                        stored_frozen_model,
                        stored_model_settings,
                        stored_model_settings_adjustments: stored_model_settings_adjustments
                            .into_vec(),
                    },
                )
                .map_err(&fail)?;
                let applied_interrupt = match interrupt_predecessor {
                    Some(expected_active_turn) => {
                        AppliedInterruptCommandResult::from_correlated_submit(
                            self.command.command_id,
                            result_session,
                            expected_active_turn,
                            result_accepted_input,
                            result_turn,
                            queue_order,
                        )
                        .map(Box::new)
                        .ok_or_else(|| {
                            fail(SubmitInputReconstitutionFailure::QueuePriorityMismatch)
                        })?
                        .into()
                    }
                    None => None,
                };

                SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(
                    SubmitInputTurnOriginAppliedResult {
                        accepted_input: result_accepted_input,
                        session: result_session,
                        acceptance_position: accepted_position,
                        turn: result_turn,
                        queue_order,
                        origin_configuration: Box::new(origin_configuration),
                        applied_interrupt,
                    },
                ))
            }
            SubmitInputReconstitutionFacts::AppliedPendingSteering(facts) => {
                let SubmitInputPendingSteeringAppliedReconstitutionFacts {
                    result_session,
                    result_accepted_input,
                    result_source_turn,
                    source_turn_origin,
                    accepted_command,
                    accepted_input,
                    accepted_session,
                    accepted_content,
                    accepted_delivery,
                    accepted_position,
                } = *facts;
                let DeliveryRequest::NextSafePoint {
                    expected_active_turn,
                } = self.command.delivery
                else {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::AppliedDeliveryIsNotNextSafePoint,
                    ));
                };
                if result_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ResultSessionMismatch,
                    ));
                }
                if result_source_turn != expected_active_turn {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::SteeringSourceTurnMismatch,
                    ));
                }
                if accepted_command != self.command.command_id {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::AcceptedCommandMismatch,
                    ));
                }
                if accepted_input != result_accepted_input {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::AcceptedInputMismatch,
                    ));
                }
                let Some(source_origin) =
                    validate_turn_origin_reconstitution_input(&source_turn_origin)
                else {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::SteeringSourceTurnOriginMismatch,
                    ));
                };
                if source_origin.session != self.command.session
                    || source_origin.turn != result_source_turn
                {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::SteeringSourceTurnOriginMismatch,
                    ));
                }
                if source_origin.accepted_inputs.contains(&accepted_input) {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::SteeringSourceAcceptedInputReused,
                    ));
                }
                if source_origin.command_ids.contains(&accepted_command) {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::SteeringSourceCommandReused,
                    ));
                }
                if accepted_position <= source_origin.acceptance_position {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::SteeringAcceptanceDoesNotFollowSourceOrigin,
                    ));
                }
                if accepted_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::AcceptedSessionMismatch,
                    ));
                }
                if accepted_content != self.command.content {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::AcceptedContentMismatch,
                    ));
                }
                if accepted_delivery != self.command.delivery {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::AcceptedDeliveryMismatch,
                    ));
                }
                let binding = SteeringBinding::new(result_source_turn);

                SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(
                    SubmitInputPendingSteeringAppliedResult {
                        accepted_input: result_accepted_input,
                        session: result_session,
                        acceptance_position: accepted_position,
                        binding,
                    },
                ))
            }
            SubmitInputReconstitutionFacts::RejectedAttachmentBlobNotFound {
                result_session,
                result_digest,
                verified_prefix,
            } => {
                if result_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ResultSessionMismatch,
                    ));
                }
                let digests = self
                    .command
                    .content
                    .parts()
                    .iter()
                    .filter_map(|part| match part {
                        crate::UserContentPart::Attachment { digest, .. } => Some(*digest),
                        crate::UserContentPart::Text { .. } => None,
                    })
                    .collect::<std::collections::BTreeSet<_>>();
                let expected_prefix = digests
                    .iter()
                    .copied()
                    .take_while(|digest| *digest < result_digest)
                    .collect::<Vec<_>>();
                if !digests.contains(&result_digest)
                    || verified_prefix
                        .as_ref()
                        .is_some_and(|prefix| expected_prefix.as_slice() != prefix.as_ref())
                {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::AttachmentDigestMismatch,
                    ));
                }
                SubmitInputResult::Rejected(SubmitInputRejectedResult::AttachmentBlobNotFound {
                    digest: result_digest,
                })
            }
            SubmitInputReconstitutionFacts::RejectedAttachmentByteBudgetExceeded {
                result_session,
                result_maximum_bytes,
            } => {
                if result_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ResultSessionMismatch,
                    ));
                }
                if result_maximum_bytes == 0 {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::AttachmentBudgetMismatch,
                    ));
                }
                SubmitInputResult::Rejected(
                    SubmitInputRejectedResult::AttachmentByteBudgetExceeded {
                        maximum_bytes: result_maximum_bytes,
                    },
                )
            }
            SubmitInputReconstitutionFacts::RejectedSessionNotFound { result_session } => {
                if result_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ResultSessionMismatch,
                    ));
                }
                SubmitInputResult::Rejected(SubmitInputRejectedResult::SessionNotFound {
                    session: result_session,
                })
            }
            SubmitInputReconstitutionFacts::RejectedNoActiveTurn {
                result_session,
                result_expected_active_turn,
            } => {
                if result_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ResultSessionMismatch,
                    ));
                }
                if expected_active_turn(self.command.delivery) != Some(result_expected_active_turn)
                {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ExpectedActiveTurnMismatch,
                    ));
                }
                SubmitInputResult::Rejected(SubmitInputRejectedResult::NoActiveTurn {
                    session: result_session,
                    expected_active_turn: result_expected_active_turn,
                })
            }
            SubmitInputReconstitutionFacts::RejectedActiveTurnPresent {
                result_session,
                result_active_turn,
                active_turn_origin,
            } => {
                if result_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ResultSessionMismatch,
                    ));
                }
                if !matches!(
                    self.command.delivery,
                    DeliveryRequest::StartWhenNoActiveTurn { .. }
                ) {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ActiveTurnPresentRejectionMismatch,
                    ));
                }
                validate_rejection_active_turn_origin(
                    &self.command,
                    Some(result_active_turn),
                    Some(&active_turn_origin),
                )
                .map_err(&fail)?;

                SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnPresent {
                    session: result_session,
                    active_turn: result_active_turn,
                })
            }
            SubmitInputReconstitutionFacts::RejectedActiveTurnMismatch {
                result_session,
                result_expected_active_turn,
                result_actual_active_turn,
                actual_turn_origin,
            } => {
                if result_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ResultSessionMismatch,
                    ));
                }
                if expected_active_turn(self.command.delivery) != Some(result_expected_active_turn)
                {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ExpectedActiveTurnMismatch,
                    ));
                }
                if result_expected_active_turn == result_actual_active_turn {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::RejectedActiveTurnsAreEqual,
                    ));
                }
                validate_rejection_active_turn_origin(
                    &self.command,
                    Some(result_actual_active_turn),
                    Some(&actual_turn_origin),
                )
                .map_err(&fail)?;

                SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnMismatch {
                    session: result_session,
                    expected_active_turn: result_expected_active_turn,
                    actual_active_turn: result_actual_active_turn,
                })
            }
            SubmitInputReconstitutionFacts::RejectedDefaultsVersionMismatch {
                result_session,
                result_expected,
                result_current,
                active_turn_origin,
            } => {
                if result_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ResultSessionMismatch,
                    ));
                }
                let (configuration, expected_origin) =
                    rejection_configuration(self.command.delivery).map_err(&fail)?;
                validate_rejection_active_turn_origin(
                    &self.command,
                    expected_origin,
                    active_turn_origin.as_ref(),
                )
                .map_err(&fail)?;
                if result_expected != configuration.expected_session_defaults_version() {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ExpectedDefaultsVersionMismatch,
                    ));
                }
                if result_expected == result_current {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::RejectedDefaultsVersionsAreEqual,
                    ));
                }
                SubmitInputResult::Rejected(
                    SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                        session: result_session,
                        expected: result_expected,
                        current: result_current,
                    },
                )
            }
            SubmitInputReconstitutionFacts::RejectedUnknownModelAlias {
                result_session,
                result_alias,
                defaults_session,
                defaults_version,
                defaults,
                active_turn_origin,
            } => {
                if result_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ResultSessionMismatch,
                    ));
                }
                let (configuration, expected_origin) =
                    rejection_configuration(self.command.delivery).map_err(&fail)?;
                validate_rejection_active_turn_origin(
                    &self.command,
                    expected_origin,
                    active_turn_origin.as_ref(),
                )
                .map_err(&fail)?;
                if defaults_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::DefaultsSessionMismatch,
                    ));
                }
                if defaults_version != configuration.expected_session_defaults_version() {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::DefaultsVersionMismatch,
                    ));
                }
                let versioned =
                    VersionedSessionConfigurationDefaults::reconstitute(defaults_version, defaults);
                let checked = versioned
                    .derive_request(defaults_version, configuration.model())
                    .map_err(|_| fail(SubmitInputReconstitutionFailure::DefaultsVersionMismatch))?;
                match checked.request().model() {
                    ModelSelectionRequest::Alias(alias) if alias == result_alias => {}
                    ModelSelectionRequest::Alias(_) => {
                        return Err(fail(SubmitInputReconstitutionFailure::UnknownAliasMismatch));
                    }
                    ModelSelectionRequest::Direct(_) => {
                        return Err(fail(
                            SubmitInputReconstitutionFailure::RejectionDidNotSelectAlias,
                        ));
                    }
                }

                SubmitInputResult::Rejected(SubmitInputRejectedResult::UnknownModelAlias {
                    session: result_session,
                    alias: result_alias,
                })
            }
            SubmitInputReconstitutionFacts::RejectedAcceptancePositionExhausted {
                result_session,
                result_last_position,
                active_turn_origin,
            } => {
                if result_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ResultSessionMismatch,
                    ));
                }
                let expected_origin =
                    position_exhaustion_origin(self.command.delivery).map_err(&fail)?;
                validate_rejection_active_turn_origin(
                    &self.command,
                    expected_origin,
                    active_turn_origin.as_ref(),
                )
                .map_err(&fail)?;
                if result_last_position.checked_next().is_some() {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::PositionIsNotExhausted,
                    ));
                }
                SubmitInputResult::Rejected(
                    SubmitInputRejectedResult::AcceptancePositionExhausted {
                        session: result_session,
                        last: result_last_position,
                    },
                )
            }
            SubmitInputReconstitutionFacts::RejectedSafePointUnavailableWhileStopping {
                result_session,
                result_active_turn,
                active_turn_origin,
                existing_interrupt,
            } => {
                if result_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ResultSessionMismatch,
                    ));
                }
                if !matches!(
                    self.command.delivery,
                    DeliveryRequest::NextSafePoint {
                        expected_active_turn
                    } if expected_active_turn == result_active_turn
                ) {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::StoppingRejectionMismatch,
                    ));
                }
                validate_rejection_active_turn_origin(
                    &self.command,
                    Some(result_active_turn),
                    Some(&active_turn_origin),
                )
                .map_err(&fail)?;
                validate_existing_interrupt(
                    &self.command,
                    result_active_turn,
                    existing_interrupt,
                    None,
                )
                .map_err(&fail)?;
                SubmitInputResult::Rejected(
                    SubmitInputRejectedResult::SafePointUnavailableWhileStopping {
                        session: result_session,
                        active_turn: result_active_turn,
                        existing_command: existing_interrupt.proof().command(),
                    },
                )
            }
            SubmitInputReconstitutionFacts::RejectedInterruptAlreadyApplied {
                result_session,
                result_active_turn,
                result_existing_command,
                active_turn_origin,
                existing_interrupt,
            } => {
                if result_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ResultSessionMismatch,
                    ));
                }
                if !matches!(
                    self.command.delivery,
                    DeliveryRequest::Interrupt {
                        expected_active_turn,
                        ..
                    } if expected_active_turn == result_active_turn
                ) {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::StoppingRejectionMismatch,
                    ));
                }
                validate_rejection_active_turn_origin(
                    &self.command,
                    Some(result_active_turn),
                    Some(&active_turn_origin),
                )
                .map_err(&fail)?;
                validate_existing_interrupt(
                    &self.command,
                    result_active_turn,
                    existing_interrupt,
                    Some(result_existing_command),
                )
                .map_err(&fail)?;
                SubmitInputResult::Rejected(SubmitInputRejectedResult::InterruptAlreadyApplied {
                    session: result_session,
                    active_turn: result_active_turn,
                    existing_command: result_existing_command,
                })
            }
            SubmitInputReconstitutionFacts::RejectedInterruptUnavailableWhileAwaitingApproval {
                result_session,
                result_active_turn,
                active_turn_origin,
            } => {
                if result_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ResultSessionMismatch,
                    ));
                }
                if !matches!(
                    self.command.delivery,
                    DeliveryRequest::Interrupt {
                        expected_active_turn,
                        ..
                    } if expected_active_turn == result_active_turn
                ) {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::StoppingRejectionMismatch,
                    ));
                }
                validate_rejection_active_turn_origin(
                    &self.command,
                    Some(result_active_turn),
                    Some(&active_turn_origin),
                )
                .map_err(&fail)?;
                SubmitInputResult::Rejected(
                    SubmitInputRejectedResult::InterruptUnavailableWhileAwaitingApproval {
                        session: result_session,
                        active_turn: result_active_turn,
                    },
                )
            }
        };

        Ok(ReconstitutedSubmitInput {
            command: self.command,
            result,
        })
    }
}
