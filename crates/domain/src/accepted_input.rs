use crate::{AcceptedInputId, ModelCallId, TurnId};

/// The canonical public boundary for validated accepted-input disposition transitions.
///
/// The consuming transition methods preserve the accepted-input identity while
/// applying the validated disposition transitions defined below. External callers
/// cannot invoke those transition methods on a bare [`AcceptedInputDisposition`].
/// Rejected transitions return this lifecycle value unchanged, including its
/// [`AcceptedInputId`].
///
/// This is a local lifecycle projection, not the complete accepted-input
/// aggregate or a persistence record. It deliberately omits content, session,
/// delivery request, order, configuration provenance, command handling, and
/// transaction boundaries. Future aggregate transitions must validate those
/// facts together with model-call ownership, turn termination, inherited
/// configuration, and queue ordering where ADR-0027 requires them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedInputLifecycle {
    id: AcceptedInputId,
    disposition: AcceptedInputDisposition,
}

impl AcceptedInputLifecycle {
    /// Couples an accepted-input identity to an existing valid disposition.
    ///
    /// This constructor does not accept or acknowledge user input. Boundary and
    /// aggregate code must establish the omitted ADR-0027 acceptance facts before
    /// constructing a lifecycle projection for newly accepted input.
    pub const fn new(id: AcceptedInputId, disposition: AcceptedInputDisposition) -> Self {
        Self { id, disposition }
    }

    /// Returns the accepted-input identity preserved by this lifecycle.
    pub const fn id(&self) -> AcceptedInputId {
        self.id
    }

    /// Borrows the accepted input's current disposition.
    pub const fn disposition(&self) -> &AcceptedInputDisposition {
        &self.disposition
    }

    /// Consumes pending steering into the identified model call.
    pub fn consume_as_steering(
        self,
        call: ModelCallId,
    ) -> Result<Self, AcceptedInputLifecycleTransitionError> {
        let Self { id, disposition } = self;

        match disposition.consume_as_steering(call) {
            Ok(disposition) => Ok(Self { id, disposition }),
            Err(error) => Err(
                AcceptedInputLifecycleTransitionError::CannotConsumeAsSteering {
                    lifecycle: Self {
                        id,
                        disposition: error.into_current(),
                    },
                },
            ),
        }
    }

    /// Reclassifies pending steering as the origin of a new turn.
    pub fn reclassify_as_turn_origin(
        self,
        turn: TurnId,
        reason: SteeringReclassificationReason,
    ) -> Result<Self, AcceptedInputLifecycleTransitionError> {
        let Self { id, disposition } = self;

        match disposition.reclassify_as_turn_origin(turn, reason) {
            Ok(disposition) => Ok(Self { id, disposition }),
            Err(error) => Err(
                AcceptedInputLifecycleTransitionError::CannotReclassifyAsTurnOrigin {
                    lifecycle: Self {
                        id,
                        disposition: error.into_current(),
                    },
                },
            ),
        }
    }
}

/// Reports a rejected identity-preserving accepted-input lifecycle transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AcceptedInputLifecycleTransitionError {
    /// The current disposition cannot be consumed as steering.
    CannotConsumeAsSteering {
        /// The unchanged lifecycle on which consumption was attempted.
        lifecycle: AcceptedInputLifecycle,
    },
    /// The current disposition cannot be reclassified as turn-origin work.
    CannotReclassifyAsTurnOrigin {
        /// The unchanged lifecycle on which reclassification was attempted.
        lifecycle: AcceptedInputLifecycle,
    },
}

impl AcceptedInputLifecycleTransitionError {
    /// Borrows the unchanged lifecycle on which the transition was rejected.
    pub const fn lifecycle(&self) -> &AcceptedInputLifecycle {
        match self {
            Self::CannotConsumeAsSteering { lifecycle }
            | Self::CannotReclassifyAsTurnOrigin { lifecycle } => lifecycle,
        }
    }

