//! Caller-selected accepted-input delivery treatment.
//!
//! ADR-0027 (`docs/decisions/0027-input-delivery-lifecycle.md`) is the
//! normative specification. This module represents only the canonical typed
//! caller payload: origin-producing requests carry model-selection choices
//! bound to an expected session-defaults version, while safe-point steering
//! carries only the caller's expected active turn and no independent
//! configuration by construction.

use crate::{ModelSelectionOverride, SessionConfigurationDefaultsVersion, TurnId};

/// The caller's complete per-input configuration choice for new logical work.
///
/// The expected defaults version and model-selection override are one value so
/// a delivery request cannot carry either without the other. Aggregate input
/// acceptance validates the version and derives server-owned configuration
/// provenance; this value contains no derived configuration.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PerInputConfigurationChoices {
    expected_session_defaults_version: SessionConfigurationDefaultsVersion,
    model: ModelSelectionOverride,
}

impl PerInputConfigurationChoices {
    /// Binds a model-selection override to the defaults version the caller
    /// expects to be current.
    pub const fn new(
        expected_session_defaults_version: SessionConfigurationDefaultsVersion,
        model: ModelSelectionOverride,
    ) -> Self {
        Self {
            expected_session_defaults_version,
            model,
        }
    }

    /// Returns the defaults version the caller expects to be current.
    pub const fn expected_session_defaults_version(&self) -> SessionConfigurationDefaultsVersion {
        self.expected_session_defaults_version
    }

    /// Returns the caller's model-selection override.
    pub const fn model(&self) -> ModelSelectionOverride {
        self.model
    }
}

/// The explicit treatment requested for one submitted input.
///
/// `StartWhenNoActiveTurn`, `Interrupt`, and `AfterCurrentTurn` create new
/// logical work and therefore require [`PerInputConfigurationChoices`].
/// `NextSafePoint` instead carries only the caller's expected active turn and
/// cannot carry an independent configuration request:
///
/// ```compile_fail
/// use signalbox_domain::{
///     DeliveryRequest, PerInputConfigurationChoices, TurnId,
/// };
///
/// fn steering_cannot_supply_configuration(
///     expected_active_turn: TurnId,
///     configuration: PerInputConfigurationChoices,
/// ) {
///     let _ = DeliveryRequest::NextSafePoint {
///         expected_active_turn,
///         configuration,
///     };
/// }
/// ```
///
/// # Scope
///
/// This is neither a wire message nor the complete `SubmitInput` command. It
/// deliberately omits command and accepted-input identity, session identity,
/// content, server-derived configuration, authoritative-state validation,
/// command deduplication, persistence, and acknowledgement.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DeliveryRequest {
    /// Start new logical work when the session has no active turn.
    StartWhenNoActiveTurn {
        /// The caller's version-bound configuration choice.
        configuration: PerInputConfigurationChoices,
    },
    /// Interrupt the exact active predecessor and create successor work.
    Interrupt {
        /// The turn the caller expects to be active.
        expected_active_turn: TurnId,
        /// The successor's version-bound configuration choice.
        configuration: PerInputConfigurationChoices,
    },
    /// Steer the exact active turn at its next safe point.
    NextSafePoint {
        /// The turn the caller expects to be active.
        expected_active_turn: TurnId,
    },
    /// Queue new logical work after the exact active turn.
    AfterCurrentTurn {
        /// The turn the caller expects to be active.
        expected_active_turn: TurnId,
        /// The queued origin's version-bound configuration choice.
        configuration: PerInputConfigurationChoices,
    },
}

#[cfg(test)]
mod tests {
    use super::{DeliveryRequest, PerInputConfigurationChoices};
    use crate::test_support::{direct, turn_id};
    use crate::{
        ModelSelectionOverride, ModelSelectionRequest, SessionConfigurationDefaultsVersion,
    };

    fn first_version() -> SessionConfigurationDefaultsVersion {
        SessionConfigurationDefaultsVersion::first()
    }

    fn choices() -> PerInputConfigurationChoices {
        PerInputConfigurationChoices::new(
            first_version(),
            ModelSelectionOverride::UseSessionDefault,
        )
    }

    /// S01 / INV-008 / INV-028: no-active-turn input carries one complete
    /// version-bound configuration choice for its new logical work.
    #[test]
    fn s01_inv008_inv028_start_request_carries_version_bound_choices() {
        let expected_version = first_version();
        let model = ModelSelectionOverride::UseSessionDefault;
        let configuration = PerInputConfigurationChoices::new(expected_version, model);
        let request = DeliveryRequest::StartWhenNoActiveTurn { configuration };

        let DeliveryRequest::StartWhenNoActiveTurn {
            configuration: carried_configuration,
        } = request
        else {
            panic!("constructed start request must remain start-when-no-active-turn");
        };
        assert_eq!(carried_configuration, configuration);
        assert_eq!(
            configuration.expected_session_defaults_version(),
            expected_version
        );
        assert_eq!(configuration.model(), model);
    }

