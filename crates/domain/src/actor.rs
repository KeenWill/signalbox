//! Typed provenance for durable commands and recorded transitions.
//!
//! The normative specification is `docs/spec/identity-and-commands.md`.
//! This value records agency only; it grants no lifecycle, authentication,
//! authorization, or approval authority.

use crate::{ToolRequestId, TurnId};

/// The initiating agency of a durable command or attributed transition.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Actor {
    /// The single user's authority, however connected.
    User,
    /// Daemon core acting without model, tool, or recovery agency.
    Core,
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

    /// carried identities retain their exact kind and do not make
    /// different actor variants interchangeable.
    #[test]
    fn actor_equality_is_structural() {
        assert_eq!(Actor::User, Actor::User);
        assert_ne!(Actor::User, Actor::Core);
        assert_ne!(Actor::User, Actor::Recovery);
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

    /// model agency remains a distinct typed value and cannot equal
    /// user agency.
    #[test]
    fn model_agency_cannot_masquerade_as_user() {
        assert_ne!(Actor::Model { turn: turn_id(1) }, Actor::User);
    }
}
