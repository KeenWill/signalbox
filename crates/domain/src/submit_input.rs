//! Canonical durable input submission and authoritative-state preparation.
//!
//! ADR-0027 owns accepted-input delivery, configuration, ordering, and
//! disposition semantics. ADR-0034 owns structural replay equality, ADR-0035
//! owns checked reconstitution, ADR-0037 owns content, and ADR-0039 owns
//! actor attribution. This slice prepares accepted origin work with no active
//! turn or after the exact active turn, and pending steering for the exact
//! active turn. Applied receipt replay validates the complete canonical source
//! or predecessor origin without consulting mutable steering disposition.
//! Rejected replay likewise requires the canonical origin for every result
//! that names or depends on an occupied active slot. Safe-point stopping replay
//! remains closed until complete stop evidence can be supplied. This slice does
//! not consume steering, apply interruption, construct an interrupt proof,
//! transition turn lifecycle, or perform persistence.

use std::hash::{Hash, Hasher};

use crate::{
    AcceptedInputDisposition, AcceptedInputId, AcceptedInputQueueOrder, AcceptedInputQueuePriority,
    AcceptedInputSchedulingProjection, Actor, DeliveryRequest, DurableCommandId,
    FrozenAliasDefinition, FrozenModelSelection, ModelAlias, ModelSelectionRequest,
    OriginConfiguration, PerInputConfigurationChoices, Session, SessionConfigurationDefaults,
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
    /// next-safe-point input creates pending steering. Interrupt application
    /// remains a nonclaiming preparation failure, and stopping-phase handling
    /// remains closed until its complete owner projection exists.
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
        match delivery {
            DeliveryRequest::Interrupt { .. } => Err(SubmitInputPreparationError {
                command: Box::new(self),
                failure: SubmitInputPreparationFailure::InterruptApplicationUnavailable,
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
    predecessor_origin: Option<ReconstitutedSubmitInput>,
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
        active_turn_origin: ReconstitutedSubmitInput,
    },
    RejectedActiveTurnMismatch {
        result_session: SessionId,
        result_expected_active_turn: TurnId,
        result_actual_active_turn: TurnId,
        actual_turn_origin: ReconstitutedSubmitInput,
    },
    RejectedDefaultsVersionMismatch {
        result_session: SessionId,
        result_expected: SessionConfigurationDefaultsVersion,
        result_current: SessionConfigurationDefaultsVersion,
        active_turn_origin: Option<ReconstitutedSubmitInput>,
    },
    RejectedUnknownModelAlias {
        result_session: SessionId,
        result_alias: ModelAlias,
        defaults_session: SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
        active_turn_origin: Option<ReconstitutedSubmitInput>,
    },
    RejectedAcceptancePositionExhausted {
        result_session: SessionId,
        result_last_position: SessionInputPosition,
        active_turn_origin: Option<ReconstitutedSubmitInput>,
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
        predecessor_origin: Option<ReconstitutedSubmitInput>,
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
        source_turn_origin: ReconstitutedSubmitInput,
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
        active_turn_origin: ReconstitutedSubmitInput,
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
        actual_turn_origin: ReconstitutedSubmitInput,
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
        active_turn_origin: Option<ReconstitutedSubmitInput>,
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
        active_turn_origin: Option<ReconstitutedSubmitInput>,
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
        active_turn_origin: Option<ReconstitutedSubmitInput>,
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
                let expected_predecessor = match self.command.delivery {
                    DeliveryRequest::StartWhenNoActiveTurn { .. } => None,
                    DeliveryRequest::AfterCurrentTurn {
                        expected_active_turn,
                        ..
                    } => {
                        if expected_active_turn == result_turn {
                            return Err(fail(SubmitInputReconstitutionFailure::QueueTurnMismatch));
                        }
                        Some(expected_active_turn)
                    }
                    DeliveryRequest::Interrupt { .. } | DeliveryRequest::NextSafePoint { .. } => {
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
                if queue_order.priority() != AcceptedInputQueuePriority::Ordinary {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::QueuePriorityMismatch,
                    ));
                }
                match (expected_predecessor, predecessor_origin) {
                    (None, None) => {}
                    (Some(expected_predecessor), Some(predecessor_origin)) => {
                        let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(
                            predecessor,
                        )) = predecessor_origin.result()
                        else {
                            return Err(fail(
                                SubmitInputReconstitutionFailure::AfterCurrentPredecessorOriginMismatch,
                            ));
                        };
                        if predecessor.session() != self.command.session
                            || predecessor.turn() != expected_predecessor
                        {
                            return Err(fail(
                                SubmitInputReconstitutionFailure::AfterCurrentPredecessorOriginMismatch,
                            ));
                        }
                        if predecessor.accepted_input() == accepted_input {
                            return Err(fail(
                                SubmitInputReconstitutionFailure::AfterCurrentPredecessorAcceptedInputReused,
                            ));
                        }
                        if predecessor_origin.command().command_id() == accepted_command {
                            return Err(fail(
                                SubmitInputReconstitutionFailure::AfterCurrentPredecessorCommandReused,
                            ));
                        }
                        if accepted_position <= predecessor.acceptance_position() {
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
                if source_origin.accepted_input() == accepted_input {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::SteeringSourceAcceptedInputReused,
                    ));
                }
                if source_turn_origin.command().command_id() == accepted_command {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::SteeringSourceCommandReused,
                    ));
                }
                if accepted_position <= source_origin.acceptance_position() {
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

fn rejection_configuration(
    delivery: DeliveryRequest,
) -> Result<(PerInputConfigurationChoices, Option<TurnId>), SubmitInputReconstitutionFailure> {
    match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { configuration } => Ok((configuration, None)),
        DeliveryRequest::AfterCurrentTurn {
            expected_active_turn,
            configuration,
        } => Ok((configuration, Some(expected_active_turn))),
        DeliveryRequest::Interrupt { .. } => {
            Err(SubmitInputReconstitutionFailure::InterruptConfigurationRejectionUnavailable)
        }
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
        | DeliveryRequest::AfterCurrentTurn {
            expected_active_turn,
            ..
        } => Ok(Some(expected_active_turn)),
        DeliveryRequest::Interrupt { .. } => {
            Err(SubmitInputReconstitutionFailure::PositionExhaustionDeliveryUnavailable)
        }
    }
}

fn validate_rejection_active_turn_origin(
    command: &SubmitInput,
    expected_turn: Option<TurnId>,
    origin: Option<&ReconstitutedSubmitInput>,
) -> Result<(), SubmitInputReconstitutionFailure> {
    match (expected_turn, origin) {
        (None, None) => Ok(()),
        (Some(expected_turn), Some(origin)) => {
            let SubmitInputResult::Applied(SubmitInputAppliedResult::TurnOrigin(result)) =
                origin.result()
            else {
                return Err(SubmitInputReconstitutionFailure::RejectionActiveTurnOriginMismatch);
            };
            if result.session() != command.session || result.turn() != expected_turn {
                return Err(SubmitInputReconstitutionFailure::RejectionActiveTurnOriginMismatch);
            }
            if origin.command().command_id() == command.command_id {
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
    /// Matching interrupt cannot record a configuration rejection in this
    /// milestone.
    InterruptConfigurationRejectionUnavailable,
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
    /// Interrupt application cannot record position exhaustion in this
    /// milestone.
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
        ReconstitutedSubmitInput, SubmitInput, SubmitInputAppliedResult,
        SubmitInputPreparationFailure, SubmitInputReconstitutionFailure,
        SubmitInputReconstitutionInput, SubmitInputRejectedResult, SubmitInputResult,
    };
    use crate::test_support::{accepted_input_id, alias, command_id, direct, session_id, turn_id};
    use crate::test_support::{context_frontier_id, semantic_transcript_entry_id, turn_attempt_id};
    use crate::{
        AcceptedInputDisposition, AcceptedInputLifecycle, AcceptedInputQueueOrder,
        AcceptedInputSchedulingProjection, AcceptedInputSchedulingReconstitutionInput,
        AcceptedInputStartingLineage, AcceptedInputTurnSchedulingRecord,
        AcceptedInputTurnSchedulingRecordState, ActiveTurnSchedulingReconstitutionInput, Actor,
        DeliveryRequest, FrozenAliasDefinition, FrozenModelSelection,
        InitialSemanticTranscriptEntryPayload, ModelSelectionOverride, ModelSelectionRequest,
        OriginConfiguration, PerInputConfigurationChoices,
        ResolvedContextFrontierReconstitutionInput, SemanticTranscriptEntryReconstitutionInput,
        SemanticTranscriptEntryRef, Session, SessionAcceptanceTailEntryReconstitutionInput,
        SessionAcceptanceTailReconstitutionInput, SessionConfigurationDefaults,
        SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
        SessionInputPosition, SessionReconstitutionInput, SteeringBinding, TranscriptAncestry,
        UserContent,
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
            None,
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

    fn after_applied_input() -> SubmitInputReconstitutionInput {
        let command = after_command(1, 7);
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
                &active_turn(&current),
                accepted_input_id(3),
                Some(turn_id(8)),
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
            .prepare_with_active_turn(&active_turn(&current), accepted_input_id(3), None, |_| {
                panic!("safe-point acceptance has no configuration")
            })
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

    /// S01 / INV-012 / INV-028: a vacant-slot start submitted while the slot
    /// is occupied records the exact authoritative active turn.
    #[test]
    fn occupied_slot_start_records_active_turn_presence() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let active = active_turn(&current);
        let start = start_command(1, "hello", 1)
            .prepare_with_active_turn(&active, accepted_input_id(3), Some(turn_id(8)), |_| None)
            .expect("active presence is an authoritative rejection");
        assert!(matches!(
            start.result(),
            SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnPresent {
                session,
                active_turn,
            }) if *session == session_id(1) && *active_turn == turn_id(7)
        ));
    }

    /// S07 / S08 / S09 / INV-012 / INV-028: every active-work delivery mode
    /// records its stale target against the exact authoritative active turn.
    #[test]
    fn occupied_slot_active_work_records_stale_target() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let active = active_turn(&current);

        let stale_after = after_command(2, 9)
            .prepare_with_active_turn(&active, accepted_input_id(3), Some(turn_id(8)), |_| None)
            .expect("a stale after-current target is an authoritative rejection");
        assert!(matches!(
            stale_after.result(),
            SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnMismatch {
                expected_active_turn,
                actual_active_turn,
                ..
            }) if *expected_active_turn == turn_id(9) && *actual_active_turn == turn_id(7)
        ));

        let stale_safe_point = safe_point_command(3, 9)
            .prepare_with_active_turn(&active, accepted_input_id(3), None, |_| None)
            .expect("a stale safe-point target is an authoritative rejection");
        assert!(matches!(
            stale_safe_point.result(),
            SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnMismatch {
                expected_active_turn,
                actual_active_turn,
                ..
            }) if *expected_active_turn == turn_id(9) && *actual_active_turn == turn_id(7)
        ));

        let stale_interrupt = interrupt_command(4, 9)
            .prepare_with_active_turn(&active, accepted_input_id(3), Some(turn_id(8)), |_| None)
            .expect("a stale interrupt target is an authoritative rejection");
        assert!(matches!(
            stale_interrupt.result(),
            SubmitInputResult::Rejected(SubmitInputRejectedResult::ActiveTurnMismatch {
                expected_active_turn,
                actual_active_turn,
                ..
            }) if *expected_active_turn == turn_id(9) && *actual_active_turn == turn_id(7)
        ));
    }

    /// S07 / INV-012 / INV-028: a matching interrupt remains nonclaiming until
    /// its correlated application boundary exists.
    #[test]
    fn occupied_slot_matching_interrupt_remains_nonclaiming() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let active = active_turn(&current);
        let interrupt = interrupt_command(6, 7);
        let unavailable = interrupt
            .clone()
            .prepare_with_active_turn(&active, accepted_input_id(3), Some(turn_id(8)), |_| None)
            .expect_err("interrupt application cannot claim a command in this slice");
        assert_eq!(
            unavailable.failure(),
            SubmitInputPreparationFailure::InterruptApplicationUnavailable
        );
        assert_eq!(unavailable.command(), &interrupt);
    }

    /// S09 / INV-008 / INV-012 / INV-028: after-current preparation records
    /// the exact stale session-defaults version.
    #[test]
    fn occupied_slot_after_current_records_stale_defaults_version() {
        let stale_session = session(1, 2, ModelSelectionRequest::Direct(direct(2)));
        let stale = after_command(1, 7)
            .prepare_with_active_turn(
                &active_turn(&stale_session),
                accepted_input_id(3),
                Some(turn_id(8)),
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
    }

    /// S09 / INV-008 / INV-012: after-current preparation records the exact
    /// unresolved model alias.
    #[test]
    fn occupied_slot_after_current_records_unknown_alias() {
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
                &active_turn(&current),
                accepted_input_id(3),
                Some(turn_id(8)),
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
    }

    /// S08 / S09 / INV-012 / INV-028: both occupied-slot acceptance paths
    /// record exhaustion of the validated session acceptance tail.
    #[test]
    fn occupied_slot_acceptance_records_position_exhaustion() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let maximum = SessionInputPosition::try_from_u64(u64::MAX).expect("positive maximum");
        let active = active_turn_at_position(&current, maximum);

        let after = after_command(3, 7)
            .prepare_with_active_turn(&active, accepted_input_id(3), Some(turn_id(8)), |_| None)
            .expect("after-current position exhaustion is authoritative");
        assert!(matches!(
            after.result(),
            SubmitInputResult::Rejected(
                SubmitInputRejectedResult::AcceptancePositionExhausted { last, .. }
            ) if *last == maximum
        ));

        let safe_point = safe_point_command(4, 7)
            .prepare_with_active_turn(&active, accepted_input_id(3), None, |_| None)
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
    fn occupied_slot_preparation_rejects_cross_session_projection() {
        let command = after_command(1, 7);
        let wrong_session = session(2, 1, ModelSelectionRequest::Direct(direct(2)));
        let wrong_active_session = command
            .clone()
            .prepare_with_active_turn(
                &active_turn(&wrong_session),
                accepted_input_id(3),
                Some(turn_id(8)),
                |_| None,
            )
            .expect_err("a cross-session active projection is nonterminal");
        assert_eq!(
            wrong_active_session.failure(),
            SubmitInputPreparationFailure::SessionMismatch {
                provided_session: session_id(2),
            }
        );
        assert_eq!(wrong_active_session.command(), &command);
    }

    /// S09 / INV-002 / INV-012: a queued projection cannot stand in for the
    /// authoritative active turn.
    #[test]
    fn occupied_slot_preparation_requires_active_projection() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let command = after_command(1, 7);
        let not_active = command
            .clone()
            .prepare_with_active_turn(
                &queued_turn(&current),
                accepted_input_id(3),
                Some(turn_id(8)),
                |_| None,
            )
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
    fn occupied_slot_preparation_rejects_mismatched_turn_candidate_shape() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let active = active_turn(&current);

        let missing_turn = after_command(1, 7)
            .prepare_with_active_turn(&active_turn(&current), accepted_input_id(3), None, |_| None)
            .expect_err("after-current input requires a minted turn candidate");
        assert_eq!(
            missing_turn.failure(),
            SubmitInputPreparationFailure::TurnCandidateMismatch
        );

        let reused_active_turn = after_command(2, 7)
            .prepare_with_active_turn(&active, accepted_input_id(3), Some(turn_id(7)), |_| None)
            .expect_err("after-current work cannot reuse its active predecessor");
        assert_eq!(
            reused_active_turn.failure(),
            SubmitInputPreparationFailure::TurnCandidateMismatch
        );

        let extra_turn = safe_point_command(3, 7)
            .prepare_with_active_turn(&active, accepted_input_id(3), Some(turn_id(8)), |_| None)
            .expect_err("safe-point input cannot receive a turn candidate");
        assert_eq!(
            extra_turn.failure(),
            SubmitInputPreparationFailure::TurnCandidateMismatch
        );
    }

    /// S08 / S09 / INV-001 / INV-012: no occupied-slot acceptance path can
    /// reuse the active turn's canonical origin identity.
    #[test]
    fn occupied_slot_preparation_rejects_active_origin_identity_reuse() {
        let current = session(1, 1, ModelSelectionRequest::Direct(direct(2)));
        let active = active_turn(&current);
        let active_origin = active
            .active_turn()
            .expect("the test projection has one active turn")
            .accepted_input()
            .id();

        let after = after_command(2, 7)
            .prepare_with_active_turn(&active, active_origin, Some(turn_id(8)), |_| None)
            .expect_err("after-current acceptance cannot reuse the active origin");
        assert_eq!(
            after.failure(),
            SubmitInputPreparationFailure::AcceptedInputCandidateReusesActiveOrigin {
                active_turn: turn_id(7),
                accepted_input: active_origin,
            }
        );

        let safe_point = safe_point_command(3, 7)
            .prepare_with_active_turn(&active, active_origin, None, |_| None)
            .expect_err("safe-point acceptance cannot reuse the active origin");
        assert_eq!(
            safe_point.failure(),
            SubmitInputPreparationFailure::AcceptedInputCandidateReusesActiveOrigin {
                active_turn: turn_id(7),
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
        let advanced = [
            initial
                .clone()
                .consume_as_steering(crate::test_support::model_call_id(0x81))
                .expect("pending steering can be consumed"),
            initial
                .reclassify_as_turn_origin(
                    turn_id(8),
                    crate::SteeringReclassificationReason::NoSafePointBeforeTerminal,
                )
                .expect("pending steering can be reclassified"),
        ];

        for current in advanced {
            assert!(!matches!(
                current.disposition(),
                AcceptedInputDisposition::PendingSteering { .. }
            ));
            let replayed = pending_steering_input()
                .reconstitute()
                .expect("mutable lifecycle progress cannot rewrite the receipt");
            assert!(matches!(
                replayed.result(),
                SubmitInputResult::Applied(SubmitInputAppliedResult::PendingSteering(_))
            ));
        }
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
    }

    /// S08 / INV-002 / INV-012: every independent pending-steering fact is
    /// checked before the immutable receipt is reconstructed.
    #[test]
    fn pending_steering_reconstitution_rejects_cross_wired_facts() {
        let mut wrong_delivery = pending_steering_input();
        wrong_delivery.command = start_command(1, "hello", 1);
        assert_eq!(
            wrong_delivery
                .reconstitute()
                .expect_err("pending facts require a safe-point command")
                .failure(),
            SubmitInputReconstitutionFailure::AppliedDeliveryIsNotNextSafePoint
        );

        let cases: [CrossWiredCase; 9] = [
            (
                |input| input.stored_actor = Actor::Recovery,
                SubmitInputReconstitutionFailure::StoredActorMismatch,
            ),
            (
                |input| pending_facts(input).result_session = session_id(2),
                SubmitInputReconstitutionFailure::ResultSessionMismatch,
            ),
            (
                |input| pending_facts(input).result_source_turn = turn_id(9),
                SubmitInputReconstitutionFailure::SteeringSourceTurnMismatch,
            ),
            (
                |input| pending_facts(input).accepted_command = command_id(2),
                SubmitInputReconstitutionFailure::AcceptedCommandMismatch,
            ),
            (
                |input| pending_facts(input).accepted_input = accepted_input_id(9),
                SubmitInputReconstitutionFailure::AcceptedInputMismatch,
            ),
            (
                |input| pending_facts(input).accepted_session = session_id(2),
                SubmitInputReconstitutionFailure::AcceptedSessionMismatch,
            ),
            (
                |input| pending_facts(input).accepted_content = content("different"),
                SubmitInputReconstitutionFailure::AcceptedContentMismatch,
            ),
            (
                |input| {
                    pending_facts(input).accepted_delivery = DeliveryRequest::NextSafePoint {
                        expected_active_turn: turn_id(9),
                    };
                },
                SubmitInputReconstitutionFailure::AcceptedDeliveryMismatch,
            ),
            (
                |input| pending_facts(input).accepted_position = SessionInputPosition::first(),
                SubmitInputReconstitutionFailure::SteeringAcceptanceDoesNotFollowSourceOrigin,
            ),
        ];
        for (mutate, expected) in cases {
            let mut wrong = pending_steering_input();
            mutate(&mut wrong);
            assert_eq!(
                wrong
                    .reconstitute()
                    .expect_err("one cross-wired pending-steering fact fails closed")
                    .failure(),
                expected
            );
        }

        let mut wrong_source_origin = pending_steering_input();
        pending_facts(&mut wrong_source_origin).source_turn_origin = after_applied_input()
            .reconstitute()
            .expect("the cross-wired origin is independently canonical");
        assert_eq!(
            wrong_source_origin
                .reconstitute()
                .expect_err("the source receipt must establish the exact source turn")
                .failure(),
            SubmitInputReconstitutionFailure::SteeringSourceTurnOriginMismatch
        );
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
                    applied_facts(input).accepted_position = SessionInputPosition::first()
                        .checked_next()
                        .expect("the second position exists");
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
                None,
            )
            .reconstitute()
            .expect_err("a different expected version fails closed")
            .failure(),
            SubmitInputReconstitutionFailure::ExpectedDefaultsVersionMismatch
        );
    }

    /// INV-012 / ADR-0039: the baseline rejected-result projections fail
    /// closed for independently cross-wired actor, session, delivery,
    /// configuration, alias, and position facts.
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
                None,
            ),
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
            SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
                start.clone(),
                Actor::Owner,
                session_id(2),
                maximum,
                None,
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
                    None,
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
                interrupt_command(1, 9),
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
                safe_point_command(1, 9),
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
                after_command(1, 9),
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
                after_command(1, 7),
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
                safe_point_command(1, 7),
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
                after_command(1, 7),
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
                after_command(1, 7),
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

        let wrong_turn_origin = applied_input()
            .reconstitute()
            .expect("the independent turn-four origin is canonical");
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
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_active_turn_present(
                start_command(1, "hello", 1),
                Actor::Owner,
                session_id(1),
                turn_id(7),
                steering_receipt,
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
    }

    /// S01 / S08 / S09 / INV-012 / INV-028: state-carrying rejection replay
    /// validates the delivery discriminator and both expected/actual turns.
    #[test]
    fn inv012_inv028_state_rejections_validate_delivery_and_turns() {
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_active_turn_present(
                safe_point_command(1, 7),
                Actor::Owner,
                session_id(1),
                turn_id(7),
                source_turn_origin(),
            ),
            SubmitInputReconstitutionFailure::ActiveTurnPresentRejectionMismatch,
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_active_turn_mismatch(
                after_command(1, 9),
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
                after_command(1, 7),
                Actor::Owner,
                session_id(1),
                turn_id(7),
                turn_id(7),
                source_turn_origin(),
            ),
            SubmitInputReconstitutionFailure::RejectedActiveTurnsAreEqual,
        );
    }

    /// S07 / S08 / INV-012 / INV-028: replay exposes no terminal result that
    /// matching interrupt preparation cannot record in this milestone, and
    /// safe-point stopping remains closed until exact stop evidence exists.
    #[test]
    fn inv012_inv028_unimplemented_rejection_paths_fail_closed() {
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                interrupt_command(1, 7),
                Actor::Owner,
                session_id(1),
                version(1),
                version(2),
                Some(source_turn_origin()),
            ),
            SubmitInputReconstitutionFailure::InterruptConfigurationRejectionUnavailable,
        );
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                safe_point_command(1, 7),
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
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_unknown_model_alias(
                interrupt_alias,
                Actor::Owner,
                session_id(1),
                alias(2),
                session_id(1),
                version(1),
                defaults(ModelSelectionRequest::Direct(direct(3))),
                Some(source_turn_origin()),
            ),
            SubmitInputReconstitutionFailure::InterruptConfigurationRejectionUnavailable,
        );

        let safe_point = safe_point_command(1, 7);
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
        assert_rejection_reconstitution_fails(
            SubmitInputReconstitutionInput::rejected_acceptance_position_exhausted(
                interrupt_command(1, 7),
                Actor::Owner,
                session_id(1),
                maximum,
                Some(source_turn_origin()),
            ),
            SubmitInputReconstitutionFailure::PositionExhaustionDeliveryUnavailable,
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
