//! Canonical durable input submission and no-active-turn preparation.
//!
//! ADR-0027 owns accepted-input delivery, configuration, ordering, and
//! disposition semantics. ADR-0034 owns structural replay equality, ADR-0035
//! owns checked reconstitution, ADR-0037 owns content, and ADR-0039 owns
//! actor attribution. This slice prepares only the authoritative state that
//! exists before turn machinery: a session with no active turn. It creates
//! durable queued logical-work facts but no turn lifecycle, eligibility,
//! frontier, slot, attempt, steering consumption, or interrupt transition.

use std::hash::{Hash, Hasher};

use crate::{
    AcceptedInputDisposition, AcceptedInputId, AcceptedInputQueueOrder, AcceptedInputQueuePriority,
    Actor, DeliveryRequest, DurableCommandId, FrozenAliasDefinition, FrozenModelSelection,
    ModelAlias, ModelSelectionRequest, OriginConfiguration, PerInputConfigurationChoices, Session,
    SessionConfigurationDefaults, SessionConfigurationDefaultsVersion, SessionId,
    SessionInputPosition, TurnId, UserContent, VersionedSessionConfigurationDefaults,
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
    /// Constructs the complete canonical typed payload.
    pub const fn new(
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
        turn: TurnId,
        previous_position: Option<SessionInputPosition>,
        select_definition: impl FnOnce(ModelAlias) -> Option<FrozenAliasDefinition>,
    ) -> Result<PreparedSubmitInput, SubmitInputPreparationError> {
        if session.id() != self.session {
            return Err(SubmitInputPreparationError {
                command: Box::new(self),
                provided_session: session.id(),
            });
        }

        let DeliveryRequest::StartWhenNoActiveTurn { configuration } = self.delivery else {
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
            result: SubmitInputResult::Applied(SubmitInputAppliedResult {
                accepted_input,
                session: target_session,
                turn,
                queue_order: AcceptedInputQueueOrder::ordinary(acceptance_position),
                origin_configuration,
            }),
        })
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
    /// Acceptance created complete durable queued-work facts.
    Applied(SubmitInputAppliedResult),
    /// Authoritative state rejected the caller's requested treatment.
    Rejected(SubmitInputRejectedResult),
}

/// The complete applied receipt for durable queued input.
///
/// Construction is sealed behind preparation and checked reconstitution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmitInputAppliedResult {
    accepted_input: AcceptedInputId,
    session: SessionId,
    turn: TurnId,
    queue_order: AcceptedInputQueueOrder,
    origin_configuration: OriginConfiguration,
}

