//! Host-side verification and attribution for program session input.

use crate::{Actor, ProgramActor, ProgramRunId};

/// Storage port that verifies a retained program run before capability issuance.
pub trait ProgramRunVerifier {
    /// A storage failure or corrupt journal.
    type Error;

    /// Whether the exact run has a valid retained journal.
    fn verify_run(
        &self,
        run: ProgramRunId,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;
}

/// The program substrate's host-side session capability issuer.
pub struct ProgramSessionHost<Verifier> {
    verifier: Verifier,
}

impl<Verifier: ProgramRunVerifier> ProgramSessionHost<Verifier> {
    /// Binds the host to its retained-run storage verifier.
    pub const fn new(verifier: Verifier) -> Self {
        Self { verifier }
    }

    /// Issues a capability only after storage verifies the exact retained run.
    pub async fn session_capability(
        &self,
        run: ProgramRunId,
    ) -> Result<Option<ProgramSessionCapability>, Verifier::Error> {
        Ok(self
            .verifier
            .verify_run(run)
            .await?
            .then(|| ProgramSessionCapability::new(run)))
    }
}

/// Host-verified attribution, granting no authentication or lifecycle authority.
///
/// ```compile_fail
/// use signalbox_domain::program_session::ProgramSessionCapability;
/// use signalbox_domain::ProgramRunId;
/// fn forge(run: ProgramRunId) { let _ = ProgramSessionCapability::new(run); }
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProgramSessionCapability {
    reference: ProgramActor,
}

impl ProgramSessionCapability {
    const fn new(run: ProgramRunId) -> Self {
        Self {
            reference: ProgramActor::from_recorded_run(run),
        }
    }

    /// The exact verified reference this capability fixes for submitted input.
    pub const fn reference(self) -> ProgramActor {
        self.reference
    }

    /// The issuing run's provenance; it grants no authority.
    pub const fn actor(self) -> Actor {
        Actor::Program {
            run: self.reference,
        }
    }
}

use crate::{
    AcceptedInputId, DurableCommandId, PerInputConfigurationChoices, SessionId, TurnId,
    UserContent, program_registration::ProgramContentDigest,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramSessionTurn {
    pub command: DurableCommandId,
    pub session: SessionId,
    pub content: UserContent,
    pub configuration: PerInputConfigurationChoices,
}

/// A terminal turn's authenticated metadata; no transcript payload is retained.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramSessionOutcome {
    pub session: SessionId,
    pub turn: TurnId,
    pub accepted_input: AcceptedInputId,
    pub digest: ProgramContentDigest,
    pub disposition: ProgramSessionDisposition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgramSessionDisposition {
    Completed,
    Refused,
    Failed,
    Cancelled,
    Retired,
    Ambiguous,
}

/// Initial defaults and durable identity for a program-created session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramSessionCreate {
    pub command: crate::DurableCommandId,
    pub defaults: crate::SessionConfigurationDefaults,
}
