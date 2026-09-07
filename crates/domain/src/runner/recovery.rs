//! User-command payloads and terminal outcomes for runner recovery.

use crate::{DurableCommandId, RunnerEnrollmentRequestId, RunnerGeneration, RunnerId, SessionId};

use super::WorkspaceRevision;

/// Closed reasons a runner can refuse replacement provisioning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerProvisioningFailureKind {
    /// Required credential authority is unavailable.
    CredentialUnavailable,
    /// The named repository cannot be acquired.
    RepositoryUnavailable,
    /// The requested sandbox cannot be created.
    SandboxUnavailable,
    /// Existing workspace facts disagree with this authorization.
    WorkspaceConflict,
}

/// One reference-only placement entry and the exact frontier extending its predecessor.
#[derive(Debug)]
pub struct RunnerPlacementBoundary {
    entry: crate::SemanticTranscriptEntry,
    frontier: crate::ResolvedContextFrontierSnapshot,
}

impl RunnerPlacementBoundary {
    /// Appends the replacement reference to the complete authoritative prefix.
    pub fn prepare(
        replacement: &crate::RunnerPlacementReplacement,
        entry: crate::SemanticTranscriptEntryId,
        frontier: crate::ContextFrontierId,
        prior: Option<&crate::ResolvedContextFrontierSnapshot>,
    ) -> Result<Self, crate::RunnerDomainError> {
        let session = replacement.placement.session();
        if replacement.change.session != session
            || replacement.change.prior_revision.checked_next()
                != Some(replacement.placement.revision())
            || replacement.change.replacement_revision != replacement.placement.revision()
            || prior.is_some_and(|prior| prior.frontier().owning_session() != session)
        {
            return Err(crate::RunnerDomainError::CorrelationMismatch);
        }
        let entry = crate::SemanticTranscriptEntry::from_validated_parts(
            entry,
            session,
            crate::SemanticTranscriptEntryPayload::RunnerPlacementChanged {
                placement_revision: replacement.placement.revision(),
            },
        );
        let reference = entry.reference();
        let frontier = match prior {
            Some(prior) => prior
                .derive_appending_candidate(frontier, vec![reference])
                .map_err(|_| crate::RunnerDomainError::InvalidState)?,
            None => crate::ResolvedContextFrontierSnapshot::try_from_candidate(
                session,
                frontier,
                vec![reference],
            )
            .map_err(|_| crate::RunnerDomainError::InvalidState)?,
        };
        Ok(Self { entry, frontier })
    }

    /// The semantic reference to the exact successor record.
    pub fn entry(&self) -> &crate::SemanticTranscriptEntry {
        &self.entry
    }

    /// The successor frontier including the complete prior prefix.
    pub fn frontier(&self) -> &crate::ResolvedContextFrontierSnapshot {
        &self.frontier
    }
}

/// Immutable authority for one command-bound replacement workspace operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerReplacementProvisioning {
    /// Single-use provisioning identity.
    pub authorization: crate::RunnerProvisioningAuthorizationId,
    /// Command that owns this workspace operation.
    pub command: DurableCommandId,
    /// Exact candidate enrollment.
    pub enrollment: crate::RunnerEnrollmentId,
    /// Exact candidate registration revision.
    pub registration_revision: RunnerGeneration,
    /// Owning session.
    pub session: SessionId,
    /// Successor placement revision.
    pub placement_revision: RunnerGeneration,
    /// Cleanup-owning runner.
    pub runner: RunnerId,
    /// Repository to provision; absent for a private root.
    pub repository: Option<crate::WorkspaceRepositoryKey>,
    /// Requested sandbox.
    pub sandbox: crate::RunnerSandboxProfile,
    /// Profile authorized for this repository operation.
    pub credential_profile: Option<crate::CredentialProfileName>,
    /// Exact retained or explicitly requested Git recovery facts.
    pub recovery: Option<crate::WorkspaceRecovery>,
}

/// Replaces a session's lost runner under one durable command identity.
#[derive(Clone, Debug, Eq)]
pub struct ReplaceLostRunner {
    /// User-global command identity.
    pub command_id: DurableCommandId,
    /// Session whose lost placement is recovered.
    pub session: SessionId,
    /// Explicit checkout revision; absence uses the retained workspace recovery facts.
    pub revision: Option<WorkspaceRevision>,
}

impl PartialEq for ReplaceLostRunner {
    fn eq(&self, other: &Self) -> bool {
        self.session == other.session && self.revision == other.revision
    }
}

/// Retires a session's lost placement without cancelling its turn.
#[derive(Clone, Debug, Eq)]
pub struct AbandonLostRunner {
    /// User-global command identity.
    pub command_id: DurableCommandId,
    /// Session whose lost placement is retired.
    pub session: SessionId,
}

impl PartialEq for AbandonLostRunner {
    fn eq(&self, other: &Self) -> bool {
        self.session == other.session
    }
}

/// Promotes the exact pending enrollment without changing any session placement.
#[derive(Clone, Debug, Eq)]
pub struct PromotePendingRunner {
    /// User-global command identity.
    pub command_id: DurableCommandId,
    /// Pending runner-created enrollment request to promote.
    pub enrollment_request: RunnerEnrollmentRequestId,
}

impl PartialEq for PromotePendingRunner {
    fn eq(&self, other: &Self) -> bool {
        self.enrollment_request == other.enrollment_request
    }
}

/// Closed terminal refusals shared by the three administrative commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerRecoveryRejection {
    /// The session identity does not exist.
    SessionNotFound,
    /// The session has no lost placement to consume.
    PlacementNotLost,
    /// An active turn must first finish its existing control flow.
    ExistingControlRequired,
    /// No pending candidate matches the command's subject.
    PendingRunnerNotFound,
    /// The predecessor is not durably lost or the candidate is not connected.
    RunnerUnavailable,
    /// Another command already owns replacement of this placement.
    ReplacementPending,
    /// The candidate does not support the retained placement request.
    PlacementUnavailable,
    /// A checkout revision was supplied for a placement without a repository.
    RevisionWithoutRepository,
    /// The runner refused the command's workspace provisioning operation.
    ProvisioningFailed,
}

/// Committed result of replacement, including replacement before the first pin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplaceLostRunnerResult {
    /// The successor placement is installed at this revision.
    Replaced {
        /// Installed runner identity.
        runner: RunnerId,
        /// Installed positive placement revision.
        placement_revision: RunnerGeneration,
    },
    /// The command settled without installing a successor.
    Rejected(RunnerRecoveryRejection),
}

/// Committed abandonment result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AbandonLostRunnerResult {
    /// The lost placement was retired.
    Abandoned,
    /// The placement was left unchanged.
    Rejected(RunnerRecoveryRejection),
}

/// Committed promotion result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromotePendingRunnerResult {
    /// The candidate's enrollment is now active.
    Promoted {
        /// Promoted runner identity.
        runner: RunnerId,
    },
    /// Enrollment authority was left unchanged.
    Rejected(RunnerRecoveryRejection),
}