    /// S07 / INV-008 / INV-028: interrupt payloads bind both the exact active
    /// predecessor and the successor's complete configuration choice.
    #[test]
    fn s07_inv008_inv028_interrupt_carries_target_and_choices() {
        let expected_active_turn = turn_id(1);
        let configuration = choices();
        let request = DeliveryRequest::Interrupt {
            expected_active_turn,
            configuration,
        };

        let DeliveryRequest::Interrupt {
            expected_active_turn: carried_turn,
            configuration: carried_configuration,
        } = request
        else {
            panic!("constructed interrupt must remain an interrupt");
        };
        assert_eq!(carried_turn, expected_active_turn);
        assert_eq!(carried_configuration, configuration);
    }

    /// S08 / INV-028: next-safe-point steering binds to the exact active turn
    /// and has no independent configuration field.
    #[test]
    fn s08_inv028_next_safe_point_carries_only_its_target() {
        let expected_active_turn = turn_id(1);
        let request = DeliveryRequest::NextSafePoint {
            expected_active_turn,
        };

        let DeliveryRequest::NextSafePoint {
            expected_active_turn: carried_turn,
        } = request
        else {
            panic!("constructed steering request must remain next-safe-point");
        };
        assert_eq!(carried_turn, expected_active_turn);
    }

    /// S09 / INV-008 / INV-028: after-current input binds the exact active
    /// turn and the queued origin's complete configuration choice.
    #[test]
    fn s09_inv008_inv028_after_current_carries_target_and_choices() {
        let expected_active_turn = turn_id(1);
        let configuration = choices();
        let request = DeliveryRequest::AfterCurrentTurn {
            expected_active_turn,
            configuration,
        };

        let DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: carried_turn,
            configuration: carried_configuration,
        } = request
        else {
            panic!("constructed after-current request must remain after-current");
        };
        assert_eq!(carried_turn, expected_active_turn);
        assert_eq!(carried_configuration, configuration);
    }

    /// S01 / S07 / S08 / S09 / INV-012: canonical comparison includes every
    /// delivery discriminator, target turn, expected defaults version, and
    /// override.
    #[test]
    fn s01_s07_s08_s09_inv012_delivery_payload_equality_is_structural() {
        let configuration = choices();
        let later_configuration = PerInputConfigurationChoices::new(
            first_version()
                .checked_next()
                .expect("the second version is representable"),
            ModelSelectionOverride::UseSessionDefault,
        );
        let explicit_configuration = PerInputConfigurationChoices::new(
            first_version(),
            ModelSelectionOverride::ReplaceWith(ModelSelectionRequest::Direct(direct(3))),
        );
        let start = DeliveryRequest::StartWhenNoActiveTurn { configuration };
        let interrupt = DeliveryRequest::Interrupt {
            expected_active_turn: turn_id(1),
            configuration,
        };
        let next_safe_point = DeliveryRequest::NextSafePoint {
            expected_active_turn: turn_id(1),
        };
        let after_current = DeliveryRequest::AfterCurrentTurn {
            expected_active_turn: turn_id(1),
            configuration,
        };

        assert_ne!(start, interrupt);
        assert_ne!(start, next_safe_point);
        assert_ne!(start, after_current);
        assert_ne!(interrupt, next_safe_point);
        assert_ne!(interrupt, after_current);
        assert_ne!(next_safe_point, after_current);

        assert_ne!(
            interrupt,
            DeliveryRequest::Interrupt {
                expected_active_turn: turn_id(2),
                configuration,
            }
        );
        assert_ne!(
            next_safe_point,
            DeliveryRequest::NextSafePoint {
                expected_active_turn: turn_id(2),
            }
        );
        assert_ne!(
            after_current,
            DeliveryRequest::AfterCurrentTurn {
                expected_active_turn: turn_id(2),
                configuration,
            }
        );

        assert_ne!(
            start,
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: later_configuration,
            }
        );
        assert_ne!(
            start,
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: explicit_configuration,
            }
        );
        assert_ne!(
            interrupt,
            DeliveryRequest::Interrupt {
                expected_active_turn: turn_id(1),
                configuration: later_configuration,
            }
        );
        assert_ne!(
            interrupt,
            DeliveryRequest::Interrupt {
                expected_active_turn: turn_id(1),
                configuration: explicit_configuration,
            }
        );
        assert_ne!(
            after_current,
            DeliveryRequest::AfterCurrentTurn {
                expected_active_turn: turn_id(1),
                configuration: later_configuration,
            }
        );
        assert_ne!(
            after_current,
            DeliveryRequest::AfterCurrentTurn {
                expected_active_turn: turn_id(1),
                configuration: explicit_configuration,
            }
        );
    }
}
