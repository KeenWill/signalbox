//! Runner enrollment, catalog, lease, placement, and credential grants.
//!
//! The normative specification is `docs/spec/runner-protocol.md`.

mod catalog;
mod credential_grant;
mod enrollment;
mod lease;
mod names;
mod placement;
mod recovery;

#[cfg(test)]
mod tests;

pub use catalog::{
    CredentialProfilePolicy, CredentialToolApproval, RunnerAdvertisement, RunnerCatalog,
    RunnerRepositoryEntry, RunnerSandboxProfile, RunnerToolDeclaration, RunnerToolEffectClass,
    RunnerToolModelDefinition, RunnerToolPermissionOverride, RunnerToolPermissionOverrides,
    ToolAdmissibleLoci, WorkspaceCapability,
};
pub use credential_grant::{
    CredentialDispatchAuthorization, CredentialProfileChange, CredentialProfileGrant,
    CredentialProfileGrantReconstitutionInput, CredentialProfileGrantReplacement,
    CredentialProfileGrantState, RunnerCredentialGrantChange,
};
pub use enrollment::{
    PreparedRunnerRegistration, RunnerEnrollment, RunnerEnrollmentReconstitutionInput,
    RunnerEnrollmentState, ValidatedRunnerRegistration,
    ValidatedRunnerRegistrationReconstitutionInput,
};
pub use lease::{
    RunnerClaimedAttemptReplacement, RunnerLease, RunnerLeaseCorrelation, RunnerLeaseLoss,
    RunnerLeaseNoExecutionProof, RunnerLeaseOfferRequest, RunnerLeaseReconstitutionInput,
    RunnerLeaseRetryAuthority, RunnerLeaseRetryPreparation, RunnerLeaseState,
    RunnerToolAttemptAuthorization, RunnerUnclaimedAttemptReauthorization,
};
pub use names::{
    CanonicalCloneUrlDigest, CredentialProfileName, RunnerCapabilityClass, RunnerDomainError,
    RunnerGeneration, RunnerSelector, RunnerWorkingDirectory, WorkspaceBranchName,
    WorkspaceRecovery, WorkspaceRelativePath, WorkspaceRepositoryKey, WorkspaceRevision,
};
pub use placement::{
    AbandonedRunnerPlacement, CredentialProfilePlacementReplacement, LostPinnedRunnerPlacement,
    PinnedRunnerPlacement, ProvisionedWorkspace, RunnerCredentialGrantLineage, RunnerLostBeforePin,
    RunnerPlacementChange, RunnerPlacementLossSource, RunnerPlacementReconstitutionHistory,
    RunnerPlacementReplacement, RunnerPrePinReplacement, RunnerPrePinReplacementHistory,
    SessionRunnerPin, SessionRunnerPlacement, SessionRunnerPlacementReconstitutionInput,
    SessionRunnerPlacementRequest, SessionRunnerPlacementState, WorkingDirectorySelection,
    WorkspaceRequirement,
};
pub use recovery::{
    AbandonLostRunner, AbandonLostRunnerResult, PromotePendingRunner, PromotePendingRunnerResult,
    ReplaceLostRunner, ReplaceLostRunnerResult, RunnerPlacementBoundary,
    RunnerProvisioningFailureKind, RunnerRecoveryRejection, RunnerReplacementProvisioning,
};
