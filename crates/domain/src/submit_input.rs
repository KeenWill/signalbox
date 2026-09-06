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

use std::{
    collections::HashSet,
    hash::{Hash, Hasher},
};

use crate::{
    AcceptedInputDisposition, AcceptedInputId, AcceptedInputLifecycle, AcceptedInputQueueOrder,
    AcceptedInputQueuePriority, AcceptedInputQueueWork, AcceptedInputSchedulingProjection, Actor,
    AppliedInterruptCommandResult, AppliedInterruptState, BlobDigest, CurrentTurnAttemptState,
    DeliveryRequest, DescendantTerminationScope, DurableCommandId, FrozenAliasDefinition,
    FrozenModelSelection, GoalGeneration, GoalTurnSource, ModelAlias, ModelCapabilityCatalog,
    ModelChangeAdjustment, ModelSelectionRequest, ModelSettingsOverlay, OriginConfiguration,
    OriginModelSettingsError, PerInputConfigurationChoices, ReconciliationReason, Session,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionId,
    SessionInputPosition, SteeringBinding, TurnDisposition, TurnId, UserContent,
    ValidatedModelSettings, VersionedSessionConfigurationDefaults,
    derive_accepted_input_total_order,
};

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
    chain: Vec<SubmitInputTurnOriginReconstitutionFacts>,
}

#[derive(Clone, Debug)]
struct SubmitInputTurnOriginReconstitutionFacts {
    provenance: TurnOriginProvenance,
    lifecycle: AcceptedInputLifecycle,
    queue_accepted_input: AcceptedInputId,
    queue_session: SessionId,
    queue_turn: TurnId,
    queue_order: AcceptedInputQueueOrder,
    source_terminal: Option<SubmitInputTerminalFacts>,
}

#[derive(Clone, Debug)]
enum TurnOriginProvenance {
    Submit(Box<ReconstitutedSubmitInput>),
    Goal(GoalTurnOriginFacts),
}

