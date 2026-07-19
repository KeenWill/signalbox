//! Canonical durable input submission and authoritative-state preparation.
//!
//! ADR-0027 owns accepted-input delivery, configuration, ordering, and
//! disposition semantics. ADR-0034 owns structural replay equality, ADR-0035
//! owns checked reconstitution, ADR-0037 owns content, and ADR-0039 owns
//! actor attribution. This slice prepares accepted origin work with no active
//! turn or after the exact active turn, and pending steering for the exact
//! active turn. It does not consume steering, apply interruption, construct an
//! interrupt proof, transition turn lifecycle, or perform persistence.

use std::hash::{Hash, Hasher};

use crate::{
    AcceptedInputDisposition, AcceptedInputId, AcceptedInputQueueOrder, AcceptedInputQueuePriority,
    AcceptedInputTurnSchedulingProjection, AcceptedInputTurnSchedulingStatus, ActiveTurnPhase,
    Actor, CurrentTurnAttemptState, DeliveryRequest, DurableCommandId, FrozenAliasDefinition,
    FrozenModelSelection, ModelAlias, ModelSelectionRequest, OriginConfiguration,
    PerInputConfigurationChoices, Session, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionId, SessionInputPosition, SteeringBinding, TurnId,
    UserContent, VersionedSessionConfigurationDefaults,
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
    /// ADR-0039 admits no non-owner actor at this durable-command boundary.
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
                },
            )),
        })
    }

    /// Prepares handling against the exact authoritative active turn.
    ///
    /// `StartWhenNoActiveTurn` records the active slot owner, stale
    /// active-work requests record both expected and actual turns, matching
    /// after-current input creates ordinary queued origin work, and matching
    /// next-safe-point input creates pending steering unless the current
    /// attempt is already `StopRequested`. Interrupt application remains a
    /// nonclaiming preparation failure in this slice.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_with_active_turn(
        self,
        session: &Session,
        active_turn: &AcceptedInputTurnSchedulingProjection,
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
        if active_turn.session() != self.session {
            return Err(SubmitInputPreparationError {
                command: Box::new(self),
                failure: SubmitInputPreparationFailure::ActiveTurnSessionMismatch {
                    provided_session: active_turn.session(),
                },
            });
        }
        if active_turn.status() != AcceptedInputTurnSchedulingStatus::Active {
            return Err(SubmitInputPreparationError {
                command: Box::new(self),
                failure: SubmitInputPreparationFailure::ActiveTurnProjectionIsNotActive {
                    provided_turn: active_turn.turn(),
                },
            });
        }
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

        match delivery {
            DeliveryRequest::Interrupt { .. } => Err(SubmitInputPreparationError {
                command: Box::new(self),
                failure: SubmitInputPreparationFailure::InterruptApplicationUnavailable,
            }),
            DeliveryRequest::NextSafePoint { .. } => {
                let phase = active_turn
                    .active_phase()
                    .expect("validated active scheduling projection has an exact phase");
                if matches!(
                    phase,
                    ActiveTurnPhase::Running { current_attempt }
                        if matches!(
                            current_attempt.state(),
                            CurrentTurnAttemptState::StopRequested { .. }
                        )
                ) {
                    return Ok(PreparedSubmitInput {
                        command: self,
                        result: SubmitInputResult::Rejected(
                            SubmitInputRejectedResult::SafePointUnavailableWhileStopping {
                                session: target_session,
                                active_turn: actual_active_turn,
                            },
                        ),
                    });
                }

                if let Err(failure) =
                    validate_occupied_acceptance_tail(active_turn, previous_position)
                {
                    return Err(SubmitInputPreparationError {
                        command: Box::new(self),
                        failure,
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
                if let Err(failure) =
                    validate_occupied_acceptance_tail(active_turn, previous_position)
                {
                    return Err(SubmitInputPreparationError {
                        command: Box::new(self),
                        failure,
                    });
                }
                let Some(turn) = turn else {
                    unreachable!("turn-candidate correlation was validated above");
                };
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
                        },
                    )),
                })
            }
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

