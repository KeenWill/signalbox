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
    AppliedInterruptCommandResult, AppliedInterruptState, CurrentTurnAttemptState, DeliveryRequest,
    DurableCommandId, FrozenAliasDefinition, FrozenModelSelection, ModelAlias,
    ModelSelectionRequest, OriginConfiguration, PerInputConfigurationChoices, ReconciliationReason,
    Session, SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionId,
    SessionInputPosition, SteeringBinding, TurnDisposition, TurnId, UserContent,
    VersionedSessionConfigurationDefaults, derive_accepted_input_total_order,
};

/// One canonical owner-global durable input command.
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
    /// Constructs the complete canonical typed payload for the baseline owner.
    ///
    /// docs/spec/identity-and-commands.md admits no non-owner actor at
    /// this durable-command boundary.
    pub const fn new(
        command_id: DurableCommandId,
        session: SessionId,
        content: UserContent,
        delivery: DeliveryRequest,
    ) -> Self {
        Self {
            command_id,
            session,
            actor: Actor::Owner,
            content,
            delivery,
        }
    }

    /// Returns the owner-global command identity.
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
        if session.id() != self.session {
            return Err(SubmitInputPreparationError {
                command: Box::new(self),
                failure: SubmitInputPreparationFailure::SessionMismatch {
                    provided_session: session.id(),
                },
            });
        }

        let DeliveryRequest::StartWhenNoActiveTurn { configuration } = self.delivery else {
            if matches!(self.delivery, DeliveryRequest::NextSafePoint { .. }) != turn.is_none() {
                return Err(SubmitInputPreparationError {
                    command: Box::new(self),
                    failure: SubmitInputPreparationFailure::TurnCandidateMismatch,
                });
            }
            #[expect(
                clippy::expect_used,
                reason = "temporary ledger site: non-start delivery exhaustiveness is validated above; typed conversion is commissioned by the 2026-07-20 audit"
            )]
            let expected_active_turn =
                expected_active_turn(self.delivery).expect("non-start delivery names a turn");
            let target_session = self.session;
            return Ok(PreparedSubmitInput {
                command: self,
                result: SubmitInputResult::Rejected(SubmitInputRejectedResult::NoActiveTurn {
                    session: target_session,
                    expected_active_turn,
                }),
            });
        };
        let Some(turn) = turn else {
            return Err(SubmitInputPreparationError {
                command: Box::new(self),
                failure: SubmitInputPreparationFailure::TurnCandidateMismatch,
            });
        };

        let checked = match session.current_configuration_defaults().derive_request(
            configuration.expected_session_defaults_version(),
            configuration.model(),
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

        let origin_configuration = match OriginConfiguration::freeze(checked, select_definition) {
            Ok(configuration) => configuration,
            Err(unknown) => {
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
                    origin_configuration,
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
        #[expect(
            clippy::expect_used,
            reason = "temporary ledger site: scheduling reconstitution validates every active acceptance tail; typed conversion is commissioned by the 2026-07-20 audit"
        )]
        let previous_position = Some(
            scheduling
                .active_acceptance_tail()
                .expect("an active scheduling projection has a validated acceptance tail")
                .observed_last_position(),
        );
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
            DeliveryRequest::StartWhenNoActiveTurn { .. } => {
                return Ok(PreparedSubmitInput {
                    command: self,
                    result: SubmitInputResult::Rejected(
                        SubmitInputRejectedResult::ActiveTurnPresent {
                            session: target_session,
                            active_turn: actual_active_turn,
                        },
                    ),
                });
            }
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
            } => expected_active_turn,
        };
        if expected_active_turn != actual_active_turn {
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
            crate::ActiveTurnPhase::AwaitingApproval { .. } => None,
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
                let checked = match session.current_configuration_defaults().derive_request(
                    configuration.expected_session_defaults_version(),
                    configuration.model(),
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
                    match OriginConfiguration::freeze(checked, select_definition) {
                        Ok(configuration) => configuration,
                        Err(unknown) => {
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
                            origin_configuration,
                            applied_interrupt: Some(Box::new(applied_interrupt)),
                        },
                    )),
                })
            }
            DeliveryRequest::NextSafePoint { .. } => {
                if let Some(existing) = existing_interrupt {
                    return Ok(PreparedSubmitInput {
                        command: self,
                        result: SubmitInputResult::Rejected(
                            SubmitInputRejectedResult::SafePointUnavailableWhileStopping {
                                session: target_session,
                                active_turn: actual_active_turn,
                                existing_command: existing.command(),
                            },
                        ),
                    });
                }
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
                #[expect(
                    clippy::unreachable,
                    reason = "temporary ledger site: delivery-to-candidate correlation is validated above; typed conversion is commissioned by the 2026-07-20 audit"
                )]
                let Some(turn) = turn else {
                    unreachable!("turn-candidate correlation was validated above");
                };
                if turn == actual_active_turn {
                    return Err(SubmitInputPreparationError {
                        command: Box::new(self),
                        failure: SubmitInputPreparationFailure::TurnCandidateMismatch,
                    });
                }
                let checked = match session.current_configuration_defaults().derive_request(
                    configuration.expected_session_defaults_version(),
                    configuration.model(),
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
                    match OriginConfiguration::freeze(checked, select_definition) {
                        Ok(configuration) => configuration,
                        Err(unknown) => {
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
                            origin_configuration,
                            applied_interrupt: None,
                        },
                    )),
                })
            }
            #[expect(
                clippy::unreachable,
                reason = "temporary ledger site: the start variant returns before this active-turn match; typed conversion is commissioned by the 2026-07-20 audit"
            )]
            DeliveryRequest::StartWhenNoActiveTurn { .. } => {
                unreachable!("start returned the active-turn rejection above")
            }
        }
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
    origin_configuration: OriginConfiguration,
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
    receipt: ReconstitutedSubmitInput,
    lifecycle: AcceptedInputLifecycle,
    queue_accepted_input: AcceptedInputId,
    queue_session: SessionId,
    queue_turn: TurnId,
    queue_order: AcceptedInputQueueOrder,
    source_terminal: Option<SubmitInputTerminalFacts>,
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

impl SubmitInputTerminalSourceReconstitutionInput {
    /// Supplies the source turn's canonical origin facts, terminal-record
    /// owner, and disposition.
    pub fn new(
        origin: SubmitInputTurnOriginReconstitutionInput,
        turn: TurnId,
        disposition: TurnDisposition,
    ) -> Self {
        Self {
            origin,
            turn,
            disposition,
        }
    }
}

