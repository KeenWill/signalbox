//! Durable submit-input commands and authoritative-state preparation for
//! `docs/spec/turn-lifecycle-and-scheduling.md`.

use super::prepared::PreparedSubmitInput;
use super::prepared::SubmitInputPreparationError;
use super::prepared::SubmitInputPreparationFailure;
use super::result::SubmitInputAppliedResult;
use super::result::SubmitInputPendingSteeringAppliedResult;
use super::result::SubmitInputRejectedResult;
use super::result::SubmitInputResult;
use super::result::SubmitInputTurnOriginAppliedResult;
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
    pub(super) command_id: DurableCommandId,
    pub(super) session: SessionId,
    pub(super) actor: Actor,
    pub(super) content: UserContent,
    pub(super) delivery: DeliveryRequest,
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

    /// Constructs a daemon-core successor after a terminal continuation needs compaction.
    pub const fn new_core_continuation(
        command_id: DurableCommandId,
        session: SessionId,
        content: UserContent,
        configuration: PerInputConfigurationChoices,
    ) -> Self {
        Self {
            command_id,
            session,
            actor: Actor::Core,
            content,
            delivery: DeliveryRequest::StartWhenNoActiveTurn { configuration },
        }
    }

    /// Constructs input attributed only to the verified host session capability.
    pub const fn new_program(
        command_id: DurableCommandId,
        session: SessionId,
        content: UserContent,
        delivery: DeliveryRequest,
        capability: crate::ProgramSessionCapability,
    ) -> Self {
        Self::from_recorded_fields(command_id, session, capability.actor(), content, delivery)
    }

    /// Reconstitutes canonical fields after storage validates their references and spelling.
    pub const fn from_recorded_fields(
        command_id: DurableCommandId,
        session: SessionId,
        actor: Actor,
        content: UserContent,
        delivery: DeliveryRequest,
    ) -> Self {
        Self {
            command_id,
            session,
            actor,
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

pub(super) fn freeze_origin_configuration(
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