fn validate_occupied_acceptance_tail(
    active_turn: &AcceptedInputTurnSchedulingProjection,
    observed_tail: Option<SessionInputPosition>,
) -> Result<(), SubmitInputPreparationFailure> {
    let active_position = active_turn.order().acceptance_position();
    if observed_tail.is_some_and(|tail| tail >= active_position) {
        return Ok(());
    }
    Err(
        SubmitInputPreparationFailure::AcceptanceTailPrecedesActiveOrigin {
            active_turn: active_turn.turn(),
            active_position,
            observed_tail,
        },
    )
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
    /// Acceptance created complete durable queued-work facts.
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
    /// Safe-point steering cannot be accepted after stop was requested.
    SafePointUnavailableWhileStopping {
        /// The target session.
        session: SessionId,
        /// The exact stopped active turn.
        active_turn: TurnId,
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
    /// The supplied active turn belongs to another session.
    ActiveTurnSessionMismatch {
        /// The cross-wired session carried by the projection.
        provided_session: SessionId,
    },
    /// The supplied scheduling turn does not own the active slot.
    ActiveTurnProjectionIsNotActive {
        /// The non-active turn supplied.
        provided_turn: TurnId,
    },
    /// The complete accepted-input tail cannot precede or omit the active
    /// turn's own immutable origin position.
    AcceptanceTailPrecedesActiveOrigin {
        /// The authoritative active turn.
        active_turn: TurnId,
        /// The active origin's immutable acceptance position.
        active_position: SessionInputPosition,
        /// The contradictory complete tail observation.
        observed_tail: Option<SessionInputPosition>,
    },
    /// This slice cannot yet apply interruption or claim its command result.
    InterruptApplicationUnavailable,
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

#[derive(Clone, Debug)]
struct SubmitInputTurnOriginAppliedReconstitutionFacts {
    result_session: SessionId,
    result_accepted_input: AcceptedInputId,
    result_turn: TurnId,
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
    source_turn_origin: ReconstitutedSubmitInput,
    accepted_command: DurableCommandId,
    accepted_input: AcceptedInputId,
    accepted_session: SessionId,
    accepted_content: UserContent,
    accepted_delivery: DeliveryRequest,
    accepted_position: SessionInputPosition,
    accepted_disposition: AcceptedInputDisposition,
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
    },
    RejectedActiveTurnMismatch {
        result_session: SessionId,
        result_expected_active_turn: TurnId,
        result_actual_active_turn: TurnId,
    },
    RejectedSafePointUnavailableWhileStopping {
        result_session: SessionId,
        result_active_turn: TurnId,
    },
    RejectedDefaultsVersionMismatch {
        result_session: SessionId,
        result_expected: SessionConfigurationDefaultsVersion,
        result_current: SessionConfigurationDefaultsVersion,
    },
    RejectedUnknownModelAlias {
        result_session: SessionId,
        result_alias: ModelAlias,
        defaults_session: SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
    },
    RejectedAcceptancePositionExhausted {
        result_session: SessionId,
        result_last_position: SessionInputPosition,
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

    /// Supplies every recorded pending-steering result and accepted-input
    /// effect correlation.
    #[allow(clippy::too_many_arguments)]
    pub fn applied_pending_steering(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_accepted_input: AcceptedInputId,
        result_source_turn: TurnId,
        source_turn_origin: ReconstitutedSubmitInput,
        accepted_command: DurableCommandId,
        accepted_input: AcceptedInputId,
        accepted_session: SessionId,
        accepted_content: UserContent,
        accepted_delivery: DeliveryRequest,
        accepted_position: SessionInputPosition,
        accepted_disposition: AcceptedInputDisposition,
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
                    accepted_disposition,
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

    /// Supplies a recorded active-turn-present result for a start request.
    pub const fn rejected_active_turn_present(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_active_turn: TurnId,
    ) -> Self {
        Self {
            command,
            stored_actor,
            facts: SubmitInputReconstitutionFacts::RejectedActiveTurnPresent {
                result_session,
                result_active_turn,
            },
        }
    }

    /// Supplies a recorded stale-active-turn result.
    pub const fn rejected_active_turn_mismatch(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_expected_active_turn: TurnId,
        result_actual_active_turn: TurnId,
    ) -> Self {
        Self {
            command,
            stored_actor,
            facts: SubmitInputReconstitutionFacts::RejectedActiveTurnMismatch {
                result_session,
                result_expected_active_turn,
                result_actual_active_turn,
            },
        }
    }

    /// Supplies a recorded safe-point-unavailable-while-stopping result.
    pub const fn rejected_safe_point_unavailable_while_stopping(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_active_turn: TurnId,
    ) -> Self {
        Self {
            command,
            stored_actor,
            facts: SubmitInputReconstitutionFacts::RejectedSafePointUnavailableWhileStopping {
                result_session,
                result_active_turn,
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
    ) -> Self {
        Self {
            command,
            stored_actor,
            facts: SubmitInputReconstitutionFacts::RejectedDefaultsVersionMismatch {
                result_session,
                result_expected,
                result_current,
            },
        }
    }

    /// Supplies a recorded unknown-alias result and its exact selected
    /// defaults version.
    pub const fn rejected_unknown_model_alias(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_alias: ModelAlias,
        defaults_session: SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
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
            },
        }
    }

    /// Supplies a recorded exhausted-position result.
    pub const fn rejected_acceptance_position_exhausted(
        command: SubmitInput,
        stored_actor: Actor,
        result_session: SessionId,
        result_last_position: SessionInputPosition,
    ) -> Self {
        Self {
            command,
            stored_actor,
            facts: SubmitInputReconstitutionFacts::RejectedAcceptancePositionExhausted {
                result_session,
                result_last_position,
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
                if !matches!(
                    self.command.delivery,
                    DeliveryRequest::StartWhenNoActiveTurn { .. }
                        | DeliveryRequest::AfterCurrentTurn { .. }
                ) {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::AppliedDeliveryIsNotTurnOrigin,
                    ));
                }
                if matches!(
                    self.command.delivery,
                    DeliveryRequest::AfterCurrentTurn {
                        expected_active_turn,
                        ..
                    } if expected_active_turn == result_turn
                ) {
                    return Err(fail(SubmitInputReconstitutionFailure::QueueTurnMismatch));
                }
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
                if queue_order.priority() != AcceptedInputQueuePriority::Ordinary {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::QueuePriorityMismatch,
                    ));
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

                SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(
                    SubmitInputTurnOriginAppliedResult {
                        accepted_input: result_accepted_input,
                        session: result_session,
                        acceptance_position: accepted_position,
                        turn: result_turn,
                        queue_order,
                        origin_configuration,
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
                    accepted_disposition,
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
                let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(source_origin)) =
                    source_turn_origin.result()
                else {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::SteeringSourceTurnOriginMismatch,
                    ));
                };
                if source_origin.session() != self.command.session
                    || source_origin.turn() != result_source_turn
                {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::SteeringSourceTurnOriginMismatch,
                    ));
                }
                if accepted_position <= source_origin.acceptance_position() {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::SteeringAcceptanceDoesNotFollowSourceOrigin,
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
                if source_origin.accepted_input() == accepted_input
                    || source_turn_origin.command().command_id() == accepted_command
                {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::SteeringSourceTurnOriginMismatch,
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
                if accepted_disposition != (AcceptedInputDisposition::PendingSteering { binding }) {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::AcceptedDispositionMismatch,
                    ));
                }

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
                        SubmitInputReconstitutionFailure::RejectionDeliveryIsNotStart,
                    ));
                }
                SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnPresent {
                    session: result_session,
                    active_turn: result_active_turn,
                })
            }
            SubmitInputReconstitutionFacts::RejectedActiveTurnMismatch {
                result_session,
                result_expected_active_turn,
                result_actual_active_turn,
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
                if result_actual_active_turn == result_expected_active_turn {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::RejectedActiveTurnsAreEqual,
                    ));
                }
                SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnMismatch {
                    session: result_session,
                    expected_active_turn: result_expected_active_turn,
                    actual_active_turn: result_actual_active_turn,
                })
            }
            SubmitInputReconstitutionFacts::RejectedSafePointUnavailableWhileStopping {
                result_session,
                result_active_turn,
            } => {
                if result_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ResultSessionMismatch,
                    ));
                }
                if !matches!(
                    self.command.delivery,
                    DeliveryRequest::NextSafePoint {
                        expected_active_turn,
                    } if expected_active_turn == result_active_turn
                ) {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::SafePointRejectionMismatch,
                    ));
                }
                SubmitInputResult::Rejected(
                    SubmitInputRejectedResult::SafePointUnavailableWhileStopping {
                        session: result_session,
                        active_turn: result_active_turn,
                    },
                )
            }
            SubmitInputReconstitutionFacts::RejectedDefaultsVersionMismatch {
                result_session,
                result_expected,
                result_current,
            } => {
                if result_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ResultSessionMismatch,
                    ));
                }
                let Some(configuration) = explicit_origin_configuration(self.command.delivery)
                else {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::RejectionHasNoExplicitOriginConfiguration,
                    ));
                };
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
            } => {
                if result_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ResultSessionMismatch,
                    ));
                }
                let Some(configuration) = explicit_origin_configuration(self.command.delivery)
                else {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::RejectionHasNoExplicitOriginConfiguration,
                    ));
                };
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
            } => {
                if result_session != self.command.session {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::ResultSessionMismatch,
                    ));
                }
                if !matches!(
                    self.command.delivery,
                    DeliveryRequest::StartWhenNoActiveTurn { .. }
                        | DeliveryRequest::NextSafePoint { .. }
                        | DeliveryRequest::AfterCurrentTurn { .. }
                ) {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::PositionExhaustionDeliveryUnavailable,
                    ));
                }
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
        };

        Ok(ReconstitutedSubmitInput {
            command: self.command,
            result,
        })
    }
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
        | DeliveryRequest::AfterCurrentTurn { configuration, .. } => Some(configuration),
        DeliveryRequest::Interrupt { .. } | DeliveryRequest::NextSafePoint { .. } => None,
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
    /// Turn-origin applied facts carry a delivery that does not create the
    /// admitted start or after-current origin.
    AppliedDeliveryIsNotTurnOrigin,
    /// Pending-steering applied facts carry a non-safe-point delivery.
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
    /// The stored disposition is not the result turn's origin relation.
    AcceptedDispositionMismatch,
    /// The applied steering result names another source turn.
    SteeringSourceTurnMismatch,
    /// The supplied source-turn origin does not establish the exact
    /// session-owned source and its canonical configuration.
    SteeringSourceTurnOriginMismatch,
    /// Pending steering does not follow its source origin in acceptance order.
    SteeringAcceptanceDoesNotFollowSourceOrigin,
    /// The queue fact belongs to another session.
    QueueSessionMismatch,
    /// The queue fact names another future turn or an after-current result
    /// reuses its active predecessor.
    QueueTurnMismatch,
    /// The accepted-input and queue positions differ.
    QueuePositionMismatch,
    /// This slice's queue fact is not ordinary priority.
    QueuePriorityMismatch,
    /// A required rejection was recorded for a non-start request.
    RejectionDeliveryIsNotStart,
    /// A configuration rejection carries no explicit origin configuration.
    RejectionHasNoExplicitOriginConfiguration,
    /// A no-active-turn result names a different expected turn or a start
    /// request.
    ExpectedActiveTurnMismatch,
    /// A stale-active rejection claims equal expected and actual turns.
    RejectedActiveTurnsAreEqual,
    /// A stopped-safe-point rejection does not match the command target.
    SafePointRejectionMismatch,
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
    /// This slice cannot record position exhaustion for interrupt application.
    PositionExhaustionDeliveryUnavailable,
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
        ReconstitutedSubmitInput, SubmitInput, SubmitInputPreparationFailure,
        SubmitInputReconstitutionFailure, SubmitInputReconstitutionInput,
        SubmitInputRejectedResult, SubmitInputResult,
    };
    use crate::test_support::{accepted_input_id, alias, command_id, direct, session_id, turn_id};
    use crate::test_support::{context_frontier_id, semantic_transcript_entry_id, turn_attempt_id};
    use crate::{
        AcceptedInputDisposition, AcceptedInputLifecycle, AcceptedInputQueueOrder,
        AcceptedInputSchedulingReconstitutionInput, AcceptedInputStartingLineage,
        AcceptedInputTurnSchedulingProjection, AcceptedInputTurnSchedulingRecord,
        AcceptedInputTurnSchedulingRecordState, ActiveTurnPhase,
        ActiveTurnSchedulingReconstitutionInput, Actor, CurrentTurnAttempt, DeliveryRequest,
        FrozenAliasDefinition, FrozenModelSelection, InitialSemanticTranscriptEntryPayload,
        ModelSelectionOverride, ModelSelectionRequest, OriginConfiguration,
        PerInputConfigurationChoices, ResolvedContextFrontierReconstitutionInput,
        SemanticTranscriptEntryReconstitutionInput, SemanticTranscriptEntryRef, Session,
        SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionCreationCause,
        SessionCreationProvenance, SessionInputPosition, SessionReconstitutionInput,
        SteeringBinding, TranscriptAncestry, UserContent,
        applied_interrupt::test_applied_interrupt_proof,
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

    fn after_command(command: u128, expected_active_turn: u128) -> SubmitInput {
        SubmitInput::new(
            command_id(command),
            session_id(1),
            content("hello"),
            DeliveryRequest::AfterCurrentTurn {
                expected_active_turn: turn_id(expected_active_turn),
                configuration: choices(1, ModelSelectionOverride::UseSessionDefault),
            },
        )
    }

    fn safe_point_command(command: u128, expected_active_turn: u128) -> SubmitInput {
        SubmitInput::new(
            command_id(command),
            session_id(1),
            content("hello"),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: turn_id(expected_active_turn),
            },
        )
    }

    fn interrupt_command(command: u128, expected_active_turn: u128) -> SubmitInput {
        SubmitInput::new(
            command_id(command),
            session_id(1),
            content("hello"),
            DeliveryRequest::Interrupt {
                expected_active_turn: turn_id(expected_active_turn),
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

    fn active_turn(current: &Session) -> AcceptedInputTurnSchedulingProjection {
        active_turn_with_phase(
            current,
            ActiveTurnPhase::Running {
                current_attempt: CurrentTurnAttempt::prepared(turn_attempt_id(0x51)),
            },
        )
    }

    fn active_turn_with_phase(
        current: &Session,
        phase: ActiveTurnPhase,
    ) -> AcceptedInputTurnSchedulingProjection {
        active_turn_at_position(current, SessionInputPosition::first(), phase)
    }

    fn active_turn_at_position(
        current: &Session,
        position: SessionInputPosition,
        phase: ActiveTurnPhase,
    ) -> AcceptedInputTurnSchedulingProjection {
        let origin_entry = semantic_transcript_entry_id(0x31);
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
                AcceptedInputQueueOrder::ordinary(position),
                origin_configuration(current),
                AcceptedInputTurnSchedulingRecordState::Active {
                    starting_lineage: AcceptedInputStartingLineage::FirstInSession,
                    starting_frontier: context_frontier_id(0x41),
                    phase: ActiveTurnSchedulingReconstitutionInput::new(turn_id(7), phase),
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
        )
        .reconstitute()
        .expect("test active scheduling facts are complete")
        .active_turn()
        .expect("the test turn owns the active slot")
        .clone()
    }

    fn queued_turn(current: &Session) -> AcceptedInputTurnSchedulingProjection {
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
                origin_configuration(current),
                AcceptedInputTurnSchedulingRecordState::Queued,
            )],
            vec![],
            vec![],
        )
        .reconstitute()
        .expect("test queued scheduling facts are complete")
        .turn(turn_id(7))
        .expect("the test turn is present")
        .clone()
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

    fn after_applied_input() -> SubmitInputReconstitutionInput {
        let command = after_command(1, 7);
        SubmitInputReconstitutionInput::applied_turn_origin(
            command.clone(),
            Actor::Owner,
            session_id(1),
            accepted_input_id(3),
            turn_id(8),
            command_id(1),
            accepted_input_id(3),
            session_id(1),
            content("hello"),
            command.delivery(),
            SessionInputPosition::first(),
            AcceptedInputDisposition::OriginOf(turn_id(8)),
            session_id(1),
            turn_id(8),
            AcceptedInputQueueOrder::ordinary(SessionInputPosition::first()),
            session_id(1),
            version(1),
            defaults(ModelSelectionRequest::Direct(direct(2))),
            ModelSelectionRequest::Direct(direct(2)),
            FrozenModelSelection::Direct(direct(2)),
        )
    }

    fn source_turn_origin() -> ReconstitutedSubmitInput {
        source_turn_origin_with_identities(0x70, 0x71)
    }

    fn source_turn_origin_with_identities(
        source_command: u128,
        source_accepted_input: u128,
    ) -> ReconstitutedSubmitInput {
        let command = start_command(source_command, "source", 1);
        SubmitInputReconstitutionInput::applied_turn_origin(
            command.clone(),
            Actor::Owner,
            session_id(1),
            accepted_input_id(source_accepted_input),
            turn_id(7),
            command_id(source_command),
            accepted_input_id(source_accepted_input),
            session_id(1),
            content("source"),
            command.delivery(),
            SessionInputPosition::first(),
            AcceptedInputDisposition::OriginOf(turn_id(7)),
            session_id(1),
            turn_id(7),
            AcceptedInputQueueOrder::ordinary(SessionInputPosition::first()),
            session_id(1),
            version(1),
            defaults(ModelSelectionRequest::Direct(direct(2))),
            ModelSelectionRequest::Direct(direct(2)),
            FrozenModelSelection::Direct(direct(2)),
        )
        .reconstitute()
        .expect("the source turn origin facts are complete")
    }

    fn pending_steering_input() -> SubmitInputReconstitutionInput {
        let command = safe_point_command(1, 7);
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
            AcceptedInputDisposition::PendingSteering {
                binding: SteeringBinding::new(turn_id(7)),
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

    /// S01 / INV-012 / ADR-0039: comparison excludes only command identity and
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
        let SubmitInputResult::Applied(applied) = prepared.result() else {
            panic!("matching start request applies");
        };
        let applied = applied
            .turn_origin()
            .expect("start acceptance creates turn-origin work");
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
        let SubmitInputResult::Applied(applied) = frozen.result() else {
            panic!("selectable alias applies");
        };
        let applied = applied
            .turn_origin()
            .expect("start acceptance creates turn-origin work");
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

    /// S01 / INV-012 / INV-028: active-work variants record the exact
    /// expected turn in a no-active-turn rejection.
    #[test]
    fn s01_inv012_inv028_active_modes_reject_when_no_turn_is_active() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let configuration = choices(1, ModelSelectionOverride::UseSessionDefault);
        for (delivery, turn_candidate) in [
            (
                DeliveryRequest::Interrupt {
                    expected_active_turn: turn_id(7),
                    configuration,
                },
                Some(turn_id(4)),
            ),
            (
                DeliveryRequest::NextSafePoint {
                    expected_active_turn: turn_id(7),
                },
                None,
            ),
            (
                DeliveryRequest::AfterCurrentTurn {
                    expected_active_turn: turn_id(7),
                    configuration,
                },
                Some(turn_id(4)),
            ),
        ] {
            let prepared =
                SubmitInput::new(command_id(1), session_id(1), content("hello"), delivery)
                    .prepare_when_no_active_turn(
                        &current,
                        accepted_input_id(3),
                        turn_candidate,
                        None,
                        |_| panic!("active-work rejection does not resolve configuration"),
                    )
                    .expect("session matches");
            assert!(matches!(
                prepared.result(),
                SubmitInputResult::Rejected(SubmitInputRejectedResult::NoActiveTurn {
                    session,
                    expected_active_turn,
                }) if *session == session_id(1) && *expected_active_turn == turn_id(7)
            ));
        }

        let mismatch = SubmitInput::new(
            command_id(1),
            session_id(1),
            content("hello"),
            DeliveryRequest::NextSafePoint {
                expected_active_turn: turn_id(7),
            },
        )
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
    fn s09_matching_after_current_prepares_ordinary_turn_origin() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let command = after_command(1, 7);
        let prepared = command
            .clone()
            .prepare_with_active_turn(
                &current,
                &active_turn(&current),
                accepted_input_id(3),
                Some(turn_id(8)),
                Some(SessionInputPosition::first()),
                |_| None,
            )
            .expect("matching after-current input is available");

        let SubmitInputResult::Applied(applied) = prepared.result() else {
            panic!("matching after-current input applies");
        };
        let origin = applied
            .turn_origin()
            .expect("after-current input creates origin work");
        assert_eq!(origin.accepted_input(), accepted_input_id(3));
        assert_eq!(origin.turn(), turn_id(8));
        assert_eq!(
            origin.disposition(),
            AcceptedInputDisposition::OriginOf(turn_id(8))
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
    fn s08_matching_next_safe_point_prepares_pending_steering() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let prepared = safe_point_command(1, 7)
            .prepare_with_active_turn(
                &current,
                &active_turn(&current),
                accepted_input_id(3),
                None,
                Some(SessionInputPosition::first()),
                |_| panic!("safe-point acceptance has no configuration"),
            )
            .expect("matching safe-point input is available");

        let SubmitInputResult::Applied(applied) = prepared.result() else {
            panic!("matching safe-point input applies");
        };
        assert_eq!(applied.accepted_input(), accepted_input_id(3));
        assert_eq!(applied.acceptance_position().as_u64(), 2);
        assert_eq!(
            applied.disposition(),
            AcceptedInputDisposition::PendingSteering {
                binding: SteeringBinding::new(turn_id(7)),
            }
        );
        assert!(applied.turn_origin().is_none());
        let steering = applied
            .pending_steering()
            .expect("safe-point acceptance creates pending steering");
        assert_eq!(steering.binding().source_turn(), turn_id(7));
    }

    /// S01 / S08 / S09 / INV-012 / INV-016 / INV-028: occupied-slot
    /// validation records exact active/stale/stopping rejections, while a
    /// matching interrupt remains nonclaiming until interrupt application is
    /// implemented.
    #[test]
    fn occupied_slot_rejections_and_interrupt_unavailability_are_exact() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let active = active_turn(&current);

        let start = start_command(1, "hello", 1)
            .prepare_with_active_turn(
                &current,
                &active,
                accepted_input_id(3),
                Some(turn_id(8)),
                Some(SessionInputPosition::first()),
                |_| None,
            )
            .expect("active presence is an authoritative rejection");
        assert!(matches!(
            start.result(),
            SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnPresent {
                session,
                active_turn,
            }) if *session == session_id(1) && *active_turn == turn_id(7)
        ));

        for (command, candidate) in [
            (after_command(2, 9), Some(turn_id(8))),
            (safe_point_command(3, 9), None),
            (interrupt_command(4, 9), Some(turn_id(8))),
        ] {
            let stale = command
                .prepare_with_active_turn(
                    &current,
                    &active,
                    accepted_input_id(3),
                    candidate,
                    None,
                    |_| None,
                )
                .expect("a stale active target is an authoritative rejection");
            assert!(matches!(
                stale.result(),
                SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnMismatch {
                    expected_active_turn,
                    actual_active_turn,
                    ..
                }) if *expected_active_turn == turn_id(9) && *actual_active_turn == turn_id(7)
            ));
        }

        let stopping_attempt = CurrentTurnAttempt::prepared(turn_attempt_id(0x52))
            .begin_running()
            .expect("prepared test attempt begins running")
            .request_cancellation(test_applied_interrupt_proof(command_id(9), turn_id(7)))
            .expect("running test attempt accepts its first cancellation");
        let stopping = active_turn_with_phase(
            &current,
            ActiveTurnPhase::Running {
                current_attempt: stopping_attempt,
            },
        );
        let stopped_safe_point = safe_point_command(5, 7)
            .prepare_with_active_turn(
                &current,
                &stopping,
                accepted_input_id(3),
                None,
                None,
                |_| None,
            )
            .expect("stopping is an authoritative safe-point rejection");
        assert!(matches!(
            stopped_safe_point.result(),
            SubmitInputResult::Rejected(
                SubmitInputRejectedResult::SafePointUnavailableWhileStopping {
                    active_turn,
                    ..
                }
            ) if *active_turn == turn_id(7)
        ));

        let interrupt = interrupt_command(6, 7);
        let unavailable = interrupt
            .clone()
            .prepare_with_active_turn(
                &current,
                &active,
                accepted_input_id(3),
                Some(turn_id(8)),
                Some(SessionInputPosition::first()),
                |_| None,
            )
            .expect_err("interrupt application cannot claim a command in this slice");
        assert_eq!(
            unavailable.failure(),
            SubmitInputPreparationFailure::InterruptApplicationUnavailable
        );
        assert_eq!(unavailable.command(), &interrupt);
    }

    /// S08 / S09 / INV-008 / INV-012 / INV-028: the occupied-slot
    /// configuration and position checks record the same exact terminal
    /// failures for the delivery modes that reach them.
    #[test]
    fn occupied_slot_configuration_and_position_rejections_are_typed() {
        let stale_session = session(1, 2, ModelSelectionRequest::Direct(direct(2)));
        let stale = after_command(1, 7)
            .prepare_with_active_turn(
                &stale_session,
                &active_turn(&stale_session),
                accepted_input_id(3),
                Some(turn_id(8)),
                Some(SessionInputPosition::first()),
                |_| panic!("stale defaults cannot reach alias resolution"),
            )
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

        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let alias_command = SubmitInput::new(
            command_id(2),
            session_id(1),
            content("hello"),
            DeliveryRequest::AfterCurrentTurn {
                expected_active_turn: turn_id(7),
                configuration: choices(
                    1,
                    ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Alias(alias(9))),
                ),
            },
        );
        let unknown_alias = alias_command
            .prepare_with_active_turn(
                &current,
                &active_turn(&current),
                accepted_input_id(3),
                Some(turn_id(8)),
                Some(SessionInputPosition::first()),
                |_| None,
            )
            .expect("an unresolved alias is an authoritative rejection");
        assert!(matches!(
            unknown_alias.result(),
            SubmitInputResult::Rejected(SubmitInputRejectedResult::UnknownModelAlias {
                alias: unknown,
                ..
            }) if *unknown == alias(9)
        ));

        let maximum = SessionInputPosition::try_from_u64(u64::MAX).expect("positive maximum");
        for (command, candidate) in [
            (after_command(3, 7), Some(turn_id(8))),
            (safe_point_command(4, 7), None),
        ] {
            let exhausted = command
                .prepare_with_active_turn(
                    &current,
                    &active_turn(&current),
                    accepted_input_id(3),
                    candidate,
                    Some(maximum),
                    |_| None,
                )
                .expect("position exhaustion is an authoritative rejection");
            assert!(matches!(
                exhausted.result(),
                SubmitInputResult::Rejected(
                    SubmitInputRejectedResult::AcceptancePositionExhausted { last, .. }
                ) if *last == maximum
            ));
        }
    }

    /// INV-002 / INV-012: every occupied-slot correlation failure retains the
    /// unchanged command and constructs no terminal result.
    #[test]
    fn occupied_slot_preparation_rejects_cross_wired_projection_and_candidates() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let command = after_command(1, 7);
        let wrong_session = session(2, 1, ModelSelectionRequest::Direct(direct(2)));

        let wrong_active_session = command
            .clone()
            .prepare_with_active_turn(
                &current,
                &active_turn(&wrong_session),
                accepted_input_id(3),
                Some(turn_id(8)),
                None,
                |_| None,
            )
            .expect_err("a cross-session active projection is nonterminal");
        assert_eq!(
            wrong_active_session.failure(),
            SubmitInputPreparationFailure::ActiveTurnSessionMismatch {
                provided_session: session_id(2),
            }
        );

        let not_active = command
            .clone()
            .prepare_with_active_turn(
                &current,
                &queued_turn(&current),
                accepted_input_id(3),
                Some(turn_id(8)),
                None,
                |_| None,
            )
            .expect_err("a queued projection cannot stand in for the active turn");
        assert_eq!(
            not_active.failure(),
            SubmitInputPreparationFailure::ActiveTurnProjectionIsNotActive {
                provided_turn: turn_id(7),
            }
        );

        let missing_turn = command
            .clone()
            .prepare_with_active_turn(
                &current,
                &active_turn(&current),
                accepted_input_id(3),
                None,
                None,
                |_| None,
            )
            .expect_err("after-current input requires a minted turn candidate");
        assert_eq!(
            missing_turn.failure(),
            SubmitInputPreparationFailure::TurnCandidateMismatch
        );

        let extra_turn = safe_point_command(2, 7)
            .prepare_with_active_turn(
                &current,
                &active_turn(&current),
                accepted_input_id(3),
                Some(turn_id(8)),
                None,
                |_| None,
            )
            .expect_err("safe-point input cannot receive a turn candidate");
        assert_eq!(
            extra_turn.failure(),
            SubmitInputPreparationFailure::TurnCandidateMismatch
        );

        for (active, observed_tail) in [
            (active_turn(&current), None),
            (
                active_turn_at_position(
                    &current,
                    SessionInputPosition::try_from_u64(2).expect("positive position"),
                    ActiveTurnPhase::Running {
                        current_attempt: CurrentTurnAttempt::prepared(turn_attempt_id(0x51)),
                    },
                ),
                Some(SessionInputPosition::first()),
            ),
        ] {
            let error = command
                .clone()
                .prepare_with_active_turn(
                    &current,
                    &active,
                    accepted_input_id(3),
                    Some(turn_id(8)),
                    observed_tail,
                    |_| None,
                )
                .expect_err("the complete tail must include the active origin");
            assert!(matches!(
                error.failure(),
                SubmitInputPreparationFailure::AcceptanceTailPrecedesActiveOrigin {
                    active_turn,
                    active_position,
                    observed_tail: actual,
                } if active_turn == turn_id(7)
                    && active_position == active.order().acceptance_position()
                    && actual == observed_tail
            ));
        }
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
        let SubmitInputResult::Applied(applied) = reconstructed.result() else {
            panic!("applied facts reconstruct an applied result");
        };
        let applied = applied
            .turn_origin()
            .expect("turn-origin facts reconstruct turn-origin acceptance");
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

    /// S08 / S09 / INV-002 / INV-012 / INV-016: both new applied shapes
    /// reconstruct only from their exact delivery, position, disposition,
    /// configuration, and source-turn correlations.
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

        let mut wrong_delivery = pending_steering_input();
        wrong_delivery.command = start_command(1, "hello", 1);
        assert_eq!(
            wrong_delivery
                .reconstitute()
                .expect_err("pending facts require a safe-point command")
                .failure(),
            SubmitInputReconstitutionFailure::AppliedDeliveryIsNotNextSafePoint
        );

        let mut wrong_source = pending_steering_input();
        pending_facts(&mut wrong_source).result_source_turn = turn_id(9);
        assert_eq!(
            wrong_source
                .reconstitute()
                .expect_err("the result source must match the command target")
                .failure(),
            SubmitInputReconstitutionFailure::SteeringSourceTurnMismatch
        );

        let mut wrong_source_origin = pending_steering_input();
        pending_facts(&mut wrong_source_origin).source_turn_origin = after_applied_input()
            .reconstitute()
            .expect("the cross-wired origin is independently canonical");
        assert_eq!(
            wrong_source_origin
                .reconstitute()
                .expect_err("the source origin must establish the exact source turn")
                .failure(),
            SubmitInputReconstitutionFailure::SteeringSourceTurnOriginMismatch
        );

        let mut wrong_position = pending_steering_input();
        pending_facts(&mut wrong_position).accepted_position = SessionInputPosition::first();
        assert_eq!(
            wrong_position
                .reconstitute()
                .expect_err("pending steering must follow its source origin")
                .failure(),
            SubmitInputReconstitutionFailure::SteeringAcceptanceDoesNotFollowSourceOrigin
        );

        let mut wrong_disposition = pending_steering_input();
        pending_facts(&mut wrong_disposition).accepted_disposition =
            AcceptedInputDisposition::PendingSteering {
                binding: SteeringBinding::new(turn_id(9)),
            };
        assert_eq!(
            wrong_disposition
                .reconstitute()
                .expect_err("the accepted disposition must retain the exact binding")
                .failure(),
            SubmitInputReconstitutionFailure::AcceptedDispositionMismatch
        );
    }

    /// S09 / INV-012: after-current replay cannot reuse the active predecessor
    /// as its claimed new turn, even when every result/queue field agrees.
    #[test]
    fn s09_inv012_after_reconstitution_rejects_active_turn_identity_reuse() {
        let mut self_successor = after_applied_input();
        let facts = applied_facts(&mut self_successor);
        facts.result_turn = turn_id(7);
        facts.accepted_disposition = AcceptedInputDisposition::OriginOf(turn_id(7));
        facts.queue_turn = turn_id(7);

        assert_eq!(
            self_successor
                .reconstitute()
                .expect_err("after-current work cannot reuse its active predecessor")
                .failure(),
            SubmitInputReconstitutionFailure::QueueTurnMismatch
        );
    }

    /// S08 / INV-012: pending-steering replay cannot reuse either owner-global
    /// identity from its canonical source origin.
    #[test]
    fn s08_inv012_pending_steering_rejects_source_identity_reuse() {
        for source_turn_origin in [
            source_turn_origin_with_identities(0x70, 3),
            source_turn_origin_with_identities(1, 0x71),
        ] {
            let mut reused_identity = pending_steering_input();
            pending_facts(&mut reused_identity).source_turn_origin = source_turn_origin;
            assert_eq!(
                reused_identity
                    .reconstitute()
                    .expect_err("source origin and pending steering must have distinct identities")
                    .failure(),
                SubmitInputReconstitutionFailure::SteeringSourceTurnOriginMismatch
            );
        }
    }

    /// One cross-wiring mutation and the exact failure it must produce.
    type CrossWiredCase = (
        fn(&mut SubmitInputReconstitutionInput),
        SubmitInputReconstitutionFailure,
    );

    /// INV-002 / INV-012 / ADR-0039: every applied-path reconstitution
    /// failure variant is reachable from exactly one cross-wired fact and
    /// fails closed instead of constructing authority.
    #[test]
    fn inv002_inv012_applied_reconstitution_rejects_every_cross_wired_fact() {
        let cases: [CrossWiredCase; 17] = [
            (
                |input| input.stored_actor = Actor::Recovery,
                SubmitInputReconstitutionFailure::StoredActorMismatch,
            ),
            (
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
            ),
            (
                |input| applied_facts(input).result_session = session_id(2),
                SubmitInputReconstitutionFailure::ResultSessionMismatch,
            ),
            (
                |input| applied_facts(input).accepted_command = command_id(2),
                SubmitInputReconstitutionFailure::AcceptedCommandMismatch,
            ),
            (
                |input| applied_facts(input).accepted_input = accepted_input_id(9),
                SubmitInputReconstitutionFailure::AcceptedInputMismatch,
            ),
            (
                |input| applied_facts(input).accepted_session = session_id(2),
                SubmitInputReconstitutionFailure::AcceptedSessionMismatch,
            ),
            (
                |input| applied_facts(input).accepted_content = content("different"),
                SubmitInputReconstitutionFailure::AcceptedContentMismatch,
            ),
            (
                |input| {
                    applied_facts(input).accepted_delivery =
                        DeliveryRequest::StartWhenNoActiveTurn {
                            configuration: choices(2, ModelSelectionOverride::UseSessionDefault),
                        };
                },
                SubmitInputReconstitutionFailure::AcceptedDeliveryMismatch,
            ),
            (
                |input| {
                    applied_facts(input).accepted_disposition =
                        AcceptedInputDisposition::OriginOf(turn_id(9));
                },
                SubmitInputReconstitutionFailure::AcceptedDispositionMismatch,
            ),
            (
                |input| applied_facts(input).queue_session = session_id(2),
                SubmitInputReconstitutionFailure::QueueSessionMismatch,
            ),
            (
                |input| applied_facts(input).queue_turn = turn_id(9),
                SubmitInputReconstitutionFailure::QueueTurnMismatch,
            ),
            (
                |input| {
                    applied_facts(input).queue_order = AcceptedInputQueueOrder::ordinary(
                        SessionInputPosition::first()
                            .checked_next()
                            .expect("the second position exists"),
                    );
                },
                SubmitInputReconstitutionFailure::QueuePositionMismatch,
            ),
            (
                |input| {
                    applied_facts(input).queue_order =
                        crate::AcceptedInputQueueOrder::interrupt_immediately_after(
                            SessionInputPosition::first(),
                            turn_id(9),
                        );
                },
                SubmitInputReconstitutionFailure::QueuePriorityMismatch,
            ),
            (
                |input| applied_facts(input).defaults_session = session_id(2),
                SubmitInputReconstitutionFailure::DefaultsSessionMismatch,
            ),
            (
                |input| applied_facts(input).defaults_version = version(2),
                SubmitInputReconstitutionFailure::DefaultsVersionMismatch,
            ),
            (
                |input| {
                    applied_facts(input).stored_requested_model =
                        ModelSelectionRequest::Direct(direct(9));
                },
                SubmitInputReconstitutionFailure::RequestedModelMismatch,
            ),
            (
                |input| {
                    applied_facts(input).stored_frozen_model =
                        FrozenModelSelection::Direct(direct(9));
                },
                SubmitInputReconstitutionFailure::FrozenModelMismatch,
            ),
        ];

        for (mutate, expected) in cases {
            let mut wrong = applied_input();
            mutate(&mut wrong);
            assert_eq!(
                wrong
                    .reconstitute()
                    .expect_err("one cross-wired applied fact fails closed")
                    .failure(),
                expected
            );
        }
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
            )
            .reconstitute()
            .expect_err("a different expected version fails closed")
            .failure(),
            SubmitInputReconstitutionFailure::ExpectedDefaultsVersionMismatch
        );
    }

    /// INV-012 / ADR-0039: every rejected-path reconstitution failure
    /// variant is reachable from exactly one cross-wired fact and fails
    /// closed, while each matching typed projection reconstructs its
    /// recorded rejection.
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

        let cross_wired_sessions = [
            SubmitInputReconstitutionInput::rejected_session_not_found(
                start.clone(),
                Actor::Owner,
                session_id(2),
            ),
            SubmitInputReconstitutionInput::rejected_no_active_turn(
                safe_point.clone(),
                Actor::Owner,
                session_id(2),
                turn_id(7),
            ),
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                start.clone(),
                Actor::Owner,
                session_id(2),
                version(1),
                version(2),
            ),
            SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                start.clone(),
                Actor::Owner,
                session_id(2),
                alias(3),
                session_id(1),
                version(1),
                defaults(ModelSelectionRequest::Direct(direct(2))),
            ),
            SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
                start.clone(),
                Actor::Owner,
                session_id(2),
                maximum,
            ),
        ];
        for input in cross_wired_sessions {
            assert_eq!(
                input
                    .reconstitute()
                    .expect_err("another result session fails closed")
                    .failure(),
                SubmitInputReconstitutionFailure::ResultSessionMismatch
            );
        }

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

        SubmitInputReconstitutionInput::rejected_active_turn_present(
            start.clone(),
            Actor::Owner,
            session_id(1),
            turn_id(7),
        )
        .reconstitute()
        .expect("a start request can record the exact active slot owner");
        assert_eq!(
            SubmitInputReconstitutionInput::rejected_active_turn_present(
                safe_point.clone(),
                Actor::Owner,
                session_id(1),
                turn_id(7),
            )
            .reconstitute()
            .expect_err("only start can record active-turn-present")
            .failure(),
            SubmitInputReconstitutionFailure::RejectionDeliveryIsNotStart
        );

        let after = after_command(2, 7);
        SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
            after.clone(),
            Actor::Owner,
            session_id(1),
            turn_id(7),
            turn_id(9),
        )
        .reconstitute()
        .expect("different expected and actual active turns reconstruct");
        assert_eq!(
            SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
                after,
                Actor::Owner,
                session_id(1),
                turn_id(7),
                turn_id(7),
            )
            .reconstitute()
            .expect_err("equal turns are not a stale-active rejection")
            .failure(),
            SubmitInputReconstitutionFailure::RejectedActiveTurnsAreEqual
        );

        SubmitInputReconstitutionInput::rejected_safe_point_unavailable_while_stopping(
            safe_point.clone(),
            Actor::Owner,
            session_id(1),
            turn_id(7),
        )
        .reconstitute()
        .expect("the exact stopped safe-point target reconstructs");
        assert_eq!(
            SubmitInputReconstitutionInput::rejected_safe_point_unavailable_while_stopping(
                safe_point.clone(),
                Actor::Owner,
                session_id(1),
                turn_id(9),
            )
            .reconstitute()
            .expect_err("another stopped turn does not match the command")
            .failure(),
            SubmitInputReconstitutionFailure::SafePointRejectionMismatch
        );

        assert_eq!(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                safe_point,
                Actor::Owner,
                session_id(1),
                version(1),
                version(2),
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
        )
        .reconstitute()
        .expect("the matching unresolved alias reconstructs");
        for (defaults_session, defaults_version, result_alias, expected) in [
            (
                session_id(2),
                version(1),
                alias(2),
                SubmitInputReconstitutionFailure::DefaultsSessionMismatch,
            ),
            (
                session_id(1),
                version(2),
                alias(2),
                SubmitInputReconstitutionFailure::DefaultsVersionMismatch,
            ),
            (
                session_id(1),
                version(1),
                alias(3),
                SubmitInputReconstitutionFailure::UnknownAliasMismatch,
            ),
        ] {
            assert_eq!(
                SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                    alias_command.clone(),
                    Actor::Owner,
                    session_id(1),
                    result_alias,
                    defaults_session,
                    defaults_version,
                    defaults(ModelSelectionRequest::Direct(direct(2))),
                )
                .reconstitute()
                .expect_err("one cross-wired unknown-alias fact fails closed")
                .failure(),
                expected
            );
        }
        assert_eq!(
            SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                start.clone(),
                Actor::Owner,
                session_id(1),
                alias(3),
                session_id(1),
                version(1),
                defaults(ModelSelectionRequest::Direct(direct(2))),
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
        )
        .reconstitute()
        .expect("the exhausted maximum position reconstructs");
        assert_eq!(
            SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
                start,
                Actor::Owner,
                session_id(1),
                SessionInputPosition::first(),
            )
            .reconstitute()
            .expect_err("a position with a successor is not exhausted")
            .failure(),
            SubmitInputReconstitutionFailure::PositionIsNotExhausted
        );
        assert_eq!(
            SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
                interrupt_command(3, 7),
                Actor::Owner,
                session_id(1),
                maximum,
            )
            .reconstitute()
            .expect_err("interrupt application is nonclaiming in this slice")
            .failure(),
            SubmitInputReconstitutionFailure::PositionExhaustionDeliveryUnavailable
        );
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