impl SubmitInputTurnOriginReconstitutionInput {
    /// Supplies a directly created origin's immutable receipt, current
    /// accepted-input lifecycle, and accepted-input-keyed queue facts.
    pub fn new(
        receipt: ReconstitutedSubmitInput,
        lifecycle: AcceptedInputLifecycle,
        queue_accepted_input: AcceptedInputId,
        queue_session: SessionId,
        queue_turn: TurnId,
        queue_order: AcceptedInputQueueOrder,
    ) -> Self {
        Self {
            chain: vec![SubmitInputTurnOriginReconstitutionFacts {
                receipt,
                lifecycle,
                queue_accepted_input,
                queue_session,
                queue_turn,
                queue_order,
                source_terminal: None,
            }],
        }
    }

    /// Supplies reclassified steering's immutable receipt, current lifecycle,
    /// accepted-input-keyed queue facts, and canonical terminal source turn.
    #[allow(clippy::too_many_arguments)]
    pub fn reclassified(
        receipt: ReconstitutedSubmitInput,
        lifecycle: AcceptedInputLifecycle,
        queue_accepted_input: AcceptedInputId,
        queue_session: SessionId,
        queue_turn: TurnId,
        queue_order: AcceptedInputQueueOrder,
        source_terminal: SubmitInputTerminalSourceReconstitutionInput,
    ) -> Self {
        let SubmitInputTerminalSourceReconstitutionInput {
            mut origin,
            turn,
            disposition,
        } = source_terminal;
        origin.chain.push(SubmitInputTurnOriginReconstitutionFacts {
            receipt,
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
}

/// Complete checked domain inputs for reconstructing one recorded submission.
///
/// The stored actor is the durable spelling of the command's attributed
/// agency; the canonical command cannot carry it because its constructor
/// fixes the baseline owner, so every path supplies it separately for the
/// domain-owned comparison.
#[derive(Clone, Debug)]
pub struct SubmitInputReconstitutionInput {
    command: SubmitInput,
    stored_actor: Actor,
    facts: SubmitInputReconstitutionFacts,
}

impl SubmitInputReconstitutionInput {
    /// Supplies every recorded turn-origin result and durable effect
    /// correlation.
    #[allow(clippy::too_many_arguments)]
    pub fn applied_turn_origin(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_accepted_input: AcceptedInputId,
        result_turn: TurnId,
        predecessor_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
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
    ) -> Self {
        Self {
            command,
            stored_actor,
            facts: SubmitInputReconstitutionFacts::AppliedTurnOrigin(Box::new(
                SubmitInputTurnOriginAppliedReconstitutionFacts {
                    result_session,
                    result_accepted_input,
                    result_turn,
                    predecessor_origin,
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
                },
            )),
        }
    }

    /// Supplies the immutable receipt facts for one accepted safe-point input.
    ///
    /// The accepted input's mutable current disposition is deliberately not
    /// an input: normal steering consumption or reclassification cannot
    /// rewrite the original command result.
    #[allow(clippy::too_many_arguments)]
    pub fn applied_pending_steering(
        command: SubmitInput,
        stored_actor: Actor,
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
    ) -> Self {
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

    /// Supplies a recorded missing-session result.
    pub const fn rejected_session_not_found(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
    ) -> Self {
        Self {
            command,
            stored_actor,
            facts: SubmitInputReconstitutionFacts::RejectedSessionNotFound { result_session },
        }
    }

    /// Supplies a recorded no-active-turn result.
    pub const fn rejected_no_active_turn(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_expected_active_turn: TurnId,
    ) -> Self {
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
    pub const fn rejected_active_turn_present(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_active_turn: TurnId,
        active_turn_origin: SubmitInputTurnOriginReconstitutionInput,
    ) -> Self {
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
    pub const fn rejected_active_turn_mismatch(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_expected_active_turn: TurnId,
        result_actual_active_turn: TurnId,
        actual_turn_origin: SubmitInputTurnOriginReconstitutionInput,
    ) -> Self {
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
    pub const fn rejected_defaults_version_mismatch(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_expected: SessionConfigurationDefaultsVersion,
        result_current: SessionConfigurationDefaultsVersion,
        active_turn_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
    ) -> Self {
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
    #[allow(clippy::too_many_arguments)]
    pub const fn rejected_unknown_model_alias(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_alias: ModelAlias,
        defaults_session: SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
        active_turn_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
    ) -> Self {
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
    pub const fn rejected_acceptance_position_exhausted(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_last_position: SessionInputPosition,
        active_turn_origin: Option<SubmitInputTurnOriginReconstitutionInput>,
    ) -> Self {
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
    pub const fn rejected_safe_point_unavailable_while_stopping(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_active_turn: TurnId,
        active_turn_origin: SubmitInputTurnOriginReconstitutionInput,
        existing_interrupt: AppliedInterruptCommandResult,
    ) -> Self {
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
    pub const fn rejected_interrupt_already_applied(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_active_turn: TurnId,
        result_existing_command: DurableCommandId,
        active_turn_origin: SubmitInputTurnOriginReconstitutionInput,
        existing_interrupt: AppliedInterruptCommandResult,
    ) -> Self {
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
                } = *facts;
                let (expected_predecessor, expected_priority) = match self.command.delivery {
                    DeliveryRequest::StartWhenNoActiveTurn { .. } => {
                        (None, AcceptedInputQueuePriority::Ordinary)
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
                match (expected_predecessor, predecessor_origin) {
                    (None, None) => {}
                    (Some(expected_predecessor), Some(predecessor_origin)) => {
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
                    (None, Some(_)) | (Some(_), None) => {
                        return Err(fail(
                            SubmitInputReconstitutionFailure::AfterCurrentPredecessorOriginMismatch,
                        ));
                    }
                }

                let origin_configuration = reconstruct_origin_configuration(
                    &self.command,
                    defaults_session,
                    defaults_version,
                    defaults,
                    stored_requested_model,
                    stored_frozen_model,
                )
                .map_err(&fail)?;
                let applied_interrupt = match self.command.delivery {
                    DeliveryRequest::Interrupt {
                        expected_active_turn,
                        ..
                    } => AppliedInterruptCommandResult::from_correlated_submit(
                        self.command.command_id,
                        result_session,
                        expected_active_turn,
                        result_accepted_input,
                        result_turn,
                        queue_order,
                    )
                    .map(Box::new)
                    .ok_or_else(|| fail(SubmitInputReconstitutionFailure::QueuePriorityMismatch))?
                    .into(),
                    DeliveryRequest::StartWhenNoActiveTurn { .. }
                    | DeliveryRequest::AfterCurrentTurn { .. } => None,
                    DeliveryRequest::NextSafePoint { .. } => unreachable!(),
                };

                SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(
                    SubmitInputTurnOriginAppliedResult {
                        accepted_input: result_accepted_input,
                        session: result_session,
                        acceptance_position: accepted_position,
                        turn: result_turn,
                        queue_order,
                        origin_configuration,
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

fn reconstruct_origin_configuration(
    command: &SubmitInput,
    defaults_session: SessionId,
    defaults_version: SessionConfigurationDefaultsVersion,
    defaults: SessionConfigurationDefaults,
    stored_requested_model: ModelSelectionRequest,
    stored_frozen_model: FrozenModelSelection,
) -> Result<OriginConfiguration, SubmitInputReconstitutionFailure> {
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
        .derive_request(defaults_version, configuration.model())
        .map_err(|_| SubmitInputReconstitutionFailure::DefaultsVersionMismatch)?;
    if checked.request().model() != stored_requested_model {
        return Err(SubmitInputReconstitutionFailure::RequestedModelMismatch);
    }

    let frozen = OriginConfiguration::freeze(checked, |alias| match stored_frozen_model {
        FrozenModelSelection::FrozenAlias {
            alias: stored_alias,
            definition,
        } if stored_alias == alias => Some(definition),
        FrozenModelSelection::Direct(_) | FrozenModelSelection::FrozenAlias { .. } => None,
    })
    .map_err(|_| SubmitInputReconstitutionFailure::FrozenModelMismatch)?;
    if frozen.effective().model() != &stored_frozen_model {
        return Err(SubmitInputReconstitutionFailure::FrozenModelMismatch);
    }
    Ok(frozen)
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
        let SubmitInputResult::Applied(applied) = facts.receipt.result() else {
            return None;
        };
        if !accepted_inputs.insert(applied.accepted_input())
            || !command_ids.insert(facts.receipt.command().command_id())
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
            content: facts.receipt.command().content().clone(),
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
        TurnDisposition::Completed | TurnDisposition::Refused | TurnDisposition::Failed => None,
        TurnDisposition::Cancelled { cause } => Some(cause.command()),
        TurnDisposition::ReconciliationRequired { marker } => match marker.reason() {
            ReconciliationReason::OwnerChoseReconciliation { decision } => {
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
        },
    }
}

fn terminal_disposition_matches_turn(disposition: &TurnDisposition, turn: TurnId) -> bool {
    match disposition {
        TurnDisposition::Completed | TurnDisposition::Refused | TurnDisposition::Failed => true,
        TurnDisposition::Cancelled { cause } => cause.predecessor() == turn,
        TurnDisposition::ReconciliationRequired { marker } => match marker.reason() {
            ReconciliationReason::OwnerChoseReconciliation { decision } => decision.turn() == turn,
            ReconciliationReason::InterruptRequiresReconciliation { interrupt } => {
                interrupt.predecessor() == turn
            }
            ReconciliationReason::FatalMismatchRequiresReconciliation { causes } => {
                match causes.interrupt() {
                    AppliedInterruptState::NoAppliedInterrupt => true,
                    AppliedInterruptState::Applied { proof } => proof.predecessor() == turn,
                }
            }
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
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    use super::{
        ReconstitutedSubmitInput, SubmitInput, SubmitInputAppliedResult,
        SubmitInputPreparationFailure, SubmitInputReconstitutionFailure,
        SubmitInputReconstitutionInput, SubmitInputRejectedResult, SubmitInputResult,
        SubmitInputTerminalSourceReconstitutionInput, SubmitInputTurnOriginReconstitutionInput,
    };
    use crate::applied_interrupt::test_applied_interrupt_proof;
    use crate::test_support::{
        accepted_input_id, alias, command_id, direct, model_call_id, provider_target_evidence_id,
        session_id, turn_id,
    };
    use crate::test_support::{context_frontier_id, semantic_transcript_entry_id, turn_attempt_id};
    use crate::turn_attempt::test_fatal_mismatch_stop_causes;
    use crate::turn_lifecycle::{
        test_applied_stop_for_reconciliation_proof, test_reconciliation_marker,
    };
    use crate::{
        AcceptedInputDisposition, AcceptedInputLifecycle, AcceptedInputQueueOrder,
        AcceptedInputQueuePriority, AcceptedInputSchedulingProjection,
        AcceptedInputSchedulingReconstitutionInput, AcceptedInputStartingLineage,
        AcceptedInputTurnSchedulingRecord, AcceptedInputTurnSchedulingRecordState,
        ActiveTurnSchedulingReconstitutionInput, Actor, DeliveryRequest, FrozenAliasDefinition,
        FrozenModelSelection, InitialSemanticTranscriptEntryPayload, IssuedOperationRef,
        ModelSelectionOverride, ModelSelectionRequest, NonEmptyIssuedOperationRefs,
        OriginConfiguration, PerInputConfigurationChoices, ReconciliationReason,
        ResolvedContextFrontierReconstitutionInput, SemanticTranscriptEntryReconstitutionInput,
        SemanticTranscriptEntryRef, Session, SessionAcceptanceTailEntryReconstitutionInput,
        SessionAcceptanceTailReconstitutionInput, SessionConfigurationDefaults,
        SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
        SessionInputPosition, SessionReconstitutionInput, SteeringBinding, TranscriptAncestry,
        TurnDisposition, UserContent,
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
                SessionCreationCause::OwnerInitiated,
                TranscriptAncestry::None,
            ),
            session_id(id),
            version(current),
            session_id(id),
            version(current),
            defaults(selection),
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
                    phase: ActiveTurnSchedulingReconstitutionInput::prepared(
                        turn_id(7),
                        turn_attempt_id(0x51),
                    ),
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
            source_turn_origin(),
            turn_id(7),
            disposition,
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
            command.clone(),
            Actor::Owner,
            session_id(1),
            accepted_input_id(3),
            turn_id(4),
            None,
            command_id(1),
            accepted_input_id(3),
            session_id(1),
            content("hello"),
            command.delivery(),
            SessionInputPosition::first(),
            AcceptedInputDisposition::OriginOf(turn_id(4)),
            session_id(1),
            turn_id(4),
            crate::AcceptedInputQueueOrder::ordinary(SessionInputPosition::first()),
            session_id(1),
            version(1),
            defaults(ModelSelectionRequest::Direct(direct(2))),
            ModelSelectionRequest::Direct(direct(2)),
            FrozenModelSelection::Direct(direct(2)),
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
                receipt: ReconstitutedSubmitInput {
                    command,
                    result: SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(
                        super::SubmitInputPendingSteeringAppliedResult {
                            accepted_input,
                            session: session_id(1),
                            acceptance_position: position,
                            binding: SteeringBinding::new(source_turn),
                        },
                    )),
                },
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
            command.clone(),
            Actor::Owner,
            session_id(1),
            accepted_input_id(source_accepted_input),
            turn_id(7),
            None,
            command_id(source_command),
            accepted_input_id(source_accepted_input),
            session_id(1),
            content("source"),
            command.delivery(),
            position,
            AcceptedInputDisposition::OriginOf(turn_id(7)),
            session_id(1),
            turn_id(7),
            AcceptedInputQueueOrder::ordinary(position),
            session_id(1),
            version(1),
            defaults(ModelSelectionRequest::Direct(direct(2))),
            ModelSelectionRequest::Direct(direct(2)),
            FrozenModelSelection::Direct(direct(2)),
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
            receipt,
            AcceptedInputLifecycle::new(accepted_input, AcceptedInputDisposition::OriginOf(turn)),
            accepted_input,
            session,
            turn,
            queue_order,
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
            command.clone(),
            Actor::Owner,
            session_id(1),
            accepted_input_id(0x73),
            turn_id(7),
            source_turn_origin(),
            command.command_id(),
            accepted_input_id(0x73),
            session_id(1),
            content("reclassified steering"),
            command.delivery(),
            position,
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
            receipt,
            lifecycle,
            accepted_input_id(0x73),
            session_id(1),
            turn_id(8),
            AcceptedInputQueueOrder::ordinary(position),
            terminal_source_turn_with_disposition(disposition),
        )
    }

    fn after_applied_input() -> SubmitInputReconstitutionInput {
        let command = after_command(1, turn_id(7));
        let position = SessionInputPosition::first()
            .checked_next()
            .expect("after-current acceptance follows its predecessor");
        SubmitInputReconstitutionInput::applied_turn_origin(
            command.clone(),
            Actor::Owner,
            session_id(1),
            accepted_input_id(3),
            turn_id(8),
            Some(source_turn_origin()),
            command_id(1),
            accepted_input_id(3),
            session_id(1),
            content("hello"),
            command.delivery(),
            position,
            AcceptedInputDisposition::OriginOf(turn_id(8)),
            session_id(1),
            turn_id(8),
            AcceptedInputQueueOrder::ordinary(position),
            session_id(1),
            version(1),
            defaults(ModelSelectionRequest::Direct(direct(2))),
            ModelSelectionRequest::Direct(direct(2)),
            FrozenModelSelection::Direct(direct(2)),
        )
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
            command.clone(),
            Actor::Owner,
            session_id(1),
            accepted_input_id(accepted_input_value),
            result_turn,
            Some(append_unchecked_reclassified_origin(
                source_turn_origin(),
                2,
                0x102,
                0x202,
            )),
            command_id(command_value),
            accepted_input_id(accepted_input_value),
            session_id(1),
            content("hello"),
            command.delivery(),
            position,
            AcceptedInputDisposition::OriginOf(result_turn),
            session_id(1),
            result_turn,
            AcceptedInputQueueOrder::ordinary(position),
            session_id(1),
            version(1),
            defaults(ModelSelectionRequest::Direct(direct(2))),
            ModelSelectionRequest::Direct(direct(2)),
            FrozenModelSelection::Direct(direct(2)),
        )
    }

    fn pending_steering_input() -> SubmitInputReconstitutionInput {
        let command = safe_point_command(1, turn_id(7));
        SubmitInputReconstitutionInput::applied_pending_steering(
            command.clone(),
            Actor::Owner,
            session_id(1),
            accepted_input_id(3),
            turn_id(7),
            source_turn_origin(),
            command_id(1),
            accepted_input_id(3),
            session_id(1),
            content("hello"),
            command.delivery(),
            SessionInputPosition::first()
                .checked_next()
                .expect("pending steering follows its source origin"),
        )
    }

    fn pending_steering_input_with_chained_source(
        command_value: u128,
        accepted_input_value: u128,
    ) -> SubmitInputReconstitutionInput {
        let command = safe_point_command(command_value, turn_id(8));
        SubmitInputReconstitutionInput::applied_pending_steering(
            command.clone(),
            Actor::Owner,
            session_id(1),
            accepted_input_id(accepted_input_value),
            turn_id(8),
            append_unchecked_reclassified_origin(source_turn_origin(), 2, 0x102, 0x202),
            command_id(command_value),
            accepted_input_id(accepted_input_value),
            session_id(1),
            content("hello"),
            command.delivery(),
            SessionInputPosition::try_from_u64(3)
                .expect("pending steering follows the complete source chain"),
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

    /// S01 / INV-012: comparison excludes only command identity and
    /// includes the fixed owner actor, session, exact content, delivery
    /// discriminator, and every delivery field.
    #[test]
    fn s01_inv012_comparison_payload_is_structural() {
        let baseline = start_command(1, "hello", 1);
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
        assert_eq!(baseline.actor(), Actor::Owner);
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
    }

    /// S01 / INV-007 / INV-008 / INV-028: start preparation creates exact
    /// queued-origin disposition, ordinary position, and frozen provenance.
    #[test]
    fn s01_inv007_inv008_inv028_start_prepares_complete_queued_work() {
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

    /// S01 / INV-008: explicit alias requests freeze the supplied immutable
    /// definition, while a missing definition is a typed recorded rejection.
    #[test]
    fn s01_inv008_alias_definition_is_frozen_or_rejected() {
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

    /// S01 / INV-012 / INV-028: active-work variants record the exact
    /// expected turn in a no-active-turn rejection.
    #[test]
    fn s01_inv012_inv028_active_modes_reject_when_no_turn_is_active() {
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

    /// S09 / INV-007 / INV-008 / INV-028: matching after-current input
    /// creates ordinary queued origin work with the next acceptance position
    /// and exact frozen configuration.
    #[test]
    fn s09_inv007_inv008_inv028_matching_after_current_prepares_ordinary_turn_origin() {
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

    /// S08 / INV-007 / INV-016 / INV-028: matching safe-point input creates
    /// pending steering bound to the exact active turn and carries no
    /// turn-origin fields.
    #[test]
    fn s08_inv007_inv016_inv028_matching_next_safe_point_prepares_pending_steering() {
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

    /// S01 / INV-012 / INV-028: a vacant-slot start submitted while the slot
    /// is occupied records the exact authoritative active turn.
    #[test]
    fn s01_inv012_inv028_occupied_slot_start_records_active_turn_presence() {
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

    /// S07 / S08 / S09 / INV-012 / INV-028: every active-work delivery mode
    /// records its stale target against the exact authoritative active turn.
    #[test]
    fn s07_s08_s09_inv012_inv028_occupied_slot_active_work_records_stale_target() {
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

    /// S07 / INV-012 / INV-029 / INV-037: matching interrupt preparation
    /// creates the exact immediate successor and sole cancellation proof.
    #[test]
    fn s07_inv012_inv029_inv037_occupied_slot_matching_interrupt_applies() {
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

    /// S09 / INV-008 / INV-012 / INV-028: after-current preparation records
    /// the exact stale session-defaults version.
    #[test]
    fn s09_inv008_inv012_inv028_occupied_slot_after_current_records_stale_defaults_version() {
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

    /// S09 / INV-008 / INV-012: after-current preparation records the exact
    /// unresolved model alias.
    #[test]
    fn s09_inv008_inv012_occupied_slot_after_current_records_unknown_alias() {
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

    /// S08 / S09 / INV-012 / INV-028: both occupied-slot acceptance paths
    /// record exhaustion of the validated session acceptance tail.
    #[test]
    fn s08_s09_inv012_inv028_occupied_slot_acceptance_records_position_exhaustion() {
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

    /// S09 / INV-002 / INV-012: occupied-slot preparation rejects a scheduling
    /// projection from another session without claiming the command.
    #[test]
    fn s09_inv002_inv012_occupied_slot_preparation_rejects_cross_session_projection() {
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

    /// S09 / INV-002 / INV-012: a queued projection cannot stand in for the
    /// authoritative active turn.
    #[test]
    fn s09_inv002_inv012_occupied_slot_preparation_requires_active_projection() {
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

    /// S08 / S09 / INV-012: each occupied-slot delivery mode requires the
    /// exact candidate shape it can apply.
    #[test]
    fn s08_s09_inv012_occupied_slot_preparation_rejects_mismatched_turn_candidate_shape() {
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

    /// S08 / S09 / INV-001 / INV-012: no occupied-slot acceptance path can
    /// reuse the active turn's canonical origin identity.
    #[test]
    fn s08_s09_inv001_inv012_occupied_slot_preparation_rejects_active_origin_identity_reuse() {
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

    /// S01 / INV-008 / INV-012: missing sessions, stale defaults, unknown
    /// aliases, and exhausted positions remain distinct terminal results.
    #[test]
    fn s01_inv008_inv012_authoritative_rejections_are_typed() {
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

    /// INV-002 / INV-012: complete applied facts reconstruct the canonical
    /// result, while a cross-wired content fact fails closed.
    #[test]
    fn inv002_inv012_applied_reconstitution_checks_complete_correlations() {
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

    /// S08 / S09 / INV-002 / INV-012 / INV-016: both occupied applied
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

    /// S08 / INV-012 / INV-016: replay reconstructs the immutable original
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

    /// S08 / S09 / INV-009 / INV-012: a canonical turn origin can come from
    /// either an original turn-producing receipt or a later visible
    /// reclassification of immutable pending steering.
    #[test]
    fn s08_s09_inv009_inv012_reclassified_turn_origins_support_replay() {
        let predecessor_position = SessionInputPosition::first()
            .checked_next()
            .expect("the reclassified origin follows its source");
        let accepted_position = predecessor_position
            .checked_next()
            .expect("later input follows the reclassified origin");

        let after_command = after_command(0x80, turn_id(8));
        let after = SubmitInputReconstitutionInput::applied_turn_origin(
            after_command.clone(),
            Actor::Owner,
            session_id(1),
            accepted_input_id(0x81),
            turn_id(9),
            Some(reclassified_turn_origin()),
            after_command.command_id(),
            accepted_input_id(0x81),
            session_id(1),
            content("hello"),
            after_command.delivery(),
            accepted_position,
            AcceptedInputDisposition::OriginOf(turn_id(9)),
            session_id(1),
            turn_id(9),
            AcceptedInputQueueOrder::ordinary(accepted_position),
            session_id(1),
            version(1),
            defaults(ModelSelectionRequest::Direct(direct(2))),
            ModelSelectionRequest::Direct(direct(2)),
            FrozenModelSelection::Direct(direct(2)),
        )
        .reconstitute()
        .expect("after-current replay accepts a reclassified predecessor");
        assert!(matches!(
            after.result(),
            SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(origin))
                if origin.turn() == turn_id(9)
        ));

        let steering_command = SubmitInput::new(
            command_id(0x82),
            session_id(1),
            content("later steering"),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: turn_id(8),
            },
        );
        let steering = SubmitInputReconstitutionInput::applied_pending_steering(
            steering_command.clone(),
            Actor::Owner,
            session_id(1),
            accepted_input_id(0x83),
            turn_id(8),
            reclassified_turn_origin(),
            steering_command.command_id(),
            accepted_input_id(0x83),
            session_id(1),
            content("later steering"),
            steering_command.delivery(),
            accepted_position,
        )
        .reconstitute()
        .expect("pending-steering replay accepts a reclassified source");
        assert!(matches!(
            steering.result(),
            SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(_))
        ));

        let rejection = SubmitInputReconstitutionInput::rejected_active_turn_present(
            start_command(0x84, "rejected start", 1),
            Actor::Owner,
            session_id(1),
            turn_id(8),
            reclassified_turn_origin(),
        )
        .reconstitute()
        .expect("rejection replay accepts a reclassified active origin");
        assert!(matches!(
            rejection.result(),
            SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnPresent {
                active_turn,
                ..
            }) if *active_turn == turn_id(8)
        ));
    }

    /// S08 / INV-005 / INV-016: model rendering recovers the final accepted
    /// input's exact user content from a fully checked reclassification chain.
    #[test]
    fn s08_inv005_inv016_reclassified_origin_preserves_renderable_user_content() {
        let origin = reclassified_turn_origin();
        let content = crate::ModelCallOriginContent::from_reconstituted_turn_origin(&origin)
            .expect("the canonical reclassified origin has exact accepted content");

        assert_eq!(content.accepted_input(), accepted_input_id(0x73));
        assert_eq!(content.content().text().as_str(), "reclassified steering");
    }

    /// Replays a rejection whose reclassified origin's source turn ended with
    /// the given terminal disposition and asserts replay authenticates it.
    #[track_caller]
    fn assert_terminal_source_authenticates_reclassification(disposition: TurnDisposition) {
        SubmitInputReconstitutionInput::rejected_active_turn_present(
            start_command(0x84, "rejected start", 1),
            Actor::Owner,
            session_id(1),
            turn_id(8),
            reclassified_turn_origin_with_disposition(disposition),
        )
        .reconstitute()
        .expect("every terminal source disposition authenticates reclassification");
    }

    /// S08 / INV-009 / INV-012: reclassification replay admits every
    /// terminal disposition and recursively validates a source turn that was
    /// itself created by steering reclassification.
    #[test]
    fn s08_inv009_inv012_reclassification_accepts_all_terminal_sources_and_chains() {
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
            command.clone(),
            Actor::Owner,
            session_id(1),
            accepted_input_id(0x75),
            turn_id(8),
            source_origin.clone(),
            command.command_id(),
            accepted_input_id(0x75),
            session_id(1),
            content("second reclassified steering"),
            command.delivery(),
            position,
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
            receipt,
            lifecycle,
            accepted_input_id(0x75),
            session_id(1),
            turn_id(9),
            AcceptedInputQueueOrder::ordinary(position),
            SubmitInputTerminalSourceReconstitutionInput::new(
                source_origin,
                turn_id(8),
                TurnDisposition::Refused,
            ),
        );

        SubmitInputReconstitutionInput::rejected_active_turn_present(
            start_command(0x85, "second rejected start", 1),
            Actor::Owner,
            session_id(1),
            turn_id(9),
            chained_origin,
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
                start_command(0x84, "rejected start", 1),
                Actor::Owner,
                session_id(1),
                turn_id(8),
                origin,
            )
            .reconstitute()
            .expect_err("cross-wired reclassified origin facts fail closed")
            .failure(),
            SubmitInputReconstitutionFailure::RejectionActiveTurnOriginMismatch
        );
    }

    /// S08 / INV-009 / INV-012: a pending receipt becomes canonical origin
    /// evidence only with its exact reclassified lifecycle, queue facts, and
    /// earlier distinct terminal source origin.
    #[test]
    fn s08_inv009_inv012_reclassified_turn_origin_rejects_cross_wired_facts() {
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

    /// S08 / INV-009 / INV-012: validation remains bounded by heap-backed
    /// input size rather than call-stack depth.
    #[test]
    fn s08_inv009_inv012_reclassified_origin_validation_is_iterative() {
        let origin = reclassified_origin_chain_ending_at(16_384);

        let validated = super::validate_turn_origin_reconstitution_input(&origin)
            .expect("a long coherent origin chain validates without recursion");
        assert_eq!(validated.turn, turn_id(16_390));
    }

    /// S08 / INV-001 / INV-012: command, accepted-input, and turn identities
    /// remain unique across the complete reclassification chain, not only
    /// adjacent source/origin pairs.
    #[test]
    fn s08_inv001_inv012_reclassified_origin_rejects_ancestor_identity_reuse() {
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
    /// terminal disposition and asserts the tracked owner-global command set
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

    /// S08 / INV-001 / INV-012: the owner-global command identity set includes
    /// every command carried by terminal authority in the origin chain.
    #[test]
    fn s08_inv001_inv012_reclassified_origin_tracks_terminal_proof_commands() {
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
                    ReconciliationReason::OwnerChoseReconciliation {
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
            start_command(replay_command, "rejected start", 1),
            Actor::Owner,
            session_id(1),
            turn_id(8),
            reclassified_turn_origin_with_disposition(TurnDisposition::Cancelled {
                cause: test_applied_interrupt_proof(command_id(replay_command), turn_id(7)),
            }),
        );
        assert_eq!(
            rejection
                .reconstitute()
                .expect_err("the replay command cannot reuse terminal authority")
                .failure(),
            SubmitInputReconstitutionFailure::RejectionActiveTurnOriginCommandReused
        );
    }

    /// S09 / INV-012: after-current replay carries the active predecessor's
    /// canonical origin and must follow it in session acceptance order.
    #[test]
    fn s09_inv012_after_reconstitution_requires_predecessor_chronology() {
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

    /// S09 / INV-012: after-current replay cannot reuse any identity from its
    /// active predecessor origin.
    #[test]
    fn s09_inv012_after_reconstitution_rejects_predecessor_identity_reuse() {
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

    /// S08 / INV-012: pending-steering replay cannot reuse either owner-global
    /// identity from its canonical source origin.
    #[test]
    fn s08_inv012_pending_steering_rejects_source_identity_reuse() {
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

    /// S08 / INV-002 / INV-012: every independent pending-steering fact is
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

    /// INV-002 / INV-012: every applied-path reconstitution failure variant
    /// is reachable from exactly one cross-wired fact and fails closed
    /// instead of constructing authority.
    #[test]
    fn inv002_inv012_applied_reconstitution_rejects_every_cross_wired_fact() {
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

    /// INV-012: each rejected receipt reconstructs only from a matching
    /// command-specific typed projection.
    #[test]
    fn inv012_rejected_reconstitution_is_checked() {
        let command = start_command(1, "hello", 1);
        let ReconstitutedSubmitInput { .. } =
            SubmitInputReconstitutionInput::rejected_session_not_found(
                command.clone(),
                Actor::Owner,
                session_id(1),
            )
            .reconstitute()
            .expect("matching missing-session facts reconstruct");

        assert_eq!(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                command,
                Actor::Owner,
                session_id(1),
                version(2),
                version(3),
                None,
            )
            .reconstitute()
            .expect_err("a different expected version fails closed")
            .failure(),
            SubmitInputReconstitutionFailure::ExpectedDefaultsVersionMismatch
        );
    }

    /// INV-012: the baseline rejected-result projections fail closed for
    /// independently cross-wired actor, session, delivery, configuration,
    /// alias, and position facts.
    #[test]
    fn inv012_rejected_reconstitution_rejects_every_cross_wired_fact() {
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
                start.clone(),
                Actor::Recovery,
                session_id(1),
            )
            .reconstitute()
            .expect_err("a stored non-owner actor fails closed")
            .failure(),
            SubmitInputReconstitutionFailure::StoredActorMismatch
        );

        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_session_not_found(
                start.clone(),
                Actor::Owner,
                session_id(2),
            ),
            SubmitInputReconstitutionFailure::ResultSessionMismatch,
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_no_active_turn(
                safe_point.clone(),
                Actor::Owner,
                session_id(2),
                turn_id(7),
            ),
            SubmitInputReconstitutionFailure::ResultSessionMismatch,
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                start.clone(),
                Actor::Owner,
                session_id(2),
                version(1),
                version(2),
                None,
            ),
            SubmitInputReconstitutionFailure::ResultSessionMismatch,
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                start.clone(),
                Actor::Owner,
                session_id(2),
                alias(3),
                session_id(1),
                version(1),
                defaults(ModelSelectionRequest::Direct(direct(2))),
                None,
            ),
            SubmitInputReconstitutionFailure::ResultSessionMismatch,
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
                start.clone(),
                Actor::Owner,
                session_id(2),
                maximum,
                None,
            ),
            SubmitInputReconstitutionFailure::ResultSessionMismatch,
        );

        SubmitInputReconstitutionInput::rejected_no_active_turn(
            safe_point.clone(),
            Actor::Owner,
            session_id(1),
            turn_id(7),
        )
        .reconstitute()
        .expect("the matching expected turn reconstructs");
        assert_eq!(
            SubmitInputReconstitutionInput::rejected_no_active_turn(
                safe_point.clone(),
                Actor::Owner,
                session_id(1),
                turn_id(8),
            )
            .reconstitute()
            .expect_err("another expected turn fails closed")
            .failure(),
            SubmitInputReconstitutionFailure::ExpectedActiveTurnMismatch
        );

        assert_eq!(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                safe_point,
                Actor::Owner,
                session_id(1),
                version(1),
                version(2),
                None,
            )
            .reconstitute()
            .expect_err("a non-start delivery fails closed")
            .failure(),
            SubmitInputReconstitutionFailure::RejectionHasNoExplicitOriginConfiguration
        );
        assert_eq!(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                start.clone(),
                Actor::Owner,
                session_id(1),
                version(1),
                version(1),
                None,
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
            alias_command.clone(),
            Actor::Owner,
            session_id(1),
            alias(2),
            session_id(1),
            version(1),
            defaults(ModelSelectionRequest::Direct(direct(2))),
            None,
        )
        .reconstitute()
        .expect("the matching unresolved alias reconstructs");
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                alias_command.clone(),
                Actor::Owner,
                session_id(1),
                alias(2),
                session_id(2),
                version(1),
                defaults(ModelSelectionRequest::Direct(direct(2))),
                None,
            ),
            SubmitInputReconstitutionFailure::DefaultsSessionMismatch,
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                alias_command.clone(),
                Actor::Owner,
                session_id(1),
                alias(2),
                session_id(1),
                version(2),
                defaults(ModelSelectionRequest::Direct(direct(2))),
                None,
            ),
            SubmitInputReconstitutionFailure::DefaultsVersionMismatch,
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                alias_command.clone(),
                Actor::Owner,
                session_id(1),
                alias(3),
                session_id(1),
                version(1),
                defaults(ModelSelectionRequest::Direct(direct(2))),
                None,
            ),
            SubmitInputReconstitutionFailure::UnknownAliasMismatch,
        );
        assert_eq!(
            SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                start.clone(),
                Actor::Owner,
                session_id(1),
                alias(3),
                session_id(1),
                version(1),
                defaults(ModelSelectionRequest::Direct(direct(2))),
                None,
            )
            .reconstitute()
            .expect_err("a direct-selecting request cannot record an unknown alias")
            .failure(),
            SubmitInputReconstitutionFailure::RejectionDidNotSelectAlias
        );

        SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
            start.clone(),
            Actor::Owner,
            session_id(1),
            maximum,
            None,
        )
        .reconstitute()
        .expect("the exhausted maximum position reconstructs");
        assert_eq!(
            SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
                start,
                Actor::Owner,
                session_id(1),
                SessionInputPosition::first(),
                None,
            )
            .reconstitute()
            .expect_err("a position with a successor is not exhausted")
            .failure(),
            SubmitInputReconstitutionFailure::PositionIsNotExhausted
        );
    }

    /// S01 / S08 / S09 / INV-012 / INV-028: every rejection that records an
    /// authoritative active turn carries that turn's exact canonical origin.
    #[test]
    fn inv012_inv028_active_state_rejections_reconstruct_from_canonical_origins() {
        assert_reconstitutes_rejection(
            SubmitInputReconstitutionInput::rejected_active_turn_present(
                start_command(1, "hello", 1),
                Actor::Owner,
                session_id(1),
                turn_id(7),
                source_turn_origin(),
            ),
            SubmitInputRejectedResult::ActiveTurnPresent {
                session: session_id(1),
                active_turn: turn_id(7),
            },
        );

        assert_reconstitutes_rejection(
            SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
                interrupt_command(1, turn_id(9)),
                Actor::Owner,
                session_id(1),
                turn_id(9),
                turn_id(7),
                source_turn_origin(),
            ),
            SubmitInputRejectedResult::ActiveTurnMismatch {
                session: session_id(1),
                expected_active_turn: turn_id(9),
                actual_active_turn: turn_id(7),
            },
        );
        assert_reconstitutes_rejection(
            SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
                safe_point_command(1, turn_id(9)),
                Actor::Owner,
                session_id(1),
                turn_id(9),
                turn_id(7),
                source_turn_origin(),
            ),
            SubmitInputRejectedResult::ActiveTurnMismatch {
                session: session_id(1),
                expected_active_turn: turn_id(9),
                actual_active_turn: turn_id(7),
            },
        );
        assert_reconstitutes_rejection(
            SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
                after_command(1, turn_id(9)),
                Actor::Owner,
                session_id(1),
                turn_id(9),
                turn_id(7),
                source_turn_origin(),
            ),
            SubmitInputRejectedResult::ActiveTurnMismatch {
                session: session_id(1),
                expected_active_turn: turn_id(9),
                actual_active_turn: turn_id(7),
            },
        );
    }

    /// S01 / S08 / S09 / INV-012 / INV-028: configuration and position
    /// rejections reconstruct only for delivery modes that can record them,
    /// with occupied modes carrying their exact active origin.
    #[test]
    fn inv012_inv028_configuration_and_position_rejections_follow_delivery() {
        let maximum = SessionInputPosition::try_from_u64(u64::MAX).expect("positive maximum");
        assert_reconstitutes_rejection(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                start_command(1, "hello", 1),
                Actor::Owner,
                session_id(1),
                version(1),
                version(2),
                None,
            ),
            SubmitInputRejectedResult::SessionDefaultsVersionMismatch {
                session: session_id(1),
                expected: version(1),
                current: version(2),
            },
        );
        assert_reconstitutes_rejection(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                after_command(1, turn_id(7)),
                Actor::Owner,
                session_id(1),
                version(1),
                version(2),
                Some(source_turn_origin()),
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
                start_alias,
                Actor::Owner,
                session_id(1),
                alias(2),
                session_id(1),
                version(1),
                defaults(ModelSelectionRequest::Direct(direct(3))),
                None,
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
                after_alias,
                Actor::Owner,
                session_id(1),
                alias(2),
                session_id(1),
                version(1),
                defaults(ModelSelectionRequest::Direct(direct(3))),
                Some(source_turn_origin()),
            ),
            SubmitInputRejectedResult::UnknownModelAlias {
                session: session_id(1),
                alias: alias(2),
            },
        );

        assert_reconstitutes_rejection(
            SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
                start_command(1, "hello", 1),
                Actor::Owner,
                session_id(1),
                maximum,
                None,
            ),
            SubmitInputRejectedResult::AcceptancePositionExhausted {
                session: session_id(1),
                last: maximum,
            },
        );
        assert_reconstitutes_rejection(
            SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
                safe_point_command(1, turn_id(7)),
                Actor::Owner,
                session_id(1),
                maximum,
                Some(source_turn_origin()),
            ),
            SubmitInputRejectedResult::AcceptancePositionExhausted {
                session: session_id(1),
                last: maximum,
            },
        );
        assert_reconstitutes_rejection(
            SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
                after_command(1, turn_id(7)),
                Actor::Owner,
                session_id(1),
                maximum,
                Some(source_turn_origin()),
            ),
            SubmitInputRejectedResult::AcceptancePositionExhausted {
                session: session_id(1),
                last: maximum,
            },
        );
    }

    /// S08 / S09 / INV-012: rejection replay fails closed when required
    /// active-origin evidence is omitted, extra, cross-wired, or command-ID
    /// aliased.
    #[test]
    fn inv012_rejected_active_origin_evidence_is_exact() {
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                after_command(1, turn_id(7)),
                Actor::Owner,
                session_id(1),
                version(1),
                version(2),
                None,
            ),
            SubmitInputReconstitutionFailure::RejectionActiveTurnOriginMismatch,
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                start_command(1, "hello", 1),
                Actor::Owner,
                session_id(1),
                version(1),
                version(2),
                Some(source_turn_origin()),
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
                start_command(1, "hello", 1),
                Actor::Owner,
                session_id(1),
                turn_id(7),
                wrong_turn_origin,
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
            steering_receipt.clone(),
            AcceptedInputLifecycle::new(
                steering.accepted_input(),
                AcceptedInputDisposition::PendingSteering {
                    binding: steering.binding(),
                },
            ),
            steering.accepted_input(),
            steering.session(),
            turn_id(7),
            AcceptedInputQueueOrder::ordinary(steering.acceptance_position()),
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_active_turn_present(
                start_command(1, "hello", 1),
                Actor::Owner,
                session_id(1),
                turn_id(7),
                invalid_origin,
            ),
            SubmitInputReconstitutionFailure::RejectionActiveTurnOriginMismatch,
        );

        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_active_turn_present(
                start_command(1, "hello", 1),
                Actor::Owner,
                session_id(1),
                turn_id(7),
                source_turn_origin_with_identities(1, 0x71),
            ),
            SubmitInputReconstitutionFailure::RejectionActiveTurnOriginCommandReused,
        );

        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_active_turn_present(
                start_command(0x70, "hello", 1),
                Actor::Owner,
                session_id(1),
                turn_id(8),
                append_unchecked_reclassified_origin(source_turn_origin(), 2, 0x102, 0x202),
            ),
            SubmitInputReconstitutionFailure::RejectionActiveTurnOriginCommandReused,
        );
    }

    /// S01 / S08 / S09 / INV-012 / INV-028: state-carrying rejection replay
    /// validates the delivery discriminator and both expected/actual turns.
    #[test]
    fn inv012_inv028_state_rejections_validate_delivery_and_turns() {
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_active_turn_present(
                safe_point_command(1, turn_id(7)),
                Actor::Owner,
                session_id(1),
                turn_id(7),
                source_turn_origin(),
            ),
            SubmitInputReconstitutionFailure::ActiveTurnPresentRejectionMismatch,
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
                after_command(1, turn_id(9)),
                Actor::Owner,
                session_id(1),
                turn_id(8),
                turn_id(7),
                source_turn_origin(),
            ),
            SubmitInputReconstitutionFailure::ExpectedActiveTurnMismatch,
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
                after_command(1, turn_id(7)),
                Actor::Owner,
                session_id(1),
                turn_id(7),
                turn_id(7),
                source_turn_origin(),
            ),
            SubmitInputReconstitutionFailure::RejectedActiveTurnsAreEqual,
        );
    }

    /// S07 / S08 / INV-012 / INV-028: interrupt replay admits the same
    /// configuration and position rejections as preparation, while a
    /// safe-point request still carries no configurable model choice.
    #[test]
    fn inv012_inv028_interrupt_rejections_reconstitute_exactly() {
        SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
            interrupt_command(1, turn_id(7)),
            Actor::Owner,
            session_id(1),
            version(1),
            version(2),
            Some(source_turn_origin()),
        )
        .reconstitute()
        .expect("an interrupt defaults-version rejection reconstructs");
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                safe_point_command(1, turn_id(7)),
                Actor::Owner,
                session_id(1),
                version(1),
                version(2),
                Some(source_turn_origin()),
            ),
            SubmitInputReconstitutionFailure::RejectionHasNoExplicitOriginConfiguration,
        );

        let interrupt_alias = SubmitInput::new(
            command_id(1),
            session_id(1),
            content("hello"),
            DeliveryRequest::Interrupt {
                expected_active_turn: turn_id(7),
                configuration: choices(
                    1,
                    ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Alias(alias(2))),
                ),
            },
        );
        SubmitInputReconstitutionInput::rejected_unknown_model_alias(
            interrupt_alias,
            Actor::Owner,
            session_id(1),
            alias(2),
            session_id(1),
            version(1),
            defaults(ModelSelectionRequest::Direct(direct(3))),
            Some(source_turn_origin()),
        )
        .reconstitute()
        .expect("an interrupt unknown-alias rejection reconstructs");

        let safe_point = safe_point_command(1, turn_id(7));
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                safe_point,
                Actor::Owner,
                session_id(1),
                alias(2),
                session_id(1),
                version(1),
                defaults(ModelSelectionRequest::Direct(direct(3))),
                Some(source_turn_origin()),
            ),
            SubmitInputReconstitutionFailure::RejectionHasNoExplicitOriginConfiguration,
        );

        let maximum = SessionInputPosition::try_from_u64(u64::MAX).expect("positive maximum");
        SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
            interrupt_command(1, turn_id(7)),
            Actor::Owner,
            session_id(1),
            maximum,
            Some(source_turn_origin()),
        )
        .reconstitute()
        .expect("an interrupt position-exhaustion rejection reconstructs");
    }

    /// S01 / INV-012: preparation against another command's session is a
    /// nonterminal correlation failure retaining the unchanged command.
    #[test]
    fn s01_inv012_preparation_rejects_a_cross_wired_session() {
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
