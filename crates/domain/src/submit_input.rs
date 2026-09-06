//! Canonical durable input submission and authoritative-state preparation.
//!
//! docs/spec/turn-lifecycle-and-scheduling.md owns accepted-input delivery,
//! ordering, and disposition semantics;
//! docs/spec/configuration-and-credentials.md owns configuration
//! validation; docs/spec/identity-and-commands.md owns structural replay
//! equality and actor attribution; docs/spec/persistence-protocol.md owns
//! checked reconstitution; and docs/spec/sessions-and-transcript.md owns
//! content. This slice prepares accepted origin work with no active
//! turn or after the exact active turn, and pending steering for the exact
//! active turn. Applied and rejected replay validate complete canonical source
//! or predecessor origin facts, including the current lifecycle and queue facts
//! that make an immutable pending-steering receipt visible as reclassified
//! origin work. Replaying the pending receipt itself remains independent of its
//! later mutable disposition.

mod reconstituted;
mod reconstitution;
mod reconstitution_input;
mod validation;

#[cfg(test)]
mod tests;

pub use reconstituted::{
    ReconstitutedSubmitInput, SubmitInputReconstitutionError, SubmitInputReconstitutionFailure,
};
pub use reconstitution::SubmitInputReconstitutionInput;
pub use reconstitution_input::{
    GoalTurnOriginConstructionInput, NonAcceptedTurnPredecessorReconstitutionInput,
    SubmitInputAppliedPendingSteeringReconstitutionInput,
    SubmitInputAppliedTurnOriginReconstitutionInput,
    SubmitInputAutomaticReconciliationConstructionInput,
    SubmitInputDirectTurnOriginConstructionInput,
    SubmitInputInterruptedModelCallReconciliationConstructionInput,
    SubmitInputInterruptedToolReconciliationConstructionInput,
    SubmitInputReclassifiedTurnOriginConstructionInput,
    SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput,
    SubmitInputRejectedActiveTurnMismatchReconstitutionInput,
    SubmitInputRejectedActiveTurnPresentReconstitutionInput,
    SubmitInputRejectedAttachmentBlobNotFoundReconstitutionInput,
    SubmitInputRejectedAttachmentByteBudgetExceededReconstitutionInput,
    SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput,
    SubmitInputRejectedInterruptAlreadyAppliedReconstitutionInput,
    SubmitInputRejectedInterruptUnavailableWhileAwaitingApprovalReconstitutionInput,
    SubmitInputRejectedNoActiveTurnReconstitutionInput,
    SubmitInputRejectedSafePointUnavailableWhileStoppingReconstitutionInput,
    SubmitInputRejectedSessionNotFoundReconstitutionInput,
    SubmitInputRejectedUnknownModelAliasReconstitutionInput,
    SubmitInputTerminalSourceConstructionInput, SubmitInputTerminalSourceReconstitutionInput,
    SubmitInputTurnOriginReconstitutionInput,
};

use crate::AcceptedInputDisposition;
use crate::AcceptedInputId;
use crate::AcceptedInputQueueOrder;
use crate::AcceptedInputQueueWork;
use crate::AcceptedInputSchedulingProjection;
use crate::Actor;
use crate::AppliedInterruptCommandResult;
use crate::AppliedInterruptState;
use crate::BlobDigest;
use crate::CurrentTurnAttemptState;
use crate::DeliveryRequest;
use crate::DescendantTerminationScope;
use crate::DurableCommandId;
use crate::FrozenAliasDefinition;
use crate::ModelAlias;
use crate::ModelCapabilityCatalog;
use crate::OriginConfiguration;
use crate::OriginModelSettingsError;
use crate::PerInputConfigurationChoices;
use crate::Session;
use crate::SessionConfigurationDefaultsVersion;
use crate::SessionId;
use crate::SessionInputPosition;
use crate::SteeringBinding;
use crate::TurnId;
use crate::UserContent;
use crate::derive_accepted_input_total_order;
use std::hash::Hash;
use std::hash::Hasher;

/// One canonical globally claimed durable input command.
///
/// Equality and hashing intentionally exclude [`DurableCommandId`]. They
/// include the command discriminator by type and every other caller-supplied
/// semantic field.
#[derive(Clone, Debug)]
pub struct SubmitInput {
    command_id: DurableCommandId,
    session: SessionId,
    actor: Actor,
    content: UserContent,
    delivery: DeliveryRequest,
}

impl SubmitInput {
    /// Constructs the complete canonical typed payload for the baseline user.
    ///
    /// Lifecycle closure uses the separate core-only interrupt constructor.
    pub const fn new(
        command_id: DurableCommandId,
        session: SessionId,
        content: UserContent,
        delivery: DeliveryRequest,
    ) -> Self {
        Self {
            command_id,
            session,
            actor: Actor::User,
            content,
            delivery,
        }
    }

    /// Constructs a daemon-core interrupt without model, tool, or user agency.
    pub const fn new_core_interrupt(
        command_id: DurableCommandId,
        session: SessionId,
        content: UserContent,
        expected_active_turn: TurnId,
        descendant_scope: DescendantTerminationScope,
        configuration: PerInputConfigurationChoices,
    ) -> Self {
        Self {
            command_id,
            session,
            actor: Actor::Core,
            content,
            delivery: DeliveryRequest::Interrupt {
                expected_active_turn,
                descendant_scope,
                configuration,
            },
        }
    }