    /// Returns the unchanged lifecycle on which the transition was rejected.
    pub fn into_lifecycle(self) -> AcceptedInputLifecycle {
        match self {
            Self::CannotConsumeAsSteering { lifecycle }
            | Self::CannotReclassifyAsTurnOrigin { lifecycle } => lifecycle,
        }
    }
}

/// Binds pending steering to the exact turn it was accepted to steer.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SteeringBinding {
    source_turn: TurnId,
}

impl SteeringBinding {
    /// Creates a binding to the source turn.
    pub const fn new(source_turn: TurnId) -> Self {
        Self { source_turn }
    }

    /// Returns the source-turn identity.
    pub const fn source_turn(&self) -> TurnId {
        self.source_turn
    }
}

/// Records how one durably accepted input is accounted for.
///
/// An accepted input either originates a turn, remains pending for a source
/// turn's next safe point, is consumed by an exact model call, or becomes new
/// turn-origin work when the source turn terminates before a safe point.
///
/// [`AcceptedInputLifecycle`] is the canonical public boundary for changing a
/// disposition because it preserves the associated [`AcceptedInputId`]. The
/// lower-level transition implementation on this enum is crate-private.
/// External callers therefore cannot invoke the validated transition methods on
/// a bare disposition:
///
/// ```compile_fail
/// use signalbox_domain::{AcceptedInputDisposition, ModelCallId};
///
/// fn consume_bare_disposition(
///     disposition: AcceptedInputDisposition,
///     call: ModelCallId,
/// ) {
///     let _ = disposition.consume_as_steering(call);
/// }
/// ```
///
/// ```compile_fail
/// use signalbox_domain::{
///     AcceptedInputDisposition, SteeringReclassificationReason, TurnId,
/// };
///
/// fn reclassify_bare_disposition(
///     disposition: AcceptedInputDisposition,
///     turn: TurnId,
///     reason: SteeringReclassificationReason,
/// ) {
///     let _ = disposition.reclassify_as_turn_origin(turn, reason);
/// }
/// ```
///
/// The lifecycle transitions do not provide complete lifecycle enforcement. They do not
/// validate that a model call belongs to the source turn or contains the
/// steering input in its context frontier. Reclassification does not validate
/// inherited configuration provenance or prove that the source turn terminated
/// without another safe point. Persistence atomicity, queue ordering, current
/// aggregate ownership, and command authorization remain responsibilities of
/// later aggregate transitions and persistence guards.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum AcceptedInputDisposition {
    /// The accepted input originated the identified turn.
    OriginOf(TurnId),
    /// The accepted input is waiting to steer its bound source turn.
    PendingSteering {
        /// The exact turn the input was accepted to steer.
        binding: SteeringBinding,
    },
    /// The accepted input became semantic history in the identified call.
    ConsumedAsSteering {
        /// The model call whose context frontier consumed the input.
        call: ModelCallId,
    },
    /// The input became new turn-origin work instead of disappearing.
    ReclassifiedAsTurnOrigin {
        /// The new turn originated by the accepted input.
        turn: TurnId,
        /// Why the accepted steering could not be consumed by its source turn.
        reason: SteeringReclassificationReason,
    },
}

impl AcceptedInputDisposition {
    /// Consumes pending steering into the identified model call.
    pub(crate) fn consume_as_steering(
        self,
        call: ModelCallId,
    ) -> Result<Self, AcceptedInputDispositionTransitionError> {
        match self {
            Self::PendingSteering { .. } => Ok(Self::ConsumedAsSteering { call }),
            current @ (Self::OriginOf(_)
            | Self::ConsumedAsSteering { .. }
            | Self::ReclassifiedAsTurnOrigin { .. }) => {
                Err(AcceptedInputDispositionTransitionError::CannotConsumeAsSteering { current })
            }
        }
    }

