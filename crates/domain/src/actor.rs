//! Typed provenance for durable commands and recorded transitions.
//!
//! The normative specification is `docs/spec/identity-and-commands.md`
//! (originally ADR-0039). This value records agency only; it grants no lifecycle,
//! authentication, authorization, or approval authority.

use crate::{ToolRequestId, TurnId};

/// The initiating agency of a durable command or attributed transition.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Actor {
    /// The single owner's authority, however connected.
    Owner,
    /// Agency exercised by model output from one exact turn.
    Model {
        /// The turn whose model output acted.
        turn: TurnId,
    },
    /// The startup recovery scan acting under its accepted authority.
    Recovery,
    /// Agency exercised by execution of one exact tool request.
    Tool {
        /// The tool request whose execution acted.
        request: ToolRequestId,
    },
}

#[cfg(test)]
mod tests {
    use super::Actor;
    use crate::test_support::{tool_request_id, turn_id};

    /// INV-001: carried identities retain their exact kind and do not make
    /// different actor variants interchangeable.
    #[test]
    fn inv001_actor_equality_is_structural() {
        assert_eq!(Actor::Owner, Actor::Owner);
        assert_ne!(Actor::Owner, Actor::Recovery);
        assert_ne!(
            Actor::Model { turn: turn_id(1) },
            Actor::Model { turn: turn_id(2) }
        );
        assert_ne!(
            Actor::Model { turn: turn_id(1) },
            Actor::Tool {
                request: tool_request_id(1),
            }
        );
    }

    /// INV-020: model agency remains a distinct typed value and cannot equal
    /// owner agency.
    #[test]
    fn inv020_model_agency_cannot_masquerade_as_owner() {
        assert_ne!(Actor::Model { turn: turn_id(1) }, Actor::Owner);
    }
}