    /// Returns the user-global command identity.
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }

    /// Returns the target session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the attributed initiating agency.
    pub const fn actor(&self) -> Actor {
        self.actor
    }

    /// Borrows the exact caller content.
    pub const fn content(&self) -> &UserContent {
        &self.content
    }

    /// Returns the explicit delivery treatment.
    pub const fn delivery(&self) -> DeliveryRequest {
        self.delivery
    }

    /// Prepares the authoritative result when the target session is absent.
    pub fn prepare_session_not_found(self) -> PreparedSubmitInput {
        let session = self.session;
        PreparedSubmitInput {
            command: self,
            result: SubmitInputResult::Rejected(SubmitInputRejectedResult::SessionNotFound {
                session,
            }),
        }
    }

    /// Prepares a terminal rejection for an attachment without a catalogued
    /// verified replica.
    pub fn prepare_attachment_blob_not_found(self, digest: BlobDigest) -> PreparedSubmitInput {
        PreparedSubmitInput {
            command: self,
            result: SubmitInputResult::Rejected(
                SubmitInputRejectedResult::AttachmentBlobNotFound { digest },
            ),
        }
    }

    /// Prepares a terminal rejection when distinct attachment bytes exceed
    /// the configured verification-work ceiling.
    pub fn prepare_attachment_byte_budget_exceeded(
        self,
        maximum_bytes: u64,
    ) -> PreparedSubmitInput {
        PreparedSubmitInput {
            command: self,
            result: SubmitInputResult::Rejected(
                SubmitInputRejectedResult::AttachmentByteBudgetExceeded { maximum_bytes },
            ),
        }
    }

    /// Prepares handling against an authoritative session with no active turn.
    ///
    /// Active-work delivery variants become recorded `NoActiveTurn`
    /// rejections. `StartWhenNoActiveTurn` freezes the current versioned
    /// configuration and creates ordinary queued-work facts. The supplied
    /// previous position is the transaction's complete locked observation of
    /// the session's accepted-input tail; `None` selects position one.
    pub fn prepare_when_no_active_turn(
        self,
        session: &Session,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        previous_position: Option<SessionInputPosition>,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
    ) -> Result<PreparedSubmitInput, SubmitInputPreparationError> {
        self.prepare_when_no_active_turn_resolving(
            session,
            accepted_input,
            turn,
            previous_position,
            select_definition,
            None,
        )
    }

    /// Prepares no-active-turn handling with settings capability resolution.
    pub fn prepare_when_no_active_turn_with_model_settings(
        self,
        session: &Session,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        previous_position: Option<SessionInputPosition>,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
        capabilities: &ModelCapabilityCatalog,
    ) -> Result<PreparedSubmitInput, SubmitInputPreparationError> {
        self.prepare_when_no_active_turn_resolving(
            session,
            accepted_input,
            turn,
            previous_position,
            select_definition,
            Some(capabilities),
        )
    }

    fn prepare_when_no_active_turn_resolving(
        self,
        session: &Session,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        previous_position: Option<SessionInputPosition>,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
        capabilities: Option<&ModelCapabilityCatalog>,
    ) -> Result<PreparedSubmitInput, SubmitInputPreparationError> {
        if session.id() != self.session {
            return Err(SubmitInputPreparationError {
                command: Box::new(self),
                failure: SubmitInputPreparationFailure::SessionMismatch {
                    provided_session: session.id(),
                },
            });
        }

        let configuration = match self.delivery {
            DeliveryRequest::StartWhenNoActiveTurn { configuration } => configuration,
            DeliveryRequest::Interrupt {
                expected_active_turn,
                ..
            }
            | DeliveryRequest::NextSafePoint {
                expected_active_turn,
            }
            | DeliveryRequest::AfterCurrentTurn {
                expected_active_turn,
                ..
            } => {
                if matches!(self.delivery, DeliveryRequest::NextSafePoint { .. }) != turn.is_none()
                {
                    return Err(SubmitInputPreparationError {
                        command: Box::new(self),
                        failure: SubmitInputPreparationFailure::TurnCandidateMismatch,
                    });
                }
                let target_session = self.session;
                return Ok(PreparedSubmitInput {
                    command: self,
                    result: SubmitInputResult::Rejected(SubmitInputRejectedResult::NoActiveTurn {
                        session: target_session,
                        expected_active_turn,
                    }),
                });
            }
        };
        let Some(turn) = turn else {
            return Err(SubmitInputPreparationError {
                command: Box::new(self),
                failure: SubmitInputPreparationFailure::TurnCandidateMismatch,
            });
        };

        let checked = match session
            .current_configuration_defaults()
            .derive_request_with_model_settings(
                configuration.expected_session_defaults_version(),
                configuration.model(),
                configuration.model_settings(),
            ) {
            Ok(checked) => checked,
            Err(mismatch) => {
                let target_session = self.session;
                return Ok(PreparedSubmitInput {
                    command: self,
                    result: SubmitInputResult::Rejected(
                        SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                            session: target_session,
                            expected: mismatch.expected(),
                            current: mismatch.current(),
                        },
                    ),
                });
            }
        };

        let origin_configuration =
            match freeze_origin_configuration(checked, select_definition, capabilities) {
                Ok(configuration) => configuration,
                Err(OriginModelSettingsError::UnknownAlias(unknown)) => {
                    let target_session = self.session;
                    return Ok(PreparedSubmitInput {
                        command: self,
                        result: SubmitInputResult::Rejected(
                            SubmitInputRejectedResult::UnknownModelAlias {
                                session: target_session,
                                alias: unknown.alias(),
                            },
                        ),
                    });
                }
                Err(failure) => {
                    return Err(SubmitInputPreparationError {
                        command: Box::new(self),
                        failure: SubmitInputPreparationFailure::ModelSettingsResolution(failure),
                    });
                }
            };

        let acceptance_position = match previous_position {
            None => SessionInputPosition::first(),
            Some(last) => match last.checked_next() {
                Some(next) => next,
                None => {
                    let target_session = self.session;
                    return Ok(PreparedSubmitInput {
                        command: self,
                        result: SubmitInputResult::Rejected(
                            SubmitInputRejectedResult::AcceptancePositionExhausted {
                                session: target_session,
                                last,
                            },
                        ),
                    });
                }
            },
        };

        let target_session = self.session;
        Ok(PreparedSubmitInput {
            command: self,
            result: SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(
                SubmitInputTurnOriginAppliedResult {
                    accepted_input,
                    session: target_session,
                    acceptance_position,
                    turn,
                    queue_order: AcceptedInputQueueOrder::ordinary(acceptance_position),
                    origin_configuration: Box::new(origin_configuration),
                    applied_interrupt: None,
                },
            )),
        })
    }

    /// Prepares handling against the exact authoritative active turn.
    ///
    /// `StartWhenNoActiveTurn` records the active slot owner, stale
    /// active-work requests record both expected and actual turns, matching
    /// after-current input creates ordinary queued origin work, and matching
    /// next-safe-point input creates pending steering. A matching interrupt
    /// prepares a proof-bearing immediate-successor origin; a stopping turn
    /// returns the treatment-specific recorded rejection.
    pub fn prepare_with_active_turn(
        self,
        scheduling: &AcceptedInputSchedulingProjection,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
    ) -> Result<PreparedSubmitInput, SubmitInputPreparationError> {
        self.prepare_with_active_turn_resolving(
            scheduling,
            accepted_input,
            turn,
            select_definition,
            None,
        )
    }

    /// Prepares active-turn handling with settings capability resolution.
    pub fn prepare_with_active_turn_with_model_settings(
        self,
        scheduling: &AcceptedInputSchedulingProjection,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
        capabilities: &ModelCapabilityCatalog,
    ) -> Result<PreparedSubmitInput, SubmitInputPreparationError> {
        self.prepare_with_active_turn_resolving(
            scheduling,
            accepted_input,
            turn,
            select_definition,
            Some(capabilities),
        )
    }

    fn prepare_with_active_turn_resolving(
        self,
        scheduling: &AcceptedInputSchedulingProjection,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
        capabilities: Option<&ModelCapabilityCatalog>,
    ) -> Result<PreparedSubmitInput, SubmitInputPreparationError> {
        let session = scheduling.session();
        if session.id() != self.session {
            return Err(SubmitInputPreparationError {
                command: Box::new(self),
                failure: SubmitInputPreparationFailure::SessionMismatch {
                    provided_session: session.id(),
                },
            });
        }
        let Some(active_turn) = scheduling.active_turn() else {
            return Err(SubmitInputPreparationError {
                command: Box::new(self),
                failure: SubmitInputPreparationFailure::ActiveTurnProjectionMissing,
            });
        };
        let Some(active_acceptance_tail) = scheduling.active_acceptance_tail() else {
            return Err(SubmitInputPreparationError {
                command: Box::new(self),
                failure: SubmitInputPreparationFailure::ActiveTurnProjectionMissing,
            });
        };
        let previous_position = Some(active_acceptance_tail.observed_last_position());
        if delivery_creates_turn(self.delivery) != turn.is_some() {
            return Err(SubmitInputPreparationError {
                command: Box::new(self),
                failure: SubmitInputPreparationFailure::TurnCandidateMismatch,
            });
        }

        let actual_active_turn = active_turn.turn();
        let target_session = self.session;
        let delivery = self.delivery;
        let expected_active_turn = match delivery {
            DeliveryRequest::StartWhenNoActiveTurn { .. } => None,
            DeliveryRequest::Interrupt {
                expected_active_turn,
                ..
            }
            | DeliveryRequest::NextSafePoint {
                expected_active_turn,
            }
            | DeliveryRequest::AfterCurrentTurn {
                expected_active_turn,
                ..
            } => Some(expected_active_turn),
        };
        if let Some(expected_active_turn) = expected_active_turn
            && expected_active_turn != actual_active_turn
        {
            return Ok(PreparedSubmitInput {
                command: self,
                result: SubmitInputResult::Rejected(
                    SubmitInputRejectedResult::ActiveTurnMismatch {
                        session: target_session,
                        expected_active_turn,
                        actual_active_turn,
                    },
                ),
            });
        }
        let existing_interrupt = active_turn.active_phase().and_then(|phase| match phase {
            crate::ActiveTurnPhase::Running { current_attempt } => match current_attempt.state() {
                CurrentTurnAttemptState::StopRequested { causes } => match causes {
                    crate::TurnAttemptStopCauses::CancellationOnly { interrupt } => {
                        Some(*interrupt)
                    }
                    crate::TurnAttemptStopCauses::FatalMismatch(causes) => {
                        match causes.interrupt() {
                            AppliedInterruptState::NoAppliedInterrupt => None,
                            AppliedInterruptState::Applied { proof } => Some(proof),
                        }
                    }
                },
                CurrentTurnAttemptState::Prepared | CurrentTurnAttemptState::Running => None,
            },
            crate::ActiveTurnPhase::AwaitingApproval { .. }
            | crate::ActiveTurnPhase::AwaitingChild { .. }
            | crate::ActiveTurnPhase::AwaitingRunnerRecovery { .. } => None,
            crate::ActiveTurnPhase::AwaitingRecoveryDecision {
                applied_interrupt, ..
            } => *applied_interrupt,
        });
        match delivery {
            DeliveryRequest::Interrupt { configuration, .. } => {
                if let Some(existing) = existing_interrupt {
                    return Ok(PreparedSubmitInput {
                        command: self,
                        result: SubmitInputResult::Rejected(
                            SubmitInputRejectedResult::InterruptAlreadyApplied {
                                session: target_session,
                                active_turn: actual_active_turn,
                                existing_command: existing.command(),
                            },
                        ),
                    });
                }
                if matches!(
                    active_turn.active_phase(),
                    Some(crate::ActiveTurnPhase::AwaitingApproval { .. })
                ) {
                    return Ok(PreparedSubmitInput {
                        command: self,
                        result: SubmitInputResult::Rejected(
                            SubmitInputRejectedResult::InterruptUnavailableWhileAwaitingApproval {
                                session: target_session,
                                active_turn: actual_active_turn,
                            },
                        ),
                    });
                }
                let Some(turn) = turn else {
                    return Err(SubmitInputPreparationError {
                        command: Box::new(self),
                        failure: SubmitInputPreparationFailure::TurnCandidateMismatch,
                    });
                };
                if turn == actual_active_turn {
                    return Err(SubmitInputPreparationError {
                        command: Box::new(self),
                        failure: SubmitInputPreparationFailure::TurnCandidateMismatch,
                    });
                }
                let checked = match session
                    .current_configuration_defaults()
                    .derive_request_with_model_settings(
                        configuration.expected_session_defaults_version(),
                        configuration.model(),
                        configuration.model_settings(),
                    ) {
                    Ok(checked) => checked,
                    Err(mismatch) => {
                        return Ok(PreparedSubmitInput {
                            command: self,
                            result: SubmitInputResult::Rejected(
                                SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                                    session: target_session,
                                    expected: mismatch.expected(),
                                    current: mismatch.current(),
                                },
                            ),
                        });
                    }
                };
                let origin_configuration =
                    match freeze_origin_configuration(checked, select_definition, capabilities) {
                        Ok(configuration) => configuration,
                        Err(OriginModelSettingsError::UnknownAlias(unknown)) => {
                            return Ok(PreparedSubmitInput {
                                command: self,
                                result: SubmitInputResult::Rejected(
                                    SubmitInputRejectedResult::UnknownModelAlias {
                                        session: target_session,
                                        alias: unknown.alias(),
                                    },
                                ),
                            });
                        }
                        Err(failure) => {
                            return Err(SubmitInputPreparationError {
                                command: Box::new(self),
                                failure: SubmitInputPreparationFailure::ModelSettingsResolution(
                                    failure,
                                ),
                            });
                        }
                    };
                let acceptance_position = match next_acceptance_position(previous_position) {
                    Ok(position) => position,
                    Err(last) => {
                        return Ok(PreparedSubmitInput {
                            command: self,
                            result: SubmitInputResult::Rejected(
                                SubmitInputRejectedResult::AcceptancePositionExhausted {
                                    session: target_session,
                                    last,
                                },
                            ),
                        });
                    }
                };
                if accepted_input == active_turn.accepted_input().id() {
                    return Err(SubmitInputPreparationError {
                        command: Box::new(self),
                        failure:
                            SubmitInputPreparationFailure::AcceptedInputCandidateReusesActiveOrigin {
                                active_turn: actual_active_turn,
                                accepted_input,
                            },
                    });
                }
                let queue_order = AcceptedInputQueueOrder::interrupt_immediately_after(
                    acceptance_position,
                    actual_active_turn,
                );
                let successor = AcceptedInputQueueWork::new(target_session, turn, queue_order);
                if derive_accepted_input_total_order(
                    scheduling
                        .turns()
                        .map(|known| {
                            AcceptedInputQueueWork::new(
                                known.session(),
                                known.turn(),
                                known.order(),
                            )
                        })
                        .chain([successor]),
                )
                .is_err()
                {
                    return Err(SubmitInputPreparationError {
                        command: Box::new(self),
                        failure: SubmitInputPreparationFailure::InterruptQueueOrderInvalid,
                    });
                }
                let Some(applied_interrupt) = AppliedInterruptCommandResult::from_correlated_submit(
                    self.command_id,
                    target_session,
                    actual_active_turn,
                    accepted_input,
                    turn,
                    queue_order,
                ) else {
                    return Err(SubmitInputPreparationError {
                        command: Box::new(self),
                        failure: SubmitInputPreparationFailure::InterruptQueueOrderInvalid,
                    });
                };
                Ok(PreparedSubmitInput {
                    command: self,
                    result: SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(
                        SubmitInputTurnOriginAppliedResult {
                            accepted_input,
                            session: target_session,
                            acceptance_position,
                            turn,
                            queue_order,
                            origin_configuration: Box::new(origin_configuration),
                            applied_interrupt: Some(Box::new(applied_interrupt)),
                        },
                    )),
                })
            }
            DeliveryRequest::NextSafePoint { .. } => {
                let acceptance_position = match next_acceptance_position(previous_position) {
                    Ok(position) => position,
                    Err(last) => {
                        return Ok(PreparedSubmitInput {
                            command: self,
                            result: SubmitInputResult::Rejected(
                                SubmitInputRejectedResult::AcceptancePositionExhausted {
                                    session: target_session,
                                    last,
                                },
                            ),
                        });
                    }
                };
                if accepted_input == active_turn.accepted_input().id() {
                    return Err(SubmitInputPreparationError {
                        command: Box::new(self),
                        failure:
                            SubmitInputPreparationFailure::AcceptedInputCandidateReusesActiveOrigin {
                                active_turn: actual_active_turn,
                                accepted_input,
                            },
                    });
                }
                let binding = SteeringBinding::new(actual_active_turn);
                Ok(PreparedSubmitInput {
                    command: self,
                    result: SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(
                        SubmitInputPendingSteeringAppliedResult {
                            accepted_input,
                            session: target_session,
                            acceptance_position,
                            binding,
                        },
                    )),
                })
            }
            DeliveryRequest::AfterCurrentTurn { configuration, .. } => {
                let Some(turn) = turn else {
                    return Err(SubmitInputPreparationError {
                        command: Box::new(self),
                        failure: SubmitInputPreparationFailure::TurnCandidateMismatch,
                    });
                };
                if turn == actual_active_turn {
                    return Err(SubmitInputPreparationError {
                        command: Box::new(self),
                        failure: SubmitInputPreparationFailure::TurnCandidateMismatch,
                    });
                }
                let checked = match session
                    .current_configuration_defaults()
                    .derive_request_with_model_settings(
                        configuration.expected_session_defaults_version(),
                        configuration.model(),
                        configuration.model_settings(),
                    ) {
                    Ok(checked) => checked,
                    Err(mismatch) => {
                        return Ok(PreparedSubmitInput {
                            command: self,
                            result: SubmitInputResult::Rejected(
                                SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                                    session: target_session,
                                    expected: mismatch.expected(),
                                    current: mismatch.current(),
                                },
                            ),
                        });
                    }
                };
                let origin_configuration =
                    match freeze_origin_configuration(checked, select_definition, capabilities) {
                        Ok(configuration) => configuration,
                        Err(OriginModelSettingsError::UnknownAlias(unknown)) => {
                            return Ok(PreparedSubmitInput {
                                command: self,
                                result: SubmitInputResult::Rejected(
                                    SubmitInputRejectedResult::UnknownModelAlias {
                                        session: target_session,
                                        alias: unknown.alias(),
                                    },
                                ),
                            });
                        }
                        Err(failure) => {
                            return Err(SubmitInputPreparationError {
                                command: Box::new(self),
                                failure: SubmitInputPreparationFailure::ModelSettingsResolution(
                                    failure,
                                ),
                            });
                        }
                    };
                let acceptance_position = match next_acceptance_position(previous_position) {
                    Ok(position) => position,
                    Err(last) => {
                        return Ok(PreparedSubmitInput {
                            command: self,
                            result: SubmitInputResult::Rejected(
                                SubmitInputRejectedResult::AcceptancePositionExhausted {
                                    session: target_session,
                                    last,
                                },
                            ),
                        });
                    }
                };
                if accepted_input == active_turn.accepted_input().id() {
                    return Err(SubmitInputPreparationError {
                        command: Box::new(self),
                        failure:
                            SubmitInputPreparationFailure::AcceptedInputCandidateReusesActiveOrigin {
                                active_turn: actual_active_turn,
                                accepted_input,
                            },
                    });
                }
                Ok(PreparedSubmitInput {
                    command: self,
                    result: SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(
                        SubmitInputTurnOriginAppliedResult {
                            accepted_input,
                            session: target_session,
                            acceptance_position,
                            turn,
                            queue_order: AcceptedInputQueueOrder::ordinary(acceptance_position),
                            origin_configuration: Box::new(origin_configuration),
                            applied_interrupt: None,
                        },
                    )),
                })
            }
            DeliveryRequest::StartWhenNoActiveTurn { .. } => Ok(PreparedSubmitInput {
                command: self,
                result: SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnPresent {
                    session: target_session,
                    active_turn: actual_active_turn,
                }),
            }),
        }
    }

    /// Prepares input control while a delegation-origin turn owns the session slot.
    ///
    /// The active turn has no accepted-input identity, so its authoritative
    /// lifecycle and the complete session acceptance tail are supplied
    /// separately. `awaiting_approval` preserves the one parked phase whose
    /// approval obligation forbids an immediate interrupt transition.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_with_delegated_active_turn(
        self,
        session: &Session,
        actual_active_turn: TurnId,
        previous_position: Option<SessionInputPosition>,
        existing_interrupt: Option<DurableCommandId>,
        awaiting_approval: bool,
        accepted_input: AcceptedInputId,
        turn: Option<TurnId>,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
    ) -> Result<PreparedSubmitInput, SubmitInputPreparationError> {
        if session.id() != self.session {
            return Err(SubmitInputPreparationError {
                command: Box::new(self),
                failure: SubmitInputPreparationFailure::SessionMismatch {
                    provided_session: session.id(),
                },
            });
        }
        if delivery_creates_turn(self.delivery) != turn.is_some() {
            return Err(SubmitInputPreparationError {
                command: Box::new(self),
                failure: SubmitInputPreparationFailure::TurnCandidateMismatch,
            });
        }
        let expected = match self.delivery {
            DeliveryRequest::StartWhenNoActiveTurn { .. } => None,
            DeliveryRequest::Interrupt {
                expected_active_turn,
                ..
            }
            | DeliveryRequest::NextSafePoint {
                expected_active_turn,
            }
            | DeliveryRequest::AfterCurrentTurn {
                expected_active_turn,
                ..
            } => Some(expected_active_turn),
        };
        if let Some(expected_active_turn) = expected
            && expected_active_turn != actual_active_turn
        {
            let target_session = self.session;
            return Ok(PreparedSubmitInput {
                command: self,
                result: SubmitInputResult::Rejected(
                    SubmitInputRejectedResult::ActiveTurnMismatch {
                        session: target_session,
                        expected_active_turn,
                        actual_active_turn,
                    },
                ),
            });
        }
        let target_session = self.session;
        match self.delivery {
            DeliveryRequest::StartWhenNoActiveTurn { .. } => Ok(PreparedSubmitInput {
                command: self,
                result: SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnPresent {
                    session: target_session,
                    active_turn: actual_active_turn,
                }),
            }),
            DeliveryRequest::NextSafePoint { .. } => {
                let acceptance_position = match next_acceptance_position(previous_position) {
                    Ok(position) => position,
                    Err(last) => {
                        return Ok(PreparedSubmitInput {
                            command: self,
                            result: SubmitInputResult::Rejected(
                                SubmitInputRejectedResult::AcceptancePositionExhausted {
                                    session: target_session,
                                    last,
                                },
                            ),
                        });
                    }
                };
                Ok(PreparedSubmitInput {
                    command: self,
                    result: SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(
                        SubmitInputPendingSteeringAppliedResult {
                            accepted_input,
                            session: target_session,
                            acceptance_position,
                            binding: SteeringBinding::new(actual_active_turn),
                        },
                    )),
                })
            }
            DeliveryRequest::Interrupt { configuration, .. } => {
                if let Some(existing) = existing_interrupt {
                    return Ok(PreparedSubmitInput {
                        command: self,
                        result: SubmitInputResult::Rejected(
                            SubmitInputRejectedResult::InterruptAlreadyApplied {
                                session: target_session,
                                active_turn: actual_active_turn,
                                existing_command: existing,
                            },
                        ),
                    });
                }
                if awaiting_approval {
                    return Ok(PreparedSubmitInput {
                        command: self,
                        result: SubmitInputResult::Rejected(
                            SubmitInputRejectedResult::InterruptUnavailableWhileAwaitingApproval {
                                session: target_session,
                                active_turn: actual_active_turn,
                            },
                        ),
                    });
                }
                let turn = turn.ok_or_else(|| SubmitInputPreparationError {
                    command: Box::new(self.clone()),
                    failure: SubmitInputPreparationFailure::TurnCandidateMismatch,
                })?;
                let prepared = prepare_delegated_successor(
                    &self,
                    session,
                    configuration,
                    previous_position,
                    select_definition,
                );
                let (origin_configuration, acceptance_position) = match prepared {
                    DelegatedSuccessorPreparation::Prepared {
                        origin_configuration,
                        acceptance_position,
                    } => (origin_configuration, acceptance_position),
                    DelegatedSuccessorPreparation::Rejected(result) => {
                        return Ok(PreparedSubmitInput {
                            command: self,
                            result: SubmitInputResult::Rejected(result),
                        });
                    }
                    DelegatedSuccessorPreparation::Failed(failure) => {
                        return Err(SubmitInputPreparationError {
                            command: Box::new(self),
                            failure: SubmitInputPreparationFailure::ModelSettingsResolution(
                                failure,
                            ),
                        });
                    }
                };
                let queue_order = AcceptedInputQueueOrder::interrupt_immediately_after(
                    acceptance_position,
                    actual_active_turn,
                );
                let applied_interrupt = AppliedInterruptCommandResult::from_correlated_submit(
                    self.command_id,
                    target_session,
                    actual_active_turn,
                    accepted_input,
                    turn,
                    queue_order,
                )
                .ok_or_else(|| SubmitInputPreparationError {
                    command: Box::new(self.clone()),
                    failure: SubmitInputPreparationFailure::InterruptQueueOrderInvalid,
                })?;
                Ok(PreparedSubmitInput {
                    command: self,
                    result: SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(
                        SubmitInputTurnOriginAppliedResult {
                            accepted_input,
                            session: target_session,
                            acceptance_position,
                            turn,
                            queue_order,
                            origin_configuration: Box::new(origin_configuration),
                            applied_interrupt: Some(Box::new(applied_interrupt)),
                        },
                    )),
                })
            }
            DeliveryRequest::AfterCurrentTurn { configuration, .. } => {
                let turn = turn.ok_or_else(|| SubmitInputPreparationError {
                    command: Box::new(self.clone()),
                    failure: SubmitInputPreparationFailure::TurnCandidateMismatch,
                })?;
                let prepared = prepare_delegated_successor(
                    &self,
                    session,
                    configuration,
                    previous_position,
                    select_definition,
                );
                let (origin_configuration, acceptance_position) = match prepared {
                    DelegatedSuccessorPreparation::Prepared {
                        origin_configuration,
                        acceptance_position,
                    } => (origin_configuration, acceptance_position),
                    DelegatedSuccessorPreparation::Rejected(result) => {
                        return Ok(PreparedSubmitInput {
                            command: self,
                            result: SubmitInputResult::Rejected(result),
                        });
                    }
                    DelegatedSuccessorPreparation::Failed(failure) => {
                        return Err(SubmitInputPreparationError {
                            command: Box::new(self),
                            failure: SubmitInputPreparationFailure::ModelSettingsResolution(
                                failure,
                            ),
                        });
                    }
                };
                Ok(PreparedSubmitInput {
                    command: self,
                    result: SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(
                        SubmitInputTurnOriginAppliedResult {
                            accepted_input,
                            session: target_session,
                            acceptance_position,
                            turn,
                            queue_order: AcceptedInputQueueOrder::ordinary(acceptance_position),
                            origin_configuration: Box::new(origin_configuration),
                            applied_interrupt: None,
                        },
                    )),
                })
            }
        }
    }
}