    /// Reclassifies pending steering as the origin of a new turn.
    pub(crate) fn reclassify_as_turn_origin(
        self,
        turn: TurnId,
        reason: SteeringReclassificationReason,
    ) -> Result<Self, AcceptedInputDispositionTransitionError> {
        match self {
            Self::PendingSteering { .. } => Ok(Self::ReclassifiedAsTurnOrigin { turn, reason }),
            current @ (Self::OriginOf(_)
            | Self::ConsumedAsSteering { .. }
            | Self::ReclassifiedAsTurnOrigin { .. }) => Err(
                AcceptedInputDispositionTransitionError::CannotReclassifyAsTurnOrigin { current },
            ),
        }
    }
}

/// Reports a rejected crate-private accepted-input disposition transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AcceptedInputDispositionTransitionError {
    /// The current disposition cannot be consumed as steering.
    CannotConsumeAsSteering {
        /// The disposition on which consumption was attempted.
        current: AcceptedInputDisposition,
    },
    /// The current disposition cannot be reclassified as turn-origin work.
    CannotReclassifyAsTurnOrigin {
        /// The disposition on which reclassification was attempted.
        current: AcceptedInputDisposition,
    },
}

impl AcceptedInputDispositionTransitionError {
    fn into_current(self) -> AcceptedInputDisposition {
        match self {
            Self::CannotConsumeAsSteering { current }
            | Self::CannotReclassifyAsTurnOrigin { current } => current,
        }
    }
}

/// Explains why accepted steering became new turn-origin work.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SteeringReclassificationReason {
    /// The source turn terminated before reaching another safe point.
    NoSafePointBeforeTerminal,
}

#[cfg(test)]
mod tests {
    use super::{
        AcceptedInputDisposition, AcceptedInputDispositionTransitionError, AcceptedInputLifecycle,
        AcceptedInputLifecycleTransitionError, SteeringBinding, SteeringReclassificationReason,
    };
    use crate::{AcceptedInputId, ModelCallId, TurnId};
    use uuid::Uuid;

    #[test]
    fn steering_binding_exposes_the_exact_source_turn() {
        let source_turn = turn_id(1);
        let binding = SteeringBinding::new(source_turn);

        assert_eq!(binding.source_turn(), source_turn);
        assert_ne!(binding, SteeringBinding::new(turn_id(2)));
    }

    #[test]
    fn disposition_equality_includes_variant_and_identity() {
        let origin = AcceptedInputDisposition::OriginOf(turn_id(1));

        assert_eq!(origin, AcceptedInputDisposition::OriginOf(turn_id(1)));
        assert_ne!(origin, AcceptedInputDisposition::OriginOf(turn_id(2)));
        assert_ne!(
            origin,
            AcceptedInputDisposition::PendingSteering {
                binding: SteeringBinding::new(turn_id(1)),
            }
        );
    }

    #[test]
    fn internal_disposition_transition_can_consume_pending_steering() {
        let call = model_call_id(2);

        assert_eq!(
            pending_steering(1).consume_as_steering(call),
            Ok(AcceptedInputDisposition::ConsumedAsSteering { call })
        );
    }

    #[test]
    fn internal_disposition_transition_can_reclassify_pending_steering() {
        let turn = turn_id(2);
        let reason = SteeringReclassificationReason::NoSafePointBeforeTerminal;

        assert_eq!(
            pending_steering(1).reclassify_as_turn_origin(turn, reason),
            Ok(AcceptedInputDisposition::ReclassifiedAsTurnOrigin { turn, reason })
        );
    }

    #[test]
    fn lifecycle_couples_identity_to_disposition() {
        let id = accepted_input_id(1);
        let disposition = pending_steering(2);
        let lifecycle = AcceptedInputLifecycle::new(id, disposition.clone());

        assert_eq!(lifecycle.id(), id);
        assert_eq!(lifecycle.disposition(), &disposition);
    }

    #[test]
    fn lifecycle_consumption_preserves_accepted_input_identity() {
        let id = accepted_input_id(1);
        let call = model_call_id(3);
        let lifecycle = AcceptedInputLifecycle::new(id, pending_steering(2));

        assert_eq!(
            lifecycle.consume_as_steering(call),
            Ok(AcceptedInputLifecycle::new(
                id,
                AcceptedInputDisposition::ConsumedAsSteering { call }
            ))
        );
    }

