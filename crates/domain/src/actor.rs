//! Typed provenance for durable commands and recorded transitions.
//!
//! The normative specification is `docs/spec/identity-and-commands.md`.
//! This value records agency only; it grants no lifecycle, authentication,
//! authorization, or approval authority.

use crate::{ProgramRunId, ToolRequestId, TurnId};

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
    /// Input issued through the host-side session capability of one verified program run.
    Program {
        /// The verified issuing run; this reference confers no authority.
        run: ProgramActor,
    },
    /// The startup recovery scan acting under its accepted authority.
    Recovery,
    /// Agency exercised by execution of one exact tool request.
    Tool {
        /// The tool request whose execution acted.
        request: ToolRequestId,
    },
}

/// A verified reference to an issuing run, carrying provenance and no authority.
///
/// ```compile_fail
/// use signalbox_domain::{ProgramActor, ProgramRunId};
/// fn forge(run: ProgramRunId) { let _ = ProgramActor { run }; }
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ProgramActor {
    run: ProgramRunId,
}

impl ProgramActor {
    /// Reconstitutes attribution after the storage reader verifies the retained run reference.
    pub const fn from_recorded_run(run: ProgramRunId) -> Self {
        Self { run }
    }

    /// Returns the exact issuing run.
    pub const fn run(self) -> ProgramRunId {
        self.run
    }
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