enum DelegatedSuccessorPreparation {
    Prepared {
        origin_configuration: OriginConfiguration,
        acceptance_position: SessionInputPosition,
    },
    Rejected(SubmitInputRejectedResult),
    Failed(OriginModelSettingsError),
}

fn prepare_delegated_successor(
    command: &SubmitInput,
    session: &Session,
    configuration: PerInputConfigurationChoices,
    previous_position: Option<SessionInputPosition>,
    select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
) -> DelegatedSuccessorPreparation {
    let checked = match session
        .current_configuration_defaults()
        .derive_request_with_model_settings(
            configuration.expected_session_defaults_version(),
            configuration.model(),
            configuration.model_settings(),
        ) {
        Ok(checked) => checked,
        Err(mismatch) => {
            return DelegatedSuccessorPreparation::Rejected(
                SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                    session: command.session,
                    expected: mismatch.expected(),
                    current: mismatch.current(),
                },
            );
        }
    };
    let origin_configuration = match freeze_origin_configuration(checked, select_definition, None) {
        Ok(configuration) => configuration,
        Err(OriginModelSettingsError::UnknownAlias(unknown)) => {
            return DelegatedSuccessorPreparation::Rejected(
                SubmitInputRejectedResult::UnknownModelAlias {
                    session: command.session,
                    alias: unknown.alias(),
                },
            );
        }
        Err(failure) => return DelegatedSuccessorPreparation::Failed(failure),
    };
    let acceptance_position = match next_acceptance_position(previous_position) {
        Ok(position) => position,
        Err(last) => {
            return DelegatedSuccessorPreparation::Rejected(
                SubmitInputRejectedResult::AcceptancePositionExhausted {
                    session: command.session,
                    last,
                },
            );
        }
    };
    DelegatedSuccessorPreparation::Prepared {
        origin_configuration,
        acceptance_position,
    }
}

