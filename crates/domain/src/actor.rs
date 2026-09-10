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
///
/// ```compile_fail
/// use signalbox_domain::{ProgramActor, ProgramRunId};
/// fn forge(run: ProgramRunId) { let _ = ProgramActor::from_recorded_run(run); }
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ProgramActor {
    run: ProgramRunId,
}

impl ProgramActor {
    /// Reconstitutes attribution after the storage reader verifies the retained run reference.
    pub(crate) const fn from_recorded_run(run: ProgramRunId) -> Self {
        Self { run }
    }

    /// Returns the exact issuing run.
    pub const fn run(self) -> ProgramRunId {
        self.run
    }
}