    #[test]
    fn lifecycle_reclassification_preserves_accepted_input_identity() {
        let id = accepted_input_id(1);
        let turn = turn_id(3);
        let reason = SteeringReclassificationReason::NoSafePointBeforeTerminal;
        let lifecycle = AcceptedInputLifecycle::new(id, pending_steering(2));

        assert_eq!(
            lifecycle.reclassify_as_turn_origin(turn, reason),
            Ok(AcceptedInputLifecycle::new(
                id,
                AcceptedInputDisposition::ReclassifiedAsTurnOrigin { turn, reason }
            ))
        );
    }

    #[test]
    fn lifecycle_consumption_rejections_return_the_unchanged_identity_and_disposition() {
        for disposition in non_pending_dispositions() {
            let lifecycle = AcceptedInputLifecycle::new(accepted_input_id(1), disposition);
            let error = AcceptedInputLifecycleTransitionError::CannotConsumeAsSteering {
                lifecycle: lifecycle.clone(),
            };

            assert_eq!(
                lifecycle.clone().consume_as_steering(model_call_id(4)),
                Err(error.clone())
            );
            assert_eq!(error.lifecycle(), &lifecycle);
            assert_eq!(error.into_lifecycle(), lifecycle);
        }
    }

    #[test]
    fn lifecycle_reclassification_rejections_return_the_unchanged_identity_and_disposition() {
        for disposition in non_pending_dispositions() {
            let lifecycle = AcceptedInputLifecycle::new(accepted_input_id(1), disposition);
            let error = AcceptedInputLifecycleTransitionError::CannotReclassifyAsTurnOrigin {
                lifecycle: lifecycle.clone(),
            };

            assert_eq!(
                lifecycle.clone().reclassify_as_turn_origin(
                    turn_id(4),
                    SteeringReclassificationReason::NoSafePointBeforeTerminal,
                ),
                Err(error.clone())
            );
            assert_eq!(error.lifecycle(), &lifecycle);
            assert_eq!(error.into_lifecycle(), lifecycle);
        }
    }

    #[test]
    fn internal_consumption_rejects_every_non_pending_disposition_with_the_current_value() {
        for current in non_pending_dispositions() {
            assert_eq!(
                current.clone().consume_as_steering(model_call_id(4)),
                Err(AcceptedInputDispositionTransitionError::CannotConsumeAsSteering { current })
            );
        }
    }

    #[test]
    fn internal_reclassification_rejects_every_non_pending_disposition_with_the_current_value() {
        for current in non_pending_dispositions() {
            assert_eq!(
                current.clone().reclassify_as_turn_origin(
                    turn_id(4),
                    SteeringReclassificationReason::NoSafePointBeforeTerminal,
                ),
                Err(
                    AcceptedInputDispositionTransitionError::CannotReclassifyAsTurnOrigin {
                        current,
                    }
                )
            );
        }
    }

    fn non_pending_dispositions() -> [AcceptedInputDisposition; 3] {
        [
            AcceptedInputDisposition::OriginOf(turn_id(1)),
            AcceptedInputDisposition::ConsumedAsSteering {
                call: model_call_id(2),
            },
            AcceptedInputDisposition::ReclassifiedAsTurnOrigin {
                turn: turn_id(3),
                reason: SteeringReclassificationReason::NoSafePointBeforeTerminal,
            },
        ]
    }

    fn pending_steering(source_turn: u128) -> AcceptedInputDisposition {
        AcceptedInputDisposition::PendingSteering {
            binding: SteeringBinding::new(turn_id(source_turn)),
        }
    }

    fn accepted_input_id(value: u128) -> AcceptedInputId {
        AcceptedInputId::from_uuid(Uuid::from_u128(value))
    }

    fn model_call_id(value: u128) -> ModelCallId {
        ModelCallId::from_uuid(Uuid::from_u128(value))
    }

    fn turn_id(value: u128) -> TurnId {
        TurnId::from_uuid(Uuid::from_u128(value))
    }
}