fn freeze_origin_configuration(
    checked: crate::VersionCheckedConfigurationRequest,
    select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
    capabilities: Option<&ModelCapabilityCatalog>,
) -> Result<OriginConfiguration, OriginModelSettingsError> {
    match capabilities {
        Some(capabilities) => OriginConfiguration::freeze_with_model_settings(
            checked,
            select_definition,
            capabilities,
        ),
        None => OriginConfiguration::freeze(checked, select_definition),
    }
}

fn delivery_creates_turn(delivery: DeliveryRequest) -> bool {
    matches!(
        delivery,
        DeliveryRequest::StartWhenNoActiveTurn { .. }
            | DeliveryRequest::Interrupt { .. }
            | DeliveryRequest::AfterCurrentTurn { .. }
    )
}

fn next_acceptance_position(
    previous_position: Option<SessionInputPosition>,
) -> Result<SessionInputPosition, SessionInputPosition> {
    match previous_position {
        None => Ok(SessionInputPosition::first()),
        Some(last) => last.checked_next().ok_or(last),
    }
}

impl PartialEq for SubmitInput {
    fn eq(&self, other: &Self) -> bool {
        self.session == other.session
            && self.actor == other.actor
            && self.content == other.content
            && self.delivery == other.delivery
    }
}

impl Eq for SubmitInput {}