#[derive(Clone, Debug)]
struct GoalTurnOriginFacts {
    _generation: GoalGeneration,
    source: GoalTurnSource,
    session: SessionId,
    accepted_input: AcceptedInputId,
    turn: TurnId,
    acceptance_position: SessionInputPosition,
    content: UserContent,
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
struct SubmitInputTerminalFacts {
    turn: TurnId,
    disposition: TurnDisposition,
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

#[derive(Clone, Debug)]
struct SubmitInputTurnOriginAppliedReconstitutionFacts {
    result_session: SessionId,
    result_accepted_input: AcceptedInputId,
    result_turn: TurnId,
    predecessor_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
    non_accepted_predecessor: Option<NonAcceptedTurnPredecessorReconstitutionInput>,
    accepted_command: DurableCommandId,
    accepted_input: AcceptedInputId,
    accepted_session: SessionId,
    accepted_content: UserContent,
    accepted_delivery: DeliveryRequest,
    accepted_position: SessionInputPosition,
    accepted_disposition: AcceptedInputDisposition,
    queue_session: SessionId,
    queue_turn: TurnId,
    queue_order: AcceptedInputQueueOrder,
    defaults_session: SessionId,
    defaults_version: SessionConfigurationDefaultsVersion,
    defaults: SessionConfigurationDefaults,
    stored_requested_model: ModelSelectionRequest,
    stored_frozen_model: FrozenModelSelection,
    stored_model_settings: Option<ValidatedModelSettings>,
    stored_model_settings_adjustments: Box<[ModelChangeAdjustment]>,
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

#[derive(Clone, Debug)]
struct SubmitInputPendingSteeringAppliedReconstitutionFacts {
    result_session: SessionId,
    result_accepted_input: AcceptedInputId,
    result_source_turn: TurnId,
    source_turn_origin: SubmitInputTurnOriginReconstitutionInput,
    accepted_command: DurableCommandId,
    accepted_input: AcceptedInputId,
    accepted_session: SessionId,
    accepted_content: UserContent,
    accepted_delivery: DeliveryRequest,
    accepted_position: SessionInputPosition,
}

#[derive(Clone, Debug)]
enum SubmitInputReconstitutionFacts {
    AppliedTurnOrigin(Box<SubmitInputTurnOriginAppliedReconstitutionFacts>),
    AppliedPendingSteering(Box<SubmitInputPendingSteeringAppliedReconstitutionFacts>),
    RejectedAttachmentBlobNotFound {
        result_session: SessionId,
        result_digest: BlobDigest,
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
    /// The configured maximum aggregate byte count stored in the result.
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

/// Complete checked domain inputs for reconstructing one recorded submission.
///
/// The stored actor is the durable spelling of the command's attributed
/// agency and is supplied separately for the domain-owned comparison.
#[derive(Clone, Debug)]
pub struct SubmitInputReconstitutionInput {
    command: SubmitInput,
    stored_actor: Actor,
    facts: SubmitInputReconstitutionFacts,
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
        } = input;
        Self {
            command,
            stored_actor,
            facts: SubmitInputReconstitutionFacts::RejectedAttachmentBlobNotFound {
                result_session,
                result_digest,
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
            } => {
                if result_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ResultSessionMismatch,
                    ));
                }
                if !self.command.content.parts().iter().any(|part| {
                    matches!(
                        part,
                        crate::UserContentPart::Attachment { digest, .. }
                            if *digest == result_digest
                    )
                }) {
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

fn validate_existing_interrupt(
    command: &SubmitInput,
    active_turn: TurnId,
    interrupt: AppliedInterruptCommandResult,
    recorded_command: Option<DurableCommandId>,
) -> Result<(), SubmitInputReconstitutionFailure> {
    if interrupt.session() != command.session
        || interrupt.proof().predecessor() != active_turn
        || interrupt.proof().command() == command.command_id
        || recorded_command.is_some_and(|recorded| recorded != interrupt.proof().command())
    {
        return Err(SubmitInputReconstitutionFailure::ExistingInterruptMismatch);
    }
    Ok(())
}

struct StoredOriginConfigurationReconstitutionFacts {
    defaults_session: SessionId,
    defaults_version: SessionConfigurationDefaultsVersion,
    defaults: SessionConfigurationDefaults,
    stored_requested_model: ModelSelectionRequest,
    stored_frozen_model: FrozenModelSelection,
    stored_model_settings: Option<ValidatedModelSettings>,
    stored_model_settings_adjustments: Vec<ModelChangeAdjustment>,
}

fn reconstruct_origin_configuration(
    command: &SubmitInput,
    facts: StoredOriginConfigurationReconstitutionFacts,
) -> Result<OriginConfiguration, SubmitInputReconstitutionFailure> {
    let StoredOriginConfigurationReconstitutionFacts {
        defaults_session,
        defaults_version,
        defaults,
        stored_requested_model,
        stored_frozen_model,
        stored_model_settings,
        stored_model_settings_adjustments,
    } = facts;
    let Some(configuration) = explicit_origin_configuration(command.delivery) else {
        return Err(SubmitInputReconstitutionFailure::AppliedDeliveryIsNotTurnOrigin);
    };
    if defaults_session != command.session {
        return Err(SubmitInputReconstitutionFailure::DefaultsSessionMismatch);
    }
    if defaults_version != configuration.expected_session_defaults_version() {
        return Err(SubmitInputReconstitutionFailure::DefaultsVersionMismatch);
    }

    let versioned = VersionedSessionConfigurationDefaults::reconstitute(defaults_version, defaults);
    let checked = versioned
        .derive_request_with_model_settings(
            defaults_version,
            configuration.model(),
            configuration.model_settings(),
        )
        .map_err(|_| SubmitInputReconstitutionFailure::DefaultsVersionMismatch)?;
    if checked.request().model() != stored_requested_model {
        return Err(SubmitInputReconstitutionFailure::RequestedModelMismatch);
    }
    let selected_direct = stored_frozen_model.selected_direct();
    let legacy_settings_are_safe = checked.request().per_call_model_settings()
        == ModelSettingsOverlay::inherit_all()
        && !checked
            .request()
            .model_settings()
            .validated_for()
            .is_some_and(|validated| validated != selected_direct);

    match stored_model_settings {
        Some(stored_model_settings) => OriginConfiguration::reconstitute_with_model_settings(
            checked,
            stored_frozen_model,
            stored_model_settings,
            stored_model_settings_adjustments,
        )
        .ok_or(SubmitInputReconstitutionFailure::FrozenModelMismatch),
        None if stored_model_settings_adjustments.is_empty() && legacy_settings_are_safe => {
            let frozen = OriginConfiguration::freeze(checked, |alias| match stored_frozen_model {
                FrozenModelSelection::FrozenAlias {
                    alias: stored_alias,
                    definition,
                } if stored_alias == alias => Some(definition),
                FrozenModelSelection::Direct(_) | FrozenModelSelection::FrozenAlias { .. } => None,
            })
            .map_err(|_| SubmitInputReconstitutionFailure::FrozenModelMismatch)?;
            (frozen.effective().model() == &stored_frozen_model)
                .then_some(frozen)
                .ok_or(SubmitInputReconstitutionFailure::FrozenModelMismatch)
        }
        None => Err(SubmitInputReconstitutionFailure::FrozenModelMismatch),
    }
}

fn explicit_origin_configuration(
    delivery: DeliveryRequest,
) -> Option<PerInputConfigurationChoices> {
    match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { configuration }
        | DeliveryRequest::Interrupt { configuration, .. }
        | DeliveryRequest::AfterCurrentTurn { configuration, .. } => Some(configuration),
        DeliveryRequest::NextSafePoint { .. } => None,
    }
}

fn rejection_configuration(
    delivery: DeliveryRequest,
) -> Result<(PerInputConfigurationChoices, Option<TurnId>), SubmitInputReconstitutionFailure> {
    match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { configuration } => Ok((configuration, None)),
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn,
            configuration,
        }
        | DeliveryRequest::Interrupt {
            expected_active_turn,
            configuration,
            ..
        } => Ok((configuration, Some(expected_active_turn))),
        DeliveryRequest::NextSafePoint { .. } => {
            Err(SubmitInputReconstitutionFailure::RejectionHasNoExplicitOriginConfiguration)
        }
    }
}

fn position_exhaustion_origin(
    delivery: DeliveryRequest,
) -> Result<Option<TurnId>, SubmitInputReconstitutionFailure> {
    match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { .. } => Ok(None),
        DeliveryRequest::NextSafePoint {
            expected_active_turn,
        }
        | DeliveryRequest::Interrupt {
            expected_active_turn,
            ..
        }
        | DeliveryRequest::AfterCurrentTurn {
            expected_active_turn,
            ..
        } => Ok(Some(expected_active_turn)),
    }
}

struct ValidatedTurnOrigin {
    session: SessionId,
    turn: TurnId,
    acceptance_position: SessionInputPosition,
    accepted_input: AcceptedInputId,
    content: UserContent,
    accepted_inputs: HashSet<AcceptedInputId>,
    command_ids: HashSet<DurableCommandId>,
    turns: HashSet<TurnId>,
}

fn goal_turn_source_references_turn(source: GoalTurnSource, turn: TurnId) -> bool {
    match source {
        GoalTurnSource::UserEvent(_) => false,
        GoalTurnSource::SuccessfulTurn(predecessor) => predecessor == turn,
    }
}

fn validate_turn_origin_reconstitution_input(
    input: &SubmitInputTurnOriginReconstitutionInput,
) -> Option<ValidatedTurnOrigin> {
    struct ValidatedOriginPosition {
        session: SessionId,
        turn: TurnId,
        acceptance_position: SessionInputPosition,
        accepted_input: AcceptedInputId,
        content: UserContent,
    }

    let mut validated: Option<ValidatedOriginPosition> = None;
    let mut accepted_inputs = HashSet::with_capacity(input.chain.len());
    let mut command_ids = HashSet::with_capacity(input.chain.len());
    let mut turns = HashSet::with_capacity(input.chain.len());

    for facts in &input.chain {
        let receipt = match &facts.provenance {
            TurnOriginProvenance::Submit(receipt) => receipt,
            TurnOriginProvenance::Goal(goal) => {
                if validated.is_some()
                    || facts.source_terminal.is_some()
                    || !accepted_inputs.insert(goal.accepted_input)
                    || !turns.insert(goal.turn)
                    || facts.lifecycle.id() != goal.accepted_input
                    || facts.lifecycle.disposition()
                        != &AcceptedInputDisposition::OriginOf(goal.turn)
                    || facts.queue_accepted_input != goal.accepted_input
                    || facts.queue_session != goal.session
                    || facts.queue_turn != goal.turn
                    || facts.queue_order
                        != AcceptedInputQueueOrder::ordinary(goal.acceptance_position)
                    || goal_turn_source_references_turn(goal.source, goal.turn)
                {
                    return None;
                }
                validated = Some(ValidatedOriginPosition {
                    session: goal.session,
                    turn: goal.turn,
                    acceptance_position: goal.acceptance_position,
                    accepted_input: goal.accepted_input,
                    content: goal.content.clone(),
                });
                continue;
            }
        };
        let SubmitInputResult::Applied(applied) = receipt.result() else {
            return None;
        };
        if !accepted_inputs.insert(applied.accepted_input())
            || !command_ids.insert(receipt.command().command_id())
        {
            return None;
        }
        let (turn, expected_queue_order) = match (
            applied,
            facts.lifecycle.disposition(),
            &facts.source_terminal,
            validated.as_ref(),
        ) {
            (
                SubmitInputAppliedResult::TurnOrigin(origin),
                AcceptedInputDisposition::OriginOf(turn),
                None,
                None,
            ) if *turn == origin.turn() => (*turn, origin.queue_order()),
            (
                SubmitInputAppliedResult::PendingSteering(pending),
                AcceptedInputDisposition::ReclassifiedAsTurnOrigin { turn, .. },
                Some(source_terminal),
                Some(source_origin),
            ) if *turn != pending.binding().source_turn() => {
                if source_origin.session != applied.session()
                    || source_origin.turn != pending.binding().source_turn()
                    || source_terminal.turn != source_origin.turn
                    || source_origin.acceptance_position >= applied.acceptance_position()
                    || !terminal_disposition_matches_turn(
                        &source_terminal.disposition,
                        source_origin.turn,
                    )
                {
                    return None;
                }
                if let Some(command) = terminal_disposition_command(&source_terminal.disposition)
                    && !command_ids.insert(command)
                {
                    return None;
                }
                (
                    *turn,
                    AcceptedInputQueueOrder::ordinary(applied.acceptance_position()),
                )
            }
            _ => return None,
        };
        if facts.lifecycle.id() != applied.accepted_input()
            || facts.queue_accepted_input != applied.accepted_input()
            || facts.queue_session != applied.session()
            || facts.queue_turn != turn
            || facts.queue_order != expected_queue_order
            || !turns.insert(turn)
        {
            return None;
        }

        validated = Some(ValidatedOriginPosition {
            session: applied.session(),
            turn,
            acceptance_position: applied.acceptance_position(),
            accepted_input: applied.accepted_input(),
            content: receipt.command().content().clone(),
        });
    }

    let validated = validated?;
    Some(ValidatedTurnOrigin {
        session: validated.session,
        turn: validated.turn,
        acceptance_position: validated.acceptance_position,
        accepted_input: validated.accepted_input,
        content: validated.content,
        accepted_inputs,
        command_ids,
        turns,
    })
}

fn terminal_disposition_command(disposition: &TurnDisposition) -> Option<DurableCommandId> {
    match disposition {
        TurnDisposition::Completed
        | TurnDisposition::Refused
        | TurnDisposition::Failed
        | TurnDisposition::Retired => None,
        TurnDisposition::Cancelled { cause } => Some(cause.command()),
        TurnDisposition::ReconciliationRequired { marker } => match marker.reason() {
            ReconciliationReason::UserChoseReconciliation { decision } => {
                Some(decision.decision_command())
            }
            ReconciliationReason::InterruptRequiresReconciliation { interrupt } => {
                Some(interrupt.command())
            }
            ReconciliationReason::FatalMismatchRequiresReconciliation { causes } => {
                match causes.interrupt() {
                    AppliedInterruptState::NoAppliedInterrupt => None,
                    AppliedInterruptState::Applied { proof } => Some(proof.command()),
                }
            }
            ReconciliationReason::AutomaticRecovery { .. } => None,
        },
    }
}

fn terminal_disposition_matches_turn(disposition: &TurnDisposition, turn: TurnId) -> bool {
    match disposition {
        TurnDisposition::Completed | TurnDisposition::Refused | TurnDisposition::Failed => true,
        // A retired turn never activated, so it was never a steering source.
        TurnDisposition::Retired => false,
        TurnDisposition::Cancelled { cause } => cause.predecessor() == turn,
        TurnDisposition::ReconciliationRequired { marker } => match marker.reason() {
            ReconciliationReason::UserChoseReconciliation { decision } => decision.turn() == turn,
            ReconciliationReason::InterruptRequiresReconciliation { interrupt } => {
                interrupt.predecessor() == turn
            }
            ReconciliationReason::FatalMismatchRequiresReconciliation { causes } => {
                match causes.interrupt() {
                    AppliedInterruptState::NoAppliedInterrupt => true,
                    AppliedInterruptState::Applied { proof } => proof.predecessor() == turn,
                }
            }
            ReconciliationReason::AutomaticRecovery { .. } => true,
        },
    }
}

fn validate_rejection_active_turn_origin(
    command: &SubmitInput,
    expected_turn: Option<TurnId>,
    origin: Option<&SubmitInputTurnOriginReconstitutionInput>,
) -> Result<(), SubmitInputReconstitutionFailure> {
    match (expected_turn, origin) {
        (None, None) => Ok(()),
        (Some(expected_turn), Some(origin)) => {
            let Some(result) = validate_turn_origin_reconstitution_input(origin) else {
                return Err(SubmitInputReconstitutionFailure::RejectionActiveTurnOriginMismatch);
            };
            if result.session != command.session || result.turn != expected_turn {
                return Err(SubmitInputReconstitutionFailure::RejectionActiveTurnOriginMismatch);
            }
            if result.command_ids.contains(&command.command_id) {
                return Err(
                    SubmitInputReconstitutionFailure::RejectionActiveTurnOriginCommandReused,
                );
            }
            Ok(())
        }
        (None, Some(_)) | (Some(_), None) => {
            Err(SubmitInputReconstitutionFailure::RejectionActiveTurnOriginMismatch)
        }
    }
}

fn expected_active_turn(delivery: DeliveryRequest) -> Option<TurnId> {
    match delivery {
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
    }
}

/// Why complete typed durable facts cannot reconstruct a recorded submission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitInputReconstitutionFailure {
    /// The stored actor attribution differs from the command.
    StoredActorMismatch,
    /// Turn-origin facts carry a delivery that creates no admitted origin.
    AppliedDeliveryIsNotTurnOrigin,
    /// Pending-steering facts carry a non-safe-point delivery.
    AppliedDeliveryIsNotNextSafePoint,
    /// A terminal result names another session.
    ResultSessionMismatch,
    /// A missing-blob rejection names no attachment in the command.
    AttachmentDigestMismatch,
    /// A byte-budget rejection has no positive maximum or attachment.
    AttachmentBudgetMismatch,
    /// The accepted-input effect names another command.
    AcceptedCommandMismatch,
    /// The result and accepted-input effect name different inputs.
    AcceptedInputMismatch,
    /// The accepted-input effect belongs to another session.
    AcceptedSessionMismatch,
    /// The stored accepted content differs from the command.
    AcceptedContentMismatch,
    /// The stored delivery treatment differs from the command.
    AcceptedDeliveryMismatch,
    /// A turn-origin record does not retain its exact origin disposition.
    AcceptedDispositionMismatch,
    /// The applied steering result names another source turn.
    SteeringSourceTurnMismatch,
    /// The supplied source receipt is not the exact same-session turn origin.
    SteeringSourceTurnOriginMismatch,
    /// Pending steering reuses its source origin's accepted-input identity.
    SteeringSourceAcceptedInputReused,
    /// Pending steering reuses its source origin's durable-command identity.
    SteeringSourceCommandReused,
    /// Pending steering does not follow its source origin in acceptance order.
    SteeringAcceptanceDoesNotFollowSourceOrigin,
    /// The queue fact belongs to another session.
    QueueSessionMismatch,
    /// The queue fact names another future turn or an after-current result
    /// reuses its active predecessor.
    QueueTurnMismatch,
    /// An after-current result omits or cross-wires its predecessor origin,
    /// or a vacant-slot start supplies one.
    AfterCurrentPredecessorOriginMismatch,
    /// An after-current result reuses its predecessor's accepted-input ID.
    AfterCurrentPredecessorAcceptedInputReused,
    /// An after-current result reuses its predecessor's durable-command ID.
    AfterCurrentPredecessorCommandReused,
    /// After-current acceptance does not follow its predecessor origin.
    AfterCurrentAcceptanceDoesNotFollowPredecessorOrigin,
    /// The accepted-input and queue positions differ.
    QueuePositionMismatch,
    /// This slice's queue fact is not ordinary priority.
    QueuePriorityMismatch,
    /// An active-turn-present rejection carries a non-start command.
    ActiveTurnPresentRejectionMismatch,
    /// A no-active-turn result names a different expected turn or a start
    /// request.
    ExpectedActiveTurnMismatch,
    /// A stale-active rejection claims equal expected and actual turns.
    RejectedActiveTurnsAreEqual,
    /// Required same-session turn-origin evidence is missing or cross-wired.
    RejectionActiveTurnOriginMismatch,
    /// A rejected command reuses its actual turn origin's command identity.
    RejectionActiveTurnOriginCommandReused,
    /// A configuration rejection carries no explicit origin configuration.
    RejectionHasNoExplicitOriginConfiguration,
    /// A mismatch result repeats a different expected defaults version.
    ExpectedDefaultsVersionMismatch,
    /// A mismatch result claims equal expected and current versions.
    RejectedDefaultsVersionsAreEqual,
    /// The selected defaults record belongs to another session.
    DefaultsSessionMismatch,
    /// The selected defaults record carries another version.
    DefaultsVersionMismatch,
    /// The stored derived request differs from the version-checked request.
    RequestedModelMismatch,
    /// The stored frozen model differs from the checked request.
    FrozenModelMismatch,
    /// The recorded unknown alias differs from the alias that failed.
    UnknownAliasMismatch,
    /// The request did not select an alias.
    RejectionDidNotSelectAlias,
    /// The recorded last position still has a successor.
    PositionIsNotExhausted,
    /// A stopping-only rejection carries another delivery or active target.
    StoppingRejectionMismatch,
    /// The stored applied interrupt does not supply the exact earlier
    /// cancellation authority named by the rejection.
    ExistingInterruptMismatch,
}

