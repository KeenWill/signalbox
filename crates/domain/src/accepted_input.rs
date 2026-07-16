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

/// Explains why accepted steering became new turn-origin work.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SteeringReclassificationReason {
    /// The source turn terminated before reaching another safe point.
    NoSafePointBeforeTerminal,
}

#[cfg(test)]
mod tests {
    use super::{AcceptedInputDisposition, SteeringBinding};
    use crate::TurnId;
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

    fn turn_id(value: u128) -> TurnId {
        TurnId::from_uuid(Uuid::from_u128(value))
    }
}