impl Hash for SubmitInput {
    fn hash<H: Hasher>(&self, state: &mut H) {
        "submit_input".hash(state);
        self.session.hash(state);
        self.actor.hash(state);
        self.content.hash(state);
        self.delivery.hash(state);
    }
}

/// The terminal recorded result of one canonical input command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubmitInputResult {
    /// The input was durably accepted with one treatment-specific effect.
    Applied(SubmitInputAppliedResult),
    /// Authoritative state rejected the caller's requested treatment.
    Rejected(SubmitInputRejectedResult),
}

/// The exact applied acceptance shape.
///
/// Both variants contain private-field values sealed behind authoritative
/// preparation and checked reconstitution. Pending steering cannot carry a
/// turn candidate, queue order, or configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubmitInputAppliedResult {
    /// Acceptance created ordinary accepted-input-origin work.
    TurnOrigin(SubmitInputTurnOriginAppliedResult),
    /// Acceptance created pending steering bound to the exact active turn.
    PendingSteering(SubmitInputPendingSteeringAppliedResult),
}

impl SubmitInputAppliedResult {
    /// Returns the durable accepted-input identity.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        match self {
            Self::TurnOrigin(result) => result.accepted_input,
            Self::PendingSteering(result) => result.accepted_input,
        }
    }

    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        match self {
            Self::TurnOrigin(result) => result.session,
            Self::PendingSteering(result) => result.session,
        }
    }

    /// Returns the immutable session acceptance position.
    pub const fn acceptance_position(&self) -> SessionInputPosition {
        match self {
            Self::TurnOrigin(result) => result.acceptance_position,
            Self::PendingSteering(result) => result.acceptance_position,
        }
    }

    /// Returns the exact initial durable disposition.
    pub const fn disposition(&self) -> AcceptedInputDisposition {
        match self {
            Self::TurnOrigin(result) => AcceptedInputDisposition::OriginOf(result.turn),
            Self::PendingSteering(result) => AcceptedInputDisposition::PendingSteering {
                binding: result.binding,
            },
        }
    }

    /// Borrows turn-origin fields when this acceptance created logical work.
    pub const fn turn_origin(&self) -> Option<&SubmitInputTurnOriginAppliedResult> {
        match self {
            Self::TurnOrigin(result) => Some(result),
            Self::PendingSteering(_) => None,
        }
    }

    /// Borrows pending-steering fields when acceptance created no turn.
    pub const fn pending_steering(&self) -> Option<&SubmitInputPendingSteeringAppliedResult> {
        match self {
            Self::PendingSteering(result) => Some(result),
            Self::TurnOrigin(_) => None,
        }
    }
}

