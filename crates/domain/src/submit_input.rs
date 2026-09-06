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
mod tests {
    use std::collections::{BTreeSet, hash_map::DefaultHasher};
    use std::hash::{Hash, Hasher};

    use super::{
        NonAcceptedTurnPredecessorReconstitutionInput, ReconstitutedSubmitInput,
        StoredOriginConfigurationReconstitutionFacts, SubmitInput,
        SubmitInputAppliedPendingSteeringReconstitutionInput, SubmitInputAppliedResult,
        SubmitInputAppliedTurnOriginReconstitutionInput,
        SubmitInputDirectTurnOriginConstructionInput, SubmitInputPreparationFailure,
        SubmitInputReclassifiedTurnOriginConstructionInput, SubmitInputReconstitutionFailure,
        SubmitInputReconstitutionInput,
        SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput,
        SubmitInputRejectedActiveTurnMismatchReconstitutionInput,
        SubmitInputRejectedActiveTurnPresentReconstitutionInput,
        SubmitInputRejectedAttachmentBlobNotFoundReconstitutionInput,
        SubmitInputRejectedAttachmentByteBudgetExceededReconstitutionInput,
        SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput,
        SubmitInputRejectedInterruptUnavailableWhileAwaitingApprovalReconstitutionInput,
        SubmitInputRejectedNoActiveTurnReconstitutionInput, SubmitInputRejectedResult,
        SubmitInputRejectedSessionNotFoundReconstitutionInput,
        SubmitInputRejectedUnknownModelAliasReconstitutionInput, SubmitInputResult,
        SubmitInputTerminalSourceConstructionInput, SubmitInputTerminalSourceReconstitutionInput,
        SubmitInputTurnOriginReconstitutionInput, freeze_origin_configuration,
        reconstruct_origin_configuration,
    };
    use crate::applied_interrupt::test_applied_interrupt_proof;
    use crate::test_support::{
        accepted_input_id, alias, command_id, direct, model_call_id, provider_target_evidence_id,
        session_id, turn_id,
    };
    use crate::test_support::{
        context_frontier_id, provider_model_identity, semantic_transcript_entry_id,
        tool_request_id, turn_attempt_id,
    };
    use crate::turn_attempt::test_fatal_mismatch_stop_causes;
    use crate::turn_lifecycle::{
        test_applied_stop_for_reconciliation_proof, test_reconciliation_marker,
    };
    use crate::{
        AcceptedInputDisposition, AcceptedInputLifecycle, AcceptedInputQueueOrder,
        AcceptedInputQueuePriority, AcceptedInputSchedulingProjection,
        AcceptedInputSchedulingReconstitutionInput, AcceptedInputStartingLineage,
        AcceptedInputTurnSchedulingRecord, AcceptedInputTurnSchedulingRecordState, ActiveTurnPhase,
        ActiveTurnSchedulingReconstitutionInput, Actor, AttachmentKind, BlobDigest,
        DangerousToolAutoApproval, DeclaredMediaType, DeliveryRequest, DescendantTerminationScope,
        FastModeOverlay, FastModeSupport, FrozenAliasDefinition, FrozenModelSelection,
        InitialSemanticTranscriptEntryPayload, IssuedOperationRef, ModelCallDisposition,
        ModelCallReconstitutionInput, ModelCallReconstitutionState, ModelCapabilities,
        ModelCapabilityCatalog, ModelCapabilityDefinition, ModelSelectionOverride,
        ModelSelectionRequest, ModelSettingsOverlay, ModelSettingsPrecedence,
        NonEmptyIssuedOperationRefs, NormalizedToolArguments, OriginConfiguration,
        OriginModelSettingsError, PerInputConfigurationChoices,
        PinnedProviderTargetReconstitutionInput, ReasoningLevel, ReconciliationReason,
        ResolvedContextFrontierReconstitutionInput, ResolvedContextFrontierSnapshot,
        ResolvedProviderTarget, SemanticTranscriptEntryReconstitutionInput,
        SemanticTranscriptEntryRef, Session, SessionAcceptanceTailEntryReconstitutionInput,
        SessionAcceptanceTailReconstitutionInput, SessionConfigurationDefaults,
        SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
        SessionInputPosition, SessionReconstitutionInput, SettingOverlay, SteeringBinding,
        ToolBatchPhaseReconstitutionInput, ToolBatchReconstitutionInput, ToolName,
        ToolRequestOrdinal, ToolRequestReconstitutionInput, TranscriptAncestry, TurnDisposition,
        UserContent, UserContentPart,
    };

    fn version(value: u64) -> SessionConfigurationDefaultsVersion {
        SessionConfigurationDefaultsVersion::try_from_u64(value).expect("positive test version")
    }

    fn choices(expected: u64, model: ModelSelectionOverride) -> PerInputConfigurationChoices {
        PerInputConfigurationChoices::new(version(expected), model)
    }

    fn defaults(selection: ModelSelectionRequest) -> SessionConfigurationDefaults {
        SessionConfigurationDefaults::new(selection)
    }

    fn session(id: u128, current: u64, selection: ModelSelectionRequest) -> Session {
        SessionReconstitutionInput::new(
            session_id(id),
            session_id(id),
            SessionCreationProvenance::new(
                SessionCreationCause::Interactive,
                TranscriptAncestry::None,
            ),
            session_id(id),
            version(current),
            session_id(id),
            version(current),
            defaults(selection),
            crate::SessionPlacementReconstitutionFacts {
                current_pointer_session: session_id(id),
                current_pointer_version: crate::SessionPlacementVersion::INITIAL,
                selected_event_session: session_id(id),
                selected_event: crate::VersionedSessionPlacement::initial(
                    crate::SessionPlacement::pathless(),
                ),
            },
        )
        .reconstitute()
        .expect("test session projection is complete")
    }

    fn content(value: &str) -> UserContent {
        UserContent::try_text(value.to_owned()).expect("test content is valid")
    }