impl SubmitInputAppliedResult {
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
        self.queue_order.acceptance_position()
    }

    /// Borrows the complete frozen origin configuration.
    pub const fn origin_configuration(&self) -> &OriginConfiguration {
        &self.origin_configuration
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

/// A supplied session belonged to another command target.
///
/// This is a preparation correlation failure, not a terminal recorded
/// rejection, and claims no command identity.
#[derive(Clone, Debug)]
pub struct SubmitInputPreparationError {
    command: Box<SubmitInput>,
    provided_session: SessionId,
}

impl SubmitInputPreparationError {
    /// Borrows the unchanged canonical command.
    pub const fn command(&self) -> &SubmitInput {
        &self.command
    }

    /// Returns the different session supplied for preparation.
    pub const fn provided_session(&self) -> SessionId {
        self.provided_session
    }

    /// Returns both unchanged correlation inputs.
    pub fn into_parts(self) -> (SubmitInput, SessionId) {
        (*self.command, self.provided_session)
    }
}

#[derive(Clone, Debug)]
struct SubmitInputAppliedReconstitutionFacts {
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
enum SubmitInputReconstitutionFacts {
    Applied(Box<SubmitInputAppliedReconstitutionFacts>),
    RejectedSessionNotFound {
        result_session: SessionId,
    },
    RejectedNoActiveTurn {
        result_session: SessionId,
        result_expected_active_turn: TurnId,
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
#[derive(Clone, Debug)]
pub struct SubmitInputReconstitutionInput {
    command: SubmitInput,
    facts: SubmitInputReconstitutionFacts,
}

impl SubmitInputReconstitutionInput {
    /// Supplies every recorded applied-result and durable effect correlation.
    #[allow(clippy::too_many_arguments)]
    pub fn applied(
        command: SubmitInput,
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
            facts: SubmitInputReconstitutionFacts::Applied(Box::new(
                SubmitInputAppliedReconstitutionFacts {
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

    /// Supplies a recorded missing-session result.
    pub const fn rejected_session_not_found(
        command: SubmitInput,
        result_session: SessionId,
    ) -> Self {
        Self {
            command,
            facts: SubmitInputReconstitutionFacts::RejectedSessionNotFound { result_session },
        }
    }

    /// Supplies a recorded no-active-turn result.
    pub const fn rejected_no_active_turn(
        command: SubmitInput,
        result_session: SessionId,
        result_expected_active_turn: TurnId,
    ) -> Self {
        Self {
            command,
            facts: SubmitInputReconstitutionFacts::RejectedNoActiveTurn {
                result_session,
                result_expected_active_turn,
            },
        }
    }

    /// Supplies a recorded defaults-version mismatch.
    pub const fn rejected_defaults_version_mismatch(
        command: SubmitInput,
        result_session: SessionId,
        result_expected: SessionConfigurationDefaultsVersion,
        result_current: SessionConfigurationDefaultsVersion,
    ) -> Self {
        Self {
            command,
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
        result_session: SessionId,
        result_alias: ModelAlias,
        defaults_session: SessionId,
        defaults_version: SessionConfigurationDefaultsVersion,
        defaults: SessionConfigurationDefaults,
    ) -> Self {
        Self {
            command,
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
        result_session: SessionId,
        result_last_position: SessionInputPosition,
    ) -> Self {
        Self {
            command,
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

        let result = match self.facts.clone() {
            SubmitInputReconstitutionFacts::Applied(facts) => {
                let SubmitInputAppliedReconstitutionFacts {
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
                ) {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::AppliedDeliveryIsNotStart,
                    ));
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

                SubmitInputResult::Applied(SubmitInputAppliedResult {
                    accepted_input: result_accepted_input,
                    session: result_session,
                    turn: result_turn,
                    queue_order,
                    origin_configuration,
                })
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
                let Some(configuration) = start_configuration(self.command.delivery) else {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::RejectionDeliveryIsNotStart,
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
                let Some(configuration) = start_configuration(self.command.delivery) else {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::RejectionDeliveryIsNotStart,
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
                if start_configuration(self.command.delivery).is_none() {
                    return Err(fail(
                        SubmitInputReconstitutionFailure::RejectionDeliveryIsNotStart,
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
    let Some(configuration) = start_configuration(command.delivery) else {
        return Err(SubmitInputReconstitutionFailure::AppliedDeliveryIsNotStart);
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

fn start_configuration(delivery: DeliveryRequest) -> Option<PerInputConfigurationChoices> {
    match delivery {
        DeliveryRequest::StartWhenNoActiveTurn { configuration } => Some(configuration),
        DeliveryRequest::Interrupt { .. }
        | DeliveryRequest::NextSafePoint { .. }
        | DeliveryRequest::AfterCurrentTurn { .. } => None,
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
    /// An applied record carries a non-start delivery request.
    AppliedDeliveryIsNotStart,
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
    /// The queue fact belongs to another session.
    QueueSessionMismatch,
    /// The queue fact names another future turn.
    QueueTurnMismatch,
    /// The accepted-input and queue positions differ.
    QueuePositionMismatch,
    /// This slice's queue fact is not ordinary priority.
    QueuePriorityMismatch,
    /// A required rejection was recorded for a non-start request.
    RejectionDeliveryIsNotStart,
    /// A no-active-turn result names a different expected turn or a start
    /// request.
    ExpectedActiveTurnMismatch,
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
        ReconstitutedSubmitInput, SubmitInput, SubmitInputReconstitutionFailure,
        SubmitInputReconstitutionInput, SubmitInputRejectedResult, SubmitInputResult,
    };
    use crate::test_support::{accepted_input_id, alias, command_id, direct, session_id, turn_id};
    use crate::{
        AcceptedInputDisposition, Actor, DeliveryRequest, FrozenAliasDefinition,
        FrozenModelSelection, ModelSelectionOverride, ModelSelectionRequest,
        PerInputConfigurationChoices, Session, SessionConfigurationDefaults,
        SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
        SessionInputPosition, SessionReconstitutionInput, TranscriptAncestry, UserContent,
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
            Actor::Owner,
            content(text),
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: choices(expected, ModelSelectionOverride::UseSessionDefault),
            },
        )
    }

    fn hash(value: &SubmitInput) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    /// S01 / INV-012: comparison excludes only command identity and includes
    /// session, actor, exact content, delivery discriminator, and every
    /// delivery field.
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
                Actor::Owner,
                content("hello"),
                baseline.delivery(),
            )
        );
        assert_ne!(
            baseline,
            SubmitInput::new(
                command_id(1),
                session_id(1),
                Actor::Recovery,
                content("hello"),
                baseline.delivery(),
            )
        );
        assert_ne!(
            baseline,
            SubmitInput::new(
                command_id(1),
                session_id(1),
                Actor::Owner,
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
                turn_id(4),
                None,
                |_| None,
            )
            .expect("session matches");

        assert_eq!(prepared.command(), &command);
        let SubmitInputResult::Applied(applied) = prepared.result() else {
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
            Actor::Owner,
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
                turn_id(5),
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
                    turn_id(5),
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
        for delivery in [
            DeliveryRequest::Interrupt {
                expected_active_turn: turn_id(7),
                configuration,
            },
            DeliveryRequest::NextSafePoint {
                expected_active_turn: turn_id(7),
            },
            DeliveryRequest::AfterCurrentTurn {
                expected_active_turn: turn_id(7),
                configuration,
            },
        ] {
            let prepared = SubmitInput::new(
                command_id(1),
                session_id(1),
                Actor::Owner,
                content("hello"),
                delivery,
            )
            .prepare_when_no_active_turn(&current, accepted_input_id(3), turn_id(4), None, |_| {
                panic!("active-work rejection does not resolve configuration")
            })
            .expect("session matches");
            assert!(matches!(
                prepared.result(),
                SubmitInputResult::Rejected(SubmitInputRejectedResult::NoActiveTurn {
                    session,
                    expected_active_turn,
                }) if *session == session_id(1) && *expected_active_turn == turn_id(7)
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
                    turn_id(4),
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
                    turn_id(4),
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
        let command = start_command(1, "hello", 1);
        let order = crate::AcceptedInputQueueOrder::ordinary(SessionInputPosition::first());
        let input = || {
            SubmitInputReconstitutionInput::applied(
                command.clone(),
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
                order,
                session_id(1),
                version(1),
                defaults(ModelSelectionRequest::Direct(direct(2))),
                ModelSelectionRequest::Direct(direct(2)),
                FrozenModelSelection::Direct(direct(2)),
            )
        };
        let reconstructed = input()
            .reconstitute()
            .expect("complete matching facts reconstruct");
        let SubmitInputResult::Applied(applied) = reconstructed.result() else {
            panic!("applied facts reconstruct an applied result");
        };
        assert_eq!(applied.turn(), turn_id(4));

        let mut wrong = input();
        if let super::SubmitInputReconstitutionFacts::Applied(facts) = &mut wrong.facts {
            facts.accepted_content = content("different");
        }
        assert_eq!(
            wrong
                .reconstitute()
                .expect_err("cross-wired content fails closed")
                .failure(),
            SubmitInputReconstitutionFailure::AcceptedContentMismatch
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
                session_id(1),
            )
            .reconstitute()
            .expect("matching missing-session facts reconstruct");

        assert_eq!(
            SubmitInputReconstitutionInput::rejected_defaults_version_mismatch(
                command,
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
}