/// The complete applied receipt for accepted-input-origin work.
///
/// Raw facts cannot construct this private-field value.
///
/// ```compile_fail
/// # use signalbox_domain::SubmitInputTurnOriginAppliedResult;
/// fn bypass_checked_construction(result: &SubmitInputTurnOriginAppliedResult) {
///     let _ = result.turn;
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmitInputTurnOriginAppliedResult {
    accepted_input: AcceptedInputId,
    session: SessionId,
    acceptance_position: SessionInputPosition,
    turn: TurnId,
    queue_order: AcceptedInputQueueOrder,
    origin_configuration: Box<OriginConfiguration>,
    applied_interrupt: Option<Box<AppliedInterruptCommandResult>>,
}

impl SubmitInputTurnOriginAppliedResult {
    /// Returns the durable accepted-input identity.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the future queued logical-work identity.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the initial durable disposition.
    pub const fn disposition(&self) -> AcceptedInputDisposition {
        AcceptedInputDisposition::OriginOf(self.turn)
    }

    /// Returns the complete ordinary queue-order fact.
    pub const fn queue_order(&self) -> AcceptedInputQueueOrder {
        self.queue_order
    }

    /// Returns the immutable session acceptance position.
    pub const fn acceptance_position(&self) -> SessionInputPosition {
        self.acceptance_position
    }