    fn start_command(command: u128, text: &str, expected: u64) -> SubmitInput {
        SubmitInput::new(
            command_id(command),
            session_id(1),
            content(text),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: choices(expected, ModelSelectionOverride::UseSessionDefault),
            },
        )
    }

    fn attachment_command(command: u128, digest: BlobDigest) -> SubmitInput {
        SubmitInput::new(
            command_id(command),
            session_id(1),
            UserContent::try_parts(vec![UserContentPart::Attachment {
                digest,
                kind: AttachmentKind::File,
                media_type: DeclaredMediaType::try_new("application/octet-stream".to_owned())
                    .expect("test media type is valid"),
                display_filename: None,
            }])
            .expect("test attachment content is valid"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: choices(1, ModelSelectionOverride::UseSessionDefault),
            },
        )
    }

    fn start_command_with_settings(
        command: u128,
        text: &str,
        expected: u64,
        settings: ModelSettingsOverlay,
    ) -> SubmitInput {
        SubmitInput::new(
            command_id(command),
            session_id(1),
            content(text),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: PerInputConfigurationChoices::with_model_settings(
                    version(expected),
                    ModelSelectionOverride::UseSessionDefault,
                    settings,
                ),
            },
        )
    }

    fn after_command(command: u128, expected_active_turn: crate::TurnId) -> SubmitInput {
        SubmitInput::new(
            command_id(command),
            session_id(1),
            content("hello"),
            DeliveryRequest::AfterCurrentTurn {
                expected_active_turn,
                configuration: choices(1, ModelSelectionOverride::UseSessionDefault),
            },
        )
    }

    fn safe_point_command(command: u128, expected_active_turn: crate::TurnId) -> SubmitInput {
        SubmitInput::new(
            command_id(command),
            session_id(1),
            content("hello"),
            DeliveryRequest::NextSafePoint {
                expected_active_turn,
            },
        )
    }

    fn interrupt_command(command: u128, expected_active_turn: crate::TurnId) -> SubmitInput {
        SubmitInput::new(
            command_id(command),
            session_id(1),
            content("hello"),
            DeliveryRequest::Interrupt {
                expected_active_turn,
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: choices(1, ModelSelectionOverride::UseSessionDefault),
            },
        )
    }

    fn origin_configuration(current: &Session) -> OriginConfiguration {
        let current_version = current.current_configuration_defaults().version();
        let checked = current
            .current_configuration_defaults()
            .derive_request(current_version, ModelSelectionOverride::UseSessionDefault)
            .expect("the test defaults version is current");
        OriginConfiguration::freeze(checked, |_| None)
            .expect("direct test selection does not require an alias")
    }

    fn active_turn(current: &Session) -> AcceptedInputSchedulingProjection {
        active_turn_at_position(current, SessionInputPosition::first())
    }

    fn active_turn_at_position(
        current: &Session,
        position: SessionInputPosition,
    ) -> AcceptedInputSchedulingProjection {
        active_turn_at_position_in_phase(
            current,
            position,
            ActiveTurnSchedulingReconstitutionInput::prepared(turn_id(7), turn_attempt_id(0x51)),
        )
    }

    fn active_turn_at_position_in_phase(
        current: &Session,
        position: SessionInputPosition,
        phase: ActiveTurnSchedulingReconstitutionInput,
    ) -> AcceptedInputSchedulingProjection {
        let origin_entry = semantic_transcript_entry_id(0x31);
        let accepted_input = AcceptedInputLifecycle::new(
            accepted_input_id(0x21),
            AcceptedInputDisposition::OriginOf(turn_id(7)),
        );
        AcceptedInputSchedulingReconstitutionInput::new(
            current.clone(),
            vec![AcceptedInputTurnSchedulingRecord::new(
                current.id(),
                turn_id(7),
                current.id(),
                accepted_input.clone(),
                current.id(),
                turn_id(7),
                AcceptedInputQueueOrder::ordinary(position),
                DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: choices(
                        current.current_configuration_defaults().version().as_u64(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
                origin_configuration(current),
                AcceptedInputTurnSchedulingRecordState::Active {
                    starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                    starting_frontier: context_frontier_id(0x41),
                    phase,
                },
            )],
            vec![SemanticTranscriptEntryReconstitutionInput::new(
                origin_entry,
                current.id(),
                InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                    accepted_input: accepted_input_id(0x21),
                },
            )],
            vec![ResolvedContextFrontierReconstitutionInput::new(
                current.id(),
                context_frontier_id(0x41),
                vec![SemanticTranscriptEntryRef::from_source(
                    current.id(),
                    origin_entry,
                )],
            )],
            Some(SessionAcceptanceTailReconstitutionInput::new(
                current.id(),
                accepted_input.id(),
                position,
                vec![SessionAcceptanceTailEntryReconstitutionInput::new(
                    current.id(),
                    accepted_input,
                    position,
                    DeliveryRequest::StartWhenNoActiveTurn {
                        configuration: choices(
                            current.current_configuration_defaults().version().as_u64(),
                            ModelSelectionOverride::UseSessionDefault,
                        ),
                    },
                )],
            )),
        )
        .reconstitute()
        .expect("test active scheduling facts are complete")
    }

    fn runner_recovery_turn(current: &Session) -> AcceptedInputSchedulingProjection {
        active_turn_at_position_in_phase(
            current,
            SessionInputPosition::first(),
            ActiveTurnSchedulingReconstitutionInput::awaiting_runner_recovery(
                turn_id(7),
                crate::RunnerId::from_uuid(uuid::Uuid::from_u128(0x81)),
                crate::RunnerGeneration::try_from_u64(2)
                    .expect("the fixture placement revision is positive"),
                None,
                None,
            ),
        )
    }

    fn queued_turn(current: &Session) -> AcceptedInputSchedulingProjection {
        AcceptedInputSchedulingReconstitutionInput::new(
            current.clone(),
            vec![AcceptedInputTurnSchedulingRecord::new(
                current.id(),
                turn_id(7),
                current.id(),
                AcceptedInputLifecycle::new(
                    accepted_input_id(0x21),
                    AcceptedInputDisposition::OriginOf(turn_id(7)),
                ),
                current.id(),
                turn_id(7),
                AcceptedInputQueueOrder::ordinary(SessionInputPosition::first()),
                DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: choices(
                        current.current_configuration_defaults().version().as_u64(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
                origin_configuration(current),
                AcceptedInputTurnSchedulingRecordState::Queued,
            )],
            vec![],
            vec![],
            None,
        )
        .reconstitute()
        .expect("test queued scheduling facts are complete")
    }

    fn terminal_source_turn_with_disposition(
        disposition: TurnDisposition,
    ) -> SubmitInputTerminalSourceReconstitutionInput {
        SubmitInputTerminalSourceReconstitutionInput::new(
            SubmitInputTerminalSourceConstructionInput {
                origin: source_turn_origin(),
                turn: turn_id(7),
                disposition,
            },
        )
    }

    fn hash(value: &SubmitInput) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    /// One complete applied projection whose every fact matches the command.
    fn applied_input() -> SubmitInputReconstitutionInput {
        let command = start_command(1, "hello", 1);
        SubmitInputReconstitutionInput::applied_turn_origin(
            SubmitInputAppliedTurnOriginReconstitutionInput {
                command: command.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_accepted_input: accepted_input_id(3),
                result_turn: turn_id(4),
                predecessor_origin: None,
                non_accepted_predecessor: None,
                accepted_command: command_id(1),
                accepted_input: accepted_input_id(3),
                accepted_session: session_id(1),
                accepted_content: content("hello"),
                accepted_delivery: command.delivery(),
                accepted_position: SessionInputPosition::first(),
                accepted_disposition: AcceptedInputDisposition::OriginOf(turn_id(4)),
                queue_session: session_id(1),
                queue_turn: turn_id(4),
                queue_order: crate::AcceptedInputQueueOrder::ordinary(SessionInputPosition::first()),
                defaults_session: session_id(1),
                defaults_version: version(1),
                defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
                stored_requested_model: ModelSelectionRequest::Direct(direct(2)),
                stored_frozen_model: FrozenModelSelection::Direct(direct(2)),
                stored_model_settings: None,
                stored_model_settings_adjustments: Vec::new(),
            },
        )
    }

    fn applied_facts(
        input: &mut SubmitInputReconstitutionInput,
    ) -> &mut super::SubmitInputTurnOriginAppliedReconstitutionFacts {
        let super::SubmitInputReconstitutionFacts::AppliedTurnOrigin(facts) = &mut input.facts
        else {
            panic!("the base reconstitution input is applied");
        };
        facts
    }

    fn terminal_source_facts(
        input: &mut SubmitInputTurnOriginReconstitutionInput,
    ) -> &mut super::SubmitInputTerminalFacts {
        let Some(source_terminal) = &mut turn_origin_facts(input).source_terminal else {
            panic!("the origin must come from reclassified steering");
        };
        source_terminal
    }

    fn turn_origin_facts(
        input: &mut SubmitInputTurnOriginReconstitutionInput,
    ) -> &mut super::SubmitInputTurnOriginReconstitutionFacts {
        input.chain.last_mut().expect("an origin chain is nonempty")
    }

    fn replace_source_origin(
        input: &mut SubmitInputTurnOriginReconstitutionInput,
        mut source: SubmitInputTurnOriginReconstitutionInput,
    ) {
        let current = input.chain.pop().expect("a reclassified origin has a head");
        source.chain.push(current);
        input.chain = source.chain;
    }

    fn append_unchecked_reclassified_origin(
        mut source: SubmitInputTurnOriginReconstitutionInput,
        position_value: u64,
        command_value: u128,
        accepted_input_value: u128,
    ) -> SubmitInputTurnOriginReconstitutionInput {
        let position = SessionInputPosition::try_from_u64(position_value)
            .expect("the test position is positive");
        let source_turn = turn_id(u128::from(position_value) + 5);
        let turn = turn_id(u128::from(position_value) + 6);
        let command = SubmitInput::new(
            command_id(command_value),
            session_id(1),
            content("chained steering"),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: source_turn,
            },
        );
        let accepted_input = accepted_input_id(accepted_input_value);
        source
            .chain
            .push(super::SubmitInputTurnOriginReconstitutionFacts {
                provenance: super::TurnOriginProvenance::Submit(Box::new(
                    ReconstitutedSubmitInput {
                        command,
                        result: SubmitInputResult::Applied(
                            SubmitInputAppliedResult::PendingSteering(
                                super::SubmitInputPendingSteeringAppliedResult {
                                    accepted_input,
                                    session: session_id(1),
                                    acceptance_position: position,
                                    binding: SteeringBinding::new(source_turn),
                                },
                            ),
                        ),
                    },
                )),
                lifecycle: AcceptedInputLifecycle::new(
                    accepted_input,
                    AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                        turn,
                        reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
                    },
                ),
                queue_accepted_input: accepted_input,
                queue_session: session_id(1),
                queue_turn: turn,
                queue_order: AcceptedInputQueueOrder::ordinary(position),
                source_terminal: Some(super::SubmitInputTerminalFacts {
                    turn: source_turn,
                    disposition: TurnDisposition::Completed,
                }),
            });
        source
    }

    fn source_turn_origin() -> SubmitInputTurnOriginReconstitutionInput {
        source_turn_origin_with_identities(0x70, 0x71)
    }

    fn source_turn_origin_with_identities(
        source_command: u128,
        source_accepted_input: u128,
    ) -> SubmitInputTurnOriginReconstitutionInput {
        source_turn_origin_with_position(
            source_command,
            source_accepted_input,
            SessionInputPosition::first(),
        )
    }

    fn source_turn_origin_with_position(
        source_command: u128,
        source_accepted_input: u128,
        position: SessionInputPosition,
    ) -> SubmitInputTurnOriginReconstitutionInput {
        let command = start_command(source_command, "source", 1);
        let receipt = SubmitInputReconstitutionInput::applied_turn_origin(
            SubmitInputAppliedTurnOriginReconstitutionInput {
                command: command.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_accepted_input: accepted_input_id(source_accepted_input),
                result_turn: turn_id(7),
                predecessor_origin: None,
                non_accepted_predecessor: None,
                accepted_command: command_id(source_command),
                accepted_input: accepted_input_id(source_accepted_input),
                accepted_session: session_id(1),
                accepted_content: content("source"),
                accepted_delivery: command.delivery(),
                accepted_position: position,
                accepted_disposition: AcceptedInputDisposition::OriginOf(turn_id(7)),
                queue_session: session_id(1),
                queue_turn: turn_id(7),
                queue_order: AcceptedInputQueueOrder::ordinary(position),
                defaults_session: session_id(1),
                defaults_version: version(1),
                defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
                stored_requested_model: ModelSelectionRequest::Direct(direct(2)),
                stored_frozen_model: FrozenModelSelection::Direct(direct(2)),
                stored_model_settings: None,
                stored_model_settings_adjustments: Vec::new(),
            },
        )
        .reconstitute()
        .expect("the source turn origin facts are complete");
        explicit_turn_origin_input(receipt)
    }

    fn explicit_turn_origin_input(
        receipt: ReconstitutedSubmitInput,
    ) -> SubmitInputTurnOriginReconstitutionInput {
        let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(origin)) =
            receipt.result()
        else {
            panic!("the receipt must be an explicit turn origin");
        };
        let accepted_input = origin.accepted_input();
        let session = origin.session();
        let turn = origin.turn();
        let queue_order = origin.queue_order();
        SubmitInputTurnOriginReconstitutionInput::new(
            SubmitInputDirectTurnOriginConstructionInput {
                receipt,
                lifecycle: AcceptedInputLifecycle::new(
                    accepted_input,
                    AcceptedInputDisposition::OriginOf(turn),
                ),
                queue_accepted_input: accepted_input,
                queue_session: session,
                queue_turn: turn,
                queue_order,
            },
        )
    }

    fn reclassified_turn_origin() -> SubmitInputTurnOriginReconstitutionInput {
        reclassified_turn_origin_with_disposition(TurnDisposition::Failed)
    }

    fn reclassified_turn_origin_with_disposition(
        disposition: TurnDisposition,
    ) -> SubmitInputTurnOriginReconstitutionInput {
        let position = SessionInputPosition::first()
            .checked_next()
            .expect("the pending input follows its source");
        let command = SubmitInput::new(
            command_id(0x72),
            session_id(1),
            content("reclassified steering"),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: turn_id(7),
            },
        );
        let receipt = SubmitInputReconstitutionInput::applied_pending_steering(
            SubmitInputAppliedPendingSteeringReconstitutionInput {
                command: command.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_accepted_input: accepted_input_id(0x73),
                result_source_turn: turn_id(7),
                source_turn_origin: source_turn_origin(),
                accepted_command: command.command_id(),
                accepted_input: accepted_input_id(0x73),
                accepted_session: session_id(1),
                accepted_content: content("reclassified steering"),
                accepted_delivery: command.delivery(),
                accepted_position: position,
            },
        )
        .reconstitute()
        .expect("the pending-steering receipt is canonical");
        let lifecycle = AcceptedInputLifecycle::new(
            accepted_input_id(0x73),
            AcceptedInputDisposition::PendingSteering {
                binding: SteeringBinding::new(turn_id(7)),
            },
        )
        .reclassify_as_turn_origin(
            turn_id(8),
            crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
        )
        .expect("pending steering can become visible origin work");
        SubmitInputTurnOriginReconstitutionInput::reclassified(
            SubmitInputReclassifiedTurnOriginConstructionInput {
                receipt,
                lifecycle,
                queue_accepted_input: accepted_input_id(0x73),
                queue_session: session_id(1),
                queue_turn: turn_id(8),
                queue_order: AcceptedInputQueueOrder::ordinary(position),
                source_terminal: terminal_source_turn_with_disposition(disposition),
            },
        )
    }

    fn after_applied_input() -> SubmitInputReconstitutionInput {
        let command = after_command(1, turn_id(7));
        let position = SessionInputPosition::first()
            .checked_next()
            .expect("after-current acceptance follows its predecessor");
        SubmitInputReconstitutionInput::applied_turn_origin(
            SubmitInputAppliedTurnOriginReconstitutionInput {
                command: command.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_accepted_input: accepted_input_id(3),
                result_turn: turn_id(8),
                predecessor_origin: Some(source_turn_origin()),
                non_accepted_predecessor: None,
                accepted_command: command_id(1),
                accepted_input: accepted_input_id(3),
                accepted_session: session_id(1),
                accepted_content: content("hello"),
                accepted_delivery: command.delivery(),
                accepted_position: position,
                accepted_disposition: AcceptedInputDisposition::OriginOf(turn_id(8)),
                queue_session: session_id(1),
                queue_turn: turn_id(8),
                queue_order: AcceptedInputQueueOrder::ordinary(position),
                defaults_session: session_id(1),
                defaults_version: version(1),
                defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
                stored_requested_model: ModelSelectionRequest::Direct(direct(2)),
                stored_frozen_model: FrozenModelSelection::Direct(direct(2)),
                stored_model_settings: None,
                stored_model_settings_adjustments: Vec::new(),
            },
        )
    }

    fn interrupt_applied_input_with_non_accepted_predecessor(
        predecessor_session: crate::SessionId,
        predecessor_turn: crate::TurnId,
    ) -> SubmitInputReconstitutionInput {
        let mut input = after_applied_input();
        let command = SubmitInput::new(
            command_id(1),
            session_id(1),
            content("hello"),
            DeliveryRequest::Interrupt {
                expected_active_turn: turn_id(7),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: choices(1, ModelSelectionOverride::UseSessionDefault),
            },
        );
        input.command = command.clone();
        let facts = applied_facts(&mut input);
        facts.predecessor_origin = None;
        facts.non_accepted_predecessor = Some(NonAcceptedTurnPredecessorReconstitutionInput {
            session: predecessor_session,
            turn: predecessor_turn,
        });
        facts.accepted_delivery = command.delivery();
        facts.queue_order = AcceptedInputQueueOrder::interrupt_immediately_after(
            facts.accepted_position,
            turn_id(7),
        );
        input
    }

    fn after_applied_input_with_chained_predecessor(
        command_value: u128,
        accepted_input_value: u128,
        result_turn: crate::TurnId,
    ) -> SubmitInputReconstitutionInput {
        let command = after_command(command_value, turn_id(8));
        let position = SessionInputPosition::try_from_u64(3)
            .expect("after-current acceptance follows the complete predecessor chain");
        SubmitInputReconstitutionInput::applied_turn_origin(
            SubmitInputAppliedTurnOriginReconstitutionInput {
                command: command.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_accepted_input: accepted_input_id(accepted_input_value),
                result_turn,
                predecessor_origin: Some(append_unchecked_reclassified_origin(
                    source_turn_origin(),
                    2,
                    0x102,
                    0x202,
                )),
                non_accepted_predecessor: None,
                accepted_command: command_id(command_value),
                accepted_input: accepted_input_id(accepted_input_value),
                accepted_session: session_id(1),
                accepted_content: content("hello"),
                accepted_delivery: command.delivery(),
                accepted_position: position,
                accepted_disposition: AcceptedInputDisposition::OriginOf(result_turn),
                queue_session: session_id(1),
                queue_turn: result_turn,
                queue_order: AcceptedInputQueueOrder::ordinary(position),
                defaults_session: session_id(1),
                defaults_version: version(1),
                defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
                stored_requested_model: ModelSelectionRequest::Direct(direct(2)),
                stored_frozen_model: FrozenModelSelection::Direct(direct(2)),
                stored_model_settings: None,
                stored_model_settings_adjustments: Vec::new(),
            },
        )
    }

    fn pending_steering_input() -> SubmitInputReconstitutionInput {
        let command = safe_point_command(1, turn_id(7));
        SubmitInputReconstitutionInput::applied_pending_steering(
            SubmitInputAppliedPendingSteeringReconstitutionInput {
                command: command.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_accepted_input: accepted_input_id(3),
                result_source_turn: turn_id(7),
                source_turn_origin: source_turn_origin(),
                accepted_command: command_id(1),
                accepted_input: accepted_input_id(3),
                accepted_session: session_id(1),
                accepted_content: content("hello"),
                accepted_delivery: command.delivery(),
                accepted_position: SessionInputPosition::first()
                    .checked_next()
                    .expect("pending steering follows its source origin"),
            },
        )
    }

    fn pending_steering_input_with_chained_source(
        command_value: u128,
        accepted_input_value: u128,
    ) -> SubmitInputReconstitutionInput {
        let command = safe_point_command(command_value, turn_id(8));
        SubmitInputReconstitutionInput::applied_pending_steering(
            SubmitInputAppliedPendingSteeringReconstitutionInput {
                command: command.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_accepted_input: accepted_input_id(accepted_input_value),
                result_source_turn: turn_id(8),
                source_turn_origin: append_unchecked_reclassified_origin(
                    source_turn_origin(),
                    2,
                    0x102,
                    0x202,
                ),
                accepted_command: command_id(command_value),
                accepted_input: accepted_input_id(accepted_input_value),
                accepted_session: session_id(1),
                accepted_content: content("hello"),
                accepted_delivery: command.delivery(),
                accepted_position: SessionInputPosition::try_from_u64(3)
                    .expect("pending steering follows the complete source chain"),
            },
        )
    }

    fn pending_facts(
        input: &mut SubmitInputReconstitutionInput,
    ) -> &mut super::SubmitInputPendingSteeringAppliedReconstitutionFacts {
        let super::SubmitInputReconstitutionFacts::AppliedPendingSteering(facts) = &mut input.facts
        else {
            panic!("the base reconstitution input is pending steering");
        };
        facts
    }

    #[track_caller]
    fn assert_reconstitutes_rejection(
        input: SubmitInputReconstitutionInput,
        expected: SubmitInputRejectedResult,
    ) {
        let reconstructed = input
            .reconstitute()
            .expect("complete rejection facts reconstruct");
        assert_eq!(
            reconstructed.result(),
            &SubmitInputResult::Rejected(expected),
            "replay must return the exact immutable rejection"
        );
    }

    #[track_caller]
    fn assert_rejection_reconstitution_fails(
        input: SubmitInputReconstitutionInput,
        expected: SubmitInputReconstitutionFailure,
    ) {
        assert_eq!(
            input
                .reconstitute()
                .expect_err("cross-wired rejection facts must fail closed")
                .failure(),
            expected
        );
    }

    /// S01: comparison excludes only command identity and
    /// includes the fixed user actor, session, exact content, delivery
    /// discriminator, and every delivery field.
    #[test]
    fn s01_comparison_payload_is_structural() {
        let baseline = start_command(1, "hello", 1);
        let parent_alone_interrupt = SubmitInput::new(
            command_id(1),
            session_id(1),
            content("hello"),
            DeliveryRequest::Interrupt {
                expected_active_turn: turn_id(9),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: choices(1, ModelSelectionOverride::UseSessionDefault),
            },
        );
        let equal_interrupt_replay = SubmitInput::new(
            command_id(1),
            session_id(1),
            content("hello"),
            DeliveryRequest::Interrupt {
                expected_active_turn: turn_id(9),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: choices(1, ModelSelectionOverride::UseSessionDefault),
            },
        );
        let conflicting_interrupt_replay = SubmitInput::new(
            command_id(1),
            session_id(1),
            content("hello"),
            DeliveryRequest::Interrupt {
                expected_active_turn: turn_id(9),
                descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                configuration: choices(1, ModelSelectionOverride::UseSessionDefault),
            },
        );

        assert_eq!(baseline, start_command(2, "hello", 1));
        assert_eq!(hash(&baseline), hash(&start_command(2, "hello", 1)));
        assert_ne!(baseline, start_command(1, "hello ", 1));
        assert_ne!(baseline, start_command(1, "hello", 2));
        assert_ne!(
            baseline,
            SubmitInput::new(
                command_id(1),
                session_id(2),
                content("hello"),
                baseline.delivery(),
            )
        );
        assert_eq!(baseline.actor(), Actor::User);
        assert_ne!(
            baseline,
            SubmitInput::new(
                command_id(1),
                session_id(1),
                content("hello"),
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: turn_id(9),
                },
            )
        );
        assert_eq!(parent_alone_interrupt, equal_interrupt_replay);
        assert_ne!(parent_alone_interrupt, conflicting_interrupt_replay);
    }

    /// S01: start preparation creates exact
    /// queued-origin disposition, ordinary position, and frozen provenance.
    #[test]
    fn s01_start_prepares_complete_queued_work() {
        let command = start_command(1, "hello", 1);
        let prepared = command
            .clone()
            .prepare_when_no_active_turn(
                &session(1, 1, ModelSelectionRequest::Direct(direct(2))),
                accepted_input_id(3),
                Some(turn_id(4)),
                None,
                |_| None,
            )
            .expect("session matches");

        assert_eq!(prepared.command(), &command);
        let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)) =
            prepared.result()
        else {
            panic!("matching start request applies");
        };
        assert_eq!(applied.accepted_input(), accepted_input_id(3));
        assert_eq!(applied.session(), session_id(1));
        assert_eq!(applied.turn(), turn_id(4));
        assert_eq!(
            applied.disposition(),
            AcceptedInputDisposition::OriginOf(turn_id(4))
        );
        assert_eq!(applied.acceptance_position(), SessionInputPosition::first());
        assert_eq!(
            applied.origin_configuration().session_defaults_version(),
            version(1)
        );
        assert_eq!(
            applied.origin_configuration().requested().model(),
            ModelSelectionRequest::Direct(direct(2))
        );
        assert_eq!(
            applied.origin_configuration().effective().model(),
            &FrozenModelSelection::Direct(direct(2))
        );
    }

    /// S37: per-call settings participate in authoritative
    /// origin derivation and remain explicit in the frozen request.
    #[test]
    fn s37_per_call_settings_are_frozen_for_the_origin() {
        let selection = direct(2);
        let per_call = ModelSettingsOverlay::new(
            SettingOverlay::Value(ReasoningLevel::High),
            FastModeOverlay::Inherit,
            SettingOverlay::Inherit,
        );
        let command = start_command_with_settings(1, "settings input", 1, per_call);
        let catalog =
            ModelCapabilityCatalog::try_from_definitions([ModelCapabilityDefinition::new(
                selection,
                ModelCapabilities::new(
                    BTreeSet::from([ReasoningLevel::High]),
                    FastModeSupport::Unsupported,
                    BTreeSet::new(),
                ),
            )])
            .expect("the fixture catalog has one direct selection");

        let prepared = command
            .prepare_when_no_active_turn_with_model_settings(
                &session(1, 1, ModelSelectionRequest::Direct(selection)),
                accepted_input_id(3),
                Some(turn_id(4)),
                None,
                |_| None,
                &catalog,
            )
            .expect("the explicit level is supported");

        let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)) =
            prepared.result()
        else {
            panic!("the supported request applies");
        };
        assert_eq!(
            applied
                .origin_configuration()
                .effective()
                .model_settings()
                .effective()
                .reasoning_level(),
            Some(ReasoningLevel::High)
        );
        assert_eq!(
            applied
                .origin_configuration()
                .requested()
                .per_call_model_settings(),
            per_call
        );
        let event = applied
            .model_settings_event()
            .expect("the frozen settings match the selected direct model");
        assert_eq!(event.per_call_override(), per_call);
        assert_eq!(
            event.settings(),
            applied.origin_configuration().effective().model_settings()
        );
    }

    /// S37: the legacy preparation path fails closed when a caller
    /// supplies settings that require a capability record.
    #[test]
    fn s37_legacy_preparation_rejects_unvalidated_per_call_settings() {
        let selection = direct(2);
        let per_call = ModelSettingsOverlay::new(
            SettingOverlay::Value(ReasoningLevel::High),
            FastModeOverlay::Inherit,
            SettingOverlay::Inherit,
        );
        let command = start_command_with_settings(1, "settings input", 1, per_call);

        let error = command
            .prepare_when_no_active_turn(
                &session(1, 1, ModelSelectionRequest::Direct(selection)),
                accepted_input_id(3),
                Some(turn_id(4)),
                None,
                |_| None,
            )
            .expect_err("the legacy path has no capability record");

        assert_eq!(
            error.failure(),
            SubmitInputPreparationFailure::ModelSettingsResolution(
                OriginModelSettingsError::MissingCapabilities { selection }
            )
        );
    }

    /// S37: catalog-free preparation cannot carry settings
    /// validated for an alias's prior direct target across a retarget.
    #[test]
    fn s37_legacy_preparation_rejects_alias_retarget_settings() {
        let prior_selection = direct(2);
        let installed_selection = direct(3);
        let stored = ModelCapabilities::new(
            BTreeSet::from([ReasoningLevel::High]),
            FastModeSupport::Unsupported,
            BTreeSet::new(),
        )
        .validate_precedence(
            prior_selection,
            ModelSettingsPrecedence::new(
                ModelSettingsOverlay::inherit_all(),
                ModelSettingsOverlay::new(
                    SettingOverlay::Value(ReasoningLevel::High),
                    FastModeOverlay::Inherit,
                    SettingOverlay::Inherit,
                ),
                ModelSettingsOverlay::inherit_all(),
                ModelSettingsOverlay::inherit_all(),
            ),
        )
        .expect("the prior model supports the stored level");
        let defaults = SessionConfigurationDefaults::complete_with_model_settings(
            ModelSelectionRequest::Alias(alias(1)),
            DangerousToolAutoApproval::Disabled,
            None,
            stored,
        )
        .expect("an alias retains its prior validation identity");
        let versioned = crate::VersionedSessionConfigurationDefaults::establish(defaults);
        let checked = versioned
            .derive_request_with_model_settings(
                versioned.version(),
                ModelSelectionOverride::UseSessionDefault,
                ModelSettingsOverlay::inherit_all(),
            )
            .expect("the fixture names the current defaults epoch");

        let error = freeze_origin_configuration(
            checked,
            |requested| {
                assert_eq!(requested, alias(1));
                Some(FrozenAliasDefinition::selecting(installed_selection))
            },
            None,
        )
        .expect_err("alias retargeting requires the new target capability record");

        assert_eq!(
            error,
            OriginModelSettingsError::MissingCapabilities {
                selection: installed_selection,
            }
        );
    }

    /// a legacy origin row cannot omit settings evidence while the
    /// caller contributes an explicit per-call setting.
    #[test]
    fn legacy_reconstitution_rejects_explicit_per_call_settings() {
        let selection = direct(2);
        let per_call = ModelSettingsOverlay::new(
            SettingOverlay::Value(ReasoningLevel::High),
            FastModeOverlay::Inherit,
            SettingOverlay::Inherit,
        );
        let command = start_command_with_settings(1, "settings input", 1, per_call);
        let facts = StoredOriginConfigurationReconstitutionFacts {
            defaults_session: session_id(1),
            defaults_version: version(1),
            defaults: defaults(ModelSelectionRequest::Direct(selection)),
            stored_requested_model: ModelSelectionRequest::Direct(selection),
            stored_frozen_model: FrozenModelSelection::Direct(selection),
            stored_model_settings: None,
            stored_model_settings_adjustments: Vec::new(),
        };

        let result = reconstruct_origin_configuration(&command, facts);

        assert_eq!(
            result.expect_err("explicit settings require stored evidence"),
            SubmitInputReconstitutionFailure::FrozenModelMismatch
        );
    }

    /// a legacy origin row cannot carry defaults settings
    /// validated for an alias's prior direct selection across a retarget.
    #[test]
    fn legacy_reconstitution_rejects_alias_retarget_settings() {
        let requested_alias = alias(1);
        let prior_selection = direct(2);
        let installed_selection = direct(3);
        let stored_settings = ModelCapabilities::new(
            BTreeSet::from([ReasoningLevel::High]),
            FastModeSupport::Unsupported,
            BTreeSet::new(),
        )
        .validate_precedence(
            prior_selection,
            ModelSettingsPrecedence::new(
                ModelSettingsOverlay::inherit_all(),
                ModelSettingsOverlay::new(
                    SettingOverlay::Value(ReasoningLevel::High),
                    FastModeOverlay::Inherit,
                    SettingOverlay::Inherit,
                ),
                ModelSettingsOverlay::inherit_all(),
                ModelSettingsOverlay::inherit_all(),
            ),
        )
        .expect("the prior selection supports the stored defaults");
        let stored_defaults = SessionConfigurationDefaults::complete_with_model_settings(
            ModelSelectionRequest::Alias(requested_alias),
            DangerousToolAutoApproval::Disabled,
            None,
            stored_settings,
        )
        .expect("alias defaults can retain prior validation evidence");
        let command = start_command(1, "settings input", 1);
        let facts = StoredOriginConfigurationReconstitutionFacts {
            defaults_session: session_id(1),
            defaults_version: version(1),
            defaults: stored_defaults,
            stored_requested_model: ModelSelectionRequest::Alias(requested_alias),
            stored_frozen_model: FrozenModelSelection::FrozenAlias {
                alias: requested_alias,
                definition: FrozenAliasDefinition::selecting(installed_selection),
            },
            stored_model_settings: None,
            stored_model_settings_adjustments: Vec::new(),
        };

        let result = reconstruct_origin_configuration(&command, facts);

        assert_eq!(
            result.expect_err("retargeted defaults require stored settings evidence"),
            SubmitInputReconstitutionFailure::FrozenModelMismatch
        );
    }

    /// S01: explicit alias requests freeze the supplied immutable
    /// definition, while a missing definition is a typed recorded rejection.
    #[test]
    fn s01_alias_definition_is_frozen_or_rejected() {
        let command = SubmitInput::new(
            command_id(1),
            session_id(1),
            content("hello"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: choices(
                    1,
                    ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Alias(alias(2))),
                ),
            },
        );
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(3)));
        let frozen = command
            .clone()
            .prepare_when_no_active_turn(
                &current,
                accepted_input_id(4),
                Some(turn_id(5)),
                None,
                |requested| {
                    assert_eq!(requested, alias(2));
                    Some(FrozenAliasDefinition::selecting(direct(6)))
                },
            )
            .expect("session matches");
        let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)) =
            frozen.result()
        else {
            panic!("selectable alias applies");
        };
        assert_eq!(
            applied.origin_configuration().effective().model(),
            &FrozenModelSelection::FrozenAlias {
                alias: alias(2),
                definition: FrozenAliasDefinition::selecting(direct(6)),
            }
        );

        assert!(matches!(
            command
                .prepare_when_no_active_turn(
                    &current,
                    accepted_input_id(4),
                    Some(turn_id(5)),
                    None,
                    |_| None,
                )
                .expect("session matches")
                .result(),
            SubmitInputResult::Rejected(SubmitInputRejectedResult::UnknownModelAlias {
                session,
                alias: rejected_alias,
            }) if *session == session_id(1) && *rejected_alias == alias(2)
        ));
    }

    /// Prepares one active-work command against the canonical vacant-slot
    /// session and asserts the exact recorded rejection; the command and
    /// every expected field stay at the call site.
    #[track_caller]
    fn assert_vacant_slot_records_rejection(
        command: SubmitInput,
        turn_candidate: Option<crate::TurnId>,
        expected: SubmitInputRejectedResult,
    ) {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let prepared = command
            .prepare_when_no_active_turn(
                &current,
                accepted_input_id(3),
                turn_candidate,
                None,
                |_| panic!("active-work rejection does not resolve configuration"),
            )
            .expect("session matches");
        assert_eq!(prepared.result(), &SubmitInputResult::Rejected(expected));
    }

    /// S01: active-work variants record the exact
    /// expected turn in a no-active-turn rejection.
    #[test]
    fn s01_active_modes_reject_when_no_turn_is_active() {
        assert_vacant_slot_records_rejection(
            interrupt_command(1, turn_id(7)),
            Some(turn_id(4)),
            SubmitInputRejectedResult::NoActiveTurn {
                session: session_id(1),
                expected_active_turn: turn_id(7),
            },
        );
        assert_vacant_slot_records_rejection(
            safe_point_command(1, turn_id(7)),
            None,
            SubmitInputRejectedResult::NoActiveTurn {
                session: session_id(1),
                expected_active_turn: turn_id(7),
            },
        );
        assert_vacant_slot_records_rejection(
            after_command(1, turn_id(7)),
            Some(turn_id(4)),
            SubmitInputRejectedResult::NoActiveTurn {
                session: session_id(1),
                expected_active_turn: turn_id(7),
            },
        );

        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let mismatch = safe_point_command(1, turn_id(7))
            .prepare_when_no_active_turn(
                &current,
                accepted_input_id(3),
                Some(turn_id(4)),
                None,
                |_| None,
            )
            .expect_err("safe-point steering initially creates no turn");
        assert_eq!(
            mismatch.failure(),
            SubmitInputPreparationFailure::TurnCandidateMismatch
        );
    }

    /// S09: matching after-current input
    /// creates ordinary queued origin work with the next acceptance position
    /// and exact frozen configuration.
    #[test]
    fn s09_matching_after_current_prepares_ordinary_turn_origin() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let active = active_turn(&current);
        let active_turn = active
            .active_turn()
            .expect("the fixture has one active turn")
            .turn();
        let accepted_input = accepted_input_id(3);
        let turn_candidate = turn_id(8);
        let command = after_command(1, active_turn);
        let prepared = command
            .clone()
            .prepare_with_active_turn(&active, accepted_input, Some(turn_candidate), |_| None)
            .expect("matching after-current input is available");

        let SubmitInputResult::Applied(applied) = prepared.result() else {
            panic!("matching after-current input applies");
        };
        let origin = applied
            .turn_origin()
            .expect("after-current input creates origin work");
        assert_eq!(origin.accepted_input(), accepted_input);
        assert_eq!(origin.turn(), turn_candidate);
        assert_eq!(
            origin.disposition(),
            AcceptedInputDisposition::OriginOf(turn_candidate)
        );
        assert_eq!(origin.acceptance_position().as_u64(), 2);
        assert_eq!(
            origin.queue_order(),
            AcceptedInputQueueOrder::ordinary(origin.acceptance_position())
        );
        assert_eq!(
            origin.origin_configuration().effective().model(),
            &FrozenModelSelection::Direct(direct(2))
        );
    }

    /// S08: matching safe-point input creates
    /// pending steering bound to the exact active turn and carries no
    /// turn-origin fields.
    #[test]
    fn s08_matching_next_safe_point_prepares_pending_steering() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let active = active_turn(&current);
        let active_turn = active
            .active_turn()
            .expect("the fixture has one active turn")
            .turn();
        let accepted_input = accepted_input_id(3);
        let prepared = safe_point_command(1, active_turn)
            .prepare_with_active_turn(&active, accepted_input, None, |_| {
                panic!("safe-point acceptance has no configuration")
            })
            .expect("matching safe-point input is available");

        let SubmitInputResult::Applied(applied) = prepared.result() else {
            panic!("matching safe-point input applies");
        };
        assert_eq!(applied.accepted_input(), accepted_input);
        assert_eq!(applied.acceptance_position().as_u64(), 2);
        assert_eq!(
            applied.disposition(),
            AcceptedInputDisposition::PendingSteering {
                binding: SteeringBinding::new(active_turn),
            }
        );
        assert!(applied.turn_origin().is_none());
        let steering = applied
            .pending_steering()
            .expect("safe-point acceptance creates pending steering");
        assert_eq!(steering.binding().source_turn(), active_turn);
    }

    /// S01: a vacant-slot start submitted while the slot
    /// is occupied records the exact authoritative active turn.
    #[test]
    fn s01_occupied_slot_start_records_active_turn_presence() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let active = active_turn(&current);
        let active_turn = active
            .active_turn()
            .expect("the fixture has one active turn")
            .turn();
        let accepted_input = accepted_input_id(3);
        let turn_candidate = turn_id(8);
        let start = start_command(1, "hello", 1)
            .prepare_with_active_turn(&active, accepted_input, Some(turn_candidate), |_| None)
            .expect("active presence is an authoritative rejection");
        assert!(matches!(
            start.result(),
            SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnPresent {
                session,
                active_turn: recorded_active_turn,
            }) if *session == current.id() && *recorded_active_turn == active_turn
        ));
    }

    #[test]
    fn delegated_active_turn_blocks_vacant_slot_start() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let delegated_turn = turn_id(7);
        let prepared = start_command(1, "hello", 1)
            .prepare_with_delegated_active_turn(
                &current,
                delegated_turn,
                Some(SessionInputPosition::first()),
                None,
                false,
                accepted_input_id(3),
                Some(turn_id(8)),
                |_| None,
            )
            .expect("delegated slot ownership is authoritative");

        let SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnPresent {
            active_turn,
            ..
        }) = prepared.result()
        else {
            panic!("the delegated turn must retain the active slot");
        };
        assert_eq!(*active_turn, delegated_turn);
    }

    #[test]
    fn delegated_active_turn_accepts_safe_point_steering() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let delegated_turn = turn_id(7);
        let prepared = safe_point_command(2, delegated_turn)
            .prepare_with_delegated_active_turn(
                &current,
                delegated_turn,
                Some(SessionInputPosition::first()),
                None,
                false,
                accepted_input_id(3),
                None,
                |_| None,
            )
            .expect("safe-point input binds to the delegated active turn");
        let steering = prepared.result();
        let steering = applied_result(steering)
            .pending_steering()
            .expect("the delegated turn receives pending steering");

        assert_eq!(steering.binding().source_turn(), delegated_turn);
        assert_eq!(
            steering.acceptance_position(),
            SessionInputPosition::first()
                .checked_next()
                .expect("the second position exists")
        );
    }

    #[test]
    fn delegated_active_turn_accepts_correlated_interrupt_successor() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let delegated_turn = turn_id(7);
        let successor = turn_id(8);
        let prepared = interrupt_command(3, delegated_turn)
            .prepare_with_delegated_active_turn(
                &current,
                delegated_turn,
                Some(SessionInputPosition::first()),
                None,
                false,
                accepted_input_id(3),
                Some(successor),
                |_| None,
            )
            .expect("the interrupt correlates to the delegated active turn");
        let origin = prepared.result();
        let origin = applied_result(origin)
            .turn_origin()
            .expect("the interrupt creates an immediate successor");

        assert_eq!(origin.turn(), successor);
        assert_eq!(
            origin.queue_order().priority(),
            AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                predecessor: delegated_turn,
            }
        );
        assert_eq!(
            origin
                .applied_interrupt()
                .expect("the interrupt carries proof")
                .proof()
                .predecessor(),
            delegated_turn
        );
    }

    /// S37: delegation-origin slot ownership cannot bypass the
    /// capability evidence required by an explicit per-call setting.
    #[test]
    fn s37_delegated_successor_rejects_unvalidated_per_call_settings() {
        let selection = direct(2);
        let current = session(1, 1, ModelSelectionRequest::Direct(selection));
        let delegated_turn = turn_id(7);
        let per_call = ModelSettingsOverlay::new(
            SettingOverlay::Value(ReasoningLevel::High),
            FastModeOverlay::Inherit,
            SettingOverlay::Inherit,
        );
        let command = SubmitInput::new(
            command_id(4),
            current.id(),
            content("settings successor"),
            DeliveryRequest::AfterCurrentTurn {
                expected_active_turn: delegated_turn,
                configuration: PerInputConfigurationChoices::with_model_settings(
                    version(1),
                    ModelSelectionOverride::UseSessionDefault,
                    per_call,
                ),
            },
        );

        let error = command
            .prepare_with_delegated_active_turn(
                &current,
                delegated_turn,
                Some(SessionInputPosition::first()),
                None,
                false,
                accepted_input_id(4),
                Some(turn_id(8)),
                |_| None,
            )
            .expect_err("the legacy delegated path has no capability record");

        assert_eq!(
            error.failure(),
            SubmitInputPreparationFailure::ModelSettingsResolution(
                OriginModelSettingsError::MissingCapabilities { selection }
            )
        );
    }

    /// S07 / S08 / S09: every active-work delivery mode
    /// records its stale target against the exact authoritative active turn.
    #[test]
    fn s07_s08_s09_occupied_slot_active_work_records_stale_target() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let active = active_turn(&current);
        let actual_active_turn = active
            .active_turn()
            .expect("the fixture has one active turn")
            .turn();
        let stale_target = turn_id(9);
        let accepted_input = accepted_input_id(3);
        let turn_candidate = turn_id(8);

        let stale_after = after_command(2, stale_target)
            .prepare_with_active_turn(&active, accepted_input, Some(turn_candidate), |_| None)
            .expect("a stale after-current target is an authoritative rejection");
        assert!(matches!(
            stale_after.result(),
            SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnMismatch {
                expected_active_turn,
                actual_active_turn: recorded_active_turn,
                ..
            }) if *expected_active_turn == stale_target
                && *recorded_active_turn == actual_active_turn
        ));

        let stale_safe_point = safe_point_command(3, stale_target)
            .prepare_with_active_turn(&active, accepted_input, None, |_| None)
            .expect("a stale safe-point target is an authoritative rejection");
        assert!(matches!(
            stale_safe_point.result(),
            SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnMismatch {
                expected_active_turn,
                actual_active_turn: recorded_active_turn,
                ..
            }) if *expected_active_turn == stale_target
                && *recorded_active_turn == actual_active_turn
        ));

        let stale_interrupt = interrupt_command(4, stale_target)
            .prepare_with_active_turn(&active, accepted_input, Some(turn_candidate), |_| None)
            .expect("a stale interrupt target is an authoritative rejection");
        assert!(matches!(
            stale_interrupt.result(),
            SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnMismatch {
                expected_active_turn,
                actual_active_turn: recorded_active_turn,
                ..
            }) if *expected_active_turn == stale_target
                && *recorded_active_turn == actual_active_turn
        ));
    }

    /// S07: matching interrupt preparation
    /// creates the exact immediate successor and sole cancellation proof.
    #[test]
    fn s07_occupied_slot_matching_interrupt_applies() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let active = active_turn(&current);
        let active_turn = active
            .active_turn()
            .expect("the fixture has one active turn")
            .turn();
        let accepted_input = accepted_input_id(3);
        let turn_candidate = turn_id(8);
        let interrupt = interrupt_command(6, active_turn);
        let prepared = interrupt
            .clone()
            .prepare_with_active_turn(&active, accepted_input, Some(turn_candidate), |_| None)
            .expect("matching interrupt creates one correlated result");
        let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)) =
            prepared.result()
        else {
            panic!("matching interrupt applies as successor origin");
        };
        let authority = applied
            .applied_interrupt()
            .expect("interrupt origin carries cancellation authority");
        assert_eq!(authority.proof().command(), interrupt.command_id());
        assert_eq!(authority.proof().predecessor(), active_turn);
        assert_eq!(authority.successor(), turn_candidate);
        assert_eq!(
            applied.queue_order().priority(),
            AcceptedInputQueuePriority::InterruptImmediatelyAfter {
                predecessor: active_turn
            }
        );
    }

    /// S07: runner recovery does not invent a new
    /// non-consuming rejection that would foreclose stop-before-abandonment.
    #[test]
    fn s07_runner_recovery_preserves_interrupt_authority() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let active = runner_recovery_turn(&current);
        let interrupted_turn = active
            .active_turn()
            .expect("the fixture has one active turn")
            .turn();
        let successor = turn_id(8);
        let prepared = interrupt_command(6, interrupted_turn)
            .prepare_with_active_turn(&active, accepted_input_id(3), Some(successor), |_| None)
            .expect("runner recovery preserves the existing interrupt algebra");
        let origin = applied_result(prepared.result())
            .turn_origin()
            .expect("the interrupt creates an immediate successor");

        assert_eq!(origin.turn(), successor);
        assert_eq!(
            origin
                .applied_interrupt()
                .expect("the interrupt carries cancellation authority")
                .proof()
                .predecessor(),
            interrupted_turn
        );
    }

    /// The canonical active-slot projection parked on one confirm request:
    /// fixture turn 7 completed its producing call, the yielded tool round
    /// proposes one request, and no decision has resolved the approval wait.
    fn approval_wait_turn(current: &Session) -> AcceptedInputSchedulingProjection {
        let origin_entry = semantic_transcript_entry_id(0x31);
        let tool_use_entry = semantic_transcript_entry_id(0x32);
        let producing_call = model_call_id(0x61);
        let undecided_request = tool_request_id(0x71);
        let starting_frontier = context_frontier_id(0x41);
        let yielded_frontier = context_frontier_id(0x42);
        let request = ToolRequestReconstitutionInput::new(
            undecided_request,
            current.id(),
            turn_id(7),
            producing_call,
            ToolRequestOrdinal::from_u32(0),
            ToolName::try_new(String::from("confirmed")).expect("fixture name is canonical"),
            NormalizedToolArguments::try_from_provider_text(String::from("{}"))
                .expect("fixture arguments are canonical"),
        )
        .into_request();
        let yielded = ResolvedContextFrontierSnapshot::try_from_candidate(
            current.id(),
            yielded_frontier,
            vec![
                SemanticTranscriptEntryRef::from_source(current.id(), origin_entry),
                SemanticTranscriptEntryRef::from_source(current.id(), tool_use_entry),
            ],
        )
        .expect("the tool response extends the starting frontier");
        let batch = ToolBatchReconstitutionInput::new(
            current.id(),
            turn_id(7),
            producing_call,
            yielded,
            vec![request],
            vec![],
            vec![],
            ToolBatchPhaseReconstitutionInput::AwaitingApproval {
                request: undecided_request,
            },
        )
        .reconstitute()
        .expect("the undecided single-request batch awaits its approval");
        let target = ResolvedProviderTarget::naming(provider_model_identity(0x62));
        let accepted_input = AcceptedInputLifecycle::new(
            accepted_input_id(0x21),
            AcceptedInputDisposition::OriginOf(turn_id(7)),
        );
        AcceptedInputSchedulingReconstitutionInput::new(
            current.clone(),
            vec![AcceptedInputTurnSchedulingRecord::new(
                current.id(),
                turn_id(7),
                current.id(),
                accepted_input.clone(),
                current.id(),
                turn_id(7),
                AcceptedInputQueueOrder::ordinary(SessionInputPosition::first()),
                DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: choices(
                        current.current_configuration_defaults().version().as_u64(),
                        ModelSelectionOverride::UseSessionDefault,
                    ),
                },
                origin_configuration(current),
                AcceptedInputTurnSchedulingRecordState::Active {
                    starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                    starting_frontier,
                    phase: ActiveTurnSchedulingReconstitutionInput::awaiting_approval(
                        turn_id(7),
                        &batch,
                    )
                    .expect("the approval wait names the fixture turn"),
                },
            )],
            vec![
                SemanticTranscriptEntryReconstitutionInput::new(
                    origin_entry,
                    current.id(),
                    InitialSemanticTranscriptEntryPayload::OriginAcceptedInput {
                        accepted_input: accepted_input_id(0x21),
                    },
                ),
                SemanticTranscriptEntryReconstitutionInput::new(
                    tool_use_entry,
                    current.id(),
                    InitialSemanticTranscriptEntryPayload::AssistantToolUse {
                        producing_call,
                        request: undecided_request,
                    },
                ),
            ],
            vec![
                ResolvedContextFrontierReconstitutionInput::new(
                    current.id(),
                    starting_frontier,
                    vec![SemanticTranscriptEntryRef::from_source(
                        current.id(),
                        origin_entry,
                    )],
                ),
                ResolvedContextFrontierReconstitutionInput::new(
                    current.id(),
                    yielded_frontier,
                    vec![
                        SemanticTranscriptEntryRef::from_source(current.id(), origin_entry),
                        SemanticTranscriptEntryRef::from_source(current.id(), tool_use_entry),
                    ],
                ),
            ],
            Some(SessionAcceptanceTailReconstitutionInput::new(
                current.id(),
                accepted_input.id(),
                SessionInputPosition::first(),
                vec![SessionAcceptanceTailEntryReconstitutionInput::new(
                    current.id(),
                    accepted_input,
                    SessionInputPosition::first(),
                    DeliveryRequest::StartWhenNoActiveTurn {
                        configuration: choices(
                            current.current_configuration_defaults().version().as_u64(),
                            ModelSelectionOverride::UseSessionDefault,
                        ),
                    },
                )],
            )),
        )
        .with_model_call_facts(
            vec![PinnedProviderTargetReconstitutionInput::new(
                turn_id(7),
                target,
            )],
            vec![ModelCallReconstitutionInput::new(
                producing_call,
                turn_id(7),
                turn_attempt_id(0x52),
                FrozenModelSelection::Direct(direct(2)),
                target,
                starting_frontier,
                ModelCallReconstitutionState::Terminal(ModelCallDisposition::Completed),
            )],
        )
        .reconstitute()
        .expect("the parked approval-wait scheduling facts are complete")
    }

    /// S07 / S10: an interrupt against a parked approval
    /// wait records the typed rejection instead of accepting a successor; the
    /// wait remains parked until its canonical decision command resolves the
    /// approval obligation.
    #[test]
    fn s07_s10_interrupt_against_parked_approval_wait_is_rejected() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let active = approval_wait_turn(&current);
        let parked_turn = active
            .active_turn()
            .expect("the fixture has one active turn");
        assert!(
            matches!(
                parked_turn.active_phase(),
                Some(ActiveTurnPhase::AwaitingApproval { .. })
            ),
            "the fixture slot must be parked on its approval wait"
        );
        let actual_active_turn = parked_turn.turn();
        let accepted_input = accepted_input_id(3);
        let turn_candidate = turn_id(8);

        let rejected = interrupt_command(6, actual_active_turn)
            .prepare_with_active_turn(&active, accepted_input, Some(turn_candidate), |_| None)
            .expect("a parked approval wait is an authoritative rejection");
        assert!(
            matches!(
                rejected.result(),
                SubmitInputResult::Rejected(
                    SubmitInputRejectedResult::InterruptUnavailableWhileAwaitingApproval {
                        session,
                        active_turn,
                    },
                ) if *session == current.id() && *active_turn == actual_active_turn
            ),
            "the interrupt must not bypass the decision command: {:?}",
            rejected.result()
        );
    }

    /// S07 / S10: the recorded parked-approval interrupt
    /// rejection reconstructs exactly.
    #[test]
    fn s07_s10_parked_approval_interrupt_rejection_reconstitutes_exactly() {
        let session = session_id(1);
        let active_turn = turn_id(7);

        assert_reconstitutes_rejection(
            SubmitInputReconstitutionInput::rejected_interrupt_unavailable_while_awaiting_approval(
                SubmitInputRejectedInterruptUnavailableWhileAwaitingApprovalReconstitutionInput {
                    command: interrupt_command(1, active_turn),
                    stored_actor: Actor::User,
                    result_session: session,
                    result_active_turn: active_turn,
                    active_turn_origin: source_turn_origin(),
                },
            ),
            SubmitInputRejectedResult::InterruptUnavailableWhileAwaitingApproval {
                session,
                active_turn,
            },
        );
    }

    /// S07 / S10: parked-approval interrupt rejection
    /// replay fails closed when the command's delivery or expected active turn
    /// is cross-wired against the recorded rejection.
    #[test]
    fn s07_s10_parked_approval_interrupt_rejection_evidence_is_exact() {
        let session = session_id(1);
        let active_turn = turn_id(7);
        let other_turn = turn_id(9);

        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_interrupt_unavailable_while_awaiting_approval(
                SubmitInputRejectedInterruptUnavailableWhileAwaitingApprovalReconstitutionInput {
                    command: interrupt_command(1, other_turn),
                    stored_actor: Actor::User,
                    result_session: session,
                    result_active_turn: active_turn,
                    active_turn_origin: source_turn_origin(),
                },
            ),
            SubmitInputReconstitutionFailure::StoppingRejectionMismatch,
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_interrupt_unavailable_while_awaiting_approval(
                SubmitInputRejectedInterruptUnavailableWhileAwaitingApprovalReconstitutionInput {
                    command: safe_point_command(1, active_turn),
                    stored_actor: Actor::User,
                    result_session: session,
                    result_active_turn: active_turn,
                    active_turn_origin: source_turn_origin(),
                },
            ),
            SubmitInputReconstitutionFailure::StoppingRejectionMismatch,
        );
    }

    /// S09: after-current preparation records
    /// the exact stale session-defaults version.
    #[test]
    fn s09_occupied_slot_after_current_records_stale_defaults_version() {
        let stale_session = session(1, 2, ModelSelectionRequest::Direct(direct(2)));
        let active = active_turn(&stale_session);
        let active_turn = active
            .active_turn()
            .expect("the fixture has one active turn")
            .turn();
        let accepted_input = accepted_input_id(3);
        let turn_candidate = turn_id(8);
        let stale = after_command(1, active_turn)
            .prepare_with_active_turn(&active, accepted_input, Some(turn_candidate), |_| {
                panic!("stale defaults cannot reach alias resolution")
            })
            .expect("a stale defaults version is an authoritative rejection");
        assert!(matches!(
            stale.result(),
            SubmitInputResult::Rejected(
                SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                    expected,
                    current,
                    ..
                }
            ) if *expected == version(1) && *current == version(2)
        ));
    }

    /// S09: after-current preparation records the exact
    /// unresolved model alias.
    #[test]
    fn s09_occupied_slot_after_current_records_unknown_alias() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let active = active_turn(&current);
        let active_turn = active
            .active_turn()
            .expect("the fixture has one active turn")
            .turn();
        let accepted_input = accepted_input_id(3);
        let turn_candidate = turn_id(8);
        let unknown_alias = alias(9);
        let alias_command = SubmitInput::new(
            command_id(2),
            session_id(1),
            content("hello"),
            DeliveryRequest::AfterCurrentTurn {
                expected_active_turn: active_turn,
                configuration: choices(
                    1,
                    ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Alias(
                        unknown_alias,
                    )),
                ),
            },
        );
        let rejected = alias_command
            .prepare_with_active_turn(&active, accepted_input, Some(turn_candidate), |_| None)
            .expect("an unresolved alias is an authoritative rejection");
        assert!(matches!(
            rejected.result(),
            SubmitInputResult::Rejected(SubmitInputRejectedResult::UnknownModelAlias {
                alias: unknown,
                ..
            }) if *unknown == unknown_alias
        ));
    }

    /// S08 / S09: both occupied-slot acceptance paths
    /// record exhaustion of the validated session acceptance tail.
    #[test]
    fn s08_s09_occupied_slot_acceptance_records_position_exhaustion() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let maximum = SessionInputPosition::try_from_u64(u64::MAX).expect("positive maximum");
        let active = active_turn_at_position(&current, maximum);
        let active_turn = active
            .active_turn()
            .expect("the fixture has one active turn")
            .turn();
        let accepted_input = accepted_input_id(3);
        let turn_candidate = turn_id(8);

        let after = after_command(3, active_turn)
            .prepare_with_active_turn(&active, accepted_input, Some(turn_candidate), |_| None)
            .expect("after-current position exhaustion is authoritative");
        assert!(matches!(
            after.result(),
            SubmitInputResult::Rejected(
                SubmitInputRejectedResult::AcceptancePositionExhausted { last, .. }
            ) if *last == maximum
        ));

        let safe_point = safe_point_command(4, active_turn)
            .prepare_with_active_turn(&active, accepted_input, None, |_| None)
            .expect("safe-point position exhaustion is authoritative");
        assert!(matches!(
            safe_point.result(),
            SubmitInputResult::Rejected(
                SubmitInputRejectedResult::AcceptancePositionExhausted { last, .. }
            ) if *last == maximum
        ));
    }

    /// S09: occupied-slot preparation rejects a scheduling
    /// projection from another session without claiming the command.
    #[test]
    fn s09_occupied_slot_preparation_rejects_cross_session_projection() {
        let wrong_session = session(2, 1, ModelSelectionRequest::Direct(direct(2)));
        let wrong_projection = active_turn(&wrong_session);
        let projected_active_turn = wrong_projection
            .active_turn()
            .expect("the fixture has one active turn")
            .turn();
        let accepted_input = accepted_input_id(3);
        let turn_candidate = turn_id(8);
        let command = after_command(1, projected_active_turn);
        let wrong_active_session = command
            .clone()
            .prepare_with_active_turn(
                &wrong_projection,
                accepted_input,
                Some(turn_candidate),
                |_| None,
            )
            .expect_err("a cross-session active projection is nonterminal");
        assert_eq!(
            wrong_active_session.failure(),
            SubmitInputPreparationFailure::SessionMismatch {
                provided_session: wrong_session.id(),
            }
        );
        assert_eq!(wrong_active_session.command(), &command);
    }

    /// S09: a queued projection cannot stand in for the
    /// authoritative active turn.
    #[test]
    fn s09_occupied_slot_preparation_requires_active_projection() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let queued = queued_turn(&current);
        let projected_turn = queued
            .turns()
            .next()
            .expect("the fixture has one queued turn")
            .turn();
        let accepted_input = accepted_input_id(3);
        let turn_candidate = turn_id(8);
        let command = after_command(1, projected_turn);
        let not_active = command
            .clone()
            .prepare_with_active_turn(&queued, accepted_input, Some(turn_candidate), |_| None)
            .expect_err("a queued projection cannot stand in for the active turn");
        assert_eq!(
            not_active.failure(),
            SubmitInputPreparationFailure::ActiveTurnProjectionMissing
        );
        assert_eq!(not_active.command(), &command);
    }

    /// S08 / S09: each occupied-slot delivery mode requires the
    /// exact candidate shape it can apply.
    #[test]
    fn s08_s09_occupied_slot_preparation_rejects_mismatched_turn_candidate_shape() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let active = active_turn(&current);
        let active_turn = active
            .active_turn()
            .expect("the fixture has one active turn")
            .turn();
        let accepted_input = accepted_input_id(3);
        let turn_candidate = turn_id(8);

        let missing_turn = after_command(1, active_turn)
            .prepare_with_active_turn(&active, accepted_input, None, |_| None)
            .expect_err("after-current input requires a minted turn candidate");
        assert_eq!(
            missing_turn.failure(),
            SubmitInputPreparationFailure::TurnCandidateMismatch
        );

        let reused_active_turn = after_command(2, active_turn)
            .prepare_with_active_turn(&active, accepted_input, Some(active_turn), |_| None)
            .expect_err("after-current work cannot reuse its active predecessor");
        assert_eq!(
            reused_active_turn.failure(),
            SubmitInputPreparationFailure::TurnCandidateMismatch
        );

        let extra_turn = safe_point_command(3, active_turn)
            .prepare_with_active_turn(&active, accepted_input, Some(turn_candidate), |_| None)
            .expect_err("safe-point input cannot receive a turn candidate");
        assert_eq!(
            extra_turn.failure(),
            SubmitInputPreparationFailure::TurnCandidateMismatch
        );
    }

    /// S08 / S09: no occupied-slot acceptance path can
    /// reuse the active turn's canonical origin identity.
    #[test]
    fn s08_s09_occupied_slot_preparation_rejects_active_origin_identity_reuse() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let active = active_turn(&current);
        let active_turn = active
            .active_turn()
            .expect("the test projection has one active turn")
            .turn();
        let active_origin = active
            .turn(active_turn)
            .expect("the fixture retains its active turn")
            .accepted_input()
            .id();
        let turn_candidate = turn_id(8);

        let after = after_command(2, active_turn)
            .prepare_with_active_turn(&active, active_origin, Some(turn_candidate), |_| None)
            .expect_err("after-current acceptance cannot reuse the active origin");
        assert_eq!(
            after.failure(),
            SubmitInputPreparationFailure::AcceptedInputCandidateReusesActiveOrigin {
                active_turn,
                accepted_input: active_origin,
            }
        );

        let safe_point = safe_point_command(3, active_turn)
            .prepare_with_active_turn(&active, active_origin, None, |_| None)
            .expect_err("safe-point acceptance cannot reuse the active origin");
        assert_eq!(
            safe_point.failure(),
            SubmitInputPreparationFailure::AcceptedInputCandidateReusesActiveOrigin {
                active_turn,
                accepted_input: active_origin,
            }
        );
    }

    /// S01: missing sessions, stale defaults, unknown
    /// aliases, and exhausted positions remain distinct terminal results.
    #[test]
    fn s01_authoritative_rejections_are_typed() {
        let command = start_command(1, "hello", 1);
        assert!(matches!(
            command.clone().prepare_session_not_found().result(),
            SubmitInputResult::Rejected(SubmitInputRejectedResult::SessionNotFound { .. })
        ));
        assert!(matches!(
            command
                .clone()
                .prepare_when_no_active_turn(
                    &session(1, 2, ModelSelectionRequest::Direct(direct(2))),
                    accepted_input_id(3),
                    Some(turn_id(4)),
                    None,
                    |_| None,
                )
                .expect("session matches")
                .result(),
            SubmitInputResult::Rejected(
                SubmitInputRejectedResult::SessionDefaultsVersionMismatch { .. }
            )
        ));
        let maximum = SessionInputPosition::try_from_u64(u64::MAX).expect("positive maximum");
        assert!(matches!(
            command
                .prepare_when_no_active_turn(
                    &session(1, 1, ModelSelectionRequest::Direct(direct(2))),
                    accepted_input_id(3),
                    Some(turn_id(4)),
                    Some(maximum),
                    |_| None,
                )
                .expect("session matches")
                .result(),
            SubmitInputResult::Rejected(
                SubmitInputRejectedResult::AcceptancePositionExhausted { last, .. }
            ) if *last == maximum
        ));
    }

    #[test]
    fn attachment_authority_rejections_reconstitute_exact_evidence() {
        let digest = BlobDigest::from_bytes([0x5a; 32]);
        let command = attachment_command(0x51, digest);

        let missing = SubmitInputReconstitutionInput::rejected_attachment_blob_not_found(
            SubmitInputRejectedAttachmentBlobNotFoundReconstitutionInput {
                command: command.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_digest: digest,
            },
        )
        .reconstitute()
        .expect("matching missing-blob evidence reconstructs");
        assert!(matches!(
            missing.result(),
            SubmitInputResult::Rejected(
                SubmitInputRejectedResult::AttachmentBlobNotFound { digest: stored }
            ) if *stored == digest
        ));

        let budget = SubmitInputReconstitutionInput::rejected_attachment_byte_budget_exceeded(
            SubmitInputRejectedAttachmentByteBudgetExceededReconstitutionInput {
                command: command.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_maximum_bytes: 4096,
            },
        )
        .reconstitute()
        .expect("matching byte-budget evidence reconstructs");
        assert!(matches!(
            budget.result(),
            SubmitInputResult::Rejected(SubmitInputRejectedResult::AttachmentByteBudgetExceeded {
                maximum_bytes: 4096
            })
        ));

        let text_budget = SubmitInputReconstitutionInput::rejected_attachment_byte_budget_exceeded(
            SubmitInputRejectedAttachmentByteBudgetExceededReconstitutionInput {
                command: start_command(0x52, "text-only frontier input", 1),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_maximum_bytes: 4096,
            },
        )
        .reconstitute()
        .expect("frontier-driven byte-budget evidence reconstructs for text input");
        assert!(matches!(
            text_budget.result(),
            SubmitInputResult::Rejected(SubmitInputRejectedResult::AttachmentByteBudgetExceeded {
                maximum_bytes: 4096
            })
        ));

        let mismatch = SubmitInputReconstitutionInput::rejected_attachment_blob_not_found(
            SubmitInputRejectedAttachmentBlobNotFoundReconstitutionInput {
                command,
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_digest: BlobDigest::from_bytes([0x6b; 32]),
            },
        )
        .reconstitute()
        .expect_err("a digest absent from the command fails closed");
        assert_eq!(
            mismatch.failure(),
            SubmitInputReconstitutionFailure::AttachmentDigestMismatch
        );
    }

    /// complete applied facts reconstruct the canonical
    /// result, while a cross-wired content fact fails closed.
    #[test]
    fn applied_reconstitution_checks_complete_correlations() {
        let reconstructed = applied_input()
            .reconstitute()
            .expect("complete matching facts reconstruct");
        let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(applied)) =
            reconstructed.result()
        else {
            panic!("applied facts reconstruct an applied result");
        };
        assert_eq!(applied.turn(), turn_id(4));

        let mut wrong = applied_input();
        applied_facts(&mut wrong).accepted_content = content("different");
        assert_eq!(
            wrong
                .reconstitute()
                .expect_err("cross-wired content fails closed")
                .failure(),
            SubmitInputReconstitutionFailure::AcceptedContentMismatch
        );
    }

    /// S08 / S09: both occupied applied
    /// shapes reconstruct only from exact treatment and source correlations.
    #[test]
    fn occupied_applied_shapes_reconstitute_exactly() {
        let after = after_applied_input()
            .reconstitute()
            .expect("complete after-current origin facts reconstruct");
        let SubmitInputResult::Applied(after) = after.result() else {
            panic!("after-current facts remain applied");
        };
        let after = after
            .turn_origin()
            .expect("after-current facts create turn-origin work");
        assert_eq!(after.turn(), turn_id(8));
        assert_eq!(
            after.origin_configuration().effective().model(),
            &FrozenModelSelection::Direct(direct(2))
        );

        let pending = pending_steering_input()
            .reconstitute()
            .expect("complete pending-steering facts reconstruct");
        let SubmitInputResult::Applied(pending) = pending.result() else {
            panic!("safe-point facts remain applied");
        };
        assert_eq!(
            pending.disposition(),
            AcceptedInputDisposition::PendingSteering {
                binding: SteeringBinding::new(turn_id(7)),
            }
        );
        assert!(pending.turn_origin().is_none());
    }

    /// Asserts the advanced lifecycle has left pending steering behind while
    /// replay of the canonical receipt still reconstructs pending steering.
    #[track_caller]
    fn assert_replay_survives_lifecycle_progress(advanced: &AcceptedInputLifecycle) {
        assert!(
            !matches!(
                advanced.disposition(),
                AcceptedInputDisposition::PendingSteering { .. }
            ),
            "the lifecycle under test must have progressed past pending steering"
        );
        let replayed = pending_steering_input()
            .reconstitute()
            .expect("mutable lifecycle progress cannot rewrite the receipt");
        assert!(matches!(
            replayed.result(),
            SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(_))
        ));
    }

    /// S08: replay reconstructs the immutable original
    /// pending-steering receipt independently of its mutable lifecycle.
    #[test]
    fn pending_steering_replay_survives_lifecycle_progress() {
        let initial = AcceptedInputLifecycle::new(
            accepted_input_id(3),
            AcceptedInputDisposition::PendingSteering {
                binding: SteeringBinding::new(turn_id(7)),
            },
        );

        let consumed = initial
            .clone()
            .consume_as_steering(crate::test_support::model_call_id(0x81))
            .expect("pending steering can be consumed");
        assert_replay_survives_lifecycle_progress(&consumed);

        let reclassified = initial
            .reclassify_as_turn_origin(
                turn_id(8),
                crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
            )
            .expect("pending steering can be reclassified");
        assert_replay_survives_lifecycle_progress(&reclassified);
    }

    fn applied_result(result: &SubmitInputResult) -> &SubmitInputAppliedResult {
        match result {
            SubmitInputResult::Applied(applied) => applied,
            SubmitInputResult::Rejected(rejected) => {
                panic!("expected an applied result, got {rejected:?}")
            }
        }
    }

    /// S08 / S09: a canonical turn origin can come from
    /// either an original turn-producing receipt or a later visible
    /// reclassification of immutable pending steering.
    #[test]
    fn s08_s09_reclassified_turn_origins_support_replay() {
        let predecessor_position = SessionInputPosition::first()
            .checked_next()
            .expect("the reclassified origin follows its source");
        let accepted_position = predecessor_position
            .checked_next()
            .expect("later input follows the reclassified origin");

        let after_command = after_command(0x80, turn_id(8));
        let after = SubmitInputReconstitutionInput::applied_turn_origin(
            SubmitInputAppliedTurnOriginReconstitutionInput {
                command: after_command.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_accepted_input: accepted_input_id(0x81),
                result_turn: turn_id(9),
                predecessor_origin: Some(reclassified_turn_origin()),
                non_accepted_predecessor: None,
                accepted_command: after_command.command_id(),
                accepted_input: accepted_input_id(0x81),
                accepted_session: session_id(1),
                accepted_content: content("hello"),
                accepted_delivery: after_command.delivery(),
                accepted_position,
                accepted_disposition: AcceptedInputDisposition::OriginOf(turn_id(9)),
                queue_session: session_id(1),
                queue_turn: turn_id(9),
                queue_order: AcceptedInputQueueOrder::ordinary(accepted_position),
                defaults_session: session_id(1),
                defaults_version: version(1),
                defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
                stored_requested_model: ModelSelectionRequest::Direct(direct(2)),
                stored_frozen_model: FrozenModelSelection::Direct(direct(2)),
                stored_model_settings: None,
                stored_model_settings_adjustments: Vec::new(),
            },
        )
        .reconstitute()
        .expect("after-current replay accepts a reclassified predecessor");
        assert_eq!(
            applied_result(after.result())
                .turn_origin()
                .expect("the after-current result creates a turn")
                .turn(),
            turn_id(9)
        );

        let steering_command = SubmitInput::new(
            command_id(0x82),
            session_id(1),
            content("later steering"),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: turn_id(8),
            },
        );
        let steering = SubmitInputReconstitutionInput::applied_pending_steering(
            SubmitInputAppliedPendingSteeringReconstitutionInput {
                command: steering_command.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_accepted_input: accepted_input_id(0x83),
                result_source_turn: turn_id(8),
                source_turn_origin: reclassified_turn_origin(),
                accepted_command: steering_command.command_id(),
                accepted_input: accepted_input_id(0x83),
                accepted_session: session_id(1),
                accepted_content: content("later steering"),
                accepted_delivery: steering_command.delivery(),
                accepted_position,
            },
        )
        .reconstitute()
        .expect("pending-steering replay accepts a reclassified source");
        assert!(
            applied_result(steering.result())
                .pending_steering()
                .is_some()
        );

        let rejection = SubmitInputReconstitutionInput::rejected_active_turn_present(
            SubmitInputRejectedActiveTurnPresentReconstitutionInput {
                command: start_command(0x84, "rejected start", 1),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_active_turn: turn_id(8),
                active_turn_origin: reclassified_turn_origin(),
            },
        )
        .reconstitute()
        .expect("rejection replay accepts a reclassified active origin");
        assert_eq!(
            rejection.result(),
            &SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnPresent {
                session: session_id(1),
                active_turn: turn_id(8),
            })
        );
    }

    /// S08: model rendering recovers the final accepted
    /// input's exact user content from a fully checked reclassification chain.
    #[test]
    fn s08_reclassified_origin_preserves_renderable_user_content() {
        let origin = reclassified_turn_origin();
        let content = crate::ModelCallOriginContent::from_reconstituted_turn_origin(&origin)
            .expect("the canonical reclassified origin has exact accepted content");

        assert_eq!(content.accepted_input(), accepted_input_id(0x73));
        assert_eq!(
            content
                .content()
                .single_text()
                .expect("the fixture has exactly one text part")
                .as_str(),
            "reclassified steering"
        );
    }

    /// Replays a rejection whose reclassified origin's source turn ended with
    /// the given terminal disposition and asserts replay authenticates it.
    #[track_caller]
    fn assert_terminal_source_authenticates_reclassification(disposition: TurnDisposition) {
        SubmitInputReconstitutionInput::rejected_active_turn_present(
            SubmitInputRejectedActiveTurnPresentReconstitutionInput {
                command: start_command(0x84, "rejected start", 1),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_active_turn: turn_id(8),
                active_turn_origin: reclassified_turn_origin_with_disposition(disposition),
            },
        )
        .reconstitute()
        .expect("every terminal source disposition authenticates reclassification");
    }

    /// S08: reclassification replay admits every
    /// terminal disposition and recursively validates a source turn that was
    /// itself created by steering reclassification.
    #[test]
    fn s08_reclassification_accepts_all_terminal_sources_and_chains() {
        assert_terminal_source_authenticates_reclassification(TurnDisposition::Completed);
        assert_terminal_source_authenticates_reclassification(TurnDisposition::Refused);
        assert_terminal_source_authenticates_reclassification(TurnDisposition::Failed);
        assert_terminal_source_authenticates_reclassification(TurnDisposition::Cancelled {
            cause: test_applied_interrupt_proof(command_id(0x90), turn_id(7)),
        });
        assert_terminal_source_authenticates_reclassification(
            TurnDisposition::ReconciliationRequired {
                marker: test_reconciliation_marker(
                    NonEmptyIssuedOperationRefs::try_from_operations([
                        IssuedOperationRef::ModelCall(model_call_id(0x91)),
                    ])
                    .expect("the test ambiguity set is nonempty"),
                    ReconciliationReason::InterruptRequiresReconciliation {
                        interrupt: test_applied_interrupt_proof(command_id(0x92), turn_id(7)),
                    },
                ),
            },
        );

        let source_origin = reclassified_turn_origin_with_disposition(TurnDisposition::Completed);
        let position = SessionInputPosition::first()
            .checked_next()
            .and_then(SessionInputPosition::checked_next)
            .expect("the chained steering follows its reclassified source");
        let command = SubmitInput::new(
            command_id(0x74),
            session_id(1),
            content("second reclassified steering"),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: turn_id(8),
            },
        );
        let receipt = SubmitInputReconstitutionInput::applied_pending_steering(
            SubmitInputAppliedPendingSteeringReconstitutionInput {
                command: command.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_accepted_input: accepted_input_id(0x75),
                result_source_turn: turn_id(8),
                source_turn_origin: source_origin.clone(),
                accepted_command: command.command_id(),
                accepted_input: accepted_input_id(0x75),
                accepted_session: session_id(1),
                accepted_content: content("second reclassified steering"),
                accepted_delivery: command.delivery(),
                accepted_position: position,
            },
        )
        .reconstitute()
        .expect("the second pending-steering receipt has a canonical reclassified source");
        let lifecycle = AcceptedInputLifecycle::new(
            accepted_input_id(0x75),
            AcceptedInputDisposition::PendingSteering {
                binding: SteeringBinding::new(turn_id(8)),
            },
        )
        .reclassify_as_turn_origin(
            turn_id(9),
            crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
        )
        .expect("the second pending steering can be reclassified");
        let chained_origin = SubmitInputTurnOriginReconstitutionInput::reclassified(
            SubmitInputReclassifiedTurnOriginConstructionInput {
                receipt,
                lifecycle,
                queue_accepted_input: accepted_input_id(0x75),
                queue_session: session_id(1),
                queue_turn: turn_id(9),
                queue_order: AcceptedInputQueueOrder::ordinary(position),
                source_terminal: SubmitInputTerminalSourceReconstitutionInput::new(
                    SubmitInputTerminalSourceConstructionInput {
                        origin: source_origin,
                        turn: turn_id(8),
                        disposition: TurnDisposition::Refused,
                    },
                ),
            },
        );

        SubmitInputReconstitutionInput::rejected_active_turn_present(
            SubmitInputRejectedActiveTurnPresentReconstitutionInput {
                command: start_command(0x85, "second rejected start", 1),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_active_turn: turn_id(9),
                active_turn_origin: chained_origin,
            },
        )
        .reconstitute()
        .expect("a terminal reclassified source authenticates the next reclassified origin");
    }

    /// Replays a rejection carrying the given cross-wired reclassified origin
    /// and asserts the replay fails closed with the origin-mismatch failure.
    #[track_caller]
    fn assert_cross_wired_reclassified_origin_fails_closed(
        origin: SubmitInputTurnOriginReconstitutionInput,
    ) {
        assert_eq!(
            SubmitInputReconstitutionInput::rejected_active_turn_present(
                SubmitInputRejectedActiveTurnPresentReconstitutionInput {
                    command: start_command(0x84, "rejected start", 1),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_active_turn: turn_id(8),
                    active_turn_origin: origin,
                }
            )
            .reconstitute()
            .expect_err("cross-wired reclassified origin facts fail closed")
            .failure(),
            SubmitInputReconstitutionFailure::RejectionActiveTurnOriginMismatch
        );
    }

    /// S08: a pending receipt becomes canonical origin
    /// evidence only with its exact reclassified lifecycle, queue facts, and
    /// earlier distinct terminal source origin.
    #[test]
    fn s08_reclassified_turn_origin_rejects_cross_wired_facts() {
        let mut wrong_lifecycle = reclassified_turn_origin();
        turn_origin_facts(&mut wrong_lifecycle).lifecycle = AcceptedInputLifecycle::new(
            accepted_input_id(0x73),
            AcceptedInputDisposition::OriginOf(turn_id(8)),
        );
        assert_cross_wired_reclassified_origin_fails_closed(wrong_lifecycle);

        let mut wrong_input = reclassified_turn_origin();
        turn_origin_facts(&mut wrong_input).lifecycle = AcceptedInputLifecycle::new(
            accepted_input_id(0x74),
            AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                turn: turn_id(8),
                reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
            },
        );
        assert_cross_wired_reclassified_origin_fails_closed(wrong_input);

        let mut wrong_queue_input = reclassified_turn_origin();
        turn_origin_facts(&mut wrong_queue_input).queue_accepted_input = accepted_input_id(0x74);
        assert_cross_wired_reclassified_origin_fails_closed(wrong_queue_input);

        let mut wrong_turn = reclassified_turn_origin();
        turn_origin_facts(&mut wrong_turn).queue_turn = turn_id(9);
        assert_cross_wired_reclassified_origin_fails_closed(wrong_turn);

        let mut source_turn_reuse = reclassified_turn_origin();
        turn_origin_facts(&mut source_turn_reuse).lifecycle = AcceptedInputLifecycle::new(
            accepted_input_id(0x73),
            AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                turn: turn_id(7),
                reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
            },
        );
        turn_origin_facts(&mut source_turn_reuse).queue_turn = turn_id(7);
        assert_cross_wired_reclassified_origin_fails_closed(source_turn_reuse);

        let mut wrong_terminal_owner = reclassified_turn_origin();
        let terminal = terminal_source_facts(&mut wrong_terminal_owner);
        terminal.turn = turn_id(9);
        terminal.disposition = TurnDisposition::Completed;
        assert_cross_wired_reclassified_origin_fails_closed(wrong_terminal_owner);

        let mut wrong_terminal_proof = reclassified_turn_origin();
        terminal_source_facts(&mut wrong_terminal_proof).disposition = TurnDisposition::Cancelled {
            cause: test_applied_interrupt_proof(command_id(0x90), turn_id(9)),
        };
        assert_cross_wired_reclassified_origin_fails_closed(wrong_terminal_proof);

        let mut reused_source_command = reclassified_turn_origin();
        replace_source_origin(
            &mut reused_source_command,
            source_turn_origin_with_identities(0x72, 0x71),
        );
        assert_cross_wired_reclassified_origin_fails_closed(reused_source_command);

        let steering_position = SessionInputPosition::first()
            .checked_next()
            .expect("the steering follows its real source");
        let mut late_source = reclassified_turn_origin();
        replace_source_origin(
            &mut late_source,
            source_turn_origin_with_position(0x70, 0x71, steering_position),
        );
        assert_cross_wired_reclassified_origin_fails_closed(late_source);

        let mut wrong_order = reclassified_turn_origin();
        turn_origin_facts(&mut wrong_order).queue_order =
            AcceptedInputQueueOrder::ordinary(SessionInputPosition::first());
        assert_cross_wired_reclassified_origin_fails_closed(wrong_order);
    }

    /// A coherent reclassification chain grown from the canonical source
    /// origin to the given final acceptance position; command and
    /// accepted-input seeds derive from each position, decorrelated, and the
    /// head turn's seed is its position plus six, the derivation
    /// `append_unchecked_reclassified_origin` states.
    fn reclassified_origin_chain_ending_at(
        final_position: u64,
    ) -> SubmitInputTurnOriginReconstitutionInput {
        let mut origin = source_turn_origin();
        for position in 2..=final_position {
            origin = append_unchecked_reclassified_origin(
                origin,
                position,
                0x10_000 + u128::from(position),
                0x20_000 + u128::from(position),
            );
        }
        origin
    }

    /// S08: validation remains bounded by heap-backed
    /// input size rather than call-stack depth.
    #[test]
    fn s08_reclassified_origin_validation_is_iterative() {
        let origin = reclassified_origin_chain_ending_at(16_384);

        let validated = super::validate_turn_origin_reconstitution_input(&origin)
            .expect("a long coherent origin chain validates without recursion");
        assert_eq!(validated.turn, turn_id(16_390));
    }

    /// S08: command, accepted-input, and turn identities
    /// remain unique across the complete reclassification chain, not only
    /// adjacent source/origin pairs.
    #[test]
    fn s08_reclassified_origin_rejects_ancestor_identity_reuse() {
        let command_reuse = append_unchecked_reclassified_origin(
            append_unchecked_reclassified_origin(source_turn_origin(), 2, 0x102, 0x202),
            3,
            0x70,
            0x203,
        );
        assert!(super::validate_turn_origin_reconstitution_input(&command_reuse).is_none());

        let accepted_input_reuse = append_unchecked_reclassified_origin(
            append_unchecked_reclassified_origin(source_turn_origin(), 2, 0x102, 0x202),
            3,
            0x103,
            0x71,
        );
        assert!(super::validate_turn_origin_reconstitution_input(&accepted_input_reuse).is_none());

        let mut turn_reuse = append_unchecked_reclassified_origin(
            append_unchecked_reclassified_origin(source_turn_origin(), 2, 0x102, 0x202),
            3,
            0x103,
            0x203,
        );
        let facts = turn_origin_facts(&mut turn_reuse);
        facts.lifecycle = AcceptedInputLifecycle::new(
            facts.lifecycle.id(),
            AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                turn: turn_id(7),
                reason: crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
            },
        );
        facts.queue_turn = turn_id(7);
        assert!(super::validate_turn_origin_reconstitution_input(&turn_reuse).is_none());
    }

    /// Validates a reclassified origin whose source turn ended with the given
    /// terminal disposition and asserts the tracked user-global command set
    /// contains the proof command the disposition carries.
    #[track_caller]
    fn assert_terminal_proof_command_is_tracked(
        disposition: TurnDisposition,
        proof_command: crate::DurableCommandId,
    ) {
        let origin = reclassified_turn_origin_with_disposition(disposition);
        let validated = super::validate_turn_origin_reconstitution_input(&origin)
            .expect("a unique terminal proof command is valid");
        assert!(
            validated.command_ids.contains(&proof_command),
            "the origin chain's command identity set must include terminal proof commands"
        );
    }

    /// S08: the user-global command identity set includes
    /// every command carried by terminal authority in the origin chain.
    #[test]
    fn s08_reclassified_origin_tracks_terminal_proof_commands() {
        let proof_command = command_id(0x90);
        assert_terminal_proof_command_is_tracked(
            TurnDisposition::Cancelled {
                cause: test_applied_interrupt_proof(proof_command, turn_id(7)),
            },
            proof_command,
        );
        assert_terminal_proof_command_is_tracked(
            TurnDisposition::ReconciliationRequired {
                marker: test_reconciliation_marker(
                    NonEmptyIssuedOperationRefs::try_from_operations([
                        IssuedOperationRef::ModelCall(model_call_id(0x91)),
                    ])
                    .expect("the test ambiguity set is nonempty"),
                    ReconciliationReason::UserChoseReconciliation {
                        decision: test_applied_stop_for_reconciliation_proof(
                            proof_command,
                            turn_id(7),
                        ),
                    },
                ),
            },
            proof_command,
        );
        assert_terminal_proof_command_is_tracked(
            TurnDisposition::ReconciliationRequired {
                marker: test_reconciliation_marker(
                    NonEmptyIssuedOperationRefs::try_from_operations([
                        IssuedOperationRef::ModelCall(model_call_id(0x92)),
                    ])
                    .expect("the test ambiguity set is nonempty"),
                    ReconciliationReason::InterruptRequiresReconciliation {
                        interrupt: test_applied_interrupt_proof(proof_command, turn_id(7)),
                    },
                ),
            },
            proof_command,
        );
        assert_terminal_proof_command_is_tracked(
            TurnDisposition::ReconciliationRequired {
                marker: test_reconciliation_marker(
                    NonEmptyIssuedOperationRefs::try_from_operations([
                        IssuedOperationRef::ModelCall(model_call_id(0x93)),
                    ])
                    .expect("the test ambiguity set is nonempty"),
                    ReconciliationReason::FatalMismatchRequiresReconciliation {
                        causes: test_fatal_mismatch_stop_causes(
                            provider_target_evidence_id(0x94),
                            crate::AppliedInterruptState::Applied {
                                proof: test_applied_interrupt_proof(proof_command, turn_id(7)),
                            },
                        ),
                    },
                ),
            },
            proof_command,
        );

        let colliding_disposition = TurnDisposition::Cancelled {
            cause: test_applied_interrupt_proof(command_id(0x72), turn_id(7)),
        };
        assert!(
            super::validate_turn_origin_reconstitution_input(
                &reclassified_turn_origin_with_disposition(colliding_disposition)
            )
            .is_none(),
            "terminal proof commands cannot reuse a receipt command"
        );

        let replay_command = 0x90;
        let rejection = SubmitInputReconstitutionInput::rejected_active_turn_present(
            SubmitInputRejectedActiveTurnPresentReconstitutionInput {
                command: start_command(replay_command, "rejected start", 1),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_active_turn: turn_id(8),
                active_turn_origin: reclassified_turn_origin_with_disposition(
                    TurnDisposition::Cancelled {
                        cause: test_applied_interrupt_proof(command_id(replay_command), turn_id(7)),
                    },
                ),
            },
        );
        assert_eq!(
            rejection
                .reconstitute()
                .expect_err("the replay command cannot reuse terminal authority")
                .failure(),
            SubmitInputReconstitutionFailure::RejectionActiveTurnOriginCommandReused
        );
    }

    /// S09: after-current replay carries the active predecessor's
    /// canonical origin and must follow it in session acceptance order.
    #[test]
    fn s09_after_reconstitution_requires_predecessor_chronology() {
        let mut missing_predecessor = after_applied_input();
        applied_facts(&mut missing_predecessor).predecessor_origin = None;
        assert_eq!(
            missing_predecessor
                .reconstitute()
                .expect_err("after-current replay requires its predecessor origin")
                .failure(),
            SubmitInputReconstitutionFailure::AfterCurrentPredecessorOriginMismatch
        );

        let mut premature = after_applied_input();
        let premature_facts = applied_facts(&mut premature);
        premature_facts.accepted_position = SessionInputPosition::first();
        premature_facts.queue_order =
            AcceptedInputQueueOrder::ordinary(SessionInputPosition::first());
        assert_eq!(
            premature
                .reconstitute()
                .expect_err("after-current acceptance must follow its predecessor origin")
                .failure(),
            SubmitInputReconstitutionFailure::AfterCurrentAcceptanceDoesNotFollowPredecessorOrigin
        );

        let mut unexpected_predecessor = applied_input();
        applied_facts(&mut unexpected_predecessor).predecessor_origin = Some(source_turn_origin());
        assert_eq!(
            unexpected_predecessor
                .reconstitute()
                .expect_err("vacant-slot start replay has no active predecessor")
                .failure(),
            SubmitInputReconstitutionFailure::AfterCurrentPredecessorOriginMismatch
        );
    }

    /// S32: an interrupt origin may follow the exact terminal
    /// delegated predecessor even though that turn has no accepted input.
    #[test]
    fn s32_interrupt_reconstitution_admits_exact_non_accepted_predecessor() {
        let input =
            interrupt_applied_input_with_non_accepted_predecessor(session_id(1), turn_id(7));

        input
            .reconstitute()
            .expect("the exact non-accepted interrupt predecessor is admitted");
    }

    /// S32: non-accepted predecessor evidence remains scoped to the
    /// command's exact session.
    #[test]
    fn s32_interrupt_reconstitution_rejects_cross_session_non_accepted_predecessor() {
        let input =
            interrupt_applied_input_with_non_accepted_predecessor(session_id(2), turn_id(7));

        assert_eq!(
            input
                .reconstitute()
                .expect_err("a non-accepted predecessor from another session is unrelated")
                .failure(),
            SubmitInputReconstitutionFailure::AfterCurrentPredecessorOriginMismatch
        );
    }

    /// S32: non-accepted predecessor evidence must name the exact
    /// turn targeted by the interrupt command.
    #[test]
    fn s32_interrupt_reconstitution_rejects_cross_wired_non_accepted_predecessor() {
        let input =
            interrupt_applied_input_with_non_accepted_predecessor(session_id(1), turn_id(6));

        assert_eq!(
            input
                .reconstitute()
                .expect_err("a different non-accepted predecessor cannot authorize the interrupt")
                .failure(),
            SubmitInputReconstitutionFailure::AfterCurrentPredecessorOriginMismatch
        );
    }

    /// S32: non-accepted predecessor evidence cannot weaken the
    /// accepted-origin chronology required by after-current delivery.
    #[test]
    fn s32_after_current_rejects_non_accepted_predecessor() {
        let mut input = after_applied_input();
        let facts = applied_facts(&mut input);
        facts.predecessor_origin = None;
        facts.non_accepted_predecessor = Some(NonAcceptedTurnPredecessorReconstitutionInput {
            session: session_id(1),
            turn: turn_id(7),
        });

        assert_eq!(
            input
                .reconstitute()
                .expect_err("after-current replay requires an accepted-input predecessor")
                .failure(),
            SubmitInputReconstitutionFailure::AfterCurrentPredecessorOriginMismatch
        );
    }

    /// S09: after-current replay cannot reuse any identity from its
    /// active predecessor origin.
    #[test]
    fn s09_after_reconstitution_rejects_predecessor_identity_reuse() {
        let mut turn_reuse = after_applied_input();
        let facts = applied_facts(&mut turn_reuse);
        facts.result_turn = turn_id(7);
        facts.accepted_disposition = AcceptedInputDisposition::OriginOf(turn_id(7));
        facts.queue_turn = turn_id(7);
        assert_eq!(
            turn_reuse
                .reconstitute()
                .expect_err("after-current work cannot reuse its active predecessor turn")
                .failure(),
            SubmitInputReconstitutionFailure::QueueTurnMismatch
        );

        let mut accepted_input_reuse = after_applied_input();
        applied_facts(&mut accepted_input_reuse).predecessor_origin =
            Some(source_turn_origin_with_identities(0x70, 3));
        assert_eq!(
            accepted_input_reuse
                .reconstitute()
                .expect_err("after-current work cannot reuse its predecessor input")
                .failure(),
            SubmitInputReconstitutionFailure::AfterCurrentPredecessorAcceptedInputReused
        );

        let mut command_reuse = after_applied_input();
        applied_facts(&mut command_reuse).predecessor_origin =
            Some(source_turn_origin_with_identities(1, 0x71));
        assert_eq!(
            command_reuse
                .reconstitute()
                .expect_err("after-current work cannot reuse its predecessor command")
                .failure(),
            SubmitInputReconstitutionFailure::AfterCurrentPredecessorCommandReused
        );

        assert_eq!(
            after_applied_input_with_chained_predecessor(1, 0x71, turn_id(9))
                .reconstitute()
                .expect_err("after-current work cannot reuse an ancestor input")
                .failure(),
            SubmitInputReconstitutionFailure::AfterCurrentPredecessorAcceptedInputReused
        );
        assert_eq!(
            after_applied_input_with_chained_predecessor(0x70, 3, turn_id(9))
                .reconstitute()
                .expect_err("after-current work cannot reuse an ancestor command")
                .failure(),
            SubmitInputReconstitutionFailure::AfterCurrentPredecessorCommandReused
        );
        assert_eq!(
            after_applied_input_with_chained_predecessor(1, 3, turn_id(7))
                .reconstitute()
                .expect_err("after-current work cannot reuse an ancestor turn")
                .failure(),
            SubmitInputReconstitutionFailure::QueueTurnMismatch
        );
    }

    /// S08: pending-steering replay cannot reuse either user-global
    /// identity from its canonical source origin.
    #[test]
    fn s08_pending_steering_rejects_source_identity_reuse() {
        let mut accepted_input_reuse = pending_steering_input();
        pending_facts(&mut accepted_input_reuse).source_turn_origin =
            source_turn_origin_with_identities(0x70, 3);
        assert_eq!(
            accepted_input_reuse
                .reconstitute()
                .expect_err("pending steering cannot reuse its source input")
                .failure(),
            SubmitInputReconstitutionFailure::SteeringSourceAcceptedInputReused
        );

        let mut command_reuse = pending_steering_input();
        pending_facts(&mut command_reuse).source_turn_origin =
            source_turn_origin_with_identities(1, 0x71);
        assert_eq!(
            command_reuse
                .reconstitute()
                .expect_err("pending steering cannot reuse its source command")
                .failure(),
            SubmitInputReconstitutionFailure::SteeringSourceCommandReused
        );

        assert_eq!(
            pending_steering_input_with_chained_source(1, 0x71)
                .reconstitute()
                .expect_err("pending steering cannot reuse an ancestor input")
                .failure(),
            SubmitInputReconstitutionFailure::SteeringSourceAcceptedInputReused
        );
        assert_eq!(
            pending_steering_input_with_chained_source(0x70, 3)
                .reconstitute()
                .expect_err("pending steering cannot reuse an ancestor command")
                .failure(),
            SubmitInputReconstitutionFailure::SteeringSourceCommandReused
        );
    }

    /// Applies one cross-wiring mutation to the canonical pending-steering
    /// projection and asserts the exact closed failure it must produce; the
    /// mutation and expected failure stay at the call site.
    #[track_caller]
    fn assert_pending_steering_fact_fails_closed(
        cross_wire: impl FnOnce(&mut SubmitInputReconstitutionInput),
        expected: SubmitInputReconstitutionFailure,
    ) {
        let mut wrong = pending_steering_input();
        cross_wire(&mut wrong);
        assert_eq!(
            wrong
                .reconstitute()
                .expect_err("one cross-wired pending-steering fact fails closed")
                .failure(),
            expected
        );
    }

    /// S08: every independent pending-steering fact is
    /// checked before the immutable receipt is reconstructed.
    #[test]
    fn pending_steering_reconstitution_rejects_cross_wired_facts() {
        assert_pending_steering_fact_fails_closed(
            |input| input.command = start_command(1, "hello", 1),
            SubmitInputReconstitutionFailure::AppliedDeliveryIsNotNextSafePoint,
        );
        assert_pending_steering_fact_fails_closed(
            |input| input.stored_actor = Actor::Recovery,
            SubmitInputReconstitutionFailure::StoredActorMismatch,
        );
        assert_pending_steering_fact_fails_closed(
            |input| pending_facts(input).result_session = session_id(2),
            SubmitInputReconstitutionFailure::ResultSessionMismatch,
        );
        assert_pending_steering_fact_fails_closed(
            |input| pending_facts(input).result_source_turn = turn_id(9),
            SubmitInputReconstitutionFailure::SteeringSourceTurnMismatch,
        );
        assert_pending_steering_fact_fails_closed(
            |input| pending_facts(input).accepted_command = command_id(2),
            SubmitInputReconstitutionFailure::AcceptedCommandMismatch,
        );
        assert_pending_steering_fact_fails_closed(
            |input| pending_facts(input).accepted_input = accepted_input_id(9),
            SubmitInputReconstitutionFailure::AcceptedInputMismatch,
        );
        assert_pending_steering_fact_fails_closed(
            |input| pending_facts(input).accepted_session = session_id(2),
            SubmitInputReconstitutionFailure::AcceptedSessionMismatch,
        );
        assert_pending_steering_fact_fails_closed(
            |input| pending_facts(input).accepted_content = content("different"),
            SubmitInputReconstitutionFailure::AcceptedContentMismatch,
        );
        assert_pending_steering_fact_fails_closed(
            |input| {
                pending_facts(input).accepted_delivery = DeliveryRequest::NextSafePoint {
                    expected_active_turn: turn_id(9),
                };
            },
            SubmitInputReconstitutionFailure::AcceptedDeliveryMismatch,
        );
        assert_pending_steering_fact_fails_closed(
            |input| pending_facts(input).accepted_position = SessionInputPosition::first(),
            SubmitInputReconstitutionFailure::SteeringAcceptanceDoesNotFollowSourceOrigin,
        );

        let mut wrong_source_origin = pending_steering_input();
        pending_facts(&mut wrong_source_origin).source_turn_origin = explicit_turn_origin_input(
            after_applied_input()
                .reconstitute()
                .expect("the cross-wired origin is independently canonical"),
        );
        assert_eq!(
            wrong_source_origin
                .reconstitute()
                .expect_err("the source receipt must establish the exact source turn")
                .failure(),
            SubmitInputReconstitutionFailure::SteeringSourceTurnOriginMismatch
        );
    }

    /// Applies one cross-wiring mutation to the canonical applied projection
    /// and asserts the exact closed failure it must produce; the mutation and
    /// expected failure stay at the call site.
    #[track_caller]
    fn assert_applied_fact_fails_closed(
        cross_wire: impl FnOnce(&mut SubmitInputReconstitutionInput),
        expected: SubmitInputReconstitutionFailure,
    ) {
        let mut wrong = applied_input();
        cross_wire(&mut wrong);
        assert_eq!(
            wrong
                .reconstitute()
                .expect_err("one cross-wired applied fact fails closed")
                .failure(),
            expected
        );
    }

    /// every applied-path reconstitution failure variant
    /// is reachable from exactly one cross-wired fact and fails closed
    /// instead of constructing authority.
    #[test]
    fn applied_reconstitution_rejects_every_cross_wired_fact() {
        assert_applied_fact_fails_closed(
            |input| input.stored_actor = Actor::Recovery,
            SubmitInputReconstitutionFailure::StoredActorMismatch,
        );
        assert_applied_fact_fails_closed(
            |input| {
                input.command = SubmitInput::new(
                    command_id(1),
                    session_id(1),
                    content("hello"),
                    DeliveryRequest::NextSafePoint {
                        expected_active_turn: turn_id(9),
                    },
                );
            },
            SubmitInputReconstitutionFailure::AppliedDeliveryIsNotTurnOrigin,
        );
        assert_applied_fact_fails_closed(
            |input| applied_facts(input).result_session = session_id(2),
            SubmitInputReconstitutionFailure::ResultSessionMismatch,
        );
        assert_applied_fact_fails_closed(
            |input| applied_facts(input).accepted_command = command_id(2),
            SubmitInputReconstitutionFailure::AcceptedCommandMismatch,
        );
        assert_applied_fact_fails_closed(
            |input| applied_facts(input).accepted_input = accepted_input_id(9),
            SubmitInputReconstitutionFailure::AcceptedInputMismatch,
        );
        assert_applied_fact_fails_closed(
            |input| applied_facts(input).accepted_session = session_id(2),
            SubmitInputReconstitutionFailure::AcceptedSessionMismatch,
        );
        assert_applied_fact_fails_closed(
            |input| applied_facts(input).accepted_content = content("different"),
            SubmitInputReconstitutionFailure::AcceptedContentMismatch,
        );
        assert_applied_fact_fails_closed(
            |input| {
                applied_facts(input).accepted_delivery = DeliveryRequest::StartWhenNoActiveTurn {
                    configuration: choices(2, ModelSelectionOverride::UseSessionDefault),
                };
            },
            SubmitInputReconstitutionFailure::AcceptedDeliveryMismatch,
        );
        assert_applied_fact_fails_closed(
            |input| {
                applied_facts(input).accepted_disposition =
                    AcceptedInputDisposition::OriginOf(turn_id(9));
            },
            SubmitInputReconstitutionFailure::AcceptedDispositionMismatch,
        );
        assert_applied_fact_fails_closed(
            |input| applied_facts(input).queue_session = session_id(2),
            SubmitInputReconstitutionFailure::QueueSessionMismatch,
        );
        assert_applied_fact_fails_closed(
            |input| applied_facts(input).queue_turn = turn_id(9),
            SubmitInputReconstitutionFailure::QueueTurnMismatch,
        );
        assert_applied_fact_fails_closed(
            |input| {
                applied_facts(input).accepted_position = SessionInputPosition::first()
                    .checked_next()
                    .expect("the second position exists");
            },
            SubmitInputReconstitutionFailure::QueuePositionMismatch,
        );
        assert_applied_fact_fails_closed(
            |input| {
                applied_facts(input).queue_order =
                    crate::AcceptedInputQueueOrder::interrupt_immediately_after(
                        SessionInputPosition::first(),
                        turn_id(9),
                    );
            },
            SubmitInputReconstitutionFailure::QueuePriorityMismatch,
        );
        assert_applied_fact_fails_closed(
            |input| applied_facts(input).defaults_session = session_id(2),
            SubmitInputReconstitutionFailure::DefaultsSessionMismatch,
        );
        assert_applied_fact_fails_closed(
            |input| applied_facts(input).defaults_version = version(2),
            SubmitInputReconstitutionFailure::DefaultsVersionMismatch,
        );
        assert_applied_fact_fails_closed(
            |input| {
                applied_facts(input).stored_requested_model =
                    ModelSelectionRequest::Direct(direct(9));
            },
            SubmitInputReconstitutionFailure::RequestedModelMismatch,
        );
        assert_applied_fact_fails_closed(
            |input| {
                applied_facts(input).stored_frozen_model = FrozenModelSelection::Direct(direct(9));
            },
            SubmitInputReconstitutionFailure::FrozenModelMismatch,
        );
    }

    /// each rejected receipt reconstructs only from a matching
    /// command-specific typed projection.
    #[test]
    fn rejected_reconstitution_is_checked() {
        let command = start_command(1, "hello", 1);
        let ReconstitutedSubmitInput { .. } =
            SubmitInputReconstitutionInput::rejected_session_not_found(
                SubmitInputRejectedSessionNotFoundReconstitutionInput {
                    command: command.clone(),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                },
            )
            .reconstitute()
            .expect("matching missing-session facts reconstruct");

        assert_eq!(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
                    command,
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_expected: version(2),
                    result_current: version(3),
                    active_turn_origin: None,
                }
            )
            .reconstitute()
            .expect_err("a different expected version fails closed")
            .failure(),
            SubmitInputReconstitutionFailure::ExpectedDefaultsVersionMismatch
        );
    }

    /// the baseline rejected-result projections fail closed for
    /// independently cross-wired actor, session, delivery, configuration,
    /// alias, and position facts.
    #[test]
    fn rejected_reconstitution_rejects_every_cross_wired_fact() {
        let start = start_command(1, "hello", 1);
        let safe_point = SubmitInput::new(
            command_id(1),
            session_id(1),
            content("hello"),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: turn_id(7),
            },
        );
        let maximum = SessionInputPosition::try_from_u64(u64::MAX).expect("positive maximum");

        assert_eq!(
            SubmitInputReconstitutionInput::rejected_session_not_found(
                SubmitInputRejectedSessionNotFoundReconstitutionInput {
                    command: start.clone(),
                    stored_actor: Actor::Recovery,
                    result_session: session_id(1),
                }
            )
            .reconstitute()
            .expect_err("a stored non-user actor fails closed")
            .failure(),
            SubmitInputReconstitutionFailure::StoredActorMismatch
        );

        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_session_not_found(
                SubmitInputRejectedSessionNotFoundReconstitutionInput {
                    command: start.clone(),
                    stored_actor: Actor::User,
                    result_session: session_id(2),
                },
            ),
            SubmitInputReconstitutionFailure::ResultSessionMismatch,
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_no_active_turn(
                SubmitInputRejectedNoActiveTurnReconstitutionInput {
                    command: safe_point.clone(),
                    stored_actor: Actor::User,
                    result_session: session_id(2),
                    result_expected_active_turn: turn_id(7),
                },
            ),
            SubmitInputReconstitutionFailure::ResultSessionMismatch,
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
                    command: start.clone(),
                    stored_actor: Actor::User,
                    result_session: session_id(2),
                    result_expected: version(1),
                    result_current: version(2),
                    active_turn_origin: None,
                },
            ),
            SubmitInputReconstitutionFailure::ResultSessionMismatch,
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                SubmitInputRejectedUnknownModelAliasReconstitutionInput {
                    command: start.clone(),
                    stored_actor: Actor::User,
                    result_session: session_id(2),
                    result_alias: alias(3),
                    defaults_session: session_id(1),
                    defaults_version: version(1),
                    defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
                    active_turn_origin: None,
                },
            ),
            SubmitInputReconstitutionFailure::ResultSessionMismatch,
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
                SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput {
                    command: start.clone(),
                    stored_actor: Actor::User,
                    result_session: session_id(2),
                    result_last_position: maximum,
                    active_turn_origin: None,
                },
            ),
            SubmitInputReconstitutionFailure::ResultSessionMismatch,
        );

        SubmitInputReconstitutionInput::rejected_no_active_turn(
            SubmitInputRejectedNoActiveTurnReconstitutionInput {
                command: safe_point.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_expected_active_turn: turn_id(7),
            },
        )
        .reconstitute()
        .expect("the matching expected turn reconstructs");
        assert_eq!(
            SubmitInputReconstitutionInput::rejected_no_active_turn(
                SubmitInputRejectedNoActiveTurnReconstitutionInput {
                    command: safe_point.clone(),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_expected_active_turn: turn_id(8),
                }
            )
            .reconstitute()
            .expect_err("another expected turn fails closed")
            .failure(),
            SubmitInputReconstitutionFailure::ExpectedActiveTurnMismatch
        );

        assert_eq!(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
                    command: safe_point,
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_expected: version(1),
                    result_current: version(2),
                    active_turn_origin: None,
                }
            )
            .reconstitute()
            .expect_err("a non-start delivery fails closed")
            .failure(),
            SubmitInputReconstitutionFailure::RejectionHasNoExplicitOriginConfiguration
        );
        assert_eq!(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
                    command: start.clone(),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_expected: version(1),
                    result_current: version(1),
                    active_turn_origin: None,
                }
            )
            .reconstitute()
            .expect_err("equal versions are not a mismatch")
            .failure(),
            SubmitInputReconstitutionFailure::RejectedDefaultsVersionsAreEqual
        );

        let alias_command = SubmitInput::new(
            command_id(1),
            session_id(1),
            content("hello"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: choices(
                    1,
                    ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Alias(alias(2))),
                ),
            },
        );
        SubmitInputReconstitutionInput::rejected_unknown_model_alias(
            SubmitInputRejectedUnknownModelAliasReconstitutionInput {
                command: alias_command.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_alias: alias(2),
                defaults_session: session_id(1),
                defaults_version: version(1),
                defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
                active_turn_origin: None,
            },
        )
        .reconstitute()
        .expect("the matching unresolved alias reconstructs");
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                SubmitInputRejectedUnknownModelAliasReconstitutionInput {
                    command: alias_command.clone(),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_alias: alias(2),
                    defaults_session: session_id(2),
                    defaults_version: version(1),
                    defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
                    active_turn_origin: None,
                },
            ),
            SubmitInputReconstitutionFailure::DefaultsSessionMismatch,
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                SubmitInputRejectedUnknownModelAliasReconstitutionInput {
                    command: alias_command.clone(),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_alias: alias(2),
                    defaults_session: session_id(1),
                    defaults_version: version(2),
                    defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
                    active_turn_origin: None,
                },
            ),
            SubmitInputReconstitutionFailure::DefaultsVersionMismatch,
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                SubmitInputRejectedUnknownModelAliasReconstitutionInput {
                    command: alias_command.clone(),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_alias: alias(3),
                    defaults_session: session_id(1),
                    defaults_version: version(1),
                    defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
                    active_turn_origin: None,
                },
            ),
            SubmitInputReconstitutionFailure::UnknownAliasMismatch,
        );
        assert_eq!(
            SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                SubmitInputRejectedUnknownModelAliasReconstitutionInput {
                    command: start.clone(),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_alias: alias(3),
                    defaults_session: session_id(1),
                    defaults_version: version(1),
                    defaults: defaults(ModelSelectionRequest::Direct(direct(2))),
                    active_turn_origin: None,
                }
            )
            .reconstitute()
            .expect_err("a direct-selecting request cannot record an unknown alias")
            .failure(),
            SubmitInputReconstitutionFailure::RejectionDidNotSelectAlias
        );

        SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
            SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput {
                command: start.clone(),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_last_position: maximum,
                active_turn_origin: None,
            },
        )
        .reconstitute()
        .expect("the exhausted maximum position reconstructs");
        assert_eq!(
            SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
                SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput {
                    command: start,
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_last_position: SessionInputPosition::first(),
                    active_turn_origin: None,
                }
            )
            .reconstitute()
            .expect_err("a position with a successor is not exhausted")
            .failure(),
            SubmitInputReconstitutionFailure::PositionIsNotExhausted
        );
    }

    /// S01 / S08 / S09: every rejection that records an
    /// authoritative active turn carries that turn's exact canonical origin.
    #[test]
    fn active_state_rejections_reconstruct_from_canonical_origins() {
        assert_reconstitutes_rejection(
            SubmitInputReconstitutionInput::rejected_active_turn_present(
                SubmitInputRejectedActiveTurnPresentReconstitutionInput {
                    command: start_command(1, "hello", 1),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_active_turn: turn_id(7),
                    active_turn_origin: source_turn_origin(),
                },
            ),
            SubmitInputRejectedResult::ActiveTurnPresent {
                session: session_id(1),
                active_turn: turn_id(7),
            },
        );

        assert_reconstitutes_rejection(
            SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
                SubmitInputRejectedActiveTurnMismatchReconstitutionInput {
                    command: interrupt_command(1, turn_id(9)),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_expected_active_turn: turn_id(9),
                    result_actual_active_turn: turn_id(7),
                    actual_turn_origin: source_turn_origin(),
                },
            ),
            SubmitInputRejectedResult::ActiveTurnMismatch {
                session: session_id(1),
                expected_active_turn: turn_id(9),
                actual_active_turn: turn_id(7),
            },
        );
        assert_reconstitutes_rejection(
            SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
                SubmitInputRejectedActiveTurnMismatchReconstitutionInput {
                    command: safe_point_command(1, turn_id(9)),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_expected_active_turn: turn_id(9),
                    result_actual_active_turn: turn_id(7),
                    actual_turn_origin: source_turn_origin(),
                },
            ),
            SubmitInputRejectedResult::ActiveTurnMismatch {
                session: session_id(1),
                expected_active_turn: turn_id(9),
                actual_active_turn: turn_id(7),
            },
        );
        assert_reconstitutes_rejection(
            SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
                SubmitInputRejectedActiveTurnMismatchReconstitutionInput {
                    command: after_command(1, turn_id(9)),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_expected_active_turn: turn_id(9),
                    result_actual_active_turn: turn_id(7),
                    actual_turn_origin: source_turn_origin(),
                },
            ),
            SubmitInputRejectedResult::ActiveTurnMismatch {
                session: session_id(1),
                expected_active_turn: turn_id(9),
                actual_active_turn: turn_id(7),
            },
        );
    }

    /// S01 / S08 / S09: configuration and position
    /// rejections reconstruct only for delivery modes that can record them,
    /// with occupied modes carrying their exact active origin.
    #[test]
    fn configuration_and_position_rejections_follow_delivery() {
        let maximum = SessionInputPosition::try_from_u64(u64::MAX).expect("positive maximum");
        assert_reconstitutes_rejection(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
                    command: start_command(1, "hello", 1),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_expected: version(1),
                    result_current: version(2),
                    active_turn_origin: None,
                },
            ),
            SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                session: session_id(1),
                expected: version(1),
                current: version(2),
            },
        );
        assert_reconstitutes_rejection(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
                    command: after_command(1, turn_id(7)),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_expected: version(1),
                    result_current: version(2),
                    active_turn_origin: Some(source_turn_origin()),
                },
            ),
            SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                session: session_id(1),
                expected: version(1),
                current: version(2),
            },
        );

        let start_alias = SubmitInput::new(
            command_id(1),
            session_id(1),
            content("hello"),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: choices(
                    1,
                    ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Alias(alias(2))),
                ),
            },
        );
        assert_reconstitutes_rejection(
            SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                SubmitInputRejectedUnknownModelAliasReconstitutionInput {
                    command: start_alias,
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_alias: alias(2),
                    defaults_session: session_id(1),
                    defaults_version: version(1),
                    defaults: defaults(ModelSelectionRequest::Direct(direct(3))),
                    active_turn_origin: None,
                },
            ),
            SubmitInputRejectedResult::UnknownModelAlias {
                session: session_id(1),
                alias: alias(2),
            },
        );

        let after_alias = SubmitInput::new(
            command_id(1),
            session_id(1),
            content("hello"),
            DeliveryRequest::AfterCurrentTurn {
                expected_active_turn: turn_id(7),
                configuration: choices(
                    1,
                    ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Alias(alias(2))),
                ),
            },
        );
        assert_reconstitutes_rejection(
            SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                SubmitInputRejectedUnknownModelAliasReconstitutionInput {
                    command: after_alias,
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_alias: alias(2),
                    defaults_session: session_id(1),
                    defaults_version: version(1),
                    defaults: defaults(ModelSelectionRequest::Direct(direct(3))),
                    active_turn_origin: Some(source_turn_origin()),
                },
            ),
            SubmitInputRejectedResult::UnknownModelAlias {
                session: session_id(1),
                alias: alias(2),
            },
        );

        assert_reconstitutes_rejection(
            SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
                SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput {
                    command: start_command(1, "hello", 1),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_last_position: maximum,
                    active_turn_origin: None,
                },
            ),
            SubmitInputRejectedResult::AcceptancePositionExhausted {
                session: session_id(1),
                last: maximum,
            },
        );
        assert_reconstitutes_rejection(
            SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
                SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput {
                    command: safe_point_command(1, turn_id(7)),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_last_position: maximum,
                    active_turn_origin: Some(source_turn_origin()),
                },
            ),
            SubmitInputRejectedResult::AcceptancePositionExhausted {
                session: session_id(1),
                last: maximum,
            },
        );
        assert_reconstitutes_rejection(
            SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
                SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput {
                    command: after_command(1, turn_id(7)),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_last_position: maximum,
                    active_turn_origin: Some(source_turn_origin()),
                },
            ),
            SubmitInputRejectedResult::AcceptancePositionExhausted {
                session: session_id(1),
                last: maximum,
            },
        );
    }

    /// S08 / S09: rejection replay fails closed when required
    /// active-origin evidence is omitted, extra, cross-wired, or command-ID
    /// aliased.
    #[test]
    fn rejected_active_origin_evidence_is_exact() {
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
                    command: after_command(1, turn_id(7)),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_expected: version(1),
                    result_current: version(2),
                    active_turn_origin: None,
                },
            ),
            SubmitInputReconstitutionFailure::RejectionActiveTurnOriginMismatch,
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
                    command: start_command(1, "hello", 1),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_expected: version(1),
                    result_current: version(2),
                    active_turn_origin: Some(source_turn_origin()),
                },
            ),
            SubmitInputReconstitutionFailure::RejectionActiveTurnOriginMismatch,
        );

        let wrong_turn_origin = explicit_turn_origin_input(
            applied_input()
                .reconstitute()
                .expect("the independent turn-four origin is canonical"),
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_active_turn_present(
                SubmitInputRejectedActiveTurnPresentReconstitutionInput {
                    command: start_command(1, "hello", 1),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_active_turn: turn_id(7),
                    active_turn_origin: wrong_turn_origin,
                },
            ),
            SubmitInputReconstitutionFailure::RejectionActiveTurnOriginMismatch,
        );

        let steering_receipt = pending_steering_input()
            .reconstitute()
            .expect("the independent pending-steering receipt is canonical");
        let SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(steering)) =
            steering_receipt.result()
        else {
            panic!("the receipt remains pending steering");
        };
        let invalid_origin = SubmitInputTurnOriginReconstitutionInput::new(
            SubmitInputDirectTurnOriginConstructionInput {
                receipt: steering_receipt.clone(),
                lifecycle: AcceptedInputLifecycle::new(
                    steering.accepted_input(),
                    AcceptedInputDisposition::PendingSteering {
                        binding: steering.binding(),
                    },
                ),
                queue_accepted_input: steering.accepted_input(),
                queue_session: steering.session(),
                queue_turn: turn_id(7),
                queue_order: AcceptedInputQueueOrder::ordinary(steering.acceptance_position()),
            },
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_active_turn_present(
                SubmitInputRejectedActiveTurnPresentReconstitutionInput {
                    command: start_command(1, "hello", 1),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_active_turn: turn_id(7),
                    active_turn_origin: invalid_origin,
                },
            ),
            SubmitInputReconstitutionFailure::RejectionActiveTurnOriginMismatch,
        );

        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_active_turn_present(
                SubmitInputRejectedActiveTurnPresentReconstitutionInput {
                    command: start_command(1, "hello", 1),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_active_turn: turn_id(7),
                    active_turn_origin: source_turn_origin_with_identities(1, 0x71),
                },
            ),
            SubmitInputReconstitutionFailure::RejectionActiveTurnOriginCommandReused,
        );

        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_active_turn_present(
                SubmitInputRejectedActiveTurnPresentReconstitutionInput {
                    command: start_command(0x70, "hello", 1),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_active_turn: turn_id(8),
                    active_turn_origin: append_unchecked_reclassified_origin(
                        source_turn_origin(),
                        2,
                        0x102,
                        0x202,
                    ),
                },
            ),
            SubmitInputReconstitutionFailure::RejectionActiveTurnOriginCommandReused,
        );
    }

    /// S01 / S08 / S09: state-carrying rejection replay
    /// validates the delivery discriminator and both expected/actual turns.
    #[test]
    fn state_rejections_validate_delivery_and_turns() {
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_active_turn_present(
                SubmitInputRejectedActiveTurnPresentReconstitutionInput {
                    command: safe_point_command(1, turn_id(7)),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_active_turn: turn_id(7),
                    active_turn_origin: source_turn_origin(),
                },
            ),
            SubmitInputReconstitutionFailure::ActiveTurnPresentRejectionMismatch,
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
                SubmitInputRejectedActiveTurnMismatchReconstitutionInput {
                    command: after_command(1, turn_id(9)),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_expected_active_turn: turn_id(8),
                    result_actual_active_turn: turn_id(7),
                    actual_turn_origin: source_turn_origin(),
                },
            ),
            SubmitInputReconstitutionFailure::ExpectedActiveTurnMismatch,
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
                SubmitInputRejectedActiveTurnMismatchReconstitutionInput {
                    command: after_command(1, turn_id(7)),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_expected_active_turn: turn_id(7),
                    result_actual_active_turn: turn_id(7),
                    actual_turn_origin: source_turn_origin(),
                },
            ),
            SubmitInputReconstitutionFailure::RejectedActiveTurnsAreEqual,
        );
    }

    /// S07 / S08: interrupt replay admits the same
    /// configuration and position rejections as preparation, while a
    /// safe-point request still carries no configurable model choice.
    #[test]
    fn interrupt_rejections_reconstitute_exactly() {
        SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
            SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
                command: interrupt_command(1, turn_id(7)),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_expected: version(1),
                result_current: version(2),
                active_turn_origin: Some(source_turn_origin()),
            },
        )
        .reconstitute()
        .expect("an interrupt defaults-version rejection reconstructs");
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                SubmitInputRejectedDefaultsVersionMismatchReconstitutionInput {
                    command: safe_point_command(1, turn_id(7)),
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_expected: version(1),
                    result_current: version(2),
                    active_turn_origin: Some(source_turn_origin()),
                },
            ),
            SubmitInputReconstitutionFailure::RejectionHasNoExplicitOriginConfiguration,
        );

        let interrupt_alias = SubmitInput::new(
            command_id(1),
            session_id(1),
            content("hello"),
            DeliveryRequest::Interrupt {
                expected_active_turn: turn_id(7),
                descendant_scope: DescendantTerminationScope::ParentAlone,
                configuration: choices(
                    1,
                    ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Alias(alias(2))),
                ),
            },
        );
        SubmitInputReconstitutionInput::rejected_unknown_model_alias(
            SubmitInputRejectedUnknownModelAliasReconstitutionInput {
                command: interrupt_alias,
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_alias: alias(2),
                defaults_session: session_id(1),
                defaults_version: version(1),
                defaults: defaults(ModelSelectionRequest::Direct(direct(3))),
                active_turn_origin: Some(source_turn_origin()),
            },
        )
        .reconstitute()
        .expect("an interrupt unknown-alias rejection reconstructs");

        let safe_point = safe_point_command(1, turn_id(7));
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                SubmitInputRejectedUnknownModelAliasReconstitutionInput {
                    command: safe_point,
                    stored_actor: Actor::User,
                    result_session: session_id(1),
                    result_alias: alias(2),
                    defaults_session: session_id(1),
                    defaults_version: version(1),
                    defaults: defaults(ModelSelectionRequest::Direct(direct(3))),
                    active_turn_origin: Some(source_turn_origin()),
                },
            ),
            SubmitInputReconstitutionFailure::RejectionHasNoExplicitOriginConfiguration,
        );

        let maximum = SessionInputPosition::try_from_u64(u64::MAX).expect("positive maximum");
        SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
            SubmitInputRejectedAcceptancePositionExhaustedReconstitutionInput {
                command: interrupt_command(1, turn_id(7)),
                stored_actor: Actor::User,
                result_session: session_id(1),
                result_last_position: maximum,
                active_turn_origin: Some(source_turn_origin()),
            },
        )
        .reconstitute()
        .expect("an interrupt position-exhaustion rejection reconstructs");
    }

    /// S01: preparation against another command's session is a
    /// nonterminal correlation failure retaining the unchanged command.
    #[test]
    fn s01_preparation_rejects_a_cross_wired_session() {
        let command = start_command(1, "hello", 1);
        let error = command
            .clone()
            .prepare_when_no_active_turn(
                &session(2, 1, ModelSelectionRequest::Direct(direct(2))),
                accepted_input_id(3),
                Some(turn_id(4)),
                None,
                |_| None,
            )
            .expect_err("another session is an adapter correlation failure");
        assert_eq!(
            error.failure(),
            SubmitInputPreparationFailure::SessionMismatch {
                provided_session: session_id(2),
            }
        );
        assert_eq!(error.command(), &command);
    }
}