/// Failed reconstitution retaining every typed input unchanged.
#[derive(Clone, Debug)]
pub struct SubmitInputReconstitutionError {
    input: Box<SubmitInputReconstitutionInput>,
    failure: SubmitInputReconstitutionFailure,
}

impl SubmitInputReconstitutionError {
    /// Returns why the complete projection was invalid.
    pub const fn failure(&self) -> SubmitInputReconstitutionFailure {
        self.failure
    }

    /// Borrows the complete unchanged input.
    pub const fn input(&self) -> &SubmitInputReconstitutionInput {
        &self.input
    }

    /// Returns the complete unchanged input and failure.
    pub fn into_parts(
        self,
    ) -> (
        SubmitInputReconstitutionInput,
        SubmitInputReconstitutionFailure,
    ) {
        (*self.input, self.failure)
    }
}

/// One complete recorded input handling reconstructed from matching facts.
///
/// This value authorizes no insertion, repair, transition, or command claim.
#[derive(Clone, Debug)]
pub struct ReconstitutedSubmitInput {
    command: SubmitInput,
    result: SubmitInputResult,
}

impl ReconstitutedSubmitInput {
    /// Borrows the reconstructed canonical command.
    pub const fn command(&self) -> &SubmitInput {
        &self.command
    }

    /// Borrows the reconstructed terminal result.
    pub const fn result(&self) -> &SubmitInputResult {
        &self.result
    }

    /// Returns the complete reconstructed command and result.
    pub fn into_parts(self) -> (SubmitInput, SubmitInputResult) {
        (self.command, self.result)
    }
}

#[cfg(test)]
mod tests;