    /// Borrows the complete frozen origin configuration.
    pub const fn origin_configuration(&self) -> &OriginConfiguration {
        &self.origin_configuration
    }

    /// Borrows the exact applied-interrupt authority when this origin
    /// immediately succeeds the interrupted active turn.
    pub const fn applied_interrupt(&self) -> Option<&AppliedInterruptCommandResult> {
        match &self.applied_interrupt {
            Some(result) => Some(result),
            None => None,
        }
    }

    /// Constructs the durable settings event that belongs to this accepted
    /// origin and its frozen configuration.
    pub fn model_settings_event(&self) -> Option<crate::TurnModelSettingsResolved> {
        crate::TurnModelSettingsResolved::try_new(
            self.accepted_input,
            self.turn,
            self.origin_configuration.session_defaults_version(),
            *self.origin_configuration.effective().model(),
            self.origin_configuration
                .requested()
                .per_call_model_settings(),
            self.origin_configuration.effective().model_settings(),
            self.origin_configuration.model_settings_adjusted_from(),
            self.origin_configuration
                .model_settings_adjustments()
                .to_vec(),
        )
    }
}

/// The complete applied receipt for pending steering.
///
/// This shape has no turn-origin, queue-order, or configuration field.
///
/// ```compile_fail
/// # use signalbox_domain::SubmitInputPendingSteeringAppliedResult;
/// fn bypass_checked_construction(result: &SubmitInputPendingSteeringAppliedResult) {
///     let _ = result.binding;
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubmitInputPendingSteeringAppliedResult {
    accepted_input: AcceptedInputId,
    session: SessionId,
    acceptance_position: SessionInputPosition,
    binding: SteeringBinding,
}

