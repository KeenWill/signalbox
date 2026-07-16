use crate::{ModelCallId, TurnId};

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
/// The transition methods on this type enforce only these local disposition
/// transitions:
///
/// - `PendingSteering` to `ConsumedAsSteering`;
/// - `PendingSteering` to `ReclassifiedAsTurnOrigin`.
///
/// They do not validate that a model call belongs to the source turn or
/// contains the steering input in its context frontier. Reclassification does
/// not validate inherited configuration provenance or prove that the source
/// turn terminated without another safe point. Persistence atomicity, queue
/// ordering, current aggregate ownership, and command authorization also
/// remain responsibilities of later aggregate transitions and persistence
/// guards.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
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
    pub fn consume_as_steering(
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
    pub fn reclassify_as_turn_origin(
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

/// Reports a rejected local accepted-input disposition transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcceptedInputDispositionTransitionError {
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

/// Explains why accepted steering became new turn-origin work.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SteeringReclassificationReason {
    /// The source turn terminated before reaching another safe point.
    NoSafePointBeforeTerminal,
}

#[cfg(test)]
mod tests {
    use super::{
        AcceptedInputDisposition, AcceptedInputDispositionTransitionError, SteeringBinding,
        SteeringReclassificationReason,
    };
    use crate::{ModelCallId, TurnId};
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
    fn pending_steering_can_be_consumed_by_an_exact_model_call() {
        let call = model_call_id(2);

        assert_eq!(
            pending_steering(1).consume_as_steering(call),
            Ok(AcceptedInputDisposition::ConsumedAsSteering { call })
        );
    }

    #[test]
    fn pending_steering_can_be_reclassified_with_an_exact_turn_and_reason() {
        let turn = turn_id(2);
        let reason = SteeringReclassificationReason::NoSafePointBeforeTerminal;

        assert_eq!(
            pending_steering(1).reclassify_as_turn_origin(turn, reason),
            Ok(AcceptedInputDisposition::ReclassifiedAsTurnOrigin { turn, reason })
        );
    }

    #[test]
    fn consumption_rejects_every_non_pending_disposition_with_the_current_value() {
        for current in non_pending_dispositions() {
            assert_eq!(
                current.consume_as_steering(model_call_id(4)),
                Err(AcceptedInputDispositionTransitionError::CannotConsumeAsSteering { current })
            );
        }
    }

    #[test]
    fn reclassification_rejects_every_non_pending_disposition_with_the_current_value() {
        for current in non_pending_dispositions() {
            assert_eq!(
                current.reclassify_as_turn_origin(
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

    fn model_call_id(value: u128) -> ModelCallId {
        ModelCallId::from_uuid(Uuid::from_u128(value))
    }

    fn turn_id(value: u128) -> TurnId {
        TurnId::from_uuid(Uuid::from_u128(value))
    }
}