impl SubmitInputPendingSteeringAppliedResult {
    /// Returns the durable accepted-input identity.
    pub const fn accepted_input(&self) -> AcceptedInputId {
        self.accepted_input
    }

    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the immutable session acceptance position.
    pub const fn acceptance_position(&self) -> SessionInputPosition {
        self.acceptance_position
    }

    /// Returns the exact active-turn steering binding.
    pub const fn binding(&self) -> SteeringBinding {
        self.binding
    }
}

/// Typed authoritative input-acceptance rejections.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SubmitInputRejectedResult {
    /// An attachment digest had no catalogued verified replica.
    AttachmentBlobNotFound {
        /// The unavailable immutable byte identity.
        digest: BlobDigest,
    },
    /// Distinct attachment bytes exceeded the deployment ceiling.
    AttachmentByteBudgetExceeded {
        /// The configured maximum aggregate byte count.
        maximum_bytes: u64,
    },
    /// The target session did not exist.
    SessionNotFound {
        /// The absent target.
        session: SessionId,
    },
    /// An active-work request named a turn while the session had none.
    NoActiveTurn {
        /// The target session.
        session: SessionId,
        /// The turn the caller expected to be active.
        expected_active_turn: TurnId,
    },
    /// A no-active-turn start was submitted while a turn owned the slot.
    ActiveTurnPresent {
        /// The target session.
        session: SessionId,
        /// The authoritative active turn.
        active_turn: TurnId,
    },
    /// An active-work request named a stale turn.
    ActiveTurnMismatch {
        /// The target session.
        session: SessionId,
        /// The turn named by the command.
        expected_active_turn: TurnId,
        /// The authoritative active turn.
        actual_active_turn: TurnId,
    },
    /// The caller's expected defaults version was no longer current.
    SessionDefaultsVersionMismatch {
        /// The target session.
        session: SessionId,
        /// The caller's expected version.
        expected: SessionConfigurationDefaultsVersion,
        /// The authoritative current version.
        current: SessionConfigurationDefaultsVersion,
    },
    /// The requested alias had no selectable current definition.
    UnknownModelAlias {
        /// The target session.
        session: SessionId,
        /// The unresolved alias.
        alias: ModelAlias,
    },
    /// The session's positive input-position ordinal had no successor.
    AcceptancePositionExhausted {
        /// The target session.
        session: SessionId,
        /// The maximum recorded position.
        last: SessionInputPosition,
    },
    /// A safe-point request arrived after interruption had already stopped the
    /// active attempt from authorizing more semantic work.
    ///
    /// Recorded by earlier daemons only; a stopping turn now accepts steering,
    /// and this variant survives for replay of those records.
    SafePointUnavailableWhileStopping {
        /// The target session.
        session: SessionId,
        /// The exact active turn retaining the slot.
        active_turn: TurnId,
        /// The command whose applied result is already stopping the turn.
        existing_command: DurableCommandId,
    },
    /// A distinct later interrupt cannot replace the exact proof already
    /// applied to the active turn.
    InterruptAlreadyApplied {
        /// The target session.
        session: SessionId,
        /// The exact active turn retaining the slot.
        active_turn: TurnId,
        /// The command whose applied result remains cancellation authority.
        existing_command: DurableCommandId,
    },
    /// An interrupt arrived while a parked approval wait held the active
    /// slot; the wait remains parked until its canonical decision command
    /// resolves the approval obligation.
    InterruptUnavailableWhileAwaitingApproval {
        /// The target session.
        session: SessionId,
        /// The exact active turn retaining the slot on its approval wait.
        active_turn: TurnId,
    },
}

/// One sealed pre-commit command/result candidate.
#[derive(Clone, Debug)]
pub struct PreparedSubmitInput {
    command: SubmitInput,
    result: SubmitInputResult,
}

impl PreparedSubmitInput {
    /// Borrows the exact canonical command.
    pub const fn command(&self) -> &SubmitInput {
        &self.command
    }

    /// Borrows the exact terminal result to record.
    pub const fn result(&self) -> &SubmitInputResult {
        &self.result
    }

    /// Consumes the candidate into correlated transaction inputs.
    pub fn into_parts(self) -> (SubmitInput, SubmitInputResult) {
        (self.command, self.result)
    }
}

/// Why authoritative-state preparation could not produce a terminal result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitInputPreparationFailure {
    /// The supplied session belonged to another command target.
    SessionMismatch {
        /// The different session supplied for preparation.
        provided_session: SessionId,
    },
    /// Turn identity supply did not match the delivery variant.
    ///
    /// `NextSafePoint` initially creates no turn; every other delivery mode
    /// needs a turn candidate for the state in which it can apply.
    TurnCandidateMismatch,
    /// A new accepted-input candidate reused the active turn's canonical
    /// origin identity.
    AcceptedInputCandidateReusesActiveOrigin {
        /// The authoritative active turn.
        active_turn: TurnId,
        /// The colliding accepted-input candidate and active origin.
        accepted_input: AcceptedInputId,
    },
    /// The supplied complete scheduling aggregate has no active slot owner.
    ActiveTurnProjectionMissing,
    /// The proposed interrupt successor would violate the checked complete
    /// queue order.
    InterruptQueueOrderInvalid,
    /// Capability-aware settings resolution failed after authoritative
    /// selection freezing.
    ModelSettingsResolution(OriginModelSettingsError),
}

/// A nonterminal correlation failure during preparation.
///
/// This is a preparation correlation failure, not a terminal recorded
/// rejection, and claims no command identity.
#[derive(Clone, Debug)]
pub struct SubmitInputPreparationError {
    command: Box<SubmitInput>,
    failure: SubmitInputPreparationFailure,
}

impl SubmitInputPreparationError {
    /// Borrows the unchanged canonical command.
    pub const fn command(&self) -> &SubmitInput {
        &self.command
    }

    /// Returns the exact nonterminal failure.
    pub const fn failure(&self) -> SubmitInputPreparationFailure {
        self.failure
    }

    /// Returns the unchanged command and exact failure.
    pub fn into_parts(self) -> (SubmitInput, SubmitInputPreparationFailure) {
        (*self.command, self.failure)
    }
}
