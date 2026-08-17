//! PostgreSQL store for runner-protocol aggregates.
//!
//! SQL rows remain adapter-private. Loads join canonical enrollment,
//! registration, placement, grant, and lease evidence before invoking the
//! domain reconstitution gates.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    num::NonZeroU64,
    pin::Pin,
    sync::Arc,
};

use rust_decimal::{Decimal, prelude::ToPrimitive};
use signalbox_application::{
    AbandonLostRunnerOutcome, AbandonLostRunnerTransaction, ClassifyOperatorFailure,
    ExecutableToolSnapshotEntry, ExecutableToolSnapshotSource, ExecutableToolSnapshotSourceError,
    InitialRunnerDispatchRequest, InitialRunnerDispatchTransaction, OperatorFailureClass,
    PinnedRunnerDispatchRequest, PinnedRunnerDispatchTransaction, PinnedRunnerLeaseOffer,
    PinnedRunnerReplacementIdentities, PinnedRunnerReplacementOutcome,
    PinnedRunnerReplacementTransaction, ReplaceLostRunnerBeforePinOutcome,
    ReplaceLostRunnerBeforePinTransaction, RunnerLeaseClaimRequest, RunnerLeaseClaimTransaction,
    RunnerLeaseResultRequest, RunnerLeaseResultTransaction, RunnerOperationFailureDetail,
    RunnerOperationFailureDetailInput, RunnerReadyManifestDigest,
    RunnerReplacementProvisioningOutcome, RunnerReplacementProvisioningStage,
    RunnerReplacementProvisioningTransaction, RunnerWorkspaceCleanupFailure,
    RunnerWorkspaceCleanupFailureTransaction, RunnerWorkspaceReadyReceipt,
    RunnerWorkspaceReadyTransaction, RunnerWorkspaceReleaseAcknowledgement,
    RunnerWorkspaceReleaseTransaction, ToolCatalog, ToolDefinition, ToolInputSchema,
};
#[cfg(feature = "postgres-integration")]
use signalbox_domain::RunnerWorkspaceReleaseCandidate;
use signalbox_domain::{
    AbandonLostRunner, AbandonLostRunnerRejection, AbandonLostRunnerResult, AbandonedLostRunner,
    AbandonedRunnerPlacement, CanonicalCloneUrlDigest, CredentialDispatchAuthorization,
    CredentialProfileGrant, CredentialProfileGrantReconstitutionInput, CredentialProfileGrantState,
    CredentialProfileName, CredentialProfilePolicy, CredentialToolApproval, DurableCommandId,
    EndedToolAttempt, InitialToolApproval, LostPinnedRunnerPlacement, NormalizedToolArguments,
    PinnedRunnerPlacement, PinnedRunnerReplacementResult, ProvisionedWorkspace, ReplaceLostRunner,
    ReplaceLostRunnerBeforePinRejection, ReplaceLostRunnerBeforePinResult,
    ReplacedLostRunnerBeforePin, ReplacedPinnedRunner, RunnerAdvertisement, RunnerAuthenticationId,
    RunnerCapabilityClass, RunnerCatalog, RunnerClaimedAttemptReplacement,
    RunnerCredentialGrantLineage, RunnerDomainError, RunnerEnrollment, RunnerEnrollmentId,
    RunnerEnrollmentReconstitutionInput, RunnerEnrollmentRequestId, RunnerEnrollmentState,
    RunnerExecutableTool, RunnerGeneration, RunnerId, RunnerLease, RunnerLeaseCompletion,
    RunnerLeaseCorrelation, RunnerLeaseId, RunnerLeaseLoss, RunnerLeaseOfferRequest,
    RunnerLeaseReconstitutionInput, RunnerLeaseRetryPreparation, RunnerLeaseState,
    RunnerLostBeforePin, RunnerPlacementLossSource, RunnerPlacementReconstitutionHistory,
    RunnerPlacementRecoveryState, RunnerPrePinReplacementHistory, RunnerRegistrationReconciliation,
    RunnerReplacementProvisioningRejection, RunnerReplacementTarget,
    RunnerReplacementTargetUnavailableReason, RunnerRepositoryEntry, RunnerSandboxProfile,
    RunnerSelector, RunnerToolDeclaration, RunnerToolEffectClass, RunnerToolModelDefinition,
    RunnerToolPermissionOverride, RunnerToolPermissionOverrides, RunnerWorkingDirectory,
    SemanticTranscriptEntryRef, SessionId, SessionRunnerPin, SessionRunnerPlacement,
    SessionRunnerPlacementReconstitutionInput, SessionRunnerPlacementRequest,
    SessionRunnerPlacementState, StoredRunnerRegistrationLossEvidence, ToolAdmissibleLoci,
    ToolArgumentsKind, ToolAttemptDispatchCorrelation,
    ToolAttemptDispatchCorrelationReconstitutionInput, ToolAttemptEnd, ToolAttemptId,
    ToolDispatchGeneration, ToolEffectClass, ToolExecutionErrorKind, ToolName,
    ToolPermissionDefault, ToolRequestId, TurnAttemptId, TurnId, ValidatedRunnerRegistration,
    ValidatedRunnerRegistrationReconstitutionInput, WorkingDirectorySelection, WorkspaceBranchName,
    WorkspaceCapability, WorkspaceManifestId, WorkspaceProvisioningAuthorizationId,
    WorkspaceRecovery, WorkspaceRelativePath, WorkspaceRepositoryKey, WorkspaceRequirement,
    WorkspaceRevision,
};
use sqlx::{
    PgConnection, PgPool, Postgres, QueryBuilder, Row, Transaction, postgres::PgRow, types::Uuid,
};

use crate::lock_inventory::{
    ABANDON_LOST_RUNNER_SCHEDULER, PROMOTE_PENDING_RUNNER_CONNECTION,
    PROMOTE_PENDING_RUNNER_ENROLLMENTS, REPLACE_LOST_RUNNER_ENROLLMENT_BY_RUNNER,
    REPLACE_LOST_RUNNER_SCHEDULER, RUNNER_CONNECTION_LOSS_HEAD, RUNNER_CONNECTION_LOSS_PROPAGATION,
    RUNNER_ENROLLMENT, RUNNER_GRANT, RUNNER_LEASE_ENROLLMENT_AUTHORITY,
    RUNNER_LEASE_GRANT_AUTHORITY, RUNNER_LEASE_HEAD, RUNNER_LEASE_PLACEMENT,
    RUNNER_PLACEMENT_CONNECTION_AUTHORITY, RUNNER_PLACEMENT_CURRENT_LOSS,
    RUNNER_PLACEMENT_ENROLLMENT_BY_RUNNER, RUNNER_PLACEMENT_HEAD,
    RUNNER_PRISTINE_ACTIVE_ENROLLMENTS, RUNNER_PRISTINE_PENDING_ENROLLMENTS,
    RUNNER_REGISTRATION_HEAD, RUNNER_REGISTRATION_RECONCILIATION,
    RUNNER_REGISTRATION_RECONCILIATION_STATE, RUNNER_RETRY_REPLACEMENT_SCHEDULER,
};
use crate::mapping::{
    AbandonLostRunnerRejectionStorageKind, AbandonLostRunnerResultStorageKind,
    ReplaceLostRunnerRejectionStorageKind, ReplaceLostRunnerResultStorageKind,
    RunnerLossPropagationStateStorageKind, RunnerOperationFailureCategoryStorageKind,
    RunnerOperationFailureOperationStorageKind, ToolAttemptDispositionStorageKind,
    abandon_lost_runner_rejection_from_str, abandon_lost_runner_rejection_to_str,
    abandon_lost_runner_result_from_str, abandon_lost_runner_result_to_str,
    replace_lost_runner_rejection_from_str, replace_lost_runner_rejection_to_str,
    replace_lost_runner_result_from_str, replace_lost_runner_result_to_str,
    runner_connection_state_from_str, runner_connection_state_to_str,
    runner_enrollment_state_from_str, runner_enrollment_state_to_str,
    runner_loss_propagation_state_from_str, runner_loss_propagation_state_to_str,
    runner_operation_failure_category_from_str, runner_operation_failure_category_to_str,
    runner_operation_failure_operation_from_str, runner_operation_failure_operation_to_str,
    runner_placement_loss_source_from_str, runner_placement_loss_source_to_str,
    runner_sandbox_from_str, runner_sandbox_to_str, tool_attempt_disposition_to_str,
    tool_permission_default_from_str, tool_permission_default_to_str,
};

use crate::command_registry::{
    self, ABANDON_LOST_RUNNER_KIND, CommandKind, REPLACE_LOST_RUNNER_KIND, RegistryInspectionError,
};

use crate::outbox::{
    self, DispatchedRunnerState, OutboxEvent, RunnerConnectionOutboxSource, RunnerStateOutboxEvent,
    RunnerStateOutboxSource,
};

#[derive(Clone, Copy)]
enum PlacementProjectionAuthority {
    Generic,
    #[cfg(feature = "postgres-integration")]
    RunnerReplacementTestProjection,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LeaseResultCommitEffect {
    Applied,
    Replay,
}

impl PlacementProjectionAuthority {
    const fn admits_runner_replacement(self) -> bool {
        match self {
            Self::Generic => false,
            #[cfg(feature = "postgres-integration")]
            Self::RunnerReplacementTestProjection => true,
        }
    }
}

/// Adapter-owned positive revision of one validated registration.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RunnerRegistrationRevision(NonZeroU64);

impl RunnerRegistrationRevision {
    /// Returns the first admitted registration revision.
    pub const fn first() -> Self {
        Self(NonZeroU64::MIN)
    }

    /// Admits one nonzero revision value.
    pub const fn try_from_u64(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the positive integer carried by this revision.
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    const fn checked_next(self) -> Option<Self> {
        match self.get().checked_add(1) {
            Some(value) => Self::try_from_u64(value),
            None => None,
        }
    }
}

/// Hub-issued positive identity of one physical runner connection.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RunnerConnectionEpoch(NonZeroU64);

impl RunnerConnectionEpoch {
    /// Admits one nonzero epoch value.
    pub const fn try_from_u64(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the positive integer carried by this epoch.
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    fn checked_next(self) -> Option<Self> {
        self.get().checked_add(1).and_then(Self::try_from_u64)
    }
}

/// Positive append-only epoch of one enrollment's terminal connection losses.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RunnerConnectionLossEpoch(NonZeroU64);

impl RunnerConnectionLossEpoch {
    /// Admits one nonzero loss-epoch value.
    pub const fn try_from_u64(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the positive integer carried by this loss epoch.
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    fn checked_next(self) -> Option<Self> {
        self.get().checked_add(1).and_then(Self::try_from_u64)
    }
}

/// Durable health state of the current physical runner connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerConnectionState {
    /// The current epoch has live admitted transport.
    Connected,
    /// One heartbeat interval elapsed without the outstanding acknowledgement.
    Suspect,
    /// An epoch-targeted clean shutdown was durably observed.
    Shutdown,
    /// Transport or heartbeat evidence durably proved the connection dead.
    Lost,
}

/// Typed evidence for the latest durable connection transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerConnectionCause {
    /// A fresh physical connection received this epoch.
    Established,
    /// A suspect connection supplied its exact outstanding acknowledgement.
    HeartbeatRecovered,
    /// The first heartbeat interval elapsed without acknowledgement.
    HeartbeatMissed,
    /// The hub ordered an epoch-targeted shutdown.
    DaemonShutdown,
    /// The runner ordered an epoch-targeted shutdown.
    RunnerShutdown,
    /// Three heartbeat intervals elapsed without acknowledgement.
    HeartbeatTimeout,
    /// The local transport closed without a shutdown order.
    TransportClosed,
    /// A malformed or inadmissible frame closed the physical connection.
    ProtocolFailure,
    /// Enrollment revocation terminalized a still-live physical connection.
    EnrollmentRevoked,
}

/// One canonical durable connection lifecycle head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunnerConnectionSnapshot {
    epoch: RunnerConnectionEpoch,
    event_ordinal: NonZeroU64,
    state: RunnerConnectionState,
    cause: RunnerConnectionCause,
}

/// Exact terminal connection source named by the current durable loss fence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunnerConnectionLossSnapshot {
    enrollment: RunnerEnrollmentId,
    loss_epoch: RunnerConnectionLossEpoch,
    connection_epoch: RunnerConnectionEpoch,
    connection_event_ordinal: NonZeroU64,
}

/// One bounded restart page for an enrollment's durable connection loss.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerConnectionLossPropagationPage {
    loss: RunnerConnectionLossSnapshot,
    propagated_through: Option<SessionId>,
    sessions: Vec<SessionId>,
    complete: bool,
}

/// Exact registration revision whose availability must be reconciled against
/// every older pinned placement for its enrollment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunnerRegistrationReconciliationSnapshot {
    enrollment: RunnerEnrollmentId,
    registration_revision: RunnerRegistrationRevision,
}

/// One bounded restart page for a durable registration reconciliation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerRegistrationReconciliationPage {
    reconciliation: RunnerRegistrationReconciliationSnapshot,
    propagated_through: Option<SessionId>,
    sessions: Vec<SessionId>,
    complete: bool,
}

/// Durable effect of reconciling one session against a current registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerRegistrationReconciliationDisposition {
    /// The current registration no longer preserves the pinned snapshot.
    RunnerLost,
    /// The current registration preserves every runner-required pinned fact.
    Preserved,
    /// A serialized placement change removed this cursor's candidate.
    Superseded,
    /// The exact session was already committed at or behind this cursor.
    Replayed,
}

/// Durable effect of applying one connection-loss cursor to one session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerConnectionLossSessionDisposition {
    /// The transaction projected the exact loss and advanced its cursor.
    Applied {
        /// Placement state appended by the projection.
        state: DispatchedRunnerState,
        /// Physical attempt retained by the runner-recovery wait, when any.
        interrupted_tool_attempt: Option<ToolAttemptId>,
    },
    /// A serialized placement change made this cursor subject no longer affected.
    Superseded,
    /// The exact session was already committed at or behind this cursor.
    Replayed,
}

impl RunnerConnectionLossPropagationPage {
    /// Returns the exact durable loss whose cursor produced this page.
    pub const fn loss(&self) -> RunnerConnectionLossSnapshot {
        self.loss
    }

    /// Returns the last session atomically committed before this page.
    pub const fn propagated_through(&self) -> Option<SessionId> {
        self.propagated_through
    }

    /// Returns at most 64 affected session identities in canonical order.
    pub fn sessions(&self) -> &[SessionId] {
        &self.sessions
    }

    /// Reports that this loss cursor has durably completed.
    pub const fn is_complete(&self) -> bool {
        self.complete
    }
}

impl RunnerRegistrationReconciliationSnapshot {
    /// Returns the enrollment whose current availability is being reconciled.
    pub const fn enrollment(self) -> RunnerEnrollmentId {
        self.enrollment
    }

    /// Returns the exact durable registration revision being reconciled.
    pub const fn registration_revision(self) -> RunnerRegistrationRevision {
        self.registration_revision
    }
}

impl RunnerRegistrationReconciliationPage {
    /// Returns the exact durable registration reconciliation for this page.
    pub const fn reconciliation(&self) -> RunnerRegistrationReconciliationSnapshot {
        self.reconciliation
    }

    /// Returns the last session atomically committed before this page.
    pub const fn propagated_through(&self) -> Option<SessionId> {
        self.propagated_through
    }

    /// Returns at most 64 pinned session identities in canonical order.
    pub fn sessions(&self) -> &[SessionId] {
        &self.sessions
    }

    /// Reports that this registration cursor has durably completed.
    pub const fn is_complete(&self) -> bool {
        self.complete
    }
}

impl RunnerConnectionLossSnapshot {
    /// Returns the enrollment whose connection became terminally lost.
    pub const fn enrollment(self) -> RunnerEnrollmentId {
        self.enrollment
    }

    /// Returns this enrollment's positive append-only loss epoch.
    pub const fn loss_epoch(self) -> RunnerConnectionLossEpoch {
        self.loss_epoch
    }

    /// Returns the exact terminal physical connection epoch.
    pub const fn connection_epoch(self) -> RunnerConnectionEpoch {
        self.connection_epoch
    }

    /// Returns the exact terminal event ordinal within the connection epoch.
    pub const fn connection_event_ordinal(self) -> u64 {
        self.connection_event_ordinal.get()
    }
}

impl RunnerConnectionSnapshot {
    /// Returns the physical connection epoch.
    pub const fn epoch(self) -> RunnerConnectionEpoch {
        self.epoch
    }

    /// Returns the positive ordinal within this epoch's append-only event stream.
    pub const fn event_ordinal(self) -> u64 {
        self.event_ordinal.get()
    }

    /// Returns the latest durable lifecycle state.
    pub const fn state(self) -> RunnerConnectionState {
        self.state
    }

    /// Returns the typed evidence that produced the latest state.
    pub const fn cause(self) -> RunnerConnectionCause {
        self.cause
    }
}

/// Requested durable transition within one exact connection epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerConnectionTransition {
    /// Checks that an epoch remains current without appending an event.
    Observe,
    /// Restores a suspect epoch after its acknowledgement arrives.
    HeartbeatRecovered,
    /// Records the first missed heartbeat interval.
    HeartbeatMissed,
    /// Records hub-initiated clean shutdown.
    DaemonShutdown,
    /// Records runner-initiated clean shutdown.
    RunnerShutdown,
    /// Records terminal heartbeat loss.
    HeartbeatTimeout,
    /// Records terminal transport loss.
    TransportClosed,
    /// Records terminal protocol failure.
    ProtocolFailure,
}

/// Whether a lifecycle transition named the current or a stale epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerConnectionTransitionOutcome {
    /// The named epoch remains current and carries this lifecycle head.
    Current(RunnerConnectionSnapshot),
    /// A newer physical connection owns lifecycle authority.
    Stale {
        /// Epoch named by the refused transition.
        observed: RunnerConnectionEpoch,
        /// Epoch that currently owns lifecycle authority.
        current: RunnerConnectionEpoch,
    },
}

/// One nonterminal current connection selected by the startup scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NonterminalRunnerConnection {
    enrollment: RunnerEnrollmentId,
    epoch: RunnerConnectionEpoch,
}

impl NonterminalRunnerConnection {
    /// Returns the enrollment that owns the connection.
    pub const fn enrollment(self) -> RunnerEnrollmentId {
        self.enrollment
    }

    /// Returns the physical connection epoch selected by the scan.
    pub const fn epoch(self) -> RunnerConnectionEpoch {
        self.epoch
    }
}

/// One lifecycle event that was appended for an enrollment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AppliedRunnerConnectionTransition {
    enrollment: RunnerEnrollmentId,
    snapshot: RunnerConnectionSnapshot,
}

impl AppliedRunnerConnectionTransition {
    /// Returns the enrollment whose lifecycle event was appended.
    pub const fn enrollment(self) -> RunnerEnrollmentId {
        self.enrollment
    }

    /// Returns the durable lifecycle head produced by the append.
    pub const fn snapshot(self) -> RunnerConnectionSnapshot {
        self.snapshot
    }
}

/// Whether a requested lifecycle transition appended a durable event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerConnectionTransitionEffect {
    /// The request appended this enrollment-owned lifecycle event.
    Applied(AppliedRunnerConnectionTransition),
    /// The request left durable state unchanged and observed this outcome.
    Unchanged(RunnerConnectionTransitionOutcome),
}

impl RunnerConnectionTransitionEffect {
    /// Returns the transition outcome independently of whether it appended.
    pub const fn outcome(self) -> RunnerConnectionTransitionOutcome {
        match self {
            Self::Applied(applied) => {
                RunnerConnectionTransitionOutcome::Current(applied.snapshot())
            }
            Self::Unchanged(outcome) => outcome,
        }
    }
}

/// One canonical validated registration plus its durable adapter revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredValidatedRunnerRegistration {
    revision: RunnerRegistrationRevision,
    registration: ValidatedRunnerRegistration,
}

/// Identities issued by the daemon for one logical runner enrollment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IssuedRunnerEnrollmentIdentities {
    enrollment: RunnerEnrollmentId,
    runner: RunnerId,
    authentication: RunnerAuthenticationId,
}

impl IssuedRunnerEnrollmentIdentities {
    /// Labels the three independently issued runner identities.
    pub const fn new(
        enrollment: RunnerEnrollmentId,
        runner: RunnerId,
        authentication: RunnerAuthenticationId,
    ) -> Self {
        Self {
            enrollment,
            runner,
            authentication,
        }
    }

    /// Returns the logical enrollment identity.
    pub const fn enrollment(self) -> RunnerEnrollmentId {
        self.enrollment
    }

    /// Returns the logical runner identity.
    pub const fn runner(self) -> RunnerId {
        self.runner
    }

    /// Returns the daemon-owned authentication-reference identity.
    pub const fn authentication(self) -> RunnerAuthenticationId {
        self.authentication
    }
}

/// Complete labeled input for one pristine runner enrollment attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PristineRunnerEnrollmentRequest {
    request: RunnerEnrollmentRequestId,
    issued: IssuedRunnerEnrollmentIdentities,
    allowed_classes: Vec<RunnerCapabilityClass>,
    advertisement: RunnerAdvertisement,
}

impl PristineRunnerEnrollmentRequest {
    /// Separates daemon-issued identity and policy from peer-advertised availability.
    pub fn new(
        request: RunnerEnrollmentRequestId,
        issued: IssuedRunnerEnrollmentIdentities,
        allowed_classes: impl IntoIterator<Item = RunnerCapabilityClass>,
        advertisement: RunnerAdvertisement,
    ) -> Self {
        Self {
            request,
            issued,
            allowed_classes: allowed_classes.into_iter().collect(),
            advertisement,
        }
    }

    /// Returns the runner-created stable request identity.
    pub const fn request(&self) -> RunnerEnrollmentRequestId {
        self.request
    }

    /// Returns the candidate daemon-issued identities.
    pub const fn issued(&self) -> IssuedRunnerEnrollmentIdentities {
        self.issued
    }

    /// Iterates daemon-owned allowed capability classes.
    pub fn allowed_classes(&self) -> impl Iterator<Item = &RunnerCapabilityClass> {
        self.allowed_classes.iter()
    }

    /// Returns peer-advertised availability.
    pub const fn advertisement(&self) -> &RunnerAdvertisement {
        &self.advertisement
    }
}

/// Whether pristine enrollment created authority or replayed its exact receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerEnrollmentDisposition {
    /// This request atomically created the enrollment and first registration.
    Created,
    /// This request returned the identities and registration it created earlier.
    Replayed,
}

/// Durable authority issued by one enrollment request receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerEnrollmentAuthority {
    /// The request created ordinary active runner authority.
    Active,
    /// The request created provisioning-only successor authority.
    ReplacementPending,
}

/// Durable response facts for enrollment or registration resume.
#[derive(Debug, Eq, PartialEq)]
pub struct RunnerEnrollmentReceipt {
    request: RunnerEnrollmentRequestId,
    authority: RunnerEnrollmentAuthority,
    enrollment: RunnerEnrollment,
    registration: StoredValidatedRunnerRegistration,
}

impl RunnerEnrollmentReceipt {
    /// Returns the stable enrollment-request identity.
    pub const fn request(&self) -> RunnerEnrollmentRequestId {
        self.request
    }

    /// Returns the immutable authority kind issued to this request.
    pub const fn authority(&self) -> RunnerEnrollmentAuthority {
        self.authority
    }

    /// Returns the canonical enrollment authority.
    pub const fn enrollment(&self) -> &RunnerEnrollment {
        &self.enrollment
    }

    /// Returns the exact identities issued for this request.
    pub const fn identities(&self) -> IssuedRunnerEnrollmentIdentities {
        IssuedRunnerEnrollmentIdentities::new(
            self.enrollment.enrollment(),
            self.enrollment.runner(),
            self.enrollment.authentication(),
        )
    }

    /// Returns the canonical validated registration and durable revision.
    pub const fn registration(&self) -> &StoredValidatedRunnerRegistration {
        &self.registration
    }

    /// Reconstructs the complete availability-only advertisement.
    pub fn advertisement(&self) -> RunnerAdvertisement {
        let registration = self.registration.registration();
        RunnerAdvertisement::new(
            registration.classes().cloned(),
            registration.tool_names().cloned(),
            registration
                .profiles()
                .map(|profile| profile.name().clone()),
            registration.workspaces(),
            registration.sandboxes(),
            registration.repositories().cloned(),
        )
    }

    /// Separates the canonical enrollment authority and registration receipt.
    pub fn into_parts(
        self,
    ) -> (
        RunnerEnrollmentRequestId,
        RunnerEnrollmentAuthority,
        RunnerEnrollment,
        StoredValidatedRunnerRegistration,
    ) {
        (
            self.request,
            self.authority,
            self.enrollment,
            self.registration,
        )
    }
}

/// Evidence-bearing result of a pristine enrollment request.
#[derive(Debug, Eq, PartialEq)]
pub struct RunnerEnrollmentOutcome {
    disposition: RunnerEnrollmentDisposition,
    receipt: RunnerEnrollmentReceipt,
}

/// Immutable provisioning-only successor admission and its exact loss source.
#[derive(Debug, Eq, PartialEq)]
pub struct PendingRunnerEnrollment {
    predecessor: RunnerEnrollmentId,
    predecessor_loss_epoch: RunnerConnectionLossEpoch,
    receipt: RunnerEnrollmentReceipt,
}

impl PendingRunnerEnrollment {
    /// Returns the active enrollment whose exact durable loss admitted this candidate.
    pub const fn predecessor(&self) -> RunnerEnrollmentId {
        self.predecessor
    }

    /// Returns the predecessor loss epoch observed by admission.
    pub const fn predecessor_loss_epoch(&self) -> RunnerConnectionLossEpoch {
        self.predecessor_loss_epoch
    }

    /// Returns the candidate's exact immutable enrollment receipt.
    pub const fn receipt(&self) -> &RunnerEnrollmentReceipt {
        &self.receipt
    }
}

impl RunnerEnrollmentOutcome {
    /// Reports whether the durable authority was created or replayed.
    pub const fn disposition(&self) -> RunnerEnrollmentDisposition {
        self.disposition
    }

    /// Returns the exact durable receipt.
    pub const fn receipt(&self) -> &RunnerEnrollmentReceipt {
        &self.receipt
    }

    /// Consumes the outcome into its exact durable receipt.
    pub fn into_receipt(self) -> RunnerEnrollmentReceipt {
        self.receipt
    }
}

impl StoredValidatedRunnerRegistration {
    /// Returns the durable adapter revision paired with the registration.
    pub const fn revision(&self) -> RunnerRegistrationRevision {
        self.revision
    }

    /// Returns the domain-validated registration snapshot.
    pub const fn registration(&self) -> &ValidatedRunnerRegistration {
        &self.registration
    }
}

/// One canonical placement record and its adapter event ordinal.
#[derive(Debug, Eq, PartialEq)]
pub struct StoredSessionRunnerPlacement {
    event_ordinal: u64,
    placement: SessionRunnerPlacement,
    registration: Option<StoredValidatedRunnerRegistration>,
    grant: Option<CredentialProfileGrant>,
    interrupted_tool_attempt: Option<ToolAttemptId>,
}

/// Immutable relational facts for one staged repository-provisioning boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredWorkspaceProvisioningAuthorization {
    command: DurableCommandId,
    authorization: WorkspaceProvisioningAuthorizationId,
    session: SessionId,
    lost_placement_event_ordinal: u64,
    lost_placement_revision: RunnerGeneration,
    successor_placement_revision: RunnerGeneration,
    enrollment: RunnerEnrollmentId,
    runner: RunnerId,
    registration_revision: RunnerGeneration,
    connection_epoch: RunnerConnectionEpoch,
    connection_event_ordinal: u64,
    repository: WorkspaceRepositoryKey,
    sandbox: RunnerSandboxProfile,
    credential_profile: Option<CredentialProfileName>,
}

impl StoredWorkspaceProvisioningAuthorization {
    /// Returns the durable replacement command that owns the authorization.
    pub const fn command(&self) -> DurableCommandId {
        self.command
    }

    /// Returns the single-use provisioning identity.
    pub const fn authorization(&self) -> WorkspaceProvisioningAuthorizationId {
        self.authorization
    }

    /// Returns the owning session.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the exact lost-placement event ordinal observed at staging.
    pub const fn lost_placement_event_ordinal(&self) -> u64 {
        self.lost_placement_event_ordinal
    }

    /// Returns the exact lost placement revision observed at staging.
    pub const fn lost_placement_revision(&self) -> RunnerGeneration {
        self.lost_placement_revision
    }

    /// Returns the successor placement revision being provisioned.
    pub const fn successor_placement_revision(&self) -> RunnerGeneration {
        self.successor_placement_revision
    }

    /// Returns the selected enrollment.
    pub const fn enrollment(&self) -> RunnerEnrollmentId {
        self.enrollment
    }

    /// Returns the selected runner.
    pub const fn runner(&self) -> RunnerId {
        self.runner
    }

    /// Returns the exact selected registration revision.
    pub const fn registration_revision(&self) -> RunnerGeneration {
        self.registration_revision
    }

    /// Returns the connected epoch observed at staging.
    pub const fn connection_epoch(&self) -> RunnerConnectionEpoch {
        self.connection_epoch
    }

    /// Returns the connected event ordinal observed at staging.
    pub const fn connection_event_ordinal(&self) -> u64 {
        self.connection_event_ordinal
    }

    /// Returns the exact repository key.
    pub const fn repository(&self) -> &WorkspaceRepositoryKey {
        &self.repository
    }

    /// Returns the immutable sandbox profile.
    pub const fn sandbox(&self) -> RunnerSandboxProfile {
        self.sandbox
    }

    /// Returns the exact optional credential profile.
    pub const fn credential_profile(&self) -> Option<&CredentialProfileName> {
        self.credential_profile.as_ref()
    }
}

/// One immutable ready-workspace receipt retained for replacement replay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredWorkspaceProvisioningReceipt {
    authorization: WorkspaceProvisioningAuthorizationId,
    session: SessionId,
    placement_revision: RunnerGeneration,
    runner: RunnerId,
    manifest: WorkspaceManifestId,
    manifest_digest: String,
    repository: WorkspaceRepositoryKey,
    canonical_clone_url_digest: CanonicalCloneUrlDigest,
    credential_profile: Option<CredentialProfileName>,
    sandbox: RunnerSandboxProfile,
    relative_path: WorkspaceRelativePath,
    recovery: WorkspaceRecovery,
}

impl StoredWorkspaceProvisioningReceipt {
    /// Returns the single-use authorization consumed by the receipt.
    pub const fn authorization(&self) -> WorkspaceProvisioningAuthorizationId {
        self.authorization
    }

    /// Returns the session whose successor workspace became ready.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the successor placement revision.
    pub const fn placement_revision(&self) -> RunnerGeneration {
        self.placement_revision
    }

    /// Returns the runner that created the workspace.
    pub const fn runner(&self) -> RunnerId {
        self.runner
    }

    /// Returns the stable workspace-manifest identity.
    pub const fn manifest_id(&self) -> WorkspaceManifestId {
        self.manifest
    }

    /// Returns the retained canonical-shaped ready-manifest digest bytes.
    pub fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }

    /// Returns the authorized repository key.
    pub const fn repository(&self) -> &WorkspaceRepositoryKey {
        &self.repository
    }

    /// Returns the runner-authored canonical clone-URL digest.
    pub const fn canonical_clone_url_digest(&self) -> &CanonicalCloneUrlDigest {
        &self.canonical_clone_url_digest
    }

    /// Returns the optional profile used while provisioning.
    pub const fn credential_profile(&self) -> Option<&CredentialProfileName> {
        self.credential_profile.as_ref()
    }

    /// Returns the sandbox profile bound by the authorization.
    pub const fn sandbox(&self) -> RunnerSandboxProfile {
        self.sandbox
    }

    /// Returns the runner-root-relative workspace path from the manifest.
    pub const fn relative_path(&self) -> &WorkspaceRelativePath {
        &self.relative_path
    }

    /// Returns the repository recovery facts from the ready manifest.
    pub const fn recovery(&self) -> &WorkspaceRecovery {
        &self.recovery
    }
}

/// One relationally authenticated pending managed-workspace release.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoredRunnerWorkspaceRelease {
    session: SessionId,
    placement_revision: RunnerGeneration,
    runner: RunnerId,
    manifest: WorkspaceManifestId,
    retired_placement_event_ordinal: u64,
    successor_placement_event_ordinal: u64,
    enrollment: RunnerEnrollmentId,
    connection_epoch: RunnerConnectionEpoch,
    connection_event_ordinal: u64,
}

impl StoredRunnerWorkspaceRelease {
    /// Returns the retired session named by the wire correlation.
    pub const fn session(self) -> SessionId {
        self.session
    }

    /// Returns the exact retired placement revision.
    pub const fn placement_revision(self) -> RunnerGeneration {
        self.placement_revision
    }

    /// Returns the cleanup-owning runner.
    pub const fn runner(self) -> RunnerId {
        self.runner
    }

    /// Returns the protected predecessor manifest identity.
    pub const fn manifest_id(self) -> WorkspaceManifestId {
        self.manifest
    }

    /// Returns the storage-only retired placement event ordinal.
    pub const fn retired_placement_event_ordinal(self) -> u64 {
        self.retired_placement_event_ordinal
    }

    /// Returns the storage-only successor placement event ordinal.
    pub const fn successor_placement_event_ordinal(self) -> u64 {
        self.successor_placement_event_ordinal
    }

    /// Returns the enrollment that retained cleanup authority at enqueue.
    pub const fn enrollment(self) -> RunnerEnrollmentId {
        self.enrollment
    }

    /// Returns the connected epoch authenticated at enqueue.
    pub const fn connection_epoch(self) -> RunnerConnectionEpoch {
        self.connection_epoch
    }

    /// Returns the connected event ordinal authenticated at enqueue.
    pub const fn connection_event_ordinal(self) -> u64 {
        self.connection_event_ordinal
    }
}

/// Immutable proof that loss of the cleanup-owning connection retired one
/// pending managed-workspace release as unowned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoredRunnerWorkspaceReleaseLossRetirement {
    session: SessionId,
    placement_revision: RunnerGeneration,
    runner: RunnerId,
    manifest: WorkspaceManifestId,
    loss: RunnerConnectionLossSnapshot,
}

impl StoredRunnerWorkspaceReleaseLossRetirement {
    /// Returns the retired session named by the release correlation.
    pub const fn session(self) -> SessionId {
        self.session
    }

    /// Returns the exact retired placement revision.
    pub const fn placement_revision(self) -> RunnerGeneration {
        self.placement_revision
    }

    /// Returns the cleanup-owning runner.
    pub const fn runner(self) -> RunnerId {
        self.runner
    }

    /// Returns the protected predecessor manifest identity.
    pub const fn manifest_id(self) -> WorkspaceManifestId {
        self.manifest
    }

    /// Returns the exact durable physical-connection loss that made the
    /// release unowned.
    pub const fn loss(self) -> RunnerConnectionLossSnapshot {
        self.loss
    }
}

/// One relationally authenticated active turn parked on runner loss.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoredRunnerRecoveryWait {
    turn: TurnId,
    runner: RunnerId,
    placement_revision: RunnerGeneration,
    interrupted_tool_attempt: Option<ToolAttemptId>,
}

impl StoredRunnerRecoveryWait {
    /// Returns the active turn retaining the session's progressing slot.
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    /// Returns the exact lost runner.
    pub const fn runner(&self) -> RunnerId {
        self.runner
    }

    /// Returns the positive placement revision against which loss was projected.
    pub const fn placement_revision(&self) -> RunnerGeneration {
        self.placement_revision
    }

    /// Returns the physical tool attempt interrupted by loss, when one exists.
    pub const fn interrupted_tool_attempt(&self) -> Option<ToolAttemptId> {
        self.interrupted_tool_attempt
    }
}

impl StoredSessionRunnerPlacement {
    /// Returns the durable placement event ordinal.
    pub const fn event_ordinal(&self) -> u64 {
        self.event_ordinal
    }

    /// Returns the domain-reconstituted placement.
    pub const fn placement(&self) -> &SessionRunnerPlacement {
        &self.placement
    }

    /// Returns the registration snapshot pinned by this placement, if any.
    pub const fn registration(&self) -> Option<&StoredValidatedRunnerRegistration> {
        self.registration.as_ref()
    }

    /// Returns the credential grant pinned by this placement, if any.
    pub const fn grant(&self) -> Option<&CredentialProfileGrant> {
        self.grant.as_ref()
    }

    /// Returns the physical tool attempt named by this exact loss record.
    pub const fn interrupted_tool_attempt(&self) -> Option<ToolAttemptId> {
        self.interrupted_tool_attempt
    }

    /// Separates the placement from its durable ordinal and pinned evidence.
    pub fn into_parts(
        self,
    ) -> (
        u64,
        SessionRunnerPlacement,
        Option<StoredValidatedRunnerRegistration>,
        Option<CredentialProfileGrant>,
        Option<ToolAttemptId>,
    ) {
        (
            self.event_ordinal,
            self.placement,
            self.registration,
            self.grant,
            self.interrupted_tool_attempt,
        )
    }
}

/// PostgreSQL adapter for runner-protocol state.
struct RegistrationAuthority<'a> {
    stored: &'a StoredValidatedRunnerRegistration,
    catalog: &'a RunnerCatalog,
}

/// PostgreSQL adapter for runner enrollment, placement, grant, and lease state.
#[derive(Clone, Debug)]
pub struct RunnerProtocolStore {
    pool: PgPool,
    catalog: Arc<RunnerCatalog>,
}

/// PostgreSQL-backed session-aware executable-tool snapshot source.
#[derive(Clone, Debug)]
pub struct PostgresExecutableToolSnapshotSource {
    store: RunnerProtocolStore,
}

impl PostgresExecutableToolSnapshotSource {
    /// Uses the runner protocol store's exact durable placement and catalog.
    pub const fn new(store: RunnerProtocolStore) -> Self {
        Self { store }
    }
}

#[derive(Debug)]
enum PostgresExecutableToolSnapshotFailure {
    Store(RunnerProtocolStoreError),
    AmbiguousCapabilitySelection,
    InvalidRunnerDefinition(ToolName),
    IncompatibleDaemonDefinition(ToolName),
}

impl fmt::Display for PostgresExecutableToolSnapshotFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(_) => formatter.write_str("runner snapshot storage failed"),
            Self::AmbiguousCapabilitySelection => {
                formatter.write_str("runner snapshot selection is ambiguous")
            }
            Self::InvalidRunnerDefinition(tool) => {
                write!(
                    formatter,
                    "runner snapshot definition for {} is invalid",
                    tool.as_str()
                )
            }
            Self::IncompatibleDaemonDefinition(tool) => {
                write!(
                    formatter,
                    "runner and daemon definitions for {} disagree",
                    tool.as_str()
                )
            }
        }
    }
}

impl Error for PostgresExecutableToolSnapshotFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::AmbiguousCapabilitySelection
            | Self::InvalidRunnerDefinition(_)
            | Self::IncompatibleDaemonDefinition(_) => None,
        }
    }
}

impl ClassifyOperatorFailure for PostgresExecutableToolSnapshotFailure {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Store(error) => error.operator_failure_class(),
            Self::AmbiguousCapabilitySelection
            | Self::InvalidRunnerDefinition(_)
            | Self::IncompatibleDaemonDefinition(_) => OperatorFailureClass::CallerOrHubBug,
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        match self {
            Self::Store(error) => error.operator_failure_cause_code(),
            Self::AmbiguousCapabilitySelection => "runner_snapshot_ambiguous_selection",
            Self::InvalidRunnerDefinition(_) => "runner_snapshot_invalid_definition",
            Self::IncompatibleDaemonDefinition(_) => "runner_snapshot_catalog_mismatch",
        }
    }
}

impl From<RunnerProtocolStoreError> for PostgresExecutableToolSnapshotFailure {
    fn from(error: RunnerProtocolStoreError) -> Self {
        Self::Store(error)
    }
}

impl From<sqlx::Error> for PostgresExecutableToolSnapshotFailure {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(RunnerProtocolStoreError::Database(error))
    }
}

impl ExecutableToolSnapshotSource for PostgresExecutableToolSnapshotSource {
    fn executable_tools<'a>(
        &'a self,
        session: SessionId,
        daemon_catalog: &'a dyn ToolCatalog,
        dangerous_tool_auto_approval: signalbox_domain::DangerousToolAutoApproval,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        Box<[ExecutableToolSnapshotEntry]>,
                        ExecutableToolSnapshotSourceError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.store
                .executable_tool_snapshot(session, daemon_catalog, dangerous_tool_auto_approval)
                .await
                .map_err(ExecutableToolSnapshotSourceError::new)
        })
    }
}

impl RunnerProtocolStore {
    /// Uses the supplied pool and runner catalog for durable protocol state.
    pub fn new(pool: PgPool, catalog: RunnerCatalog) -> Self {
        Self {
            pool,
            catalog: Arc::new(catalog),
        }
    }

    async fn executable_tool_snapshot(
        &self,
        session: SessionId,
        daemon_catalog: &dyn ToolCatalog,
        dangerous_tool_auto_approval: signalbox_domain::DangerousToolAutoApproval,
    ) -> Result<Box<[ExecutableToolSnapshotEntry]>, PostgresExecutableToolSnapshotFailure> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let row = sqlx::query(
            "SELECT record.*
               FROM runner_current_session_placement AS current_placement
               JOIN runner_session_placement_record AS record
                 ON record.session_id = current_placement.session_id
                AND record.event_ordinal = current_placement.event_ordinal
              WHERE current_placement.session_id = $1",
        )
        .bind(session.into_uuid())
        .fetch_optional(transaction.as_mut())
        .await?;
        let runner_tools = match row {
            Some(row) => {
                let stored = self
                    .decode_stored_placement_in(&mut transaction, &row)
                    .await?;
                match stored.placement().state() {
                    SessionRunnerPlacementState::RunnerLostBeforePin(_)
                    | SessionRunnerPlacementState::RunnerLost(_) => {
                        return Err(RunnerProtocolStoreError::Domain(
                            RunnerDomainError::InvalidState,
                        )
                        .into());
                    }
                    SessionRunnerPlacementState::RunnerAbandoned(_) => Box::new([]),
                    SessionRunnerPlacementState::Unpinned
                    | SessionRunnerPlacementState::Pinned(_) => {
                        let enrollment = self
                            .executable_tool_enrollment_in(&mut transaction, &stored)
                            .await?;
                        match enrollment {
                            Some(enrollment) => {
                                let registration = self
                                    .load_live_current_registration_in(&mut transaction, enrollment)
                                    .await?;
                                match registration {
                                    Some(registration) => stored
                                        .placement()
                                        .runner_executable_tools(registration.registration())
                                        .map_err(RunnerProtocolStoreError::Domain)?,
                                    None => Box::new([]),
                                }
                            }
                            None => Box::new([]),
                        }
                    }
                }
            }
            None => Box::new([]),
        };
        transaction.commit().await?;
        merge_executable_tool_snapshot(
            self.catalog.as_ref(),
            runner_tools,
            daemon_catalog,
            dangerous_tool_auto_approval,
        )
    }

    async fn executable_tool_enrollment_in(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        stored: &StoredSessionRunnerPlacement,
    ) -> Result<Option<RunnerEnrollmentId>, PostgresExecutableToolSnapshotFailure> {
        if let SessionRunnerPlacementState::Pinned(_) = stored.placement().state() {
            return Ok(stored
                .registration()
                .map(|registration| registration.registration().enrollment()));
        }
        let rows = match &stored.placement().request().selector {
            RunnerSelector::Identity(runner) => {
                sqlx::query(
                    "SELECT enrollment.enrollment_id
                       FROM runner_enrollment AS enrollment
                       JOIN runner_current_registration AS current_registration
                         ON current_registration.enrollment_id = enrollment.enrollment_id
                       JOIN LATERAL (
                            SELECT state_kind
                              FROM runner_connection_event
                             WHERE enrollment_id = enrollment.enrollment_id
                             ORDER BY connection_epoch DESC, event_ordinal DESC
                             LIMIT 1
                       ) AS connection ON connection.state_kind = 'connected'
                      WHERE enrollment.state_kind = 'active'
                        AND enrollment.runner_id = $1
                      ORDER BY enrollment.enrollment_id
                      LIMIT 2",
                )
                .bind(runner.into_uuid())
                .fetch_all(&mut **transaction)
                .await?
            }
            RunnerSelector::CapabilityClass(class) => {
                sqlx::query(
                    "SELECT enrollment.enrollment_id
                       FROM runner_enrollment AS enrollment
                       JOIN runner_enrollment_allowed_class AS allowed_class
                         ON allowed_class.enrollment_id = enrollment.enrollment_id
                       JOIN runner_current_registration AS current_registration
                         ON current_registration.enrollment_id = enrollment.enrollment_id
                       JOIN LATERAL (
                            SELECT state_kind
                              FROM runner_connection_event
                             WHERE enrollment_id = enrollment.enrollment_id
                             ORDER BY connection_epoch DESC, event_ordinal DESC
                             LIMIT 1
                       ) AS connection ON connection.state_kind = 'connected'
                      WHERE enrollment.state_kind = 'active'
                        AND allowed_class.capability_class = $1
                      ORDER BY enrollment.enrollment_id
                      LIMIT 2",
                )
                .bind(class.as_str())
                .fetch_all(&mut **transaction)
                .await?
            }
        };
        match rows.as_slice() {
            [] => Ok(None),
            [row] => Ok(Some(runner_enrollment_id(
                row.decode_column("enrollment_id")?,
            ))),
            [_, _, ..] => Err(PostgresExecutableToolSnapshotFailure::AmbiguousCapabilitySelection),
        }
    }

    async fn load_live_current_registration_in(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        enrollment_id: RunnerEnrollmentId,
    ) -> Result<Option<StoredValidatedRunnerRegistration>, RunnerProtocolStoreError> {
        let connection = load_connection_head_in(transaction.as_mut(), enrollment_id).await?;
        if connection.map(RunnerConnectionSnapshot::state) != Some(RunnerConnectionState::Connected)
        {
            return Ok(None);
        }
        let enrollment = load_enrollment_in(transaction.as_mut(), enrollment_id)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        if enrollment.state() != RunnerEnrollmentState::Active {
            return Ok(None);
        }
        let revision: Option<Decimal> = sqlx::query_scalar(
            "SELECT registration_revision
               FROM runner_current_registration
              WHERE enrollment_id = $1",
        )
        .bind(enrollment_id.into_uuid())
        .fetch_optional(&mut **transaction)
        .await?;
        let revision = revision.ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        load_registration_in(
            transaction.as_mut(),
            enrollment_id,
            decode_registration_revision(revision)?,
            Some(&enrollment),
            &self.catalog,
        )
        .await
    }

    /// Allocates and durably records the next connection epoch for an enrollment.
    pub async fn open_connection(
        &self,
        enrollment: RunnerEnrollmentId,
    ) -> Result<RunnerConnectionSnapshot, RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        let locked = sqlx::query(RUNNER_ENROLLMENT)
            .bind(enrollment.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        if locked.is_none() {
            return Err(RunnerProtocolCorruption::MissingCanonicalEnrollment.into());
        }
        let state: String = sqlx::query_scalar(
            "SELECT state_kind
               FROM runner_enrollment
              WHERE enrollment_id = $1",
        )
        .bind(enrollment.into_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        match state.as_str() {
            "pending" | "active" => {}
            "revoked" => {
                transaction.rollback().await?;
                return Err(RunnerProtocolStoreError::Domain(
                    RunnerDomainError::EnrollmentRevoked,
                ));
            }
            _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
        }
        let prior = load_connection_head_in(transaction.as_mut(), enrollment).await?;
        let prior_was_suspect = match prior {
            Some(RunnerConnectionSnapshot {
                state: RunnerConnectionState::Suspect,
                ..
            }) => true,
            None
            | Some(RunnerConnectionSnapshot {
                state:
                    RunnerConnectionState::Connected
                    | RunnerConnectionState::Shutdown
                    | RunnerConnectionState::Lost,
                ..
            }) => false,
        };
        let epoch = match prior {
            Some(prior) => prior
                .epoch()
                .checked_next()
                .ok_or(RunnerProtocolCorruption::GenerationExhausted)?,
            None => RunnerConnectionEpoch::try_from_u64(1)
                .ok_or(RunnerProtocolCorruption::InvalidEncoding)?,
        };
        sqlx::query(
            "INSERT INTO runner_connection_event
                (enrollment_id, connection_epoch, event_ordinal,
                 state_kind, cause_kind)
             VALUES ($1, $2, 1, 'connected', 'established')",
        )
        .bind(enrollment.into_uuid())
        .bind(Decimal::from(epoch.get()))
        .execute(&mut *transaction)
        .await?;
        let snapshot = RunnerConnectionSnapshot {
            epoch,
            event_ordinal: NonZeroU64::MIN,
            state: RunnerConnectionState::Connected,
            cause: RunnerConnectionCause::Established,
        };
        advance_runner_connection_authority_head(
            transaction.as_mut(),
            enrollment,
            prior,
            snapshot,
            None,
        )
        .await?;
        if prior_was_suspect {
            append_runner_connection_health_events(transaction.as_mut(), enrollment, snapshot)
                .await?;
        }
        match commit_mutation(transaction).await {
            Ok(()) => Ok(snapshot),
            Err(error @ RunnerProtocolStoreError::CommitAmbiguous(_)) => self
                .reconcile_open_connection(enrollment, epoch)
                .await?
                .ok_or(error),
            Err(error) => Err(error),
        }
    }

    async fn reconcile_open_connection(
        &self,
        enrollment: RunnerEnrollmentId,
        epoch: RunnerConnectionEpoch,
    ) -> Result<Option<RunnerConnectionSnapshot>, RunnerProtocolStoreError> {
        let row = sqlx::query(
            "SELECT state_kind, cause_kind
               FROM runner_connection_event
              WHERE enrollment_id = $1
                AND connection_epoch = $2
                AND event_ordinal = 1",
        )
        .bind(enrollment.into_uuid())
        .bind(Decimal::from(epoch.get()))
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let state: String = row.decode_column("state_kind")?;
        let cause: String = row.decode_column("cause_kind")?;
        if state != "connected" || cause != "established" {
            return Err(RunnerProtocolCorruption::InvalidEncoding.into());
        }
        Ok(Some(RunnerConnectionSnapshot {
            epoch,
            event_ordinal: NonZeroU64::MIN,
            state: RunnerConnectionState::Connected,
            cause: RunnerConnectionCause::Established,
        }))
    }

    /// Appends one lifecycle transition only when the caller names the current epoch.
    pub async fn transition_connection(
        &self,
        enrollment: RunnerEnrollmentId,
        epoch: RunnerConnectionEpoch,
        transition: RunnerConnectionTransition,
    ) -> Result<RunnerConnectionTransitionOutcome, RunnerProtocolStoreError> {
        let effect = self
            .transition_connection_with_effect(enrollment, epoch, transition)
            .await?;
        Ok(effect.outcome())
    }

    /// Appends one lifecycle transition and reports whether it changed durable state.
    pub async fn transition_connection_with_effect(
        &self,
        enrollment: RunnerEnrollmentId,
        epoch: RunnerConnectionEpoch,
        transition: RunnerConnectionTransition,
    ) -> Result<RunnerConnectionTransitionEffect, RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        let locked = sqlx::query(RUNNER_ENROLLMENT)
            .bind(enrollment.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        if locked.is_none() {
            return Err(RunnerProtocolCorruption::MissingCanonicalEnrollment.into());
        }
        let current = load_connection_head_in(transaction.as_mut(), enrollment)
            .await?
            .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        if epoch != current.epoch() {
            transaction.rollback().await?;
            return Ok(RunnerConnectionTransitionEffect::Unchanged(
                RunnerConnectionTransitionOutcome::Stale {
                    observed: epoch,
                    current: current.epoch(),
                },
            ));
        }
        if matches!(
            current.state(),
            RunnerConnectionState::Shutdown | RunnerConnectionState::Lost
        ) || transition == RunnerConnectionTransition::Observe
            || matches!(
                (current.state(), transition),
                (
                    RunnerConnectionState::Connected,
                    RunnerConnectionTransition::HeartbeatRecovered
                ) | (
                    RunnerConnectionState::Suspect,
                    RunnerConnectionTransition::HeartbeatMissed
                )
            )
        {
            transaction.rollback().await?;
            return Ok(RunnerConnectionTransitionEffect::Unchanged(
                RunnerConnectionTransitionOutcome::Current(current),
            ));
        }
        let event_ordinal = NonZeroU64::new(
            current
                .event_ordinal()
                .checked_add(1)
                .ok_or(RunnerProtocolCorruption::GenerationExhausted)?,
        )
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        let (state, cause, cause_kind) = connection_transition_values(transition)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        sqlx::query(
            "INSERT INTO runner_connection_event
                (enrollment_id, connection_epoch, event_ordinal,
                 state_kind, cause_kind)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(enrollment.into_uuid())
        .bind(Decimal::from(epoch.get()))
        .bind(Decimal::from(event_ordinal.get()))
        .bind(runner_connection_state_to_str(state))
        .bind(cause_kind)
        .execute(&mut *transaction)
        .await?;
        let snapshot = RunnerConnectionSnapshot {
            epoch,
            event_ordinal,
            state,
            cause,
        };
        let loss =
            append_runner_connection_loss_epoch(transaction.as_mut(), enrollment, snapshot).await?;
        advance_runner_connection_authority_head(
            transaction.as_mut(),
            enrollment,
            Some(current),
            snapshot,
            loss,
        )
        .await?;
        append_runner_connection_health_events(transaction.as_mut(), enrollment, snapshot).await?;
        commit_mutation(transaction).await?;
        Ok(RunnerConnectionTransitionEffect::Applied(
            AppliedRunnerConnectionTransition {
                enrollment,
                snapshot: RunnerConnectionSnapshot {
                    epoch,
                    event_ordinal,
                    state,
                    cause,
                },
            },
        ))
    }

    /// Loads the latest durable health state for one enrollment.
    pub async fn load_connection(
        &self,
        enrollment: RunnerEnrollmentId,
    ) -> Result<Option<RunnerConnectionSnapshot>, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let snapshot = load_connection_head_in(transaction.as_mut(), enrollment).await?;
        transaction.commit().await?;
        Ok(snapshot)
    }

    /// Loads the latest terminal connection source retained by the loss fence.
    pub async fn load_current_connection_loss(
        &self,
        enrollment: RunnerEnrollmentId,
    ) -> Result<Option<RunnerConnectionLossSnapshot>, RunnerProtocolStoreError> {
        let row = sqlx::query(
            "SELECT loss.loss_epoch, loss.connection_epoch,
                    loss.connection_event_ordinal
               FROM runner_current_connection_loss AS current_loss
               JOIN runner_connection_loss_epoch AS loss
                 ON loss.enrollment_id = current_loss.enrollment_id
                AND loss.loss_epoch = current_loss.loss_epoch
              WHERE current_loss.enrollment_id = $1",
        )
        .bind(enrollment.into_uuid())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            let loss_epoch = RunnerConnectionLossEpoch::try_from_u64(decode_u64(
                row.decode_column("loss_epoch")?,
            )?)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
            let connection_epoch = RunnerConnectionEpoch::try_from_u64(decode_u64(
                row.decode_column("connection_epoch")?,
            )?)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
            let connection_event_ordinal =
                NonZeroU64::new(decode_u64(row.decode_column("connection_event_ordinal")?)?)
                    .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
            Ok(RunnerConnectionLossSnapshot {
                enrollment,
                loss_epoch,
                connection_epoch,
                connection_event_ordinal,
            })
        })
        .transpose()
    }

    /// Loads every durable connection-loss cursor that still requires session
    /// propagation, including cursors left pending by an earlier daemon.
    pub async fn load_pending_connection_losses(
        &self,
    ) -> Result<Vec<RunnerConnectionLossSnapshot>, RunnerProtocolStoreError> {
        let rows = sqlx::query(
            "SELECT propagation.enrollment_id, propagation.loss_epoch,
                    loss.connection_epoch, loss.connection_event_ordinal
               FROM runner_connection_loss_propagation AS propagation
               JOIN runner_connection_loss_epoch AS loss
                 ON loss.enrollment_id = propagation.enrollment_id
                AND loss.loss_epoch = propagation.loss_epoch
              WHERE propagation.state_kind = 'pending'
              ORDER BY propagation.enrollment_id, propagation.loss_epoch",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let enrollment = runner_enrollment_id(row.decode_column("enrollment_id")?);
                let loss_epoch = RunnerConnectionLossEpoch::try_from_u64(decode_u64(
                    row.decode_column("loss_epoch")?,
                )?)
                .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
                let connection_epoch = RunnerConnectionEpoch::try_from_u64(decode_u64(
                    row.decode_column("connection_epoch")?,
                )?)
                .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
                let connection_event_ordinal =
                    NonZeroU64::new(decode_u64(row.decode_column("connection_event_ordinal")?)?)
                        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
                Ok(RunnerConnectionLossSnapshot {
                    enrollment,
                    loss_epoch,
                    connection_epoch,
                    connection_event_ordinal,
                })
            })
            .collect()
    }

    /// Loads the next bounded, ordered session page for one durable loss cursor.
    pub async fn load_connection_loss_propagation_page(
        &self,
        loss: RunnerConnectionLossSnapshot,
    ) -> Result<RunnerConnectionLossPropagationPage, RunnerProtocolStoreError> {
        const PAGE_LIMIT: i64 = 64;

        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let cursor = sqlx::query(
            "SELECT propagation.propagated_through_session_id,
                    propagation.state_kind,
                    loss.connection_epoch, loss.connection_event_ordinal
               FROM runner_connection_loss_propagation AS propagation
               JOIN runner_connection_loss_epoch AS loss
                 ON loss.enrollment_id = propagation.enrollment_id
                AND loss.loss_epoch = propagation.loss_epoch
              WHERE propagation.enrollment_id = $1
                AND propagation.loss_epoch = $2",
        )
        .bind(loss.enrollment().into_uuid())
        .bind(Decimal::from(loss.loss_epoch().get()))
        .fetch_optional(transaction.as_mut())
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalLoss)?;
        let stored_connection_epoch = RunnerConnectionEpoch::try_from_u64(decode_u64(
            cursor.decode_column("connection_epoch")?,
        )?)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        let stored_connection_event_ordinal =
            decode_u64(cursor.decode_column("connection_event_ordinal")?)?;
        if stored_connection_epoch != loss.connection_epoch()
            || stored_connection_event_ordinal != loss.connection_event_ordinal()
        {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        let propagated_through = cursor
            .decode_column::<Option<Uuid>>("propagated_through_session_id")?
            .map(session_id);
        let state: String = cursor.decode_column("state_kind")?;
        let complete = match runner_loss_propagation_state_from_str(&state)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding)?
        {
            RunnerLossPropagationStateStorageKind::Pending => false,
            RunnerLossPropagationStateStorageKind::Completed => true,
        };
        let sessions = if complete {
            Vec::new()
        } else {
            sqlx::query_scalar::<_, Uuid>(
                "WITH affected_session AS (
                    SELECT placement.session_id
                      FROM runner_current_session_placement AS current_placement
                      JOIN runner_session_placement_record AS placement
                        ON placement.session_id = current_placement.session_id
                       AND placement.event_ordinal = current_placement.event_ordinal
                      JOIN runner_enrollment AS lost_enrollment
                        ON lost_enrollment.enrollment_id = $1
                     WHERE (
                           placement.loss_fence_enrollment_id = $1
                           OR (
                               placement.loss_fence_enrollment_id IS NULL
                               AND placement.state_kind = 'unpinned'
                               AND placement.selector_kind = 'identity'
                               AND placement.selector_runner_id =
                                   lost_enrollment.runner_id
                           )
                       )
                       AND (
                           placement.observed_runner_loss_epoch IS NULL
                           OR placement.observed_runner_loss_epoch < $2
                       )
                       AND (
                           placement.state_kind = 'pinned'
                           OR (
                               placement.state_kind = 'unpinned'
                               AND placement.selector_kind = 'identity'
                           )
                       )
                    UNION
                    SELECT release.session_id
                      FROM runner_workspace_release AS release
                      LEFT JOIN runner_workspace_release_acknowledgement AS acknowledgement
                        ON acknowledgement.session_id = release.session_id
                       AND acknowledgement.placement_revision =
                           release.placement_revision
                      LEFT JOIN runner_workspace_release_loss_retirement AS retirement
                        ON retirement.session_id = release.session_id
                       AND retirement.placement_revision =
                           release.placement_revision
                     WHERE release.enrollment_id = $1
                       AND release.connection_epoch = $5
                       AND acknowledgement.session_id IS NULL
                       AND retirement.session_id IS NULL
                )
                SELECT session_id
                  FROM affected_session
                 WHERE ($3::uuid IS NULL OR session_id > $3)
                 ORDER BY session_id
                 LIMIT $4",
            )
            .bind(loss.enrollment().into_uuid())
            .bind(Decimal::from(loss.loss_epoch().get()))
            .bind(propagated_through.map(SessionId::into_uuid))
            .bind(PAGE_LIMIT)
            .bind(Decimal::from(loss.connection_epoch().get()))
            .fetch_all(transaction.as_mut())
            .await?
            .into_iter()
            .map(session_id)
            .collect()
        };
        transaction.commit().await?;
        Ok(RunnerConnectionLossPropagationPage {
            loss,
            propagated_through,
            sessions,
            complete,
        })
    }

    /// Projects one exact connection loss into one session and advances the
    /// restart cursor in the same transaction.
    pub async fn propagate_connection_loss_session(
        &self,
        loss: RunnerConnectionLossSnapshot,
        session: SessionId,
    ) -> Result<RunnerConnectionLossSessionDisposition, RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        require_runner_loss_session_scheduler(&mut transaction, session).await?;
        require_runner_loss_authority(&mut transaction, loss).await?;
        let cursor = lock_runner_loss_propagation(&mut transaction, loss).await?;
        if cursor
            .propagated_through
            .is_some_and(|committed| committed.as_uuid() >= session.as_uuid())
        {
            transaction.rollback().await?;
            return Ok(RunnerConnectionLossSessionDisposition::Replayed);
        }
        if cursor.complete {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }

        let prior = sqlx::query(RUNNER_PLACEMENT_HEAD)
            .bind(session.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
        let affected = placement_is_affected_by_loss(&prior, loss)?;
        if !affected {
            retire_workspace_releases_for_connection_loss(&mut transaction, loss, session).await?;
            advance_runner_loss_cursor(&mut transaction, loss, session).await?;
            commit_mutation(transaction).await?;
            return Ok(RunnerConnectionLossSessionDisposition::Superseded);
        }

        let stored = self
            .decode_stored_placement_in(&mut transaction, &prior)
            .await?;
        let (prior_event_ordinal, placement, registration, _grant, _prior_interrupted_tool_attempt) =
            stored.into_parts();
        let current_lease = self
            .load_current_loss_lease_in(&mut transaction, session, prior_event_ordinal)
            .await?;
        let interrupted_tool_attempt = current_lease.as_ref().map(RunnerLease::attempt);
        let selected_runner = placement_loss_fence_runner(&placement)
            .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
        let (event_kind, state, lost) = match placement.state() {
            SessionRunnerPlacementState::Unpinned => {
                let lost = placement
                    .mark_runner_lost_before_pin(selected_runner)
                    .map_err(RunnerProtocolStoreError::Domain)?;
                (
                    "runner_lost_before_pin",
                    DispatchedRunnerState::RunnerLostBeforePin,
                    lost,
                )
            }
            SessionRunnerPlacementState::Pinned(_) => {
                let lost = placement
                    .mark_runner_lost()
                    .map_err(RunnerProtocolStoreError::Domain)?;
                ("runner_lost", DispatchedRunnerState::RunnerLost, lost)
            }
            SessionRunnerPlacementState::RunnerLostBeforePin(_)
            | SessionRunnerPlacementState::RunnerLost(_)
            | SessionRunnerPlacementState::RunnerAbandoned(_) => {
                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
            }
        };
        if event_kind == "runner_lost_before_pin" && current_lease.is_some() {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        let event_ordinal = prior_event_ordinal
            .checked_add(1)
            .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
        let grant_origin = placement_grant_origin(Some(&prior), event_ordinal, &lost)?;
        insert_placement_record(
            &mut transaction,
            event_ordinal,
            event_kind,
            &lost,
            PlacementRecordEvidence {
                registration_identity: stored_registration_identity(registration.as_ref()),
                grant_origin,
                interrupted_tool_attempt,
                loss_registration_revision: None,
            },
        )
        .await?;
        let changed = sqlx::query(
            "UPDATE runner_current_session_placement
                SET event_ordinal = $2
              WHERE session_id = $1 AND event_ordinal = $3",
        )
        .bind(session.into_uuid())
        .bind(Decimal::from(event_ordinal))
        .bind(Decimal::from(prior_event_ordinal))
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }

        if let Some(lease) = current_lease {
            persist_runner_loss_lease_and_wait(&mut transaction, &lost, lease).await?;
        }
        outbox::append(
            transaction.as_mut(),
            OutboxEvent::RunnerStateTransition(RunnerStateOutboxEvent {
                session,
                runner: placement_loss_fence_runner(&lost)
                    .ok_or(RunnerProtocolCorruption::CrossWiredReference)?,
                placement_revision: lost.revision(),
                sandbox: lost.request().sandbox,
                working_directory: lost_runner_working_directory(&lost),
                state,
                source: RunnerStateOutboxSource {
                    placement_event_ordinal: event_ordinal,
                    connection: None,
                },
            }),
        )
        .await?;
        retire_workspace_releases_for_connection_loss(&mut transaction, loss, session).await?;
        advance_runner_loss_cursor(&mut transaction, loss, session).await?;
        commit_mutation(transaction).await?;
        Ok(RunnerConnectionLossSessionDisposition::Applied {
            state,
            interrupted_tool_attempt,
        })
    }

    /// Marks one loss cursor complete after every affected session committed.
    pub async fn complete_connection_loss_propagation(
        &self,
        loss: RunnerConnectionLossSnapshot,
    ) -> Result<(), RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        require_runner_loss_authority(&mut transaction, loss).await?;
        let cursor = lock_runner_loss_propagation(&mut transaction, loss).await?;
        if cursor.complete {
            transaction.rollback().await?;
            return Ok(());
        }
        let changed = sqlx::query(
            "UPDATE runner_connection_loss_propagation
                SET state_kind = 'completed'
              WHERE enrollment_id = $1 AND loss_epoch = $2
                AND state_kind = 'pending'",
        )
        .bind(loss.enrollment().into_uuid())
        .bind(Decimal::from(loss.loss_epoch().get()))
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        commit_mutation(transaction).await
    }

    async fn lock_registration_reconciliation_authority(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        reconciliation: RunnerRegistrationReconciliationSnapshot,
    ) -> Result<StoredValidatedRunnerRegistration, RunnerProtocolStoreError> {
        let enrollment_id = reconciliation.enrollment();
        let locked = sqlx::query(RUNNER_ENROLLMENT)
            .bind(enrollment_id.into_uuid())
            .fetch_optional(&mut **transaction)
            .await?;
        if locked.is_none() {
            return Err(RunnerProtocolCorruption::MissingCanonicalEnrollment.into());
        }
        sqlx::query_scalar::<_, Decimal>(RUNNER_PLACEMENT_CONNECTION_AUTHORITY)
            .bind(enrollment_id.into_uuid())
            .fetch_optional(&mut **transaction)
            .await?;
        sqlx::query_scalar::<_, Decimal>(RUNNER_PLACEMENT_CURRENT_LOSS)
            .bind(enrollment_id.into_uuid())
            .fetch_optional(&mut **transaction)
            .await?;
        let current = sqlx::query_scalar::<_, Decimal>(RUNNER_REGISTRATION_HEAD)
            .bind(enrollment_id.into_uuid())
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        if decode_registration_revision(current)? != reconciliation.registration_revision() {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        let enrollment = load_enrollment_in(transaction.as_mut(), enrollment_id)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        load_registration_in(
            transaction.as_mut(),
            enrollment_id,
            reconciliation.registration_revision(),
            Some(&enrollment),
            &self.catalog,
        )
        .await?
        .ok_or_else(|| RunnerProtocolCorruption::MissingCanonicalRegistration.into())
    }

    /// Loads every current registration whose bounded placement reconciliation
    /// remains pending.
    pub async fn load_pending_registration_reconciliations(
        &self,
    ) -> Result<Vec<RunnerRegistrationReconciliationSnapshot>, RunnerProtocolStoreError> {
        let rows = sqlx::query(
            "SELECT reconciliation.enrollment_id,
                    reconciliation.registration_revision
               FROM runner_registration_reconciliation AS reconciliation
               JOIN runner_current_registration AS current_registration
                 ON current_registration.enrollment_id = reconciliation.enrollment_id
                AND current_registration.registration_revision =
                    reconciliation.registration_revision
              WHERE reconciliation.state_kind = 'pending'
              ORDER BY reconciliation.enrollment_id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(RunnerRegistrationReconciliationSnapshot {
                    enrollment: runner_enrollment_id(row.decode_column("enrollment_id")?),
                    registration_revision: decode_registration_revision(
                        row.decode_column("registration_revision")?,
                    )?,
                })
            })
            .collect()
    }

    /// Loads the next bounded, ordered session page for one registration
    /// reconciliation cursor.
    pub async fn load_registration_reconciliation_page(
        &self,
        reconciliation: RunnerRegistrationReconciliationSnapshot,
    ) -> Result<RunnerRegistrationReconciliationPage, RunnerProtocolStoreError> {
        const PAGE_LIMIT: i64 = 64;

        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let cursor = sqlx::query(
            "SELECT reconciliation.propagated_through_session_id,
                    reconciliation.state_kind,
                    current_registration.registration_revision AS current_revision
               FROM runner_registration_reconciliation AS reconciliation
               JOIN runner_current_registration AS current_registration
                 ON current_registration.enrollment_id = reconciliation.enrollment_id
              WHERE reconciliation.enrollment_id = $1
                AND reconciliation.registration_revision = $2",
        )
        .bind(reconciliation.enrollment().into_uuid())
        .bind(Decimal::from(reconciliation.registration_revision().get()))
        .fetch_optional(transaction.as_mut())
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        let current = decode_registration_revision(cursor.decode_column("current_revision")?)?;
        if current != reconciliation.registration_revision() {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        let propagated_through = cursor
            .decode_column::<Option<Uuid>>("propagated_through_session_id")?
            .map(session_id);
        let state: String = cursor.decode_column("state_kind")?;
        let complete = match state.as_str() {
            "pending" => false,
            "completed" => true,
            _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
        };
        let sessions = if complete {
            Vec::new()
        } else {
            sqlx::query_scalar::<_, Uuid>(
                "SELECT placement.session_id
                   FROM runner_current_session_placement AS current_placement
                   JOIN runner_session_placement_record AS placement
                     ON placement.session_id = current_placement.session_id
                    AND placement.event_ordinal = current_placement.event_ordinal
                   LEFT JOIN runner_registration_reconciliation_observation AS observed
                     ON observed.enrollment_id = $1
                    AND observed.registration_revision = $2
                    AND observed.session_id = placement.session_id
                  WHERE placement.state_kind = 'pinned'
                    AND placement.registration_enrollment_id = $1
                    AND placement.registration_revision < $2
                    AND observed.session_id IS NULL
                    AND ($3::uuid IS NULL OR placement.session_id > $3)
                  ORDER BY placement.session_id
                  LIMIT $4",
            )
            .bind(reconciliation.enrollment().into_uuid())
            .bind(Decimal::from(reconciliation.registration_revision().get()))
            .bind(propagated_through.map(SessionId::into_uuid))
            .bind(PAGE_LIMIT)
            .fetch_all(transaction.as_mut())
            .await?
            .into_iter()
            .map(session_id)
            .collect()
        };
        transaction.commit().await?;
        Ok(RunnerRegistrationReconciliationPage {
            reconciliation,
            propagated_through,
            sessions,
            complete,
        })
    }

    /// Reconciles one current pinned placement against one exact current
    /// registration and advances the restart cursor atomically.
    pub async fn reconcile_registration_session(
        &self,
        reconciliation: RunnerRegistrationReconciliationSnapshot,
        session: SessionId,
    ) -> Result<RunnerRegistrationReconciliationDisposition, RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        require_runner_loss_session_scheduler(&mut transaction, session).await?;
        let registration = self
            .lock_registration_reconciliation_authority(&mut transaction, reconciliation)
            .await?;
        let cursor = lock_registration_reconciliation(&mut transaction, reconciliation).await?;
        if cursor
            .propagated_through
            .is_some_and(|committed| committed.as_uuid() >= session.as_uuid())
        {
            transaction.rollback().await?;
            return Ok(RunnerRegistrationReconciliationDisposition::Replayed);
        }
        if cursor.complete {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }

        let prior = sqlx::query(RUNNER_PLACEMENT_HEAD)
            .bind(session.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
        let prior_event_ordinal = decode_u64(prior.decode_column("event_ordinal")?)?;
        if !placement_is_registration_reconciliation_candidate(&prior, reconciliation)? {
            insert_registration_reconciliation_observation(
                &mut transaction,
                reconciliation,
                session,
                prior_event_ordinal,
                "superseded",
            )
            .await?;
            advance_registration_reconciliation_cursor(&mut transaction, reconciliation, session)
                .await?;
            commit_mutation(transaction).await?;
            return Ok(RunnerRegistrationReconciliationDisposition::Superseded);
        }

        let stored = self
            .decode_stored_placement_in(&mut transaction, &prior)
            .await?;
        let (
            stored_event_ordinal,
            placement,
            pinned_registration,
            _grant,
            prior_interrupted_attempt,
        ) = stored.into_parts();
        if stored_event_ordinal != prior_event_ordinal || prior_interrupted_attempt.is_some() {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        let current_lease = self
            .load_current_loss_lease_in(&mut transaction, session, prior_event_ordinal)
            .await?;
        let interrupted_attempt = current_lease.as_ref().map(RunnerLease::attempt);
        let pinned_registration =
            pinned_registration
                .as_ref()
                .ok_or(RunnerProtocolStoreError::Corruption(
                    RunnerProtocolCorruption::MissingCanonicalRegistration,
                ))?;
        let reconciled = placement
            .reconcile_registration(RunnerRegistrationReconciliation {
                pinned_registration: pinned_registration.registration().clone(),
                current_registration: registration.registration().clone(),
            })
            .map_err(RunnerProtocolStoreError::Domain)?;
        if matches!(reconciled.state(), SessionRunnerPlacementState::Pinned(_)) {
            insert_registration_reconciliation_observation(
                &mut transaction,
                reconciliation,
                session,
                prior_event_ordinal,
                "preserved",
            )
            .await?;
            advance_registration_reconciliation_cursor(&mut transaction, reconciliation, session)
                .await?;
            commit_mutation(transaction).await?;
            return Ok(RunnerRegistrationReconciliationDisposition::Preserved);
        }

        let event_ordinal = prior_event_ordinal
            .checked_add(1)
            .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
        let grant_origin = placement_grant_origin(Some(&prior), event_ordinal, &reconciled)?;
        insert_placement_record(
            &mut transaction,
            event_ordinal,
            "runner_lost",
            &reconciled,
            PlacementRecordEvidence {
                registration_identity: stored_registration_identity(Some(pinned_registration)),
                grant_origin,
                interrupted_tool_attempt: interrupted_attempt,
                loss_registration_revision: Some(reconciliation.registration_revision()),
            },
        )
        .await?;
        let changed = sqlx::query(
            "UPDATE runner_current_session_placement
                SET event_ordinal = $2
              WHERE session_id = $1 AND event_ordinal = $3",
        )
        .bind(session.into_uuid())
        .bind(Decimal::from(event_ordinal))
        .bind(Decimal::from(prior_event_ordinal))
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        if let Some(lease) = current_lease {
            persist_runner_loss_lease_and_wait(&mut transaction, &reconciled, lease).await?;
        }
        insert_registration_reconciliation_observation(
            &mut transaction,
            reconciliation,
            session,
            event_ordinal,
            "runner_lost",
        )
        .await?;
        outbox::append(
            transaction.as_mut(),
            OutboxEvent::RunnerStateTransition(RunnerStateOutboxEvent {
                session,
                runner: placement_loss_fence_runner(&reconciled)
                    .ok_or(RunnerProtocolCorruption::CrossWiredReference)?,
                placement_revision: reconciled.revision(),
                sandbox: reconciled.request().sandbox,
                working_directory: lost_runner_working_directory(&reconciled),
                state: DispatchedRunnerState::RunnerLost,
                source: RunnerStateOutboxSource {
                    placement_event_ordinal: event_ordinal,
                    connection: None,
                },
            }),
        )
        .await?;
        advance_registration_reconciliation_cursor(&mut transaction, reconciliation, session)
            .await?;
        commit_mutation(transaction).await?;
        Ok(RunnerRegistrationReconciliationDisposition::RunnerLost)
    }

    /// Marks one registration cursor complete after every candidate session
    /// committed an authenticated observation.
    pub async fn complete_registration_reconciliation(
        &self,
        reconciliation: RunnerRegistrationReconciliationSnapshot,
    ) -> Result<(), RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        self.lock_registration_reconciliation_authority(&mut transaction, reconciliation)
            .await?;
        let cursor = lock_registration_reconciliation(&mut transaction, reconciliation).await?;
        if cursor.complete {
            transaction.rollback().await?;
            return Ok(());
        }
        let changed = sqlx::query(
            "UPDATE runner_registration_reconciliation
                SET state_kind = 'completed'
              WHERE enrollment_id = $1 AND registration_revision = $2
                AND state_kind = 'pending'",
        )
        .bind(reconciliation.enrollment().into_uuid())
        .bind(Decimal::from(reconciliation.registration_revision().get()))
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        commit_mutation(transaction).await
    }

    /// Loads every current nonterminal connection head for startup reconciliation.
    pub async fn load_nonterminal_connection_heads(
        &self,
    ) -> Result<Vec<NonterminalRunnerConnection>, RunnerProtocolStoreError> {
        let rows = sqlx::query(
            "SELECT DISTINCT ON (enrollment_id)
                    enrollment_id, connection_epoch, state_kind
               FROM runner_connection_event
              ORDER BY enrollment_id, connection_epoch DESC, event_ordinal DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut connections = Vec::new();
        for row in rows {
            let state: String = row.decode_column("state_kind")?;
            if state != "connected" && state != "suspect" {
                continue;
            }
            let enrollment = runner_enrollment_id(row.decode_column("enrollment_id")?);
            let epoch = RunnerConnectionEpoch::try_from_u64(decode_u64(
                row.decode_column("connection_epoch")?,
            )?)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
            connections.push(NonterminalRunnerConnection { enrollment, epoch });
        }
        Ok(connections)
    }

    /// Atomically creates one pristine enrollment and first registration, or
    /// returns the exact durable receipt for an equal request replay.
    pub async fn enroll_pristine(
        &self,
        request: PristineRunnerEnrollmentRequest,
    ) -> Result<RunnerEnrollmentOutcome, RunnerProtocolStoreError> {
        let PristineRunnerEnrollmentRequest {
            request,
            issued,
            allowed_classes,
            advertisement,
        } = request;
        if advertisement.repositories().count() > RunnerAdvertisement::MAX_REPOSITORIES {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::TooManyAdvertisedRepositories,
            ));
        }

        let mut transaction = self.pool.begin().await?;
        // This admission path takes a share-row-exclusive table lock to make
        // request replay and the version-one active-enrollment check one
        // serial decision. Domain persistence remains capable of representing
        // predecessor and replacement facts required by runner replacement.
        sqlx::query("LOCK TABLE runner_enrollment IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut *transaction)
            .await?;
        if let Some(receipt) =
            load_enrollment_request_receipt_in(transaction.as_mut(), request, &self.catalog).await?
        {
            let stored_allowed: BTreeSet<_> =
                receipt.enrollment().allowed_classes().cloned().collect();
            let replayed_allowed: BTreeSet<_> = allowed_classes.iter().cloned().collect();
            if stored_allowed != replayed_allowed {
                transaction.rollback().await?;
                return Err(
                    RunnerEnrollmentRequestFailure::ReplayPolicyMismatch { request }.into(),
                );
            }
            if receipt.advertisement() != advertisement {
                transaction.rollback().await?;
                return Err(
                    RunnerEnrollmentRequestFailure::ReplayAdvertisementMismatch { request }.into(),
                );
            }
            transaction.commit().await?;
            return Ok(RunnerEnrollmentOutcome {
                disposition: RunnerEnrollmentDisposition::Replayed,
                receipt,
            });
        }

        let admission = select_pristine_enrollment_admission(&mut transaction, request).await?;
        let (enrollment, authority) = match admission {
            PristineEnrollmentAdmission::Active => (
                RunnerEnrollment::new(
                    issued.enrollment(),
                    issued.runner(),
                    issued.authentication(),
                    allowed_classes,
                ),
                RunnerEnrollmentAuthority::Active,
            ),
            PristineEnrollmentAdmission::ReplacementPending { .. } => (
                RunnerEnrollment::new_pending(
                    issued.enrollment(),
                    issued.runner(),
                    issued.authentication(),
                    allowed_classes,
                ),
                RunnerEnrollmentAuthority::ReplacementPending,
            ),
        };
        let pending = enrollment
            .prepare_registration(advertisement, &self.catalog)
            .map_err(RunnerProtocolStoreError::Domain)?;
        let revision = RunnerRegistrationRevision::first();
        if pending.registration().revision().get() != revision.get() {
            transaction.rollback().await?;
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::RegistrationChanged,
            ));
        }

        insert_enrollment_rows(&mut transaction, &enrollment).await?;
        insert_registration(&mut transaction, revision, pending.registration()).await?;
        sqlx::query(
            "INSERT INTO runner_current_registration
                (enrollment_id, registration_revision)
             VALUES ($1, $2)",
        )
        .bind(issued.enrollment().into_uuid())
        .bind(Decimal::from(revision.get()))
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO runner_enrollment_request_receipt
                (request_id, enrollment_id, runner_id,
                 authentication_reference_id, registration_revision,
                 authority_kind)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(request.into_uuid())
        .bind(issued.enrollment().into_uuid())
        .bind(issued.runner().into_uuid())
        .bind(issued.authentication().into_uuid())
        .bind(Decimal::from(revision.get()))
        .bind(encode_enrollment_authority(authority))
        .execute(&mut *transaction)
        .await?;
        if let PristineEnrollmentAdmission::ReplacementPending {
            predecessor,
            loss_epoch,
        } = admission
        {
            sqlx::query(
                "INSERT INTO runner_pending_enrollment
                    (request_id, enrollment_id, predecessor_enrollment_id,
                     predecessor_loss_epoch)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(request.into_uuid())
            .bind(issued.enrollment().into_uuid())
            .bind(predecessor.into_uuid())
            .bind(Decimal::from(loss_epoch.get()))
            .execute(&mut *transaction)
            .await?;
        }
        commit_mutation(transaction).await?;

        let registration = pending.commit().map_err(RunnerProtocolStoreError::Domain)?;
        Ok(RunnerEnrollmentOutcome {
            disposition: RunnerEnrollmentDisposition::Created,
            receipt: RunnerEnrollmentReceipt {
                request,
                authority,
                enrollment,
                registration: StoredValidatedRunnerRegistration {
                    revision,
                    registration,
                },
            },
        })
    }

    /// Loads one pending successor by its stable enrollment-request identity.
    pub async fn load_pending_enrollment(
        &self,
        request: RunnerEnrollmentRequestId,
    ) -> Result<Option<PendingRunnerEnrollment>, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let relation = sqlx::query(
            "SELECT pending.enrollment_id, pending.predecessor_enrollment_id,
                    pending.predecessor_loss_epoch
               FROM runner_pending_enrollment AS pending
               JOIN runner_enrollment AS candidate
                 ON candidate.enrollment_id = pending.enrollment_id
                AND candidate.state_kind = $2
              WHERE pending.request_id = $1",
        )
        .bind(request.into_uuid())
        .bind(runner_enrollment_state_to_str(
            RunnerEnrollmentState::Pending,
        ))
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(relation) = relation else {
            transaction.commit().await?;
            return Ok(None);
        };
        let receipt =
            load_enrollment_request_receipt_in(transaction.as_mut(), request, &self.catalog)
                .await?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        if receipt.authority() != RunnerEnrollmentAuthority::ReplacementPending
            || receipt.enrollment().enrollment()
                != runner_enrollment_id(relation.decode_column("enrollment_id")?)
        {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        let predecessor =
            runner_enrollment_id(relation.decode_column("predecessor_enrollment_id")?);
        let predecessor_loss_epoch = RunnerConnectionLossEpoch::try_from_u64(decode_u64(
            relation.decode_column("predecessor_loss_epoch")?,
        )?)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        transaction.commit().await?;
        Ok(Some(PendingRunnerEnrollment {
            predecessor,
            predecessor_loss_epoch,
            receipt,
        }))
    }

    /// Validates a reconnect and durably appends changed availability when the
    /// runner names the current registration revision.
    pub async fn resume_registration(
        &self,
        request: RunnerEnrollmentRequestId,
        observed: IssuedRunnerEnrollmentIdentities,
        prior_revision: RunnerRegistrationRevision,
        advertisement: RunnerAdvertisement,
    ) -> Result<RunnerEnrollmentReceipt, RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        let stored = load_enrollment_request_facts(transaction.as_mut(), request)
            .await?
            .ok_or(RunnerEnrollmentRequestFailure::UnknownRequest { request })?;
        let locked = sqlx::query(RUNNER_ENROLLMENT)
            .bind(stored.identities.enrollment().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        if locked.is_none() {
            return Err(RunnerProtocolCorruption::MissingCanonicalEnrollment.into());
        }
        if stored.identities != observed {
            transaction.rollback().await?;
            return Err(RunnerEnrollmentRequestFailure::ResumeIdentityMismatch {
                request,
                expected: stored.identities,
                observed,
            }
            .into());
        }
        let enrollment = load_enrollment_in(transaction.as_mut(), stored.identities.enrollment())
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        if enrollment.state() == RunnerEnrollmentState::Revoked {
            transaction.rollback().await?;
            return Err(RunnerEnrollmentRequestFailure::EnrollmentRevoked {
                request,
                enrollment: enrollment.enrollment(),
            }
            .into());
        }
        let current: Option<Decimal> = sqlx::query_scalar(RUNNER_REGISTRATION_HEAD)
            .bind(enrollment.enrollment().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        let current = current.ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        let current = decode_registration_revision(current)?;
        let registration = load_registration_in(
            transaction.as_mut(),
            enrollment.enrollment(),
            current,
            Some(&enrollment),
            &self.catalog,
        )
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        let authority = match enrollment.state() {
            RunnerEnrollmentState::Pending => RunnerEnrollmentAuthority::ReplacementPending,
            RunnerEnrollmentState::Active | RunnerEnrollmentState::Revoked => {
                RunnerEnrollmentAuthority::Active
            }
        };
        let receipt = RunnerEnrollmentReceipt {
            request,
            authority,
            enrollment,
            registration,
        };
        let advertisement_matches = receipt.advertisement() == advertisement;
        match prior_revision.cmp(&current) {
            Ordering::Less if advertisement_matches => {
                transaction.commit().await?;
                Ok(receipt)
            }
            Ordering::Less => {
                transaction.rollback().await?;
                Err(RunnerEnrollmentRequestFailure::StaleResumeAdvertisement {
                    request,
                    prior: prior_revision,
                    current,
                }
                .into())
            }
            Ordering::Equal if advertisement_matches => {
                transaction.commit().await?;
                Ok(receipt)
            }
            Ordering::Equal => {
                let (_, _, enrollment, _) = receipt.into_parts();
                require_completed_registration_reconciliation(
                    &mut transaction,
                    enrollment.enrollment(),
                    current,
                )
                .await?;
                let pending = enrollment
                    .prepare_registration(advertisement, &self.catalog)
                    .map_err(RunnerProtocolStoreError::Domain)?;
                let revision =
                    current
                        .checked_next()
                        .ok_or(RunnerProtocolStoreError::Corruption(
                            RunnerProtocolCorruption::GenerationExhausted,
                        ))?;
                if pending.registration().revision().get() != revision.get() {
                    transaction.rollback().await?;
                    return Err(RunnerProtocolStoreError::Domain(
                        RunnerDomainError::RegistrationChanged,
                    ));
                }
                insert_registration(&mut transaction, revision, pending.registration()).await?;
                insert_registration_reconciliation(
                    &mut transaction,
                    enrollment.enrollment(),
                    revision,
                )
                .await?;
                sqlx::query(
                    "UPDATE runner_current_registration
                        SET registration_revision = $2
                      WHERE enrollment_id = $1",
                )
                .bind(enrollment.enrollment().into_uuid())
                .bind(Decimal::from(revision.get()))
                .execute(&mut *transaction)
                .await?;
                commit_mutation(transaction).await?;
                let registration = pending.commit().map_err(RunnerProtocolStoreError::Domain)?;
                Ok(RunnerEnrollmentReceipt {
                    request,
                    authority,
                    enrollment,
                    registration: StoredValidatedRunnerRegistration {
                        revision,
                        registration,
                    },
                })
            }
            Ordering::Greater => {
                transaction.rollback().await?;
                Err(RunnerEnrollmentRequestFailure::ResumeRevisionMismatch {
                    request,
                    expected: current,
                    observed: prior_revision,
                }
                .into())
            }
        }
    }

    /// Inserts one pristine active logical enrollment and its exact allowed
    /// classes. An enrollment that already issued a registration through the
    /// domain-only path is rejected: persisting only its enrollment rows
    /// would reload with no issued revision while the caller-held authority
    /// disagrees with canonical storage forever after.
    pub async fn insert_enrollment(
        &self,
        enrollment: &RunnerEnrollment,
    ) -> Result<(), RunnerProtocolStoreError> {
        if enrollment.state() != RunnerEnrollmentState::Active
            || enrollment.last_issued_registration_revision().is_some()
        {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let mut transaction = self.pool.begin().await?;
        insert_enrollment_rows(&mut transaction, enrollment).await?;
        commit_mutation(transaction).await
    }

    /// Loads one enrollment through its canonical class and audit evidence.
    pub async fn load_enrollment(
        &self,
        enrollment: RunnerEnrollmentId,
    ) -> Result<Option<RunnerEnrollment>, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let loaded = load_enrollment_in(transaction.as_mut(), enrollment).await?;
        transaction.commit().await?;
        Ok(loaded)
    }

    /// Applies terminal enrollment revocation under singleton then enrollment locks.
    pub async fn revoke_enrollment(
        &self,
        enrollment: &mut RunnerEnrollment,
    ) -> Result<bool, RunnerProtocolStoreError> {
        let enrollment_id = enrollment.enrollment();
        let mut transaction = self.pool.begin().await?;
        // Pristine admission and pending-successor promotion take this
        // temporary singleton lock before any enrollment row. Revocation must
        // enter through the same order to avoid a table-lock upgrade cycle.
        sqlx::query("LOCK TABLE runner_enrollment IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut *transaction)
            .await?;
        let locked = sqlx::query(RUNNER_ENROLLMENT)
            .bind(enrollment_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        if locked.is_none() {
            transaction.rollback().await?;
            return Ok(false);
        }
        let canonical = load_enrollment_in(transaction.as_mut(), enrollment_id)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        if canonical != *enrollment {
            transaction.rollback().await?;
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::CorrelationMismatch,
            ));
        }
        terminalize_connection_for_revocation(&mut transaction, enrollment_id).await?;
        let current_revision: Decimal = sqlx::query_scalar(
            "SELECT revision
               FROM runner_enrollment
              WHERE enrollment_id = $1",
        )
        .bind(enrollment_id.into_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        let current_revision = decode_u64(current_revision)?;
        let revoked_revision = current_revision
            .checked_add(1)
            .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
        let runner = enrollment.runner();
        let authentication = enrollment.authentication();
        let classes: Vec<_> = enrollment.allowed_classes().cloned().collect();
        let revoked_state = runner_enrollment_state_to_str(RunnerEnrollmentState::Revoked);
        sqlx::query(
            "INSERT INTO runner_enrollment_audit
                (enrollment_id, revision, runner_id,
                 authentication_reference_id, allowed_class_count, state_kind)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(enrollment_id.into_uuid())
        .bind(Decimal::from(revoked_revision))
        .bind(runner.into_uuid())
        .bind(authentication.into_uuid())
        .bind(count_decimal(classes.len())?)
        .bind(revoked_state)
        .execute(&mut *transaction)
        .await?;
        for class in classes {
            sqlx::query(
                "INSERT INTO runner_enrollment_audit_allowed_class
                    (enrollment_id, revision, capability_class)
                 VALUES ($1, $2, $3)",
            )
            .bind(enrollment_id.into_uuid())
            .bind(Decimal::from(revoked_revision))
            .bind(class.as_str())
            .execute(&mut *transaction)
            .await?;
        }
        sqlx::query(
            "UPDATE runner_enrollment
                SET revision = $2, state_kind = $3
              WHERE enrollment_id = $1",
        )
        .bind(enrollment_id.into_uuid())
        .bind(Decimal::from(revoked_revision))
        .bind(revoked_state)
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        enrollment
            .revoke_in_place()
            .map_err(RunnerProtocolStoreError::Domain)?;
        Ok(true)
    }

    /// Validates and appends one complete availability advertisement.
    pub async fn register(
        &self,
        enrollment: &RunnerEnrollment,
        advertisement: signalbox_domain::RunnerAdvertisement,
    ) -> Result<StoredValidatedRunnerRegistration, RunnerProtocolStoreError> {
        self.register_checked(enrollment, None, advertisement).await
    }

    /// Appends one complete advertisement only when the caller names the
    /// enrollment-owned current registration revision.
    pub async fn register_at_revision(
        &self,
        enrollment: &RunnerEnrollment,
        expected: RunnerRegistrationRevision,
        advertisement: signalbox_domain::RunnerAdvertisement,
    ) -> Result<StoredValidatedRunnerRegistration, RunnerProtocolStoreError> {
        self.register_checked(enrollment, Some(expected), advertisement)
            .await
    }

    async fn register_checked(
        &self,
        enrollment: &RunnerEnrollment,
        expected: Option<RunnerRegistrationRevision>,
        advertisement: signalbox_domain::RunnerAdvertisement,
    ) -> Result<StoredValidatedRunnerRegistration, RunnerProtocolStoreError> {
        if advertisement.repositories().count() > RunnerAdvertisement::MAX_REPOSITORIES {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::TooManyAdvertisedRepositories,
            ));
        }
        let mut transaction = self.pool.begin().await?;
        let enrollment_id = enrollment.enrollment();
        let locked = sqlx::query(RUNNER_ENROLLMENT)
            .bind(enrollment_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        if locked.is_none() {
            return Err(RunnerProtocolStoreError::Corruption(
                RunnerProtocolCorruption::MissingCanonicalEnrollment,
            ));
        }
        let canonical = load_enrollment_in(transaction.as_mut(), enrollment_id)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        let previous: Option<Decimal> = sqlx::query_scalar(RUNNER_REGISTRATION_HEAD)
            .bind(enrollment_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        let previous = previous.map(decode_registration_revision).transpose()?;
        if let Some(expected) = expected
            && previous != Some(expected)
        {
            transaction.rollback().await?;
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::RegistrationChanged,
            ));
        }
        if canonical != *enrollment {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::CorrelationMismatch,
            ));
        }
        if let Some(previous) = previous {
            require_completed_registration_reconciliation(
                &mut transaction,
                enrollment_id,
                previous,
            )
            .await?;
        }
        let pending = enrollment
            .prepare_registration(advertisement, &self.catalog)
            .map_err(RunnerProtocolStoreError::Domain)?;
        let revision = match previous {
            Some(value) => value
                .checked_next()
                .ok_or(RunnerProtocolStoreError::Corruption(
                    RunnerProtocolCorruption::GenerationExhausted,
                ))?,
            None => RunnerRegistrationRevision::first(),
        };
        if pending.registration().revision().get() != revision.get() {
            transaction.rollback().await?;
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::RegistrationChanged,
            ));
        }
        insert_registration(&mut transaction, revision, pending.registration()).await?;
        insert_registration_reconciliation(&mut transaction, enrollment_id, revision).await?;
        sqlx::query(
            "INSERT INTO runner_current_registration
                (enrollment_id, registration_revision)
             VALUES ($1, $2)
             ON CONFLICT (enrollment_id)
             DO UPDATE SET registration_revision = EXCLUDED.registration_revision",
        )
        .bind(enrollment_id.into_uuid())
        .bind(Decimal::from(revision.get()))
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        let registration = pending.commit().map_err(RunnerProtocolStoreError::Domain)?;
        Ok(StoredValidatedRunnerRegistration {
            revision,
            registration,
        })
    }

    /// Loads one exact historical validated registration.
    pub async fn load_registration(
        &self,
        enrollment: &RunnerEnrollment,
        revision: RunnerRegistrationRevision,
    ) -> Result<Option<StoredValidatedRunnerRegistration>, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let loaded = load_registration_in(
            transaction.as_mut(),
            enrollment.enrollment(),
            revision,
            Some(enrollment),
            &self.catalog,
        )
        .await?;
        transaction.commit().await?;
        Ok(loaded)
    }

    /// Loads the current validated registration for an enrollment.
    pub async fn load_current_registration(
        &self,
        enrollment: &RunnerEnrollment,
    ) -> Result<Option<StoredValidatedRunnerRegistration>, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let revision: Option<Decimal> = sqlx::query_scalar(
            "SELECT registration_revision
               FROM runner_current_registration
              WHERE enrollment_id = $1",
        )
        .bind(enrollment.enrollment().into_uuid())
        .fetch_optional(transaction.as_mut())
        .await?;
        let loaded = match revision {
            Some(revision) => {
                load_registration_in(
                    transaction.as_mut(),
                    enrollment.enrollment(),
                    decode_registration_revision(revision)?,
                    Some(enrollment),
                    &self.catalog,
                )
                .await?
            }
            None => None,
        };
        transaction.commit().await?;
        Ok(loaded)
    }

    /// Loads one immutable staged workspace-provisioning authorization.
    pub async fn load_workspace_provisioning_authorization(
        &self,
        authorization: WorkspaceProvisioningAuthorizationId,
    ) -> Result<Option<StoredWorkspaceProvisioningAuthorization>, RunnerProtocolStoreError> {
        let row = sqlx::query(
            "SELECT staged.command_id, staged.session_id,
                    staged.lost_placement_event_ordinal,
                    staged.lost_placement_revision,
                    staged.successor_placement_revision,
                    staged.enrollment_id, staged.runner_id,
                    staged.registration_revision,
                    staged.connection_epoch,
                    staged.connection_event_ordinal,
                    staged.repository_key,
                    staged.sandbox_profile,
                    staged.credential_profile_name,
                    command.expected_placement_revision,
                    command.target_kind, command.target_runner_id,
                    command.target_pending_request_id,
                    placement.state_kind AS placement_state_kind,
                    placement.lost_runner_id,
                    placement.loss_source_kind,
                    placement.loss_registration_revision,
                    placement.workspace_requirement_kind,
                    placement.requested_repository_key,
                    placement.requested_sandbox_profile,
                    placement.requested_credential_profile_name,
                    registration.runner_id AS registration_runner_id,
                    connection.state_kind AS connection_state_kind,
                    pending.enrollment_id AS pending_enrollment_id,
                    repository.credential_profile_name AS repository_profile_name,
                    EXISTS (
                        SELECT 1 FROM runner_registration_workspace AS workspace
                         WHERE workspace.enrollment_id = staged.enrollment_id
                           AND workspace.registration_revision =
                                staged.registration_revision
                           AND workspace.workspace_kind = 'worktree_per_session'
                    ) AS advertises_workspace,
                    EXISTS (
                        SELECT 1 FROM runner_registration_sandbox AS sandbox
                         WHERE sandbox.enrollment_id = staged.enrollment_id
                           AND sandbox.registration_revision =
                                staged.registration_revision
                           AND sandbox.sandbox_profile = staged.sandbox_profile
                    ) AS advertises_sandbox,
                    staged.credential_profile_name IS NULL OR EXISTS (
                        SELECT 1 FROM runner_registration_profile AS profile
                         WHERE profile.enrollment_id = staged.enrollment_id
                           AND profile.registration_revision =
                                staged.registration_revision
                           AND profile.credential_profile_name =
                                staged.credential_profile_name
                    ) AS advertises_profile
               FROM runner_workspace_provisioning_authorization AS staged
               JOIN replace_lost_runner_command AS command
                 ON command.command_id = staged.command_id
                AND command.session_id = staged.session_id
               JOIN runner_session_placement_record AS placement
                 ON placement.session_id = staged.session_id
                AND placement.event_ordinal =
                    staged.lost_placement_event_ordinal
                AND placement.placement_revision =
                    staged.lost_placement_revision
               JOIN runner_registration AS registration
                 ON registration.enrollment_id = staged.enrollment_id
                AND registration.registration_revision =
                    staged.registration_revision
                AND registration.runner_id = staged.runner_id
               JOIN runner_connection_event AS connection
                 ON connection.enrollment_id = staged.enrollment_id
                AND connection.connection_epoch = staged.connection_epoch
                AND connection.event_ordinal =
                    staged.connection_event_ordinal
               LEFT JOIN runner_pending_enrollment AS pending
                 ON pending.request_id = command.target_pending_request_id
                AND pending.enrollment_id = staged.enrollment_id
               LEFT JOIN runner_registration_repository AS repository
                 ON repository.enrollment_id = staged.enrollment_id
                AND repository.registration_revision =
                    staged.registration_revision
                AND repository.repository_key = staged.repository_key
              WHERE staged.authorization_id = $1",
        )
        .bind(authorization.into_uuid())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let command = DurableCommandId::from_uuid(row.decode_column("command_id")?);
        let session = session_id(row.decode_column("session_id")?);
        let lost_placement_event_ordinal =
            decode_u64(row.decode_column("lost_placement_event_ordinal")?)?;
        let lost_placement_revision =
            decode_runner_generation(row.decode_column("lost_placement_revision")?)?;
        let successor_placement_revision =
            decode_runner_generation(row.decode_column("successor_placement_revision")?)?;
        let enrollment = RunnerEnrollmentId::from_uuid(row.decode_column("enrollment_id")?);
        let runner = runner_id(row.decode_column("runner_id")?);
        let registration_revision =
            decode_runner_generation(row.decode_column("registration_revision")?)?;
        let connection_epoch = RunnerConnectionEpoch::try_from_u64(decode_u64(
            row.decode_column("connection_epoch")?,
        )?)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        let connection_event_ordinal = decode_u64(row.decode_column("connection_event_ordinal")?)?;
        let repository = WorkspaceRepositoryKey::try_new(row.decode_column("repository_key")?)
            .map_err(|_| RunnerProtocolCorruption::InvalidEncoding)?;
        let sandbox: String = row.decode_column("sandbox_profile")?;
        let sandbox =
            runner_sandbox_from_str(&sandbox).ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        let credential_profile: Option<String> = row.decode_column("credential_profile_name")?;
        let credential_profile = credential_profile
            .map(CredentialProfileName::try_new)
            .transpose()
            .map_err(|_| RunnerProtocolCorruption::InvalidEncoding)?;
        let expected = decode_runner_generation(row.decode_column("expected_placement_revision")?)?;
        let target_kind: String = row.decode_column("target_kind")?;
        let target_runner: Option<Uuid> = row.decode_column("target_runner_id")?;
        let target_pending_request: Option<Uuid> =
            row.decode_column("target_pending_request_id")?;
        let pending_enrollment: Option<Uuid> = row.decode_column("pending_enrollment_id")?;
        let placement_state: String = row.decode_column("placement_state_kind")?;
        let lost_runner: Uuid = row.decode_column("lost_runner_id")?;
        let loss_source: String = row.decode_column("loss_source_kind")?;
        let loss_registration: Option<Decimal> = row.decode_column("loss_registration_revision")?;
        let workspace_requirement: String = row.decode_column("workspace_requirement_kind")?;
        let requested_repository: String = row.decode_column("requested_repository_key")?;
        let requested_sandbox: String = row.decode_column("requested_sandbox_profile")?;
        let requested_profile: Option<String> =
            row.decode_column("requested_credential_profile_name")?;
        let registration_runner: Uuid = row.decode_column("registration_runner_id")?;
        let connection_state: String = row.decode_column("connection_state_kind")?;
        let repository_profile: Option<String> = row.decode_column("repository_profile_name")?;
        let advertises_workspace: bool = row.decode_column("advertises_workspace")?;
        let advertises_sandbox: bool = row.decode_column("advertises_sandbox")?;
        let advertises_profile: bool = row.decode_column("advertises_profile")?;
        let next_revision = lost_placement_revision
            .checked_next()
            .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
        let target_matches = match target_kind.as_str() {
            "runner" => {
                target_runner == Some(runner.into_uuid())
                    && target_pending_request.is_none()
                    && lost_runner != runner.into_uuid()
            }
            "pending_enrollment" => {
                target_runner.is_none()
                    && target_pending_request.is_some()
                    && pending_enrollment == Some(enrollment.into_uuid())
                    && lost_runner != runner.into_uuid()
            }
            "same_runner_reenrollment" => {
                target_runner == Some(runner.into_uuid())
                    && target_pending_request.is_none()
                    && lost_runner == runner.into_uuid()
                    && loss_source == "registration"
                    && loss_registration.is_some()
            }
            _ => false,
        };
        if expected != lost_placement_revision
            || successor_placement_revision != next_revision
            || placement_state != "runner_lost"
            || workspace_requirement != "repository_worktree"
            || requested_repository != repository.as_str()
            || requested_sandbox != runner_sandbox_to_str(sandbox)
            || requested_profile.as_deref()
                != credential_profile
                    .as_ref()
                    .map(CredentialProfileName::as_str)
            || registration_runner != runner.into_uuid()
            || connection_state != "connected"
            || repository_profile.as_deref()
                != credential_profile
                    .as_ref()
                    .map(CredentialProfileName::as_str)
            || !advertises_workspace
            || !advertises_sandbox
            || !advertises_profile
            || !target_matches
        {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        Ok(Some(StoredWorkspaceProvisioningAuthorization {
            command,
            authorization,
            session,
            lost_placement_event_ordinal,
            lost_placement_revision,
            successor_placement_revision,
            enrollment,
            runner,
            registration_revision,
            connection_epoch,
            connection_event_ordinal,
            repository,
            sandbox,
            credential_profile,
        }))
    }

    /// Loads one immutable ready-workspace receipt and rechecks its authority.
    pub async fn load_workspace_provisioning_receipt(
        &self,
        authorization: WorkspaceProvisioningAuthorizationId,
    ) -> Result<Option<StoredWorkspaceProvisioningReceipt>, RunnerProtocolStoreError> {
        let row = sqlx::query(
            "SELECT receipt.session_id, receipt.placement_revision,
                    receipt.runner_id, receipt.manifest_id,
                    receipt.manifest_digest, receipt.repository_key,
                    receipt.canonical_clone_url_digest,
                    receipt.credential_profile_name, receipt.sandbox_profile,
                    receipt.relative_path, receipt.recovery_kind,
                    receipt.branch_name, receipt.revision,
                    staged.session_id AS staged_session_id,
                    staged.lost_placement_event_ordinal,
                    staged.lost_placement_revision,
                    staged.successor_placement_revision,
                    staged.enrollment_id, staged.runner_id AS staged_runner_id,
                    staged.registration_revision,
                    staged.repository_key AS staged_repository_key,
                    staged.sandbox_profile AS staged_sandbox_profile,
                    staged.credential_profile_name AS staged_profile_name,
                    command.expected_placement_revision,
                    command.target_kind, command.target_runner_id,
                    command.target_pending_request_id,
                    placement.event_kind AS placement_event_kind,
                    placement.state_kind AS placement_state_kind,
                    placement.lost_runner_id,
                    placement.loss_source_kind,
                    placement.loss_registration_revision,
                    registration.runner_id AS registration_runner_id,
                    connection.state_kind AS connection_state_kind,
                    pending.enrollment_id AS pending_enrollment_id
               FROM runner_replacement_workspace_receipt AS receipt
               JOIN runner_workspace_provisioning_authorization AS staged
                 ON staged.authorization_id = receipt.authorization_id
                AND staged.session_id = receipt.session_id
               JOIN replace_lost_runner_command AS command
                 ON command.command_id = staged.command_id
                AND command.session_id = staged.session_id
               JOIN runner_session_placement_record AS placement
                 ON placement.session_id = staged.session_id
                AND placement.event_ordinal =
                    staged.lost_placement_event_ordinal
                AND placement.placement_revision =
                    staged.lost_placement_revision
               JOIN runner_registration AS registration
                 ON registration.enrollment_id = staged.enrollment_id
                AND registration.registration_revision =
                    staged.registration_revision
                AND registration.runner_id = staged.runner_id
               JOIN runner_connection_event AS connection
                 ON connection.enrollment_id = staged.enrollment_id
                AND connection.connection_epoch = staged.connection_epoch
                AND connection.event_ordinal =
                    staged.connection_event_ordinal
               LEFT JOIN runner_pending_enrollment AS pending
                 ON pending.request_id = command.target_pending_request_id
                AND pending.enrollment_id = staged.enrollment_id
              WHERE receipt.authorization_id = $1",
        )
        .bind(authorization.into_uuid())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let session = session_id(row.decode_column("session_id")?);
        let placement_revision =
            decode_runner_generation(row.decode_column("placement_revision")?)?;
        let runner = runner_id(row.decode_column("runner_id")?);
        let manifest = WorkspaceManifestId::from_uuid(row.decode_column("manifest_id")?);
        let manifest_digest: String = row.decode_column("manifest_digest")?;
        let repository = WorkspaceRepositoryKey::try_new(row.decode_column("repository_key")?)
            .map_err(|_| RunnerProtocolCorruption::InvalidEncoding)?;
        let canonical_clone_url_digest =
            CanonicalCloneUrlDigest::try_new(row.decode_column("canonical_clone_url_digest")?)
                .map_err(RunnerProtocolStoreError::Domain)?;
        let credential_profile = row
            .decode_column::<Option<String>>("credential_profile_name")?
            .map(CredentialProfileName::try_new)
            .transpose()
            .map_err(RunnerProtocolStoreError::Domain)?;
        let sandbox = runner_sandbox_from_str(&row.decode_column::<String>("sandbox_profile")?)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        let relative_path = WorkspaceRelativePath::try_new(row.decode_column("relative_path")?)
            .map_err(RunnerProtocolStoreError::Domain)?;
        let recovery_kind: String = row.decode_column("recovery_kind")?;
        let branch_name: Option<String> = row.decode_column("branch_name")?;
        let revision = row.decode_column::<Option<String>>("revision")?;
        let recovery = match (recovery_kind.as_str(), branch_name, revision) {
            ("commit", None, Some(revision)) => WorkspaceRecovery::Commit {
                revision: WorkspaceRevision::try_new(revision)
                    .map_err(RunnerProtocolStoreError::Domain)?,
            },
            ("branch", Some(name), Some(revision)) => WorkspaceRecovery::Branch {
                name: WorkspaceBranchName::try_new(name)
                    .map_err(RunnerProtocolStoreError::Domain)?,
                revision: WorkspaceRevision::try_new(revision)
                    .map_err(RunnerProtocolStoreError::Domain)?,
            },
            ("unborn_branch", Some(name), None) => WorkspaceRecovery::UnbornBranch {
                name: WorkspaceBranchName::try_new(name)
                    .map_err(RunnerProtocolStoreError::Domain)?,
            },
            _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
        };
        let staged_session = session_id(row.decode_column("staged_session_id")?);
        let lost_event_ordinal = decode_u64(row.decode_column("lost_placement_event_ordinal")?)?;
        let lost_revision =
            decode_runner_generation(row.decode_column("lost_placement_revision")?)?;
        let successor_revision =
            decode_runner_generation(row.decode_column("successor_placement_revision")?)?;
        let enrollment = RunnerEnrollmentId::from_uuid(row.decode_column("enrollment_id")?);
        let staged_runner = runner_id(row.decode_column("staged_runner_id")?);
        let staged_registration_revision =
            decode_runner_generation(row.decode_column("registration_revision")?)?;
        let expected_revision =
            decode_runner_generation(row.decode_column("expected_placement_revision")?)?;
        let target_kind: String = row.decode_column("target_kind")?;
        let target_runner: Option<Uuid> = row.decode_column("target_runner_id")?;
        let target_pending: Option<Uuid> = row.decode_column("target_pending_request_id")?;
        let pending_enrollment: Option<Uuid> = row.decode_column("pending_enrollment_id")?;
        let lost_runner: Uuid = row.decode_column("lost_runner_id")?;
        let loss_source: String = row.decode_column("loss_source_kind")?;
        let loss_registration: Option<Decimal> = row.decode_column("loss_registration_revision")?;
        let target_matches = match target_kind.as_str() {
            "runner" => {
                target_runner == Some(runner.into_uuid())
                    && target_pending.is_none()
                    && lost_runner != runner.into_uuid()
            }
            "same_runner_reenrollment" => {
                target_runner == Some(runner.into_uuid())
                    && target_pending.is_none()
                    && lost_runner == runner.into_uuid()
                    && loss_source == "registration"
                    && loss_registration.is_some_and(|revision| {
                        revision <= Decimal::from(staged_registration_revision.get())
                    })
            }
            "pending_enrollment" => {
                target_runner.is_none()
                    && target_pending.is_some()
                    && pending_enrollment == Some(enrollment.into_uuid())
                    && lost_runner != runner.into_uuid()
            }
            _ => false,
        };
        let expected_relative_path = format!(
            "sessions/{}/{}/repo",
            session.as_uuid(),
            placement_revision.get()
        );
        let expected_successor_revision = lost_revision
            .checked_next()
            .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
        if !is_lower_hex_digest(&manifest_digest)
            || session != staged_session
            || expected_revision != lost_revision
            || placement_revision != successor_revision
            || successor_revision != expected_successor_revision
            || staged_runner != runner
            || row.decode_column::<String>("staged_repository_key")? != repository.as_str()
            || row.decode_column::<String>("staged_sandbox_profile")?
                != runner_sandbox_to_str(sandbox)
            || row
                .decode_column::<Option<String>>("staged_profile_name")?
                .as_deref()
                != credential_profile
                    .as_ref()
                    .map(CredentialProfileName::as_str)
            || row.decode_column::<String>("placement_event_kind")? != "runner_lost"
            || row.decode_column::<String>("placement_state_kind")? != "runner_lost"
            || row.decode_column::<Uuid>("registration_runner_id")? != runner.into_uuid()
            || row.decode_column::<String>("connection_state_kind")? != "connected"
            || relative_path.as_str() != expected_relative_path
            || lost_event_ordinal == 0
            || !target_matches
        {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        Ok(Some(StoredWorkspaceProvisioningReceipt {
            authorization,
            session,
            placement_revision,
            runner,
            manifest,
            manifest_digest,
            repository,
            canonical_clone_url_digest,
            credential_profile,
            sandbox,
            relative_path,
            recovery,
        }))
    }

    /// Atomically admits or exactly replays one replacement workspace receipt.
    pub async fn record_workspace_ready_receipt(
        &self,
        receipt: RunnerWorkspaceReadyReceipt,
    ) -> Result<RunnerWorkspaceReadyReceipt, RunnerProtocolStoreError> {
        if let Some(stored) = self
            .load_workspace_provisioning_receipt(receipt.authorization())
            .await?
        {
            return exact_workspace_ready_replay(receipt, stored);
        }
        let authority = self
            .load_workspace_provisioning_authorization(receipt.authorization())
            .await?
            .ok_or(RunnerProtocolStoreError::Domain(
                RunnerDomainError::CorrelationMismatch,
            ))?;
        validate_workspace_ready_receipt(&receipt, &authority)?;

        let mut transaction = self.pool.begin().await?;
        let scheduler = sqlx::query(REPLACE_LOST_RUNNER_SCHEDULER)
            .bind(receipt.session().into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let session_exists: bool = scheduler.decode_column("session_exists")?;
        let scheduler_session: Option<Uuid> = scheduler.decode_column("scheduler_session_id")?;
        if !session_exists || scheduler_session != Some(receipt.session().into_uuid()) {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }

        if let Some(stored) = self
            .load_workspace_provisioning_receipt(receipt.authorization())
            .await?
        {
            transaction.rollback().await?;
            return exact_workspace_ready_replay(receipt, stored);
        }

        let enrollment_locked = sqlx::query(RUNNER_ENROLLMENT)
            .bind(authority.enrollment().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        if enrollment_locked.is_none() {
            return Err(RunnerProtocolCorruption::MissingCanonicalEnrollment.into());
        }
        let enrollment = load_enrollment_in(transaction.as_mut(), authority.enrollment())
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        let target_kind: String = sqlx::query_scalar(
            "SELECT target_kind
               FROM replace_lost_runner_command
              WHERE command_id = $1",
        )
        .bind(authority.command().into_uuid())
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
        let enrollment_is_current = match target_kind.as_str() {
            "runner" | "same_runner_reenrollment" => {
                enrollment.state() == RunnerEnrollmentState::Active
            }
            "pending_enrollment" => enrollment.state() == RunnerEnrollmentState::Pending,
            _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
        };
        if !enrollment_is_current || enrollment.runner() != receipt.runner() {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::CorrelationMismatch,
            ));
        }

        let connection = sqlx::query(PROMOTE_PENDING_RUNNER_CONNECTION)
            .bind(authority.enrollment().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ))?;
        let connection_epoch = RunnerConnectionEpoch::try_from_u64(decode_u64(
            connection.decode_column("connection_epoch")?,
        )?)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        let connection_event_ordinal =
            decode_u64(connection.decode_column("connection_event_ordinal")?)?;
        let connection_state: String = connection.decode_column("state_kind")?;
        if connection_epoch != authority.connection_epoch()
            || connection_event_ordinal != authority.connection_event_ordinal()
            || connection_state != "connected"
        {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        sqlx::query_scalar::<_, Decimal>(RUNNER_PLACEMENT_CURRENT_LOSS)
            .bind(authority.enrollment().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;

        let current_registration: Decimal = sqlx::query_scalar(RUNNER_REGISTRATION_HEAD)
            .bind(authority.enrollment().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        if decode_registration_revision(current_registration)?.get()
            != authority.registration_revision().get()
        {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::RegistrationChanged,
            ));
        }

        let placement = sqlx::query(RUNNER_PLACEMENT_HEAD)
            .bind(receipt.session().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
        let placement_event_ordinal = decode_u64(placement.decode_column("event_ordinal")?)?;
        let placement_revision =
            decode_runner_generation(placement.decode_column("placement_revision")?)?;
        let placement_state: String = placement.decode_column("state_kind")?;
        if placement_event_ordinal != authority.lost_placement_event_ordinal()
            || placement_revision != authority.lost_placement_revision()
            || placement_state != "runner_lost"
        {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let terminal_result_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1
                   FROM replace_lost_runner_result
                  WHERE command_id = $1
             )",
        )
        .bind(authority.command().into_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        if terminal_result_exists {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }

        insert_workspace_ready_receipt(transaction.as_mut(), &receipt).await?;
        commit_mutation(transaction).await?;
        Ok(receipt)
    }

    /// Stores one already-authenticated receipt projection for integration tests.
    #[cfg(feature = "postgres-integration")]
    #[doc(hidden)]
    pub async fn store_workspace_provisioning_receipt_projection_for_test(
        &self,
        authorization: WorkspaceProvisioningAuthorizationId,
        workspace: &ProvisionedWorkspace,
        manifest_digest: &str,
    ) -> Result<(), RunnerProtocolStoreError> {
        let repository = workspace
            .repository
            .as_ref()
            .ok_or(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ))?;
        let clone_url_digest = workspace.canonical_clone_url_digest.as_ref().ok_or(
            RunnerProtocolStoreError::Domain(RunnerDomainError::InvalidState),
        )?;
        let recovery = workspace
            .recovery
            .as_ref()
            .ok_or(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ))?;
        let (recovery_kind, branch_name, revision) = match recovery {
            WorkspaceRecovery::Commit { revision } => ("commit", None, Some(revision.as_str())),
            WorkspaceRecovery::Branch { name, revision } => {
                ("branch", Some(name.as_str()), Some(revision.as_str()))
            }
            WorkspaceRecovery::UnbornBranch { name } => {
                ("unborn_branch", Some(name.as_str()), None)
            }
        };
        sqlx::query(
            "INSERT INTO runner_replacement_workspace_receipt
                (authorization_id, session_id, placement_revision, runner_id,
                 manifest_id, manifest_digest, repository_key,
                 canonical_clone_url_digest, credential_profile_name,
                 sandbox_profile, relative_path, recovery_kind, branch_name,
                 revision)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11,
                     $12, $13, $14)",
        )
        .bind(authorization.into_uuid())
        .bind(workspace.session.into_uuid())
        .bind(Decimal::from(workspace.placement_revision.get()))
        .bind(workspace.runner.into_uuid())
        .bind(workspace.manifest_id.into_uuid())
        .bind(manifest_digest)
        .bind(repository.as_str())
        .bind(clone_url_digest.as_str())
        .bind(
            workspace
                .credential_profile
                .as_ref()
                .map(CredentialProfileName::as_str),
        )
        .bind(runner_sandbox_to_str(workspace.sandbox))
        .bind(workspace.relative_path.as_str())
        .bind(recovery_kind)
        .bind(branch_name)
        .bind(revision)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Loads one exact pending managed-workspace release correlation.
    pub async fn load_workspace_release(
        &self,
        session: SessionId,
        placement_revision: RunnerGeneration,
    ) -> Result<Option<StoredRunnerWorkspaceRelease>, RunnerProtocolStoreError> {
        let row = sqlx::query(
            "SELECT release.runner_id, release.manifest_id,
                    release.retired_placement_event_ordinal,
                    release.successor_placement_event_ordinal,
                    release.enrollment_id, release.connection_epoch,
                    release.connection_event_ordinal, release.state_kind,
                    acknowledgement.runner_id AS acknowledgement_runner_id,
                    acknowledgement.manifest_id AS acknowledgement_manifest_id,
                    retirement.runner_id AS retirement_runner_id,
                    retirement.manifest_id AS retirement_manifest_id,
                    retirement.enrollment_id AS retirement_enrollment_id,
                    retirement.connection_epoch AS retirement_connection_epoch,
                    retirement.loss_epoch AS retirement_loss_epoch,
                    retirement.connection_event_ordinal AS
                        retirement_connection_event_ordinal,
                    retirement_loss.connection_epoch AS
                        retirement_loss_connection_epoch,
                    retirement_loss.connection_event_ordinal AS
                        retirement_loss_connection_event_ordinal,
                    failure.runner_id AS failure_runner_id,
                    failure.release_manifest_id AS failure_manifest_id,
                    failure.category_kind AS failure_category_kind,
                    retired.event_kind AS retired_event_kind,
                    retired.state_kind AS retired_state_kind,
                    retired.loss_source_kind AS retired_loss_source_kind,
                    retired.lost_runner_id AS retired_lost_runner_id,
                    retired.pinned_runner_id AS retired_pinned_runner_id,
                    retired.registration_enrollment_id AS retired_enrollment_id,
                    retired.workspace_manifest_id AS retired_manifest_id,
                    retired.workspace_placement_revision AS retired_workspace_revision,
                    successor.event_kind AS successor_event_kind,
                    successor.state_kind AS successor_state_kind,
                    successor.placement_revision AS successor_revision,
                    successor.pinned_runner_id AS successor_runner_id,
                    successor.registration_enrollment_id AS successor_enrollment_id,
                    successor.workspace_manifest_id AS successor_manifest_id,
                    successor.workspace_placement_revision AS successor_workspace_revision,
                    enrollment.runner_id AS enrollment_runner_id,
                    connection.state_kind AS connection_state_kind
               FROM runner_workspace_release AS release
               LEFT JOIN runner_workspace_release_acknowledgement AS acknowledgement
                 ON acknowledgement.session_id = release.session_id
                AND acknowledgement.placement_revision = release.placement_revision
               LEFT JOIN runner_workspace_release_loss_retirement AS retirement
                 ON retirement.session_id = release.session_id
                AND retirement.placement_revision = release.placement_revision
               LEFT JOIN runner_connection_loss_epoch AS retirement_loss
                 ON retirement_loss.enrollment_id = retirement.enrollment_id
                AND retirement_loss.loss_epoch = retirement.loss_epoch
               LEFT JOIN runner_operation_failure AS failure
                 ON failure.operation_kind = 'workspace_release'
                AND failure.release_session_id = release.session_id
                AND failure.release_placement_revision =
                    release.placement_revision
               JOIN runner_session_placement_record AS retired
                 ON retired.session_id = release.session_id
                AND retired.event_ordinal =
                    release.retired_placement_event_ordinal
                AND retired.placement_revision = release.placement_revision
               JOIN runner_session_placement_record AS successor
                 ON successor.session_id = release.session_id
                AND successor.event_ordinal =
                    release.successor_placement_event_ordinal
               JOIN runner_enrollment AS enrollment
                 ON enrollment.enrollment_id = release.enrollment_id
               JOIN runner_connection_event AS connection
                 ON connection.enrollment_id = release.enrollment_id
                AND connection.connection_epoch = release.connection_epoch
                AND connection.event_ordinal =
                    release.connection_event_ordinal
              WHERE release.session_id = $1
                AND release.placement_revision = $2",
        )
        .bind(session.into_uuid())
        .bind(Decimal::from(placement_revision.get()))
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let runner = runner_id(row.decode_column("runner_id")?);
        let manifest = WorkspaceManifestId::from_uuid(row.decode_column("manifest_id")?);
        let acknowledgement_runner: Option<Uuid> =
            row.decode_column("acknowledgement_runner_id")?;
        let acknowledgement_manifest: Option<Uuid> =
            row.decode_column("acknowledgement_manifest_id")?;
        let retirement_runner: Option<Uuid> = row.decode_column("retirement_runner_id")?;
        let retirement_manifest: Option<Uuid> = row.decode_column("retirement_manifest_id")?;
        let retirement_enrollment: Option<Uuid> = row.decode_column("retirement_enrollment_id")?;
        let retirement_connection_epoch: Option<Decimal> =
            row.decode_column("retirement_connection_epoch")?;
        let retirement_loss_epoch: Option<Decimal> = row.decode_column("retirement_loss_epoch")?;
        let retirement_connection_event_ordinal: Option<Decimal> =
            row.decode_column("retirement_connection_event_ordinal")?;
        let retirement_loss_connection_epoch: Option<Decimal> =
            row.decode_column("retirement_loss_connection_epoch")?;
        let retirement_loss_connection_event_ordinal: Option<Decimal> =
            row.decode_column("retirement_loss_connection_event_ordinal")?;
        let has_acknowledgement =
            acknowledgement_runner.is_some() || acknowledgement_manifest.is_some();
        let has_retirement = retirement_runner.is_some()
            || retirement_manifest.is_some()
            || retirement_enrollment.is_some()
            || retirement_connection_epoch.is_some()
            || retirement_loss_epoch.is_some()
            || retirement_connection_event_ordinal.is_some()
            || retirement_loss_connection_epoch.is_some()
            || retirement_loss_connection_event_ordinal.is_some();
        let failure_runner: Option<Uuid> = row.decode_column("failure_runner_id")?;
        let failure_manifest: Option<Uuid> = row.decode_column("failure_manifest_id")?;
        let failure_category: Option<String> = row.decode_column("failure_category_kind")?;
        let has_failure =
            failure_runner.is_some() || failure_manifest.is_some() || failure_category.is_some();
        if usize::from(has_acknowledgement) + usize::from(has_retirement) + usize::from(has_failure)
            > 1
        {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        if has_acknowledgement {
            if acknowledgement_runner != Some(runner.into_uuid())
                || acknowledgement_manifest != Some(manifest.into_uuid())
            {
                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
            }
            return Ok(None);
        }
        if has_failure {
            if failure_runner != Some(runner.into_uuid())
                || failure_manifest != Some(manifest.into_uuid())
                || failure_category.as_deref() != Some("workspace_cleanup_failed")
            {
                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
            }
            return Ok(None);
        }
        if has_retirement {
            let retirement_is_exact = retirement_runner == Some(runner.into_uuid())
                && retirement_manifest == Some(manifest.into_uuid())
                && retirement_enrollment == Some(row.decode_column::<Uuid>("enrollment_id")?)
                && retirement_connection_epoch
                    == Some(row.decode_column::<Decimal>("connection_epoch")?)
                && retirement_loss_epoch.is_some()
                && retirement_connection_event_ordinal.is_some()
                && retirement_loss_connection_epoch == retirement_connection_epoch
                && retirement_loss_connection_event_ordinal == retirement_connection_event_ordinal;
            if !retirement_is_exact {
                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
            }
            return Ok(None);
        }
        let retired_placement_event_ordinal =
            decode_u64(row.decode_column("retired_placement_event_ordinal")?)?;
        let successor_placement_event_ordinal =
            decode_u64(row.decode_column("successor_placement_event_ordinal")?)?;
        let enrollment = RunnerEnrollmentId::from_uuid(row.decode_column("enrollment_id")?);
        let connection_epoch = RunnerConnectionEpoch::try_from_u64(decode_u64(
            row.decode_column("connection_epoch")?,
        )?)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        let connection_event_ordinal = decode_u64(row.decode_column("connection_event_ordinal")?)?;
        let successor_revision =
            decode_runner_generation(row.decode_column("successor_revision")?)?;
        let expected_successor_revision = placement_revision
            .checked_next()
            .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
        if row.decode_column::<String>("state_kind")? != "pending"
            || row.decode_column::<String>("retired_event_kind")? != "runner_lost"
            || row.decode_column::<String>("retired_state_kind")? != "runner_lost"
            || row.decode_column::<String>("retired_loss_source_kind")? != "registration"
            || row.decode_column::<Uuid>("retired_lost_runner_id")? != runner.into_uuid()
            || row.decode_column::<Uuid>("retired_pinned_runner_id")? != runner.into_uuid()
            || row.decode_column::<Uuid>("retired_enrollment_id")? != enrollment.into_uuid()
            || row.decode_column::<Uuid>("retired_manifest_id")? != manifest.into_uuid()
            || decode_runner_generation(row.decode_column("retired_workspace_revision")?)?
                != placement_revision
            || row.decode_column::<String>("successor_event_kind")? != "runner_replaced"
            || row.decode_column::<String>("successor_state_kind")? != "pinned"
            || successor_revision != expected_successor_revision
            || row.decode_column::<Uuid>("successor_runner_id")? != runner.into_uuid()
            || row.decode_column::<Uuid>("successor_enrollment_id")? != enrollment.into_uuid()
            || row.decode_column::<Uuid>("successor_manifest_id")? == manifest.into_uuid()
            || decode_runner_generation(row.decode_column("successor_workspace_revision")?)?
                != successor_revision
            || row.decode_column::<Uuid>("enrollment_runner_id")? != runner.into_uuid()
            || row.decode_column::<String>("connection_state_kind")? != "connected"
            || successor_placement_event_ordinal
                != retired_placement_event_ordinal
                    .checked_add(1)
                    .ok_or(RunnerProtocolCorruption::GenerationExhausted)?
        {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        Ok(Some(StoredRunnerWorkspaceRelease {
            session,
            placement_revision,
            runner,
            manifest,
            retired_placement_event_ordinal,
            successor_placement_event_ordinal,
            enrollment,
            connection_epoch,
            connection_event_ordinal,
        }))
    }

    /// Loads one immutable workspace-cleanup refusal and its exact detail.
    pub async fn load_workspace_cleanup_failure(
        &self,
        session: SessionId,
        placement_revision: RunnerGeneration,
    ) -> Result<Option<RunnerWorkspaceCleanupFailure>, RunnerProtocolStoreError> {
        let row = sqlx::query(
            "SELECT failure.runner_id,
                    failure.release_manifest_id AS failure_manifest_id,
                    failure.operation_kind, failure.category_kind,
                    failure.detail_code, failure.detail_message,
                    failure.detail_payload_json,
                    release.session_id AS release_session_id,
                    release.runner_id AS release_runner_id,
                    release.manifest_id AS release_manifest_id,
                    acknowledgement.session_id AS acknowledgement_session_id,
                    retirement.session_id AS retirement_session_id
               FROM runner_operation_failure AS failure
               LEFT JOIN runner_workspace_release AS release
                 ON release.session_id = failure.release_session_id
                AND release.placement_revision =
                    failure.release_placement_revision
               LEFT JOIN runner_workspace_release_acknowledgement AS acknowledgement
                 ON acknowledgement.session_id = failure.release_session_id
                AND acknowledgement.placement_revision =
                    failure.release_placement_revision
               LEFT JOIN runner_workspace_release_loss_retirement AS retirement
                 ON retirement.session_id = failure.release_session_id
                AND retirement.placement_revision =
                    failure.release_placement_revision
              WHERE failure.operation_kind = $3
                AND failure.release_session_id = $1
                AND failure.release_placement_revision = $2",
        )
        .bind(session.into_uuid())
        .bind(Decimal::from(placement_revision.get()))
        .bind(runner_operation_failure_operation_to_str(
            RunnerOperationFailureOperationStorageKind::WorkspaceRelease,
        ))
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let runner = runner_id(row.decode_column("runner_id")?);
        let manifest = WorkspaceManifestId::from_uuid(row.decode_column("failure_manifest_id")?);
        if runner_operation_failure_operation_from_str(
            &row.decode_column::<String>("operation_kind")?,
        ) != Some(RunnerOperationFailureOperationStorageKind::WorkspaceRelease)
            || runner_operation_failure_category_from_str(
                &row.decode_column::<String>("category_kind")?,
            ) != Some(RunnerOperationFailureCategoryStorageKind::WorkspaceCleanupFailed)
            || row.decode_column::<Option<Uuid>>("release_session_id")? != Some(session.into_uuid())
            || row.decode_column::<Option<Uuid>>("release_runner_id")? != Some(runner.into_uuid())
            || row.decode_column::<Option<Uuid>>("release_manifest_id")?
                != Some(manifest.into_uuid())
            || row
                .decode_column::<Option<Uuid>>("acknowledgement_session_id")?
                .is_some()
            || row
                .decode_column::<Option<Uuid>>("retirement_session_id")?
                .is_some()
        {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        let detail = RunnerOperationFailureDetail::try_new(RunnerOperationFailureDetailInput {
            code: row.decode_column("detail_code")?,
            message: row.decode_column("detail_message")?,
            payload_json: row.decode_column("detail_payload_json")?,
        })
        .map_err(|_| RunnerProtocolCorruption::InvalidEncoding)?;
        Ok(Some(RunnerWorkspaceCleanupFailure::new(
            session,
            placement_revision,
            runner,
            manifest,
            detail,
        )))
    }

    /// Loads one immutable completed-release acknowledgement.
    pub async fn load_workspace_release_acknowledgement(
        &self,
        session: SessionId,
        placement_revision: RunnerGeneration,
    ) -> Result<Option<RunnerWorkspaceReleaseAcknowledgement>, RunnerProtocolStoreError> {
        let row = sqlx::query(
            "SELECT acknowledgement.runner_id, acknowledgement.manifest_id,
                    release.session_id AS release_session_id,
                    release.state_kind,
                    retirement.session_id AS retirement_session_id,
                    failure.release_session_id AS failure_session_id
               FROM runner_workspace_release_acknowledgement AS acknowledgement
               LEFT JOIN runner_workspace_release AS release
                 ON release.session_id = acknowledgement.session_id
                AND release.placement_revision = acknowledgement.placement_revision
                AND release.runner_id = acknowledgement.runner_id
                AND release.manifest_id = acknowledgement.manifest_id
               LEFT JOIN runner_workspace_release_loss_retirement AS retirement
                 ON retirement.session_id = acknowledgement.session_id
                AND retirement.placement_revision =
                    acknowledgement.placement_revision
               LEFT JOIN runner_operation_failure AS failure
                 ON failure.operation_kind = 'workspace_release'
                AND failure.release_session_id = acknowledgement.session_id
                AND failure.release_placement_revision =
                    acknowledgement.placement_revision
              WHERE acknowledgement.session_id = $1
                AND acknowledgement.placement_revision = $2",
        )
        .bind(session.into_uuid())
        .bind(Decimal::from(placement_revision.get()))
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.decode_column::<Option<Uuid>>("release_session_id")? != Some(session.into_uuid())
            || row
                .decode_column::<Option<String>>("state_kind")?
                .as_deref()
                != Some("pending")
            || row
                .decode_column::<Option<Uuid>>("retirement_session_id")?
                .is_some()
            || row
                .decode_column::<Option<Uuid>>("failure_session_id")?
                .is_some()
        {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        Ok(Some(RunnerWorkspaceReleaseAcknowledgement::new(
            session,
            placement_revision,
            runner_id(row.decode_column("runner_id")?),
            WorkspaceManifestId::from_uuid(row.decode_column("manifest_id")?),
        )))
    }

    /// Loads one immutable connection-loss retirement of a pending release.
    pub async fn load_workspace_release_loss_retirement(
        &self,
        session: SessionId,
        placement_revision: RunnerGeneration,
    ) -> Result<Option<StoredRunnerWorkspaceReleaseLossRetirement>, RunnerProtocolStoreError> {
        let row = sqlx::query(
            "SELECT retirement.runner_id, retirement.manifest_id,
                    retirement.enrollment_id, retirement.connection_epoch,
                    retirement.loss_epoch, retirement.connection_event_ordinal,
                    release.session_id AS release_session_id,
                    release.runner_id AS release_runner_id,
                    release.manifest_id AS release_manifest_id,
                    release.enrollment_id AS release_enrollment_id,
                    release.connection_epoch AS release_connection_epoch,
                    release.state_kind,
                    acknowledgement.session_id AS acknowledgement_session_id,
                    failure.release_session_id AS failure_session_id,
                    loss.connection_epoch AS loss_connection_epoch,
                    loss.connection_event_ordinal AS loss_connection_event_ordinal
               FROM runner_workspace_release_loss_retirement AS retirement
               LEFT JOIN runner_workspace_release AS release
                 ON release.session_id = retirement.session_id
                AND release.placement_revision = retirement.placement_revision
               LEFT JOIN runner_connection_loss_epoch AS loss
                 ON loss.enrollment_id = retirement.enrollment_id
                AND loss.loss_epoch = retirement.loss_epoch
               LEFT JOIN runner_workspace_release_acknowledgement AS acknowledgement
                 ON acknowledgement.session_id = retirement.session_id
                AND acknowledgement.placement_revision =
                    retirement.placement_revision
               LEFT JOIN runner_operation_failure AS failure
                 ON failure.operation_kind = 'workspace_release'
                AND failure.release_session_id = retirement.session_id
                AND failure.release_placement_revision =
                    retirement.placement_revision
              WHERE retirement.session_id = $1
                AND retirement.placement_revision = $2",
        )
        .bind(session.into_uuid())
        .bind(Decimal::from(placement_revision.get()))
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let runner = runner_id(row.decode_column("runner_id")?);
        let manifest = WorkspaceManifestId::from_uuid(row.decode_column("manifest_id")?);
        let enrollment = RunnerEnrollmentId::from_uuid(row.decode_column("enrollment_id")?);
        let connection_epoch = RunnerConnectionEpoch::try_from_u64(decode_u64(
            row.decode_column("connection_epoch")?,
        )?)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        let loss_epoch =
            RunnerConnectionLossEpoch::try_from_u64(decode_u64(row.decode_column("loss_epoch")?)?)
                .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        let connection_event_ordinal =
            NonZeroU64::new(decode_u64(row.decode_column("connection_event_ordinal")?)?)
                .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        if row.decode_column::<Option<Uuid>>("release_session_id")? != Some(session.into_uuid())
            || row.decode_column::<Option<Uuid>>("release_runner_id")? != Some(runner.into_uuid())
            || row.decode_column::<Option<Uuid>>("release_manifest_id")?
                != Some(manifest.into_uuid())
            || row.decode_column::<Option<Uuid>>("release_enrollment_id")?
                != Some(enrollment.into_uuid())
            || row.decode_column::<Option<Decimal>>("release_connection_epoch")?
                != Some(Decimal::from(connection_epoch.get()))
            || row
                .decode_column::<Option<String>>("state_kind")?
                .as_deref()
                != Some("pending")
            || row
                .decode_column::<Option<Uuid>>("acknowledgement_session_id")?
                .is_some()
            || row
                .decode_column::<Option<Uuid>>("failure_session_id")?
                .is_some()
            || row.decode_column::<Option<Decimal>>("loss_connection_epoch")?
                != Some(Decimal::from(connection_epoch.get()))
            || row.decode_column::<Option<Decimal>>("loss_connection_event_ordinal")?
                != Some(Decimal::from(connection_event_ordinal.get()))
        {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        Ok(Some(StoredRunnerWorkspaceReleaseLossRetirement {
            session,
            placement_revision,
            runner,
            manifest,
            loss: RunnerConnectionLossSnapshot {
                enrollment,
                loss_epoch,
                connection_epoch,
                connection_event_ordinal,
            },
        }))
    }

    /// Atomically admits or exactly replays one refused workspace cleanup.
    pub async fn record_workspace_cleanup_failure(
        &self,
        failure: RunnerWorkspaceCleanupFailure,
    ) -> Result<RunnerWorkspaceCleanupFailure, RunnerProtocolStoreError> {
        if let Some(recorded) = self
            .load_workspace_cleanup_failure(failure.session(), failure.placement_revision())
            .await?
        {
            return exact_workspace_cleanup_failure_replay(failure, recorded);
        }
        let source = sqlx::query(
            "SELECT runner_id, manifest_id, enrollment_id, connection_epoch
               FROM runner_workspace_release
              WHERE session_id = $1 AND placement_revision = $2",
        )
        .bind(failure.session().into_uuid())
        .bind(Decimal::from(failure.placement_revision().get()))
        .fetch_optional(&self.pool)
        .await?
        .ok_or(RunnerProtocolStoreError::Domain(
            RunnerDomainError::CorrelationMismatch,
        ))?;
        let source_enrollment =
            RunnerEnrollmentId::from_uuid(source.decode_column("enrollment_id")?);
        if runner_id(source.decode_column("runner_id")?) != failure.runner()
            || WorkspaceManifestId::from_uuid(source.decode_column("manifest_id")?)
                != failure.manifest_id()
        {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::CorrelationMismatch,
            ));
        }

        let mut transaction = self.pool.begin().await?;
        lock_runner_session_scheduler(&mut transaction, failure.session()).await?;
        sqlx::query(RUNNER_ENROLLMENT)
            .bind(source_enrollment.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
        let connection = sqlx::query(
            "SELECT head.connection_epoch, head.connection_event_ordinal,
                    event.state_kind
               FROM runner_connection_authority_head AS head
               JOIN runner_connection_event AS event
                 ON event.enrollment_id = head.enrollment_id
                AND event.connection_epoch = head.connection_epoch
                AND event.event_ordinal = head.connection_event_ordinal
              WHERE head.enrollment_id = $1
              FOR UPDATE OF head",
        )
        .bind(source_enrollment.into_uuid())
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
        sqlx::query(RUNNER_CONNECTION_LOSS_HEAD)
            .bind(source_enrollment.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        sqlx::query(RUNNER_REGISTRATION_HEAD)
            .bind(source_enrollment.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
        sqlx::query(RUNNER_PLACEMENT_HEAD)
            .bind(failure.session().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
        let release = sqlx::query(
            "SELECT runner_id, manifest_id, enrollment_id, connection_epoch
               FROM runner_workspace_release
              WHERE session_id = $1 AND placement_revision = $2
              FOR UPDATE",
        )
        .bind(failure.session().into_uuid())
        .bind(Decimal::from(failure.placement_revision().get()))
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(RunnerProtocolStoreError::Domain(
            RunnerDomainError::CorrelationMismatch,
        ))?;
        let release_enrollment =
            RunnerEnrollmentId::from_uuid(release.decode_column("enrollment_id")?);
        let release_connection_epoch: Decimal = release.decode_column("connection_epoch")?;
        if runner_id(release.decode_column("runner_id")?) != failure.runner()
            || WorkspaceManifestId::from_uuid(release.decode_column("manifest_id")?)
                != failure.manifest_id()
            || release_enrollment != source_enrollment
            || connection.decode_column::<String>("state_kind")? != "connected"
        {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::CorrelationMismatch,
            ));
        }
        if let Some(recorded) = load_workspace_cleanup_failure_in_transaction(
            &mut transaction,
            failure.session(),
            failure.placement_revision(),
        )
        .await?
        {
            transaction.rollback().await?;
            return exact_workspace_cleanup_failure_replay(failure, recorded);
        }
        let terminal_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1
                   FROM runner_workspace_release_acknowledgement
                  WHERE session_id = $1 AND placement_revision = $2
             ) OR EXISTS (
                 SELECT 1
                   FROM runner_workspace_release_loss_retirement
                  WHERE session_id = $1 AND placement_revision = $2
             ) OR EXISTS (
                 SELECT 1
                   FROM runner_connection_loss_epoch
                  WHERE enrollment_id = $3 AND connection_epoch = $4
             )",
        )
        .bind(failure.session().into_uuid())
        .bind(Decimal::from(failure.placement_revision().get()))
        .bind(source_enrollment.into_uuid())
        .bind(release_connection_epoch)
        .fetch_one(&mut *transaction)
        .await?;
        if terminal_exists {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        sqlx::query(
            "INSERT INTO runner_operation_failure
                (operation_kind, runner_id, release_session_id,
                 release_placement_revision, release_manifest_id,
                 category_kind, detail_code, detail_message,
                 detail_payload_json)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(runner_operation_failure_operation_to_str(
            RunnerOperationFailureOperationStorageKind::WorkspaceRelease,
        ))
        .bind(failure.runner().into_uuid())
        .bind(failure.session().into_uuid())
        .bind(Decimal::from(failure.placement_revision().get()))
        .bind(failure.manifest_id().into_uuid())
        .bind(runner_operation_failure_category_to_str(
            RunnerOperationFailureCategoryStorageKind::WorkspaceCleanupFailed,
        ))
        .bind(failure.detail().code())
        .bind(failure.detail().message())
        .bind(failure.detail().payload_json())
        .execute(&mut *transaction)
        .await?;
        match commit_mutation(transaction).await {
            Ok(()) => Ok(failure),
            Err(error @ RunnerProtocolStoreError::CommitAmbiguous(_)) => {
                match self
                    .load_workspace_cleanup_failure(failure.session(), failure.placement_revision())
                    .await?
                {
                    Some(recorded) => exact_workspace_cleanup_failure_replay(failure, recorded),
                    None => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// Atomically admits or exactly replays one completed workspace release.
    pub async fn record_workspace_release_acknowledgement(
        &self,
        acknowledgement: RunnerWorkspaceReleaseAcknowledgement,
    ) -> Result<RunnerWorkspaceReleaseAcknowledgement, RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        lock_runner_session_scheduler(&mut transaction, acknowledgement.session()).await?;
        sqlx::query(RUNNER_PLACEMENT_HEAD)
            .bind(acknowledgement.session().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
        let release = sqlx::query(
            "SELECT runner_id, manifest_id, enrollment_id, connection_epoch
               FROM runner_workspace_release
              WHERE session_id = $1 AND placement_revision = $2
              FOR UPDATE",
        )
        .bind(acknowledgement.session().into_uuid())
        .bind(Decimal::from(acknowledgement.placement_revision().get()))
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(RunnerProtocolStoreError::Domain(
            RunnerDomainError::CorrelationMismatch,
        ))?;
        let stored_runner = runner_id(release.decode_column("runner_id")?);
        let stored_manifest = WorkspaceManifestId::from_uuid(release.decode_column("manifest_id")?);
        if stored_runner != acknowledgement.runner()
            || stored_manifest != acknowledgement.manifest_id()
        {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::CorrelationMismatch,
            ));
        }
        let recorded = sqlx::query(
            "SELECT runner_id, manifest_id
               FROM runner_workspace_release_acknowledgement
              WHERE session_id = $1 AND placement_revision = $2",
        )
        .bind(acknowledgement.session().into_uuid())
        .bind(Decimal::from(acknowledgement.placement_revision().get()))
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(recorded) = recorded {
            let replay = RunnerWorkspaceReleaseAcknowledgement::new(
                acknowledgement.session(),
                acknowledgement.placement_revision(),
                runner_id(recorded.decode_column("runner_id")?),
                WorkspaceManifestId::from_uuid(recorded.decode_column("manifest_id")?),
            );
            transaction.rollback().await?;
            return exact_workspace_release_acknowledgement_replay(acknowledgement, replay);
        }
        let source_was_lost: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1
                   FROM runner_connection_loss_epoch
                  WHERE enrollment_id = $1 AND connection_epoch = $2
             )",
        )
        .bind(release.decode_column::<Uuid>("enrollment_id")?)
        .bind(release.decode_column::<Decimal>("connection_epoch")?)
        .fetch_one(&mut *transaction)
        .await?;
        if source_was_lost {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        sqlx::query(
            "INSERT INTO runner_workspace_release_acknowledgement
                (session_id, placement_revision, runner_id, manifest_id)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(acknowledgement.session().into_uuid())
        .bind(Decimal::from(acknowledgement.placement_revision().get()))
        .bind(acknowledgement.runner().into_uuid())
        .bind(acknowledgement.manifest_id().into_uuid())
        .execute(&mut *transaction)
        .await?;
        match commit_mutation(transaction).await {
            Ok(()) => Ok(acknowledgement),
            Err(error @ RunnerProtocolStoreError::CommitAmbiguous(_)) => {
                match self
                    .load_workspace_release_acknowledgement(
                        acknowledgement.session(),
                        acknowledgement.placement_revision(),
                    )
                    .await?
                {
                    Some(recorded) => {
                        exact_workspace_release_acknowledgement_replay(acknowledgement, recorded)
                    }
                    None => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// Stores a checked pending release projection for PostgreSQL integration tests.
    ///
    /// Production enqueue belongs to the later replacement terminal transaction;
    /// this feature-gated surface only exercises the durable representation and
    /// typed readback without presenting candidate evidence as cleanup authority.
    #[cfg(feature = "postgres-integration")]
    #[doc(hidden)]
    pub async fn store_workspace_release_projection_for_test(
        &self,
        candidate: &RunnerWorkspaceReleaseCandidate,
        retired_placement_event_ordinal: u64,
        successor_placement_event_ordinal: u64,
        enrollment: RunnerEnrollmentId,
        connection_epoch: RunnerConnectionEpoch,
        connection_event_ordinal: u64,
    ) -> Result<(), RunnerProtocolStoreError> {
        sqlx::query(
            "INSERT INTO runner_workspace_release
                (session_id, placement_revision, runner_id, manifest_id,
                 retired_placement_event_ordinal,
                 successor_placement_event_ordinal, enrollment_id,
                 connection_epoch, connection_event_ordinal, state_kind)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'pending')",
        )
        .bind(candidate.session().into_uuid())
        .bind(Decimal::from(candidate.placement_revision().get()))
        .bind(candidate.runner().into_uuid())
        .bind(candidate.manifest_id().into_uuid())
        .bind(Decimal::from(retired_placement_event_ordinal))
        .bind(Decimal::from(successor_placement_event_ordinal))
        .bind(enrollment.into_uuid())
        .bind(Decimal::from(connection_epoch.get()))
        .bind(Decimal::from(connection_event_ordinal))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Claims one exact-directory pinned replacement for later terminalization.
    ///
    /// This short transaction deliberately appends no placement or transcript
    /// facts. The terminal transaction can therefore wait for any daemon-local
    /// model call's observation boundary without weakening command deduplication.
    pub async fn stage_workspace_free_pinned_replacement(
        &self,
        command: ReplaceLostRunner,
        identities: PinnedRunnerReplacementIdentities,
    ) -> Result<PinnedRunnerReplacementOutcome, RunnerProtocolStoreError> {
        if command.command().as_uuid().is_nil() || command.command().as_uuid().is_max() {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let mut transaction = self.pool.begin().await?;
        if let Some(outcome) =
            load_workspace_free_replacement_outcome(transaction.as_mut(), command).await?
        {
            transaction.rollback().await?;
            return Ok(outcome);
        }
        if inspect_replacement_registry(transaction.as_mut(), command.command())
            .await?
            .is_some()
        {
            transaction.rollback().await?;
            return Ok(PinnedRunnerReplacementOutcome::ConflictingReuse {
                command: command.command(),
            });
        }
        let claimed = sqlx::query(
            "INSERT INTO durable_command
                (command_id, command_kind, storage_version, claimed_at)
             VALUES ($1, $2, 1, transaction_timestamp())
             ON CONFLICT DO NOTHING",
        )
        .bind(command.command().into_uuid())
        .bind(REPLACE_LOST_RUNNER_KIND)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;
        if !claimed {
            let outcome = load_workspace_free_replacement_outcome(transaction.as_mut(), command)
                .await?
                .unwrap_or(PinnedRunnerReplacementOutcome::ConflictingReuse {
                    command: command.command(),
                });
            transaction.rollback().await?;
            return Ok(outcome);
        }
        insert_replacement_command(transaction.as_mut(), command).await?;

        let scheduler = sqlx::query(REPLACE_LOST_RUNNER_SCHEDULER)
            .bind(command.session().into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let session_exists: bool = scheduler.decode_column("session_exists")?;
        let scheduler_session: Option<Uuid> = scheduler.decode_column("scheduler_session_id")?;
        if !session_exists {
            transaction.rollback().await?;
            return Ok(PinnedRunnerReplacementOutcome::NotApplicable);
        }
        if scheduler_session != Some(command.session().into_uuid()) {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        let prior = sqlx::query(RUNNER_PLACEMENT_HEAD)
            .bind(command.session().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        let Some(prior) = prior else {
            transaction.rollback().await?;
            return Ok(PinnedRunnerReplacementOutcome::NotApplicable);
        };
        let current_revision =
            decode_runner_generation(prior.decode_column("placement_revision")?)?;
        if current_revision != command.expected_placement_revision() {
            transaction.rollback().await?;
            return Ok(PinnedRunnerReplacementOutcome::NotApplicable);
        }
        let stored = self
            .decode_stored_placement_in(&mut transaction, &prior)
            .await?;
        let (lost_event_ordinal, placement, _, _, _) = stored.into_parts();
        let requested_working_directory = match (
            placement.state(),
            &placement.request().workspace,
            &placement.request().working_directory,
        ) {
            (
                SessionRunnerPlacementState::RunnerLost(_),
                WorkspaceRequirement::None,
                WorkingDirectorySelection::Exact(directory),
            ) => directory,
            _ => {
                transaction.rollback().await?;
                return Ok(PinnedRunnerReplacementOutcome::NotApplicable);
            }
        };
        insert_workspace_free_replacement_stage(
            transaction.as_mut(),
            command,
            lost_event_ordinal,
            requested_working_directory,
            identities,
        )
        .await?;
        commit_mutation(transaction).await?;
        Ok(PinnedRunnerReplacementOutcome::Staged {
            command: command.command(),
        })
    }

    /// Completes the frontier-root subset of a staged workspace-free replacement.
    ///
    /// Sessions with an active turn or an existing semantic frontier retain the
    /// stage for the later observation-aware frontier extension transaction.
    pub async fn complete_workspace_free_pinned_replacement(
        &self,
        command: ReplaceLostRunner,
        identities: PinnedRunnerReplacementIdentities,
    ) -> Result<PinnedRunnerReplacementOutcome, RunnerProtocolStoreError> {
        let staged = self
            .stage_workspace_free_pinned_replacement(command, identities)
            .await?;
        if !matches!(staged, PinnedRunnerReplacementOutcome::Staged { .. }) {
            return Ok(staged);
        }
        let mut transaction = self.pool.begin().await?;
        let scheduler = sqlx::query(REPLACE_LOST_RUNNER_SCHEDULER)
            .bind(command.session().into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let session_exists: bool = scheduler.decode_column("session_exists")?;
        let scheduler_session: Option<Uuid> = scheduler.decode_column("scheduler_session_id")?;
        if !session_exists || scheduler_session != Some(command.session().into_uuid()) {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        if let Some(outcome) =
            load_workspace_free_replacement_outcome(transaction.as_mut(), command).await?
            && !matches!(outcome, PinnedRunnerReplacementOutcome::Staged { .. })
        {
            transaction.rollback().await?;
            return Ok(outcome);
        }
        let active_turn: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1
                  FROM turn_lifecycle
                 WHERE session_id = $1
                   AND state_kind = 'active'
                   AND NOT delegation_runtime_terminal
            )",
        )
        .bind(command.session().into_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        let has_semantic_frontier: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1
                  FROM context_frontier
                 WHERE owning_session_id = $1 AND member_count > 0
            )",
        )
        .bind(command.session().into_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        if active_turn || has_semantic_frontier {
            transaction.rollback().await?;
            return Ok(staged);
        }
        let outcome = self
            .complete_staged_workspace_free_replacement_at_prefix(
                &mut transaction,
                command,
                None,
                0,
            )
            .await?;
        commit_mutation(transaction).await?;
        Ok(outcome)
    }

    /// Finalizes every pending exact-directory replacement after one durable
    /// model observation has established the prefix it must extend.
    pub(crate) async fn finalize_workspace_free_replacements_after_model_observation(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        session: SessionId,
        prefix: signalbox_domain::ContextFrontierId,
        prefix_member_count: Option<u64>,
    ) -> Result<(), RunnerProtocolStoreError> {
        let prefix_member_count = match prefix_member_count {
            Some(value) => value,
            None => {
                let value: Option<Decimal> = sqlx::query_scalar(
                    "SELECT member_count
                       FROM context_frontier
                      WHERE owning_session_id = $1
                        AND context_frontier_id = $2",
                )
                .bind(session.into_uuid())
                .bind(prefix.into_uuid())
                .fetch_optional(&mut **transaction)
                .await?;
                decode_u64(value.ok_or(RunnerProtocolCorruption::CrossWiredReference)?)?
            }
        };
        let rows = sqlx::query(
            "SELECT command.command_id, command.session_id,
                    command.expected_placement_revision,
                    command.target_kind, command.target_runner_id,
                    command.target_pending_request_id
               FROM runner_workspace_free_replacement_stage AS stage
               JOIN replace_lost_runner_command AS command
                 ON command.command_id = stage.command_id
                AND command.session_id = stage.session_id
               LEFT JOIN replace_lost_runner_result AS result
                 ON result.command_id = stage.command_id
                AND result.session_id = stage.session_id
              WHERE stage.session_id = $1 AND result.command_id IS NULL
              ORDER BY command.command_id",
        )
        .bind(session.into_uuid())
        .fetch_all(&mut **transaction)
        .await?;
        let commands = rows
            .iter()
            .map(|row| {
                Ok::<_, RunnerProtocolStoreError>(ReplaceLostRunner::new(
                    DurableCommandId::from_uuid(row.decode_column("command_id")?),
                    session_id(row.decode_column("session_id")?),
                    decode_runner_generation(row.decode_column("expected_placement_revision")?)?,
                    decode_replacement_target(row)?,
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        for command in commands {
            if command.session() != session {
                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
            }
            self.complete_staged_workspace_free_replacement_at_prefix(
                transaction,
                command,
                Some(prefix),
                prefix_member_count,
            )
            .await?;
        }
        Ok(())
    }

    async fn complete_staged_workspace_free_replacement_at_prefix(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        command: ReplaceLostRunner,
        prefix: Option<signalbox_domain::ContextFrontierId>,
        prefix_member_count: u64,
    ) -> Result<PinnedRunnerReplacementOutcome, RunnerProtocolStoreError> {
        if let Some(outcome) =
            load_workspace_free_replacement_outcome(transaction.as_mut(), command).await?
            && !matches!(outcome, PinnedRunnerReplacementOutcome::Staged { .. })
        {
            return Ok(outcome);
        }
        let (target_authority, checked_same_runner) = match command.replacement() {
            RunnerReplacementTarget::Runner(runner) => (
                self.lock_direct_replacement_target(transaction, runner)
                    .await?,
                false,
            ),
            RunnerReplacementTarget::SameRunnerReenrollment(runner) => (
                self.lock_direct_replacement_target(transaction, runner)
                    .await?,
                true,
            ),
            RunnerReplacementTarget::PendingEnrollment(request) => (
                self.lock_pending_replacement_target(transaction, request)
                    .await?,
                false,
            ),
        };
        let prior = sqlx::query(RUNNER_PLACEMENT_HEAD)
            .bind(command.session().into_uuid())
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
        let current_event_ordinal = decode_u64(prior.decode_column("event_ordinal")?)?;
        let current_revision =
            decode_runner_generation(prior.decode_column("placement_revision")?)?;
        let state_kind: String = prior.decode_column("state_kind")?;
        let mut evidence = ReplacementRecordEvidence {
            placement_event_ordinal: Some(current_event_ordinal),
            placement_revision: Some(current_revision),
            placement_state_kind: Some(state_kind),
            ..ReplacementRecordEvidence::default()
        };
        if current_revision != command.expected_placement_revision() {
            let rejection = RunnerReplacementProvisioningRejection::PlacementRevisionMismatch {
                session: command.session(),
                expected: command.expected_placement_revision(),
                current: current_revision,
            };
            insert_replacement_provisioning_rejection(
                transaction.as_mut(),
                command,
                rejection,
                evidence,
            )
            .await?;
            return Ok(PinnedRunnerReplacementOutcome::Recorded(
                PinnedRunnerReplacementResult::Rejected(rejection),
            ));
        }
        let stored = self.decode_stored_placement_in(transaction, &prior).await?;
        let (stored_event_ordinal, placement, _, prior_grant, interrupted_attempt) =
            stored.into_parts();
        if stored_event_ordinal != current_event_ordinal {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        let lost = match placement.state() {
            SessionRunnerPlacementState::RunnerLost(lost) => lost,
            state => {
                let state = placement_recovery_state(state)
                    .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
                let rejection = RunnerReplacementProvisioningRejection::PlacementNotLost {
                    session: command.session(),
                    placement_revision: current_revision,
                    state,
                };
                insert_replacement_provisioning_rejection(
                    transaction.as_mut(),
                    command,
                    rejection,
                    evidence,
                )
                .await?;
                return Ok(PinnedRunnerReplacementOutcome::Recorded(
                    PinnedRunnerReplacementResult::Rejected(rejection),
                ));
            }
        };
        let prior_runner = lost.pinned().runner;
        evidence.prior_runner = Some(prior_runner);
        evidence.new_runner = target_authority.runner();
        if target_authority.predecessor_runner().is_some()
            && target_authority.predecessor_runner() != Some(prior_runner)
        {
            let rejection = RunnerReplacementProvisioningRejection::ReplacementTargetUnavailable {
                session: command.session(),
                target: command.replacement(),
                reason: RunnerReplacementTargetUnavailableReason::PendingRequestMismatch,
            };
            insert_replacement_provisioning_rejection(
                transaction.as_mut(),
                command,
                rejection,
                evidence,
            )
            .await?;
            return Ok(PinnedRunnerReplacementOutcome::Recorded(
                PinnedRunnerReplacementResult::Rejected(rejection),
            ));
        }
        let selected_is_prior = target_authority.runner() == Some(prior_runner);
        if (!checked_same_runner && selected_is_prior)
            || (checked_same_runner
                && (!selected_is_prior || lost.source() != RunnerPlacementLossSource::Registration))
        {
            let rejection = RunnerReplacementProvisioningRejection::ReplacementSameRunner {
                session: command.session(),
                runner: prior_runner,
            };
            insert_replacement_provisioning_rejection(
                transaction.as_mut(),
                command,
                rejection,
                evidence,
            )
            .await?;
            return Ok(PinnedRunnerReplacementOutcome::Recorded(
                PinnedRunnerReplacementResult::Rejected(rejection),
            ));
        }
        let ReplacementTargetAuthority::Current(target) = target_authority else {
            let reason = target_authority
                .unavailable_reason()
                .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
            let rejection = RunnerReplacementProvisioningRejection::ReplacementTargetUnavailable {
                session: command.session(),
                target: command.replacement(),
                reason,
            };
            insert_replacement_provisioning_rejection(
                transaction.as_mut(),
                command,
                rejection,
                evidence,
            )
            .await?;
            return Ok(PinnedRunnerReplacementOutcome::Recorded(
                PinnedRunnerReplacementResult::Rejected(rejection),
            ));
        };
        let stage = sqlx::query(
            "SELECT lost_placement_event_ordinal, lost_placement_revision,
                    requested_working_directory, boundary_entry_id,
                    boundary_frontier_id
               FROM runner_workspace_free_replacement_stage
              WHERE command_id = $1 AND session_id = $2",
        )
        .bind(command.command().into_uuid())
        .bind(command.session().into_uuid())
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
        let stage_event_ordinal = decode_u64(stage.decode_column("lost_placement_event_ordinal")?)?;
        let stage_revision =
            decode_runner_generation(stage.decode_column("lost_placement_revision")?)?;
        let working_directory =
            RunnerWorkingDirectory::try_new(stage.decode_column("requested_working_directory")?)
                .map_err(|_| RunnerProtocolCorruption::InvalidEncoding)?;
        let identities = PinnedRunnerReplacementIdentities::new(
            signalbox_domain::SemanticTranscriptEntryId::from_uuid(
                stage.decode_column("boundary_entry_id")?,
            ),
            signalbox_domain::ContextFrontierId::from_uuid(
                stage.decode_column("boundary_frontier_id")?,
            ),
        );
        if stage_event_ordinal != current_event_ordinal
            || stage_revision != current_revision
            || placement.request().credential_profile.is_some()
            || prior_grant.is_some()
            || interrupted_attempt.is_some()
        {
            return Ok(PinnedRunnerReplacementOutcome::Staged {
                command: command.command(),
            });
        }
        let replacement_request = placement.request().clone();
        let replacement = if checked_same_runner {
            let loss_revision =
                lost.loss_registration_revision()
                    .ok_or(RunnerProtocolStoreError::Domain(
                        RunnerDomainError::InvalidState,
                    ))?;
            let loss_registration = load_registration_in(
                transaction.as_mut(),
                target.enrollment,
                RunnerRegistrationRevision::try_from_u64(loss_revision.get())
                    .ok_or(RunnerProtocolCorruption::InvalidEncoding)?,
                None,
                &self.catalog,
            )
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
            placement.replace_lost_runner_after_same_runner_registration_recovery(
                replacement_request,
                signalbox_domain::SameRunnerRegistrationRecovery {
                    loss_registration: loss_registration.registration().clone(),
                    current_registration: target.registration.registration().clone(),
                },
                working_directory.clone(),
                None,
                None,
            )
        } else {
            placement.replace_lost_runner(
                replacement_request,
                target.registration.registration(),
                working_directory.clone(),
                None,
                None,
            )
        }
        .map_err(RunnerProtocolStoreError::Domain)?;
        if let Some(activation) = target.pending_activation.as_ref() {
            activate_pending_replacement_target(transaction.as_mut(), activation).await?;
        }
        let next_event_ordinal = current_event_ordinal
            .checked_add(1)
            .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
        let history = prospective_placement_reconstitution_history(
            transaction.as_mut(),
            Some(&prior),
            "runner_replaced",
            &replacement.placement,
        )
        .await?;
        validate_placement_snapshot(
            &replacement.placement,
            Some(&target.registration),
            replacement.grant.as_ref(),
            history,
        )?;
        insert_placement_record(
            transaction.as_mut(),
            next_event_ordinal,
            "runner_replaced",
            &replacement.placement,
            PlacementRecordEvidence {
                registration_identity: (
                    Some(target.enrollment.into_uuid()),
                    Some(Decimal::from(target.registration_revision.get())),
                ),
                grant_origin: None,
                interrupted_tool_attempt: None,
                loss_registration_revision: None,
            },
        )
        .await?;
        let advanced = sqlx::query(
            "UPDATE runner_current_session_placement
                SET event_ordinal = $2
              WHERE session_id = $1 AND event_ordinal = $3",
        )
        .bind(command.session().into_uuid())
        .bind(Decimal::from(next_event_ordinal))
        .bind(Decimal::from(current_event_ordinal))
        .execute(&mut **transaction)
        .await?
        .rows_affected();
        if advanced != 1 {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        sqlx::query(
            "INSERT INTO semantic_transcript_entry
                (source_session_id, semantic_entry_id, payload_kind,
                 runner_placement_revision, runner_placement_event_ordinal)
             VALUES ($1, $2, 'runner_placement_changed', $3, $4)",
        )
        .bind(command.session().into_uuid())
        .bind(identities.semantic_entry().into_uuid())
        .bind(Decimal::from(replacement.placement.revision().get()))
        .bind(Decimal::from(next_event_ordinal))
        .execute(&mut **transaction)
        .await?;
        let member_count = prefix_member_count
            .checked_add(1)
            .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
        crate::model_execution::insert_snapshot_append(
            transaction.as_mut(),
            crate::model_execution::SnapshotAppend {
                owning_session: command.session(),
                frontier: identities.context_frontier(),
                prefix,
                member_count,
                prefix_member_count,
                appended_entries: [SemanticTranscriptEntryRef::from_source(
                    command.session(),
                    identities.semantic_entry(),
                )],
            },
        )
        .await
        .map_err(map_snapshot_append_error)?;
        sqlx::query(
            "INSERT INTO session_runner_placement_frontier
                (session_id, placement_revision, semantic_entry_id,
                 context_frontier_id)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(command.session().into_uuid())
        .bind(Decimal::from(replacement.placement.revision().get()))
        .bind(identities.semantic_entry().into_uuid())
        .bind(identities.context_frontier().into_uuid())
        .execute(&mut **transaction)
        .await?;
        outbox::append(
            transaction.as_mut(),
            OutboxEvent::RunnerStateTransition(RunnerStateOutboxEvent {
                session: command.session(),
                runner: target.runner,
                placement_revision: replacement.placement.revision(),
                sandbox: replacement.placement.request().sandbox,
                working_directory: Some(working_directory.clone()),
                state: DispatchedRunnerState::Replaced,
                source: RunnerStateOutboxSource {
                    placement_event_ordinal: next_event_ordinal,
                    connection: None,
                },
            }),
        )
        .await?;
        let applied = ReplacedPinnedRunner::new(
            command.session(),
            prior_runner,
            target.runner,
            replacement.placement.revision(),
            working_directory,
            replacement.placement.request().sandbox,
        );
        insert_pinned_replacement_result(
            transaction.as_mut(),
            command,
            &applied,
            next_event_ordinal,
            &target,
        )
        .await?;
        Ok(PinnedRunnerReplacementOutcome::Recorded(
            PinnedRunnerReplacementResult::Applied(applied),
        ))
    }

    /// Claims and durably stages one repository-backed pinned replacement.
    pub async fn stage_runner_replacement_provisioning(
        &self,
        command: ReplaceLostRunner,
        authorization: WorkspaceProvisioningAuthorizationId,
    ) -> Result<RunnerReplacementProvisioningOutcome, RunnerProtocolStoreError> {
        if command.command().as_uuid().is_nil() || command.command().as_uuid().is_max() {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let mut transaction = self.pool.begin().await?;
        if let Some(outcome) =
            load_replacement_provisioning_outcome(transaction.as_mut(), command).await?
        {
            transaction.rollback().await?;
            return Ok(outcome);
        }
        if inspect_replacement_registry(transaction.as_mut(), command.command())
            .await?
            .is_some()
        {
            transaction.rollback().await?;
            return Ok(RunnerReplacementProvisioningOutcome::ConflictingReuse {
                command: command.command(),
            });
        }
        let claimed = sqlx::query(
            "INSERT INTO durable_command
                (command_id, command_kind, storage_version, claimed_at)
             VALUES ($1, $2, 1, transaction_timestamp())
             ON CONFLICT DO NOTHING",
        )
        .bind(command.command().into_uuid())
        .bind(REPLACE_LOST_RUNNER_KIND)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;
        if !claimed {
            let outcome = load_replacement_provisioning_outcome(transaction.as_mut(), command)
                .await?
                .unwrap_or(RunnerReplacementProvisioningOutcome::ConflictingReuse {
                    command: command.command(),
                });
            transaction.rollback().await?;
            return Ok(outcome);
        }
        insert_replacement_command(transaction.as_mut(), command).await?;

        let scheduler = sqlx::query(REPLACE_LOST_RUNNER_SCHEDULER)
            .bind(command.session().into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let session_exists: bool = scheduler.decode_column("session_exists")?;
        let scheduler_session: Option<Uuid> = scheduler.decode_column("scheduler_session_id")?;
        if !session_exists {
            let rejection = RunnerReplacementProvisioningRejection::SessionNotFound {
                session: command.session(),
            };
            insert_replacement_provisioning_rejection(
                transaction.as_mut(),
                command,
                rejection,
                ReplacementRecordEvidence::default(),
            )
            .await?;
            commit_mutation(transaction).await?;
            return Ok(RunnerReplacementProvisioningOutcome::Rejected(rejection));
        }
        if scheduler_session != Some(command.session().into_uuid()) {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }

        let target_authority = match command.replacement() {
            RunnerReplacementTarget::Runner(runner)
            | RunnerReplacementTarget::SameRunnerReenrollment(runner) => {
                self.lock_direct_replacement_target(&mut transaction, runner)
                    .await?
            }
            RunnerReplacementTarget::PendingEnrollment(request) => {
                self.lock_pending_replacement_target(&mut transaction, request)
                    .await?
            }
        };
        let prior = sqlx::query(RUNNER_PLACEMENT_HEAD)
            .bind(command.session().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        let Some(prior) = prior else {
            let rejection = RunnerReplacementProvisioningRejection::RunnerPlacementNotFound {
                session: command.session(),
            };
            insert_replacement_provisioning_rejection(
                transaction.as_mut(),
                command,
                rejection,
                ReplacementRecordEvidence::default(),
            )
            .await?;
            commit_mutation(transaction).await?;
            return Ok(RunnerReplacementProvisioningOutcome::Rejected(rejection));
        };
        let state_kind: String = prior.decode_column("state_kind")?;
        let current_revision =
            decode_runner_generation(prior.decode_column("placement_revision")?)?;
        let current_event_ordinal = decode_u64(prior.decode_column("event_ordinal")?)?;
        let mut evidence = ReplacementRecordEvidence {
            placement_event_ordinal: Some(current_event_ordinal),
            placement_revision: Some(current_revision),
            placement_state_kind: Some(state_kind),
            ..ReplacementRecordEvidence::default()
        };
        if current_revision != command.expected_placement_revision() {
            let rejection = RunnerReplacementProvisioningRejection::PlacementRevisionMismatch {
                session: command.session(),
                expected: command.expected_placement_revision(),
                current: current_revision,
            };
            insert_replacement_provisioning_rejection(
                transaction.as_mut(),
                command,
                rejection,
                evidence,
            )
            .await?;
            commit_mutation(transaction).await?;
            return Ok(RunnerReplacementProvisioningOutcome::Rejected(rejection));
        }
        let stored = self
            .decode_stored_placement_in(&mut transaction, &prior)
            .await?;
        let (stored_event_ordinal, placement, _, _, _) = stored.into_parts();
        if stored_event_ordinal != current_event_ordinal {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        let lost = match placement.state() {
            SessionRunnerPlacementState::RunnerLost(lost) => lost,
            SessionRunnerPlacementState::RunnerLostBeforePin(_) => {
                transaction.rollback().await?;
                return Ok(RunnerReplacementProvisioningOutcome::NotApplicable);
            }
            state => {
                let state = placement_recovery_state(state)
                    .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
                let rejection = RunnerReplacementProvisioningRejection::PlacementNotLost {
                    session: command.session(),
                    placement_revision: current_revision,
                    state,
                };
                insert_replacement_provisioning_rejection(
                    transaction.as_mut(),
                    command,
                    rejection,
                    evidence,
                )
                .await?;
                commit_mutation(transaction).await?;
                return Ok(RunnerReplacementProvisioningOutcome::Rejected(rejection));
            }
        };
        let lost_runner = lost.pinned().runner;
        evidence.prior_runner = Some(lost_runner);
        evidence.new_runner = target_authority.runner();
        let selected_same_runner = target_authority.runner() == Some(lost_runner);
        let ordinary_same_runner = selected_same_runner
            && !matches!(
                command.replacement(),
                RunnerReplacementTarget::SameRunnerReenrollment(_)
            );
        let unchecked_same_runner = selected_same_runner
            && matches!(
                command.replacement(),
                RunnerReplacementTarget::SameRunnerReenrollment(_)
            )
            && lost.source() != RunnerPlacementLossSource::Registration;
        if ordinary_same_runner || unchecked_same_runner {
            let rejection = RunnerReplacementProvisioningRejection::ReplacementSameRunner {
                session: command.session(),
                runner: lost_runner,
            };
            insert_replacement_provisioning_rejection(
                transaction.as_mut(),
                command,
                rejection,
                evidence,
            )
            .await?;
            commit_mutation(transaction).await?;
            return Ok(RunnerReplacementProvisioningOutcome::Rejected(rejection));
        }
        let ReplacementTargetAuthority::Current(target) = target_authority else {
            let reason = target_authority
                .unavailable_reason()
                .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
            let rejection = RunnerReplacementProvisioningRejection::ReplacementTargetUnavailable {
                session: command.session(),
                target: command.replacement(),
                reason,
            };
            insert_replacement_provisioning_rejection(
                transaction.as_mut(),
                command,
                rejection,
                evidence,
            )
            .await?;
            commit_mutation(transaction).await?;
            return Ok(RunnerReplacementProvisioningOutcome::Rejected(rejection));
        };
        let target_runner = target.runner;
        if target.predecessor_runner().is_some() && target.predecessor_runner() != Some(lost_runner)
        {
            evidence.new_runner = Some(target_runner);
            let rejection = RunnerReplacementProvisioningRejection::ReplacementTargetUnavailable {
                session: command.session(),
                target: command.replacement(),
                reason: RunnerReplacementTargetUnavailableReason::PendingRequestMismatch,
            };
            insert_replacement_provisioning_rejection(
                transaction.as_mut(),
                command,
                rejection,
                evidence,
            )
            .await?;
            commit_mutation(transaction).await?;
            return Ok(RunnerReplacementProvisioningOutcome::Rejected(rejection));
        }
        let mut replacement_request = placement.request().clone();
        replacement_request.selector = RunnerSelector::Identity(target_runner);
        if matches!(replacement_request.workspace, WorkspaceRequirement::None) {
            transaction.rollback().await?;
            return Ok(RunnerReplacementProvisioningOutcome::NotApplicable);
        }
        let checked =
            match command.replacement() {
                RunnerReplacementTarget::SameRunnerReenrollment(_) => {
                    let loss_revision = lost.loss_registration_revision().ok_or(
                        RunnerProtocolStoreError::Domain(RunnerDomainError::InvalidState),
                    )?;
                    let loss_registration = load_registration_in(
                        transaction.as_mut(),
                        target.enrollment,
                        RunnerRegistrationRevision::try_from_u64(loss_revision.get())
                            .ok_or(RunnerProtocolCorruption::InvalidEncoding)?,
                        None,
                        &self.catalog,
                    )
                    .await?
                    .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
                    placement.authorize_same_runner_replacement_workspace(
                        authorization,
                        &replacement_request,
                        &signalbox_domain::SameRunnerRegistrationRecovery {
                            loss_registration: loss_registration.registration().clone(),
                            current_registration: target.registration.registration().clone(),
                        },
                    )
                }
                RunnerReplacementTarget::Runner(_)
                | RunnerReplacementTarget::PendingEnrollment(_) => placement
                    .authorize_lost_runner_replacement_workspace(
                        authorization,
                        &replacement_request,
                        target.registration.registration(),
                    ),
            };
        let checked = match checked {
            Ok(checked) => checked,
            Err(error) if replacement_target_is_unavailable(&error) => {
                let rejection =
                    RunnerReplacementProvisioningRejection::ReplacementTargetUnavailable {
                        session: command.session(),
                        target: command.replacement(),
                        reason: RunnerReplacementTargetUnavailableReason::NotAdvertised,
                    };
                insert_replacement_provisioning_rejection(
                    transaction.as_mut(),
                    command,
                    rejection,
                    evidence,
                )
                .await?;
                commit_mutation(transaction).await?;
                return Ok(RunnerReplacementProvisioningOutcome::Rejected(rejection));
            }
            Err(error) => return Err(RunnerProtocolStoreError::Domain(error)),
        };
        insert_workspace_provisioning_authorization(
            transaction.as_mut(),
            command,
            current_event_ordinal,
            &target,
            &checked,
        )
        .await?;
        let staged = RunnerReplacementProvisioningStage::from_authorization(&checked);
        commit_mutation(transaction).await?;
        Ok(RunnerReplacementProvisioningOutcome::Staged(staged))
    }

    /// Replaces one exact runner lost before pinning with a different live runner.
    pub async fn replace_lost_runner_before_pin(
        &self,
        command: ReplaceLostRunner,
    ) -> Result<ReplaceLostRunnerBeforePinOutcome, RunnerProtocolStoreError> {
        if matches!(
            command.replacement(),
            RunnerReplacementTarget::SameRunnerReenrollment(_)
        ) {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        if command.command().as_uuid().is_nil() || command.command().as_uuid().is_max() {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let mut transaction = self.pool.begin().await?;
        match inspect_replacement_registry(&mut transaction, command.command()).await? {
            Some(CommandKind::ReplaceLostRunner) => {
                let (recorded, result) =
                    load_replacement_record(transaction.as_mut(), command.command())
                        .await?
                        .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
                transaction.rollback().await?;
                return Ok(if recorded == command {
                    ReplaceLostRunnerBeforePinOutcome::Recorded(result)
                } else {
                    ReplaceLostRunnerBeforePinOutcome::ConflictingReuse {
                        command: command.command(),
                    }
                });
            }
            Some(_) => {
                transaction.rollback().await?;
                return Ok(ReplaceLostRunnerBeforePinOutcome::ConflictingReuse {
                    command: command.command(),
                });
            }
            None => {}
        }

        let claimed = sqlx::query(
            "INSERT INTO durable_command
                (command_id, command_kind, storage_version, claimed_at)
             VALUES ($1, $2, 1, transaction_timestamp())
             ON CONFLICT DO NOTHING",
        )
        .bind(command.command().into_uuid())
        .bind(REPLACE_LOST_RUNNER_KIND)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;
        if !claimed {
            let outcome = resolve_replacement_claim_winner(&mut transaction, command).await?;
            transaction.rollback().await?;
            return Ok(outcome);
        }
        insert_replacement_command(transaction.as_mut(), command).await?;

        let scheduler = sqlx::query(REPLACE_LOST_RUNNER_SCHEDULER)
            .bind(command.session().into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let session_exists: bool = scheduler.decode_column("session_exists")?;
        let scheduler_session: Option<Uuid> = scheduler.decode_column("scheduler_session_id")?;
        let mut evidence = ReplacementRecordEvidence::default();
        let result = if !session_exists {
            ReplaceLostRunnerBeforePinResult::Rejected(
                ReplaceLostRunnerBeforePinRejection::SessionNotFound {
                    session: command.session(),
                },
            )
        } else {
            if scheduler_session != Some(command.session().into_uuid()) {
                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
            }
            let target_authority = match command.replacement() {
                RunnerReplacementTarget::Runner(runner) => {
                    self.lock_direct_replacement_target(&mut transaction, runner)
                        .await?
                }
                RunnerReplacementTarget::PendingEnrollment(request) => {
                    self.lock_pending_replacement_target(&mut transaction, request)
                        .await?
                }
                RunnerReplacementTarget::SameRunnerReenrollment(_) => {
                    return Err(RunnerProtocolStoreError::Domain(
                        RunnerDomainError::InvalidState,
                    ));
                }
            };
            let prior = sqlx::query(RUNNER_PLACEMENT_HEAD)
                .bind(command.session().into_uuid())
                .fetch_optional(&mut *transaction)
                .await?;
            match prior {
                None => ReplaceLostRunnerBeforePinResult::Rejected(
                    ReplaceLostRunnerBeforePinRejection::RunnerPlacementNotFound {
                        session: command.session(),
                    },
                ),
                Some(prior) => {
                    let state_kind: String = prior.decode_column("state_kind")?;
                    let current_revision =
                        decode_runner_generation(prior.decode_column("placement_revision")?)?;
                    let current_event_ordinal = decode_u64(prior.decode_column("event_ordinal")?)?;
                    evidence = ReplacementRecordEvidence {
                        placement_event_ordinal: Some(current_event_ordinal),
                        placement_revision: Some(current_revision),
                        placement_state_kind: Some(state_kind),
                        ..ReplacementRecordEvidence::default()
                    };
                    if current_revision != command.expected_placement_revision() {
                        ReplaceLostRunnerBeforePinResult::Rejected(
                            ReplaceLostRunnerBeforePinRejection::PlacementRevisionMismatch {
                                session: command.session(),
                                expected: command.expected_placement_revision(),
                                current: current_revision,
                            },
                        )
                    } else {
                        let stored = self
                            .decode_stored_placement_in(&mut transaction, &prior)
                            .await?;
                        let (stored_event_ordinal, placement, _, grant, interrupted_attempt) =
                            stored.into_parts();
                        if stored_event_ordinal != current_event_ordinal
                            || grant.is_some()
                            || interrupted_attempt.is_some()
                        {
                            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
                        }
                        let lost_runner = match placement.state() {
                            SessionRunnerPlacementState::RunnerLostBeforePin(lost) => {
                                Some(lost.runner())
                            }
                            SessionRunnerPlacementState::Unpinned => {
                                evidence.placement_state_kind = Some("unpinned".to_owned());
                                None
                            }
                            SessionRunnerPlacementState::Pinned(_) => {
                                evidence.placement_state_kind = Some("pinned".to_owned());
                                None
                            }
                            SessionRunnerPlacementState::RunnerLost(_) => {
                                return Err(RunnerProtocolStoreError::Domain(
                                    RunnerDomainError::InvalidState,
                                ));
                            }
                            SessionRunnerPlacementState::RunnerAbandoned(_) => {
                                evidence.placement_state_kind = Some("runner_abandoned".to_owned());
                                None
                            }
                        };
                        match lost_runner {
                            None => ReplaceLostRunnerBeforePinResult::Rejected(
                                ReplaceLostRunnerBeforePinRejection::PlacementNotLost {
                                    session: command.session(),
                                    placement_revision: current_revision,
                                    state: placement_recovery_state(placement.state())
                                        .ok_or(RunnerProtocolCorruption::CrossWiredReference)?,
                                },
                            ),
                            Some(lost_runner)
                                if target_authority.predecessor_runner().is_some()
                                    && target_authority.predecessor_runner()
                                        != Some(lost_runner) =>
                            {
                                evidence.prior_runner = Some(lost_runner);
                                evidence.new_runner = target_authority.runner();
                                ReplaceLostRunnerBeforePinResult::Rejected(
                                    ReplaceLostRunnerBeforePinRejection::ReplacementTargetUnavailable {
                                        session: command.session(),
                                        target: command.replacement(),
                                        reason: RunnerReplacementTargetUnavailableReason::PendingRequestMismatch,
                                    },
                                )
                            }
                            Some(lost_runner) if target_authority.runner() == Some(lost_runner) => {
                                evidence.prior_runner = Some(lost_runner);
                                evidence.new_runner = Some(lost_runner);
                                ReplaceLostRunnerBeforePinResult::Rejected(
                                    ReplaceLostRunnerBeforePinRejection::ReplacementSameRunner {
                                        session: command.session(),
                                        runner: lost_runner,
                                    },
                                )
                            }
                            Some(lost_runner) => match target_authority {
                                ReplacementTargetAuthority::Unavailable {
                                    reason, runner, ..
                                } => {
                                    evidence.prior_runner = Some(lost_runner);
                                    evidence.new_runner = runner;
                                    ReplaceLostRunnerBeforePinResult::Rejected(
                                        ReplaceLostRunnerBeforePinRejection::ReplacementTargetUnavailable {
                                            session: command.session(),
                                            target: command.replacement(),
                                            reason,
                                        },
                                    )
                                }
                                ReplacementTargetAuthority::Current(target_authority) => {
                                    let target_runner = target_authority.runner;
                                    let registration = &target_authority.registration;
                                    let mut replacement_request = placement.request().clone();
                                    replacement_request.selector =
                                        RunnerSelector::Identity(target_runner);
                                    let replacement = placement.replace_lost_runner_before_pin(
                                        replacement_request,
                                        registration.registration(),
                                    );
                                    match replacement {
                                        Err(error) if replacement_target_is_unavailable(&error) => {
                                            evidence.prior_runner = Some(lost_runner);
                                            evidence.new_runner = Some(target_runner);
                                            ReplaceLostRunnerBeforePinResult::Rejected(
                                                ReplaceLostRunnerBeforePinRejection::ReplacementTargetUnavailable {
                                                    session: command.session(),
                                                    target: command.replacement(),
                                                    reason: RunnerReplacementTargetUnavailableReason::NotAdvertised,
                                                },
                                            )
                                        }
                                        Err(error) => {
                                            return Err(RunnerProtocolStoreError::Domain(error));
                                        }
                                        Ok(replacement) => {
                                            if let Some(activation) =
                                                target_authority.pending_activation.as_ref()
                                            {
                                                activate_pending_replacement_target(
                                                    transaction.as_mut(),
                                                    activation,
                                                )
                                                .await?;
                                            }
                                            let next_event_ordinal =
                                                current_event_ordinal.checked_add(1).ok_or(
                                                    RunnerProtocolCorruption::GenerationExhausted,
                                                )?;
                                            let history =
                                                prospective_placement_reconstitution_history(
                                                    transaction.as_mut(),
                                                    Some(&prior),
                                                    "pre_pin_replaced",
                                                    &replacement.placement,
                                                )
                                                .await?;
                                            validate_placement_snapshot(
                                                &replacement.placement,
                                                Some(registration),
                                                None,
                                                history,
                                            )?;
                                            insert_placement_record(
                                                transaction.as_mut(),
                                                next_event_ordinal,
                                                "pre_pin_replaced",
                                                &replacement.placement,
                                                PlacementRecordEvidence {
                                                    registration_identity: (None, None),
                                                    grant_origin: None,
                                                    interrupted_tool_attempt: None,
                                                    loss_registration_revision: None,
                                                },
                                            )
                                            .await?;
                                            let changed = sqlx::query(
                                                "UPDATE runner_current_session_placement
                                                    SET event_ordinal = $2
                                                  WHERE session_id = $1 AND event_ordinal = $3",
                                            )
                                            .bind(command.session().into_uuid())
                                            .bind(Decimal::from(next_event_ordinal))
                                            .bind(Decimal::from(current_event_ordinal))
                                            .execute(&mut *transaction)
                                            .await?
                                            .rows_affected();
                                            if changed != 1 {
                                                return Err(
                                                    RunnerProtocolCorruption::CrossWiredReference
                                                        .into(),
                                                );
                                            }
                                            outbox::append(
                                                transaction.as_mut(),
                                                OutboxEvent::RunnerStateTransition(
                                                    RunnerStateOutboxEvent {
                                                        session: command.session(),
                                                        runner: target_runner,
                                                        placement_revision: replacement
                                                            .placement
                                                            .revision(),
                                                        sandbox: replacement
                                                            .placement
                                                            .request()
                                                            .sandbox,
                                                        working_directory:
                                                            lost_runner_working_directory(
                                                                &replacement.placement,
                                                            ),
                                                        state: DispatchedRunnerState::Replaced,
                                                        source: RunnerStateOutboxSource {
                                                            placement_event_ordinal:
                                                                next_event_ordinal,
                                                            connection: None,
                                                        },
                                                    },
                                                ),
                                            )
                                            .await?;
                                            evidence = ReplacementRecordEvidence {
                                                placement_event_ordinal: Some(next_event_ordinal),
                                                placement_revision: Some(
                                                    replacement.placement.revision(),
                                                ),
                                                placement_state_kind: Some("unpinned".to_owned()),
                                                prior_runner: Some(lost_runner),
                                                new_runner: Some(target_runner),
                                                sandbox: Some(
                                                    replacement.placement.request().sandbox,
                                                ),
                                                target_enrollment: Some(
                                                    target_authority.enrollment,
                                                ),
                                                target_registration_revision: Some(
                                                    target_authority.registration_revision,
                                                ),
                                                target_connection_epoch: Some(
                                                    target_authority.connection_epoch,
                                                ),
                                                target_connection_event_ordinal: Some(
                                                    target_authority.connection_event_ordinal,
                                                ),
                                            };
                                            ReplaceLostRunnerBeforePinResult::Applied(
                                                ReplacedLostRunnerBeforePin::new(
                                                    command.session(),
                                                    lost_runner,
                                                    target_runner,
                                                    replacement.placement.revision(),
                                                    replacement.placement.request().sandbox,
                                                ),
                                            )
                                        }
                                    }
                                }
                            },
                        }
                    }
                }
            }
        };
        insert_replacement_result(transaction.as_mut(), command, result, evidence).await?;
        commit_mutation(transaction).await?;
        Ok(ReplaceLostRunnerBeforePinOutcome::Recorded(result))
    }

    async fn lock_direct_replacement_target(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        runner: RunnerId,
    ) -> Result<ReplacementTargetAuthority, RunnerProtocolStoreError> {
        let rows = sqlx::query(REPLACE_LOST_RUNNER_ENROLLMENT_BY_RUNNER)
            .bind(runner.into_uuid())
            .fetch_all(&mut **transaction)
            .await?;
        let decoded = rows
            .iter()
            .map(|row| {
                Ok::<_, RunnerProtocolStoreError>((
                    row.decode_column::<Uuid>("enrollment_id")?,
                    row.decode_column::<String>("state_kind")?,
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let active: Vec<Uuid> = decoded
            .into_iter()
            .filter_map(|(enrollment, state)| (state == "active").then_some(enrollment))
            .collect();
        let [enrollment_uuid] = active.as_slice() else {
            return Ok(ReplacementTargetAuthority::Unavailable {
                reason: RunnerReplacementTargetUnavailableReason::NotCurrent,
                runner: Some(runner),
                predecessor_runner: None,
            });
        };
        let enrollment = RunnerEnrollmentId::from_uuid(*enrollment_uuid);
        let connection = sqlx::query(PROMOTE_PENDING_RUNNER_CONNECTION)
            .bind(enrollment.into_uuid())
            .fetch_optional(&mut **transaction)
            .await?;
        let connection = connection
            .map(|row| {
                let state: String = row.decode_column("state_kind")?;
                Ok::<_, RunnerProtocolStoreError>((
                    RunnerConnectionEpoch::try_from_u64(decode_u64(
                        row.decode_column("connection_epoch")?,
                    )?)
                    .ok_or(RunnerProtocolCorruption::InvalidEncoding)?,
                    decode_u64(row.decode_column("connection_event_ordinal")?)?,
                    runner_connection_state_from_str(&state)
                        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?,
                ))
            })
            .transpose()?;
        let Some((connection_epoch, connection_event_ordinal, connection_state)) = connection
        else {
            return Ok(ReplacementTargetAuthority::Unavailable {
                reason: RunnerReplacementTargetUnavailableReason::NotConnected,
                runner: Some(runner),
                predecessor_runner: None,
            });
        };
        if connection_state != RunnerConnectionState::Connected {
            return Ok(ReplacementTargetAuthority::Unavailable {
                reason: RunnerReplacementTargetUnavailableReason::NotConnected,
                runner: Some(runner),
                predecessor_runner: None,
            });
        }
        let revision: Option<Decimal> = sqlx::query_scalar(RUNNER_REGISTRATION_HEAD)
            .bind(enrollment.into_uuid())
            .fetch_optional(&mut **transaction)
            .await?;
        let revision = revision
            .map(decode_registration_revision)
            .transpose()?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        let enrollment_state = load_enrollment_in(transaction.as_mut(), enrollment)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        let registration = load_registration_in(
            transaction.as_mut(),
            enrollment,
            revision,
            Some(&enrollment_state),
            &self.catalog,
        )
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        Ok(ReplacementTargetAuthority::Current(Box::new(
            ReplacementTargetEvidence {
                runner,
                enrollment,
                registration_revision: revision,
                connection_epoch,
                connection_event_ordinal,
                registration,
                pending_activation: None,
            },
        )))
    }

    async fn lock_pending_replacement_target(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        request: RunnerEnrollmentRequestId,
    ) -> Result<ReplacementTargetAuthority, RunnerProtocolStoreError> {
        // The temporary deployment singleton makes pending selection and
        // activation one serial decision without encoding singleton identity in
        // the reusable domain or wire contracts.
        sqlx::query("LOCK TABLE runner_enrollment IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut **transaction)
            .await?;
        let relation = sqlx::query(
            "SELECT pending.enrollment_id, pending.predecessor_enrollment_id,
                    pending.predecessor_loss_epoch
               FROM runner_pending_enrollment AS pending
              WHERE pending.request_id = $1",
        )
        .bind(request.into_uuid())
        .fetch_optional(&mut **transaction)
        .await?;
        let Some(relation) = relation else {
            return Ok(ReplacementTargetAuthority::Unavailable {
                reason: RunnerReplacementTargetUnavailableReason::PendingRequestMismatch,
                runner: None,
                predecessor_runner: None,
            });
        };
        let candidate = runner_enrollment_id(relation.decode_column("enrollment_id")?);
        let predecessor =
            runner_enrollment_id(relation.decode_column("predecessor_enrollment_id")?);
        let predecessor_loss_epoch = RunnerConnectionLossEpoch::try_from_u64(decode_u64(
            relation.decode_column("predecessor_loss_epoch")?,
        )?)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        let enrollment_ids = [candidate.into_uuid(), predecessor.into_uuid()];
        let rows = sqlx::query(PROMOTE_PENDING_RUNNER_ENROLLMENTS)
            .bind(enrollment_ids.as_slice())
            .fetch_all(&mut **transaction)
            .await?;
        if rows.len() != 2 {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        let mut enrollments = rows
            .into_iter()
            .map(decode_replacement_enrollment_row)
            .collect::<Result<Vec<_>, _>>()?;
        let candidate_position = enrollments
            .iter()
            .position(|row| row.enrollment == candidate)
            .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
        let candidate_row = enrollments.remove(candidate_position);
        let predecessor_row = enrollments
            .pop()
            .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
        if candidate_row.state != RunnerEnrollmentState::Pending {
            return Ok(ReplacementTargetAuthority::Unavailable {
                reason: RunnerReplacementTargetUnavailableReason::PendingRequestMismatch,
                runner: Some(candidate_row.runner),
                predecessor_runner: Some(predecessor_row.runner),
            });
        }
        if candidate_row.revision != 1
            || predecessor_row.enrollment != predecessor
            || predecessor_row.state != RunnerEnrollmentState::Active
            || !matches!(predecessor_row.revision, 1 | 2)
        {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        let source_is_loss: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1
                  FROM runner_connection_loss_epoch AS loss
                  JOIN runner_connection_event AS source
                    ON source.enrollment_id = loss.enrollment_id
                   AND source.connection_epoch = loss.connection_epoch
                   AND source.event_ordinal = loss.connection_event_ordinal
                 WHERE loss.enrollment_id = $1
                   AND loss.loss_epoch = $2
                   AND source.state_kind = 'lost'
            )",
        )
        .bind(predecessor.into_uuid())
        .bind(Decimal::from(predecessor_loss_epoch.get()))
        .fetch_one(&mut **transaction)
        .await?;
        if !source_is_loss {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        let connection = sqlx::query(PROMOTE_PENDING_RUNNER_CONNECTION)
            .bind(candidate.into_uuid())
            .fetch_optional(&mut **transaction)
            .await?;
        let Some(connection) = connection else {
            return Ok(ReplacementTargetAuthority::Unavailable {
                reason: RunnerReplacementTargetUnavailableReason::PendingRequestDisconnected,
                runner: Some(candidate_row.runner),
                predecessor_runner: Some(predecessor_row.runner),
            });
        };
        let state: String = connection.decode_column("state_kind")?;
        if runner_connection_state_from_str(&state)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding)?
            != RunnerConnectionState::Connected
        {
            return Ok(ReplacementTargetAuthority::Unavailable {
                reason: RunnerReplacementTargetUnavailableReason::PendingRequestDisconnected,
                runner: Some(candidate_row.runner),
                predecessor_runner: Some(predecessor_row.runner),
            });
        }
        let connection_epoch = RunnerConnectionEpoch::try_from_u64(decode_u64(
            connection.decode_column("connection_epoch")?,
        )?)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        let connection_event_ordinal =
            decode_u64(connection.decode_column("connection_event_ordinal")?)?;
        let receipt =
            load_enrollment_request_receipt_in(transaction.as_mut(), request, &self.catalog)
                .await?
                .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
        if receipt.authority() != RunnerEnrollmentAuthority::ReplacementPending
            || receipt.enrollment().enrollment() != candidate
            || receipt.enrollment().runner() != candidate_row.runner
        {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        Ok(ReplacementTargetAuthority::Current(Box::new(
            ReplacementTargetEvidence {
                runner: candidate_row.runner,
                enrollment: candidate,
                registration_revision: receipt.registration().revision(),
                connection_epoch,
                connection_event_ordinal,
                registration: receipt.registration().clone(),
                pending_activation: Some(PendingReplacementActivation {
                    request,
                    candidate: candidate_row,
                    predecessor: predecessor_row,
                    predecessor_loss_epoch,
                }),
            },
        )))
    }

    /// Terminalizes one exact lost placement after proving the active-turn slot empty.
    pub async fn abandon_lost_runner(
        &self,
        command: AbandonLostRunner,
    ) -> Result<AbandonLostRunnerOutcome, RunnerProtocolStoreError> {
        if command.command().as_uuid().is_nil() || command.command().as_uuid().is_max() {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let mut transaction = self.pool.begin().await?;
        match inspect_abandonment_registry(&mut transaction, command.command()).await? {
            Some(CommandKind::AbandonLostRunner) => {
                let (recorded, result) =
                    load_abandonment_record(transaction.as_mut(), command.command())
                        .await?
                        .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
                transaction.rollback().await?;
                return Ok(if recorded == command {
                    AbandonLostRunnerOutcome::Recorded(result)
                } else {
                    AbandonLostRunnerOutcome::ConflictingReuse {
                        command: command.command(),
                    }
                });
            }
            Some(_) => {
                transaction.rollback().await?;
                return Ok(AbandonLostRunnerOutcome::ConflictingReuse {
                    command: command.command(),
                });
            }
            None => {}
        }

        let claimed = sqlx::query(
            "INSERT INTO durable_command
                (command_id, command_kind, storage_version, claimed_at)
             VALUES ($1, $2, 1, transaction_timestamp())
             ON CONFLICT DO NOTHING",
        )
        .bind(command.command().into_uuid())
        .bind(ABANDON_LOST_RUNNER_KIND)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;
        if !claimed {
            let outcome = resolve_abandonment_claim_winner(&mut transaction, command).await?;
            transaction.rollback().await?;
            return Ok(outcome);
        }

        let scheduler = sqlx::query(ABANDON_LOST_RUNNER_SCHEDULER)
            .bind(command.session().into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
        let session_exists: bool = scheduler.decode_column("session_exists")?;
        let scheduler_session: Option<Uuid> = scheduler.decode_column("scheduler_session_id")?;
        let mut evidence = AbandonmentRecordEvidence::default();
        let result = if !session_exists {
            AbandonLostRunnerResult::Rejected(AbandonLostRunnerRejection::SessionNotFound {
                session: command.session(),
            })
        } else {
            if scheduler_session != Some(command.session().into_uuid()) {
                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
            }
            let active_turn: Option<Uuid> = sqlx::query_scalar(
                "SELECT turn_id
                   FROM turn_lifecycle
                  WHERE session_id = $1
                    AND state_kind = 'active'
                    AND NOT delegation_runtime_terminal",
            )
            .bind(command.session().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
            let prior = sqlx::query(RUNNER_PLACEMENT_HEAD)
                .bind(command.session().into_uuid())
                .fetch_optional(&mut *transaction)
                .await?;
            match prior {
                None => AbandonLostRunnerResult::Rejected(
                    AbandonLostRunnerRejection::RunnerPlacementNotFound {
                        session: command.session(),
                    },
                ),
                Some(prior) => {
                    let state_kind: String = prior.decode_column("state_kind")?;
                    let current_revision =
                        decode_runner_generation(prior.decode_column("placement_revision")?)?;
                    let current_event_ordinal = decode_u64(prior.decode_column("event_ordinal")?)?;
                    evidence = AbandonmentRecordEvidence {
                        placement_event_ordinal: Some(current_event_ordinal),
                        placement_revision: Some(current_revision),
                        placement_state_kind: Some(state_kind.clone()),
                        active_turn: None,
                    };
                    if current_revision != command.expected_placement_revision() {
                        AbandonLostRunnerResult::Rejected(
                            AbandonLostRunnerRejection::PlacementRevisionMismatch {
                                session: command.session(),
                                expected: command.expected_placement_revision(),
                                current: current_revision,
                            },
                        )
                    } else {
                        let stored = self
                            .decode_stored_placement_in(&mut transaction, &prior)
                            .await?;
                        let (
                            stored_event_ordinal,
                            placement,
                            registration,
                            _grant,
                            interrupted_tool_attempt,
                        ) = stored.into_parts();
                        if stored_event_ordinal != current_event_ordinal {
                            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
                        }
                        let non_lost = match placement.state() {
                            SessionRunnerPlacementState::Unpinned => {
                                Some(RunnerPlacementRecoveryState::Unpinned)
                            }
                            SessionRunnerPlacementState::Pinned(_) => {
                                Some(RunnerPlacementRecoveryState::Pinned)
                            }
                            SessionRunnerPlacementState::RunnerAbandoned(_) => {
                                Some(RunnerPlacementRecoveryState::RunnerAbandoned)
                            }
                            SessionRunnerPlacementState::RunnerLostBeforePin(_)
                            | SessionRunnerPlacementState::RunnerLost(_) => None,
                        };
                        if let Some(state) = non_lost {
                            AbandonLostRunnerResult::Rejected(
                                AbandonLostRunnerRejection::PlacementNotLost {
                                    session: command.session(),
                                    placement_revision: current_revision,
                                    state,
                                },
                            )
                        } else if let Some(active_turn) = active_turn {
                            evidence.active_turn = Some(TurnId::from_uuid(active_turn));
                            AbandonLostRunnerResult::Rejected(
                                AbandonLostRunnerRejection::ActiveTurnRequiresExistingControl {
                                    session: command.session(),
                                    active_turn: TurnId::from_uuid(active_turn),
                                },
                            )
                        } else {
                            if interrupted_tool_attempt.is_some() {
                                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
                            }
                            let abandoned = placement
                                .abandon_lost_runner()
                                .map_err(RunnerProtocolStoreError::Domain)?;
                            let next_event_ordinal = current_event_ordinal
                                .checked_add(1)
                                .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
                            let grant_origin = placement_grant_origin(
                                Some(&prior),
                                next_event_ordinal,
                                &abandoned,
                            )?;
                            insert_placement_record(
                                &mut transaction,
                                next_event_ordinal,
                                "abandoned",
                                &abandoned,
                                PlacementRecordEvidence {
                                    registration_identity: stored_registration_identity(
                                        registration.as_ref(),
                                    ),
                                    grant_origin,
                                    interrupted_tool_attempt: None,
                                    loss_registration_revision: None,
                                },
                            )
                            .await?;
                            let changed = sqlx::query(
                                "UPDATE runner_current_session_placement
                                    SET event_ordinal = $2
                                  WHERE session_id = $1 AND event_ordinal = $3",
                            )
                            .bind(command.session().into_uuid())
                            .bind(Decimal::from(next_event_ordinal))
                            .bind(Decimal::from(current_event_ordinal))
                            .execute(&mut *transaction)
                            .await?
                            .rows_affected();
                            if changed != 1 {
                                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
                            }
                            outbox::append(
                                transaction.as_mut(),
                                OutboxEvent::RunnerStateTransition(RunnerStateOutboxEvent {
                                    session: command.session(),
                                    runner: placement_loss_fence_runner(&abandoned)
                                        .ok_or(RunnerProtocolCorruption::CrossWiredReference)?,
                                    placement_revision: abandoned.revision(),
                                    sandbox: abandoned.request().sandbox,
                                    working_directory: lost_runner_working_directory(&abandoned),
                                    state: DispatchedRunnerState::Abandoned,
                                    source: RunnerStateOutboxSource {
                                        placement_event_ordinal: next_event_ordinal,
                                        connection: None,
                                    },
                                }),
                            )
                            .await?;
                            evidence = AbandonmentRecordEvidence {
                                placement_event_ordinal: Some(next_event_ordinal),
                                placement_revision: Some(abandoned.revision()),
                                placement_state_kind: Some("runner_abandoned".to_owned()),
                                active_turn: None,
                            };
                            AbandonLostRunnerResult::Applied(AbandonedLostRunner::new(
                                command.session(),
                                abandoned.revision(),
                            ))
                        }
                    }
                }
            }
        };
        insert_abandonment_record(transaction.as_mut(), command, result, evidence).await?;
        commit_mutation(transaction).await?;
        Ok(AbandonLostRunnerOutcome::Recorded(result))
    }

    /// Appends one domain-validated placement snapshot and optional grant.
    pub async fn store_placement(
        &self,
        placement: &SessionRunnerPlacement,
        registration: Option<&StoredValidatedRunnerRegistration>,
        grant: Option<&CredentialProfileGrant>,
    ) -> Result<(), RunnerProtocolStoreError> {
        self.store_placement_projection(
            placement,
            registration,
            grant,
            PlacementProjectionAuthority::Generic,
        )
        .await
    }

    /// Stores a checked runner-replacement projection for PostgreSQL integration tests.
    ///
    /// Production replacement must use the dedicated command-authorized transaction;
    /// this test-only surface exists so relational round trips can cover the committed
    /// representation before that transaction is implemented.
    #[cfg(feature = "postgres-integration")]
    #[doc(hidden)]
    pub async fn store_runner_replacement_projection_for_test(
        &self,
        placement: &SessionRunnerPlacement,
        registration: &StoredValidatedRunnerRegistration,
        grant: Option<&CredentialProfileGrant>,
    ) -> Result<(), RunnerProtocolStoreError> {
        self.store_placement_projection(
            placement,
            Some(registration),
            grant,
            PlacementProjectionAuthority::RunnerReplacementTestProjection,
        )
        .await
    }

    async fn store_placement_projection(
        &self,
        placement: &SessionRunnerPlacement,
        registration: Option<&StoredValidatedRunnerRegistration>,
        grant: Option<&CredentialProfileGrant>,
        authority: PlacementProjectionAuthority,
    ) -> Result<(), RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        lock_runner_placement_loss_baseline(&mut transaction, placement).await?;
        let prior = sqlx::query(RUNNER_PLACEMENT_HEAD)
            .bind(placement.session().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        let event_ordinal = prior
            .as_ref()
            .map(|row| decode_u64(row.decode_column("event_ordinal")?))
            .transpose()?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
        let event_kind = classify_placement_event(prior.as_ref(), placement)?;
        // Loss propagation, either replacement, and abandonment each require
        // authority outside the placement aggregate (connection/loss evidence,
        // a durable replacement command, or the scheduler's empty-turn proof).
        // Their relational representations are loadable here, but this generic
        // snapshot writer must not fabricate the multi-aggregate transaction.
        if matches!(
            event_kind,
            "pinned"
                | "runner_lost_before_pin"
                | "pre_pin_replaced"
                | "runner_lost"
                | "runner_replaced"
                | "abandoned"
        ) && !(authority.admits_runner_replacement() && event_kind == "runner_replaced")
        {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let history = prospective_placement_reconstitution_history(
            transaction.as_mut(),
            prior.as_ref(),
            event_kind,
            placement,
        )
        .await?;
        validate_placement_snapshot(placement, registration, grant, history)?;
        // Every replacement event installs successor authority, so the supplied
        // registration must still be the enrollment-owned current revision of
        // an active enrollment at commit time, verified under the enrollment
        // row lock: a replacement prepared before a concurrent revocation or
        // re-registration is rejected rather than committed as stale authority.
        if matches!(event_kind, "runner_replaced" | "profile_replaced") {
            let registration = registration.ok_or(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ))?;
            let enrollment_id = registration.registration().enrollment();
            let locked = sqlx::query(RUNNER_ENROLLMENT)
                .bind(enrollment_id.into_uuid())
                .fetch_optional(&mut *transaction)
                .await?;
            if locked.is_none() {
                return Err(RunnerProtocolStoreError::Corruption(
                    RunnerProtocolCorruption::MissingCanonicalEnrollment,
                ));
            }
            let state: String = sqlx::query_scalar(
                "SELECT state_kind
                   FROM runner_enrollment
                  WHERE enrollment_id = $1",
            )
            .bind(enrollment_id.into_uuid())
            .fetch_one(&mut *transaction)
            .await?;
            if state != "active" {
                return Err(RunnerProtocolStoreError::Domain(
                    RunnerDomainError::EnrollmentRevoked,
                ));
            }
            if event_kind == "runner_replaced" {
                let connection = load_connection_head_in(
                    transaction.as_mut(),
                    registration.registration().enrollment(),
                )
                .await?
                .ok_or(RunnerProtocolStoreError::Domain(
                    RunnerDomainError::InvalidState,
                ))?;
                match connection.state() {
                    RunnerConnectionState::Connected => {}
                    RunnerConnectionState::Suspect
                    | RunnerConnectionState::Shutdown
                    | RunnerConnectionState::Lost => {
                        return Err(RunnerProtocolStoreError::Domain(
                            RunnerDomainError::InvalidState,
                        ));
                    }
                }
            }
            let current: Option<Decimal> = sqlx::query_scalar(RUNNER_REGISTRATION_HEAD)
                .bind(enrollment_id.into_uuid())
                .fetch_optional(&mut *transaction)
                .await?;
            let current = current.ok_or(RunnerProtocolStoreError::Corruption(
                RunnerProtocolCorruption::MissingCanonicalRegistration,
            ))?;
            if decode_registration_revision(current)?.get() != registration.revision().get() {
                return Err(RunnerProtocolStoreError::Domain(
                    RunnerDomainError::RegistrationChanged,
                ));
            }
        }
        let grant_origin = placement_grant_origin(prior.as_ref(), event_ordinal, placement)?;
        // A profile replacement changes only profile axes: the placement
        // record carries the prior pinned registration snapshot forward even
        // though the domain validated the replacement against the
        // enrollment-owned current registration, which may have advanced to
        // an availability-equivalent revision since the pin.
        let registration_identity = if event_kind == "profile_replaced" {
            let prior_row = prior
                .as_ref()
                .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
            let prior_ordinal: Decimal = prior_row.decode_column("event_ordinal")?;
            let snapshot = sqlx::query(
                "SELECT registration_enrollment_id, registration_revision
                   FROM runner_session_placement_record
                  WHERE session_id = $1 AND event_ordinal = $2",
            )
            .bind(placement.session().into_uuid())
            .bind(prior_ordinal)
            .fetch_one(&mut *transaction)
            .await?;
            (
                snapshot.decode_column::<Option<Uuid>>("registration_enrollment_id")?,
                snapshot.decode_column::<Option<Decimal>>("registration_revision")?,
            )
        } else if event_kind == "pre_pin_replaced" {
            // The registration is checked under lock above, but the successor
            // remains unpinned and therefore retains no registration snapshot.
            (None, None)
        } else {
            stored_registration_identity(registration)
        };
        insert_placement_record(
            &mut transaction,
            event_ordinal,
            event_kind,
            placement,
            PlacementRecordEvidence {
                registration_identity,
                grant_origin,
                interrupted_tool_attempt: None,
                loss_registration_revision: None,
            },
        )
        .await?;
        if let (Some(grant), Some(registration)) = (grant, registration) {
            insert_grant_if_new(
                &mut transaction,
                prior.as_ref(),
                event_ordinal,
                placement,
                grant,
                RegistrationAuthority {
                    stored: registration,
                    catalog: &self.catalog,
                },
                grant_origin.ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?,
            )
            .await?;
        }
        sqlx::query(
            "INSERT INTO runner_current_session_placement
                (session_id, event_ordinal)
             VALUES ($1, $2)
             ON CONFLICT (session_id)
             DO UPDATE SET event_ordinal = EXCLUDED.event_ordinal",
        )
        .bind(placement.session().into_uuid())
        .bind(Decimal::from(event_ordinal))
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        Ok(())
    }
    /// Atomically stores the first pinned placement, grant, and offered lease.
    pub async fn store_pin(
        &self,
        pin: &SessionRunnerPin,
        registration: &StoredValidatedRunnerRegistration,
    ) -> Result<(), RunnerProtocolStoreError> {
        validate_placement_snapshot(
            &pin.placement,
            Some(registration),
            pin.grant.as_ref(),
            RunnerPlacementReconstitutionHistory::Initial,
        )?;
        if pin.lease.state() != RunnerLeaseState::Offered {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let mut transaction = self.pool.begin().await?;
        lock_runner_placement_loss_baseline(&mut transaction, &pin.placement).await?;
        let enrollment = registration.registration().enrollment();
        let locked = sqlx::query(RUNNER_ENROLLMENT)
            .bind(enrollment.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        if locked.is_none() {
            return Err(RunnerProtocolCorruption::MissingCanonicalEnrollment.into());
        }
        let enrollment_state: String = sqlx::query_scalar(
            "SELECT state_kind
               FROM runner_enrollment
              WHERE enrollment_id = $1",
        )
        .bind(enrollment.into_uuid())
        .fetch_one(&mut *transaction)
        .await?;
        match runner_enrollment_state_from_str(&enrollment_state)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding)?
        {
            RunnerEnrollmentState::Active => {}
            RunnerEnrollmentState::Pending => {
                return Err(RunnerProtocolStoreError::Domain(
                    RunnerDomainError::InvalidState,
                ));
            }
            RunnerEnrollmentState::Revoked => {
                return Err(RunnerProtocolStoreError::Domain(
                    RunnerDomainError::EnrollmentRevoked,
                ));
            }
        }
        match load_connection_head_in(transaction.as_mut(), enrollment).await? {
            None
            | Some(RunnerConnectionSnapshot {
                state: RunnerConnectionState::Connected,
                ..
            }) => {}
            Some(RunnerConnectionSnapshot {
                state:
                    RunnerConnectionState::Suspect
                    | RunnerConnectionState::Shutdown
                    | RunnerConnectionState::Lost,
                ..
            }) => {
                return Err(RunnerProtocolStoreError::Domain(
                    RunnerDomainError::InvalidState,
                ));
            }
        }
        let prior = sqlx::query(RUNNER_PLACEMENT_HEAD)
            .bind(pin.placement.session().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        let event_ordinal = prior
            .as_ref()
            .map(|row| decode_u64(row.decode_column("event_ordinal")?))
            .transpose()?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
        let event_kind = classify_placement_event(prior.as_ref(), &pin.placement)?;
        if event_kind != "pinned" {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let grant_origin = placement_grant_origin(prior.as_ref(), event_ordinal, &pin.placement)?;
        insert_placement_record(
            &mut transaction,
            event_ordinal,
            event_kind,
            &pin.placement,
            PlacementRecordEvidence {
                registration_identity: stored_registration_identity(Some(registration)),
                grant_origin,
                interrupted_tool_attempt: None,
                loss_registration_revision: None,
            },
        )
        .await?;
        if let Some(grant) = pin.grant.as_ref() {
            insert_grant_if_new(
                &mut transaction,
                prior.as_ref(),
                event_ordinal,
                &pin.placement,
                grant,
                RegistrationAuthority {
                    stored: registration,
                    catalog: &self.catalog,
                },
                grant_origin.ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?,
            )
            .await?;
        }
        sqlx::query(
            "INSERT INTO runner_current_session_placement
                (session_id, event_ordinal)
             VALUES ($1, $2)
             ON CONFLICT (session_id)
             DO UPDATE SET event_ordinal = EXCLUDED.event_ordinal",
        )
        .bind(pin.placement.session().into_uuid())
        .bind(Decimal::from(event_ordinal))
        .execute(&mut *transaction)
        .await?;
        insert_lease_generation(&mut transaction, &pin.lease).await?;
        let correlation = pin.lease.correlation();
        sqlx::query(
            "INSERT INTO runner_lease_event
                (lease_id, generation, event_ordinal, state_kind)
             VALUES ($1, $2, 1, 'offered')",
        )
        .bind(correlation.lease.into_uuid())
        .bind(Decimal::from(correlation.generation.get()))
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO runner_current_lease_event
                (lease_id, generation, event_ordinal)
             VALUES ($1, $2, 1)",
        )
        .bind(correlation.lease.into_uuid())
        .bind(Decimal::from(correlation.generation.get()))
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await
    }

    /// Loads and reconstitutes the current placement and selected grant.
    pub async fn load_placement(
        &self,
        session: SessionId,
    ) -> Result<Option<StoredSessionRunnerPlacement>, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let row = sqlx::query(
            "SELECT record.*,
                    (
                        SELECT max(history.event_ordinal)
                          FROM runner_session_placement_record AS history
                         WHERE history.session_id = record.session_id
                    ) AS maximum_event_ordinal
               FROM runner_current_session_placement AS current_placement
               JOIN runner_session_placement_record AS record
                 ON record.session_id = current_placement.session_id
                AND record.event_ordinal = current_placement.event_ordinal
              WHERE current_placement.session_id = $1",
        )
        .bind(session.into_uuid())
        .fetch_optional(transaction.as_mut())
        .await?;
        let Some(row) = row else {
            transaction.commit().await?;
            return Ok(None);
        };
        let event_ordinal = decode_u64(row.decode_column("event_ordinal")?)?;
        let maximum_event_ordinal = decode_u64(row.decode_column("maximum_event_ordinal")?)?;
        if event_ordinal != maximum_event_ordinal {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        let grant_policies = load_grant_policy_index(transaction.as_mut(), &row).await?;
        let registration =
            load_placement_registration(transaction.as_mut(), &row, &self.catalog).await?;
        let grant = if registration.is_some() {
            load_grant_for_placement(transaction.as_mut(), &row, &self.catalog, &grant_policies)
                .await?
        } else {
            None
        };
        let pinned_profile =
            row.decode_column::<Option<String>>("pinned_credential_profile_name")?;
        let profileless_tombstone = grant
            .as_ref()
            .filter(|grant| credential_grant_is_revoked(grant.state()) && pinned_profile.is_none());
        let placement = decode_placement(
            transaction.as_mut(),
            &row,
            &self.catalog,
            &grant_policies,
            registration
                .as_ref()
                .map(StoredValidatedRunnerRegistration::registration),
            profileless_tombstone,
        )
        .await?;
        let interrupted_tool_attempt = row
            .decode_column::<Option<Uuid>>("interrupted_tool_attempt_id")?
            .map(tool_attempt_id);
        let event_kind: String = row.decode_column("event_kind")?;
        if interrupted_tool_attempt.is_some() && event_kind != "runner_lost" {
            return Err(RunnerProtocolCorruption::InvalidEncoding.into());
        }
        transaction.commit().await?;
        Ok(Some(StoredSessionRunnerPlacement {
            event_ordinal,
            placement,
            registration,
            grant,
            interrupted_tool_attempt,
        }))
    }

    async fn decode_stored_placement_in(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        row: &PgRow,
    ) -> Result<StoredSessionRunnerPlacement, RunnerProtocolStoreError> {
        let event_ordinal = decode_u64(row.decode_column("event_ordinal")?)?;
        let session = session_id(row.decode_column("session_id")?);
        let maximum_event_ordinal: Option<Decimal> = sqlx::query_scalar(
            "SELECT max(event_ordinal)
               FROM runner_session_placement_record
              WHERE session_id = $1",
        )
        .bind(session.into_uuid())
        .fetch_one(&mut **transaction)
        .await?;
        if maximum_event_ordinal.map(decode_u64).transpose()? != Some(event_ordinal) {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        let grant_policies = load_grant_policy_index(transaction.as_mut(), row).await?;
        let registration =
            load_placement_registration(transaction.as_mut(), row, &self.catalog).await?;
        let grant = if registration.is_some() {
            load_grant_for_placement(transaction.as_mut(), row, &self.catalog, &grant_policies)
                .await?
        } else {
            None
        };
        let pinned_profile =
            row.decode_column::<Option<String>>("pinned_credential_profile_name")?;
        let profileless_tombstone = grant
            .as_ref()
            .filter(|grant| credential_grant_is_revoked(grant.state()) && pinned_profile.is_none());
        let placement = decode_placement(
            transaction.as_mut(),
            row,
            &self.catalog,
            &grant_policies,
            registration
                .as_ref()
                .map(StoredValidatedRunnerRegistration::registration),
            profileless_tombstone,
        )
        .await?;
        let interrupted_tool_attempt = row
            .decode_column::<Option<Uuid>>("interrupted_tool_attempt_id")?
            .map(tool_attempt_id);
        Ok(StoredSessionRunnerPlacement {
            event_ordinal,
            placement,
            registration,
            grant,
            interrupted_tool_attempt,
        })
    }

    /// Loads the exact authenticated runner-recovery wait for one session.
    pub async fn load_runner_recovery_wait(
        &self,
        session: SessionId,
    ) -> Result<Option<StoredRunnerRecoveryWait>, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let row = sqlx::query(
            "SELECT turn.turn_id, turn.runner_recovery_runner_id,
                    turn.runner_recovery_placement_revision,
                    turn.runner_recovery_tool_attempt_id,
                    placement.state_kind AS placement_state_kind,
                    placement.lost_runner_id,
                    placement.placement_revision,
                    placement.interrupted_tool_attempt_id,
                    attempt.turn_id AS interrupted_turn_id,
                    attempt.session_id AS interrupted_session_id
               FROM turn_lifecycle AS turn
               JOIN runner_current_session_placement AS current_placement
                 ON current_placement.session_id = turn.session_id
               JOIN runner_session_placement_record AS placement
                 ON placement.session_id = current_placement.session_id
                AND placement.event_ordinal = current_placement.event_ordinal
               LEFT JOIN tool_attempt AS attempt
                 ON attempt.attempt_id = turn.runner_recovery_tool_attempt_id
              WHERE turn.session_id = $1
                AND turn.state_kind = 'active'
                AND NOT turn.delegation_runtime_terminal
                AND turn.active_phase_kind = 'awaiting_runner_recovery'",
        )
        .bind(session.into_uuid())
        .fetch_optional(transaction.as_mut())
        .await?;
        let Some(row) = row else {
            transaction.commit().await?;
            return Ok(None);
        };
        let turn = TurnId::from_uuid(row.decode_column("turn_id")?);
        let runner = runner_id(row.decode_column("runner_recovery_runner_id")?);
        let placement_revision =
            decode_generation(row.decode_column("runner_recovery_placement_revision")?)?;
        let interrupted_tool_attempt = row
            .decode_column::<Option<Uuid>>("runner_recovery_tool_attempt_id")?
            .map(tool_attempt_id);
        let placement_state: String = row.decode_column("placement_state_kind")?;
        let stored_lost_runner = row
            .decode_column::<Option<Uuid>>("lost_runner_id")?
            .map(runner_id);
        let stored_revision = decode_generation(row.decode_column("placement_revision")?)?;
        let stored_interrupted_attempt = row
            .decode_column::<Option<Uuid>>("interrupted_tool_attempt_id")?
            .map(tool_attempt_id);
        let interrupted_turn = row
            .decode_column::<Option<Uuid>>("interrupted_turn_id")?
            .map(TurnId::from_uuid);
        let interrupted_session = row
            .decode_column::<Option<Uuid>>("interrupted_session_id")?
            .map(session_id);
        if !matches!(
            placement_state.as_str(),
            "runner_lost" | "runner_lost_before_pin"
        ) || stored_lost_runner != Some(runner)
            || stored_revision != placement_revision
            || stored_interrupted_attempt != interrupted_tool_attempt
            || interrupted_tool_attempt.is_some()
                != (interrupted_turn == Some(turn) && interrupted_session == Some(session))
        {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        transaction.commit().await?;
        Ok(Some(StoredRunnerRecoveryWait {
            turn,
            runner,
            placement_revision,
            interrupted_tool_attempt,
        }))
    }

    /// Returns the authenticated policy event for the current grant in
    /// PostgreSQL integration tests.
    #[cfg(feature = "postgres-integration")]
    pub async fn load_current_grant_policy_event_for_test(
        &self,
        session: SessionId,
    ) -> Result<Option<Decimal>, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let row = sqlx::query(
            "SELECT record.*
               FROM runner_current_session_placement AS current_placement
               JOIN runner_session_placement_record AS record
                 ON record.session_id = current_placement.session_id
                AND record.event_ordinal = current_placement.event_ordinal
              WHERE current_placement.session_id = $1",
        )
        .bind(session.into_uuid())
        .fetch_optional(transaction.as_mut())
        .await?;
        let Some(row) = row else {
            transaction.commit().await?;
            return Ok(None);
        };
        let policies = load_grant_policy_index(transaction.as_mut(), &row).await?;
        let policy_event = decode_stored_grant_identity(&row)?
            .map(|_| policies.policy_event_for(&row))
            .transpose()?;
        transaction.commit().await?;
        Ok(policy_event)
    }

    /// Appends the terminal revocation audit event for one current grant.
    pub async fn revoke_grant(
        &self,
        session: SessionId,
        runner: RunnerId,
        revision: RunnerGeneration,
    ) -> Result<Option<CredentialProfileGrant>, RunnerProtocolStoreError> {
        let placement = self
            .load_placement(session)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
        let Some(grant) = placement.grant else {
            return Ok(None);
        };
        if grant.runner() != runner || grant.revision() != revision {
            return Ok(None);
        }
        let revoked = grant.revoke().map_err(RunnerProtocolStoreError::Domain)?;
        let mut transaction = self.pool.begin().await?;
        let locked_placement = sqlx::query(RUNNER_PLACEMENT_HEAD)
            .bind(session.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        let Some(locked_placement) = locked_placement else {
            transaction.rollback().await?;
            return Ok(None);
        };
        let locked_origin = locked_placement
            .decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?;
        let locked_runner =
            locked_placement.decode_column::<Option<Uuid>>("credential_grant_runner_id")?;
        let locked_revision =
            locked_placement.decode_column::<Option<Decimal>>("credential_grant_revision")?;
        let locked_profile =
            locked_placement.decode_column::<Option<String>>("pinned_credential_profile_name")?;
        if locked_runner != Some(runner.into_uuid())
            || locked_revision != Some(Decimal::from(revision.get()))
            || locked_profile.as_deref() != Some(revoked.profile().as_str())
        {
            transaction.rollback().await?;
            return Ok(None);
        }
        let Some(locked_origin) = locked_origin else {
            return Err(RunnerProtocolCorruption::MissingCanonicalGrant.into());
        };
        let locked: Option<String> = sqlx::query_scalar(RUNNER_GRANT)
            .bind(session.into_uuid())
            .bind(locked_origin)
            .bind(runner.into_uuid())
            .bind(Decimal::from(revision.get()))
            .fetch_optional(&mut *transaction)
            .await?;
        let Some(locked_profile) = locked else {
            transaction.rollback().await?;
            return Ok(None);
        };
        if locked_profile != revoked.profile().as_str() {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        let already_revoked: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1
                   FROM runner_credential_grant_audit
                  WHERE session_id = $1
                    AND lineage_origin_event_ordinal = $2
                    AND runner_id = $3
                    AND grant_revision = $4
                    AND event_kind = 'revoked'
             )",
        )
        .bind(session.into_uuid())
        .bind(locked_origin)
        .bind(runner.into_uuid())
        .bind(Decimal::from(revision.get()))
        .fetch_one(&mut *transaction)
        .await?;
        if already_revoked {
            transaction.rollback().await?;
            return Ok(None);
        }
        sqlx::query(
            "INSERT INTO runner_credential_grant_audit
                (session_id, lineage_origin_event_ordinal,
                 runner_id, grant_revision, audit_ordinal,
                 event_kind, credential_profile_name)
             VALUES ($1, $2, $3, $4, 2, 'revoked', $5)",
        )
        .bind(session.into_uuid())
        .bind(locked_origin)
        .bind(runner.into_uuid())
        .bind(Decimal::from(revision.get()))
        .bind(revoked.profile().as_str())
        .execute(&mut *transaction)
        .await?;
        commit_mutation(transaction).await?;
        Ok(Some(revoked))
    }

    /// Appends an offered lease generation or a later non-unclaimed state
    /// event. A claimed-retry successor lease is persisted only through
    /// [`Self::store_claimed_retry_replacement`], which commits it atomically
    /// with the fresh replacement attempt the schema requires.
    pub async fn store_lease(&self, lease: &RunnerLease) -> Result<(), RunnerProtocolStoreError> {
        if matches!(
            lease.state(),
            RunnerLeaseState::Claimed
                | RunnerLeaseState::Completed
                | RunnerLeaseState::LostUnclaimed
        ) {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        self.store_lease_without_proof(lease).await
    }

    /// Stores a checked claimed-lease projection for PostgreSQL integration tests.
    #[cfg(feature = "postgres-integration")]
    #[doc(hidden)]
    pub async fn store_claimed_lease_projection_for_test(
        &self,
        lease: &RunnerLease,
    ) -> Result<(), RunnerProtocolStoreError> {
        if lease.state() != RunnerLeaseState::Claimed {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        self.store_lease_without_proof(lease).await
    }

    /// Stores a checked completed-lease projection for PostgreSQL integration tests.
    #[cfg(feature = "postgres-integration")]
    #[doc(hidden)]
    pub async fn store_completed_lease_projection_for_test(
        &self,
        lease: &RunnerLease,
    ) -> Result<(), RunnerProtocolStoreError> {
        if lease.state() != RunnerLeaseState::Completed {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        self.store_lease_without_proof(lease).await
    }

    /// Atomically commits one exact offered-lease claim before acknowledgement.
    pub async fn claim_lease(
        &self,
        request: RunnerLeaseClaimRequest,
    ) -> Result<RunnerLease, RunnerProtocolStoreError> {
        let correlation = request.into_correlation();
        let mut transaction = self.pool.begin().await?;
        lock_runner_session_scheduler(&mut transaction, correlation.dispatch.session()).await?;
        let enrollment: Uuid = sqlx::query_scalar(
            "SELECT registration_enrollment_id
               FROM runner_lease_generation
              WHERE lease_id = $1 AND generation = $2",
        )
        .bind(correlation.lease.into_uuid())
        .bind(Decimal::from(correlation.generation.get()))
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(RunnerProtocolStoreError::Domain(
            RunnerDomainError::InvalidState,
        ))?;
        let enrollment_state: String = sqlx::query_scalar(RUNNER_LEASE_ENROLLMENT_AUTHORITY)
            .bind(enrollment)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        if enrollment_state != "active" {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        lock_runner_lease_claim_connection_authority(&mut transaction, &correlation).await?;
        let current_registration: Option<Decimal> = sqlx::query_scalar(RUNNER_REGISTRATION_HEAD)
            .bind(enrollment)
            .fetch_optional(&mut *transaction)
            .await?;
        if current_registration.is_none() {
            return Err(RunnerProtocolCorruption::MissingCanonicalRegistration.into());
        }
        let offered = self
            .load_lease_in(&mut transaction, correlation.lease, correlation.generation)
            .await?
            .ok_or(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ))?;
        let claimed = offered
            .claim(correlation)
            .map_err(RunnerProtocolStoreError::Domain)?;
        append_lease_event_in(&mut transaction, &claimed).await?;
        commit_mutation(transaction).await?;
        Ok(claimed)
    }

    /// Atomically commits one exact runner result before acknowledgement.
    pub async fn commit_lease_result(
        &self,
        request: RunnerLeaseResultRequest,
    ) -> Result<RunnerLeaseCompletion, RunnerProtocolStoreError> {
        let (correlation, observation) = request.into_parts();
        let mut transaction = self.pool.begin().await?;
        crate::tool_loop::lock_tool_session(transaction.as_mut(), correlation.dispatch.session())
            .await
            .map_err(map_runner_tool_loop_error)?;
        let (completion, effect) = self
            .commit_lease_result_in(&mut transaction, correlation, observation)
            .await?;
        match effect {
            LeaseResultCommitEffect::Applied => commit_mutation(transaction).await?,
            LeaseResultCommitEffect::Replay => transaction.rollback().await?,
        }
        Ok(completion)
    }

    /// Authenticates one resume identity and commits its retained terminal
    /// result before registration reconciliation may consume the attempt.
    pub async fn commit_retained_result_before_resume(
        &self,
        request: RunnerEnrollmentRequestId,
        observed: IssuedRunnerEnrollmentIdentities,
        prior_revision: RunnerRegistrationRevision,
        advertisement: RunnerAdvertisement,
        result: RunnerLeaseResultRequest,
    ) -> Result<RunnerLeaseCompletion, RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        crate::tool_loop::lock_tool_session(
            transaction.as_mut(),
            result.correlation().dispatch.session(),
        )
        .await
        .map_err(map_runner_tool_loop_error)?;
        let receipt = self
            .authenticate_resume_in(
                &mut transaction,
                request,
                observed,
                prior_revision,
                advertisement,
            )
            .await?;
        if result.correlation().runner != receipt.enrollment().runner() {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::CorrelationMismatch,
            ));
        }
        let (correlation, observation) = result.into_parts();
        let (completion, effect) = self
            .commit_lease_result_in(&mut transaction, correlation, observation)
            .await?;
        match effect {
            LeaseResultCommitEffect::Applied => commit_mutation(transaction).await?,
            LeaseResultCommitEffect::Replay => transaction.rollback().await?,
        }
        Ok(completion)
    }

    /// Reads one exact claimed lease under authenticated resume facts.
    ///
    /// This repeatable-read snapshot is reconstitution evidence only. A later
    /// resume transaction must recheck it before projecting recovery frames.
    pub async fn load_claimed_lease_for_authenticated_resume(
        &self,
        request: RunnerEnrollmentRequestId,
        observed: IssuedRunnerEnrollmentIdentities,
        prior_revision: RunnerRegistrationRevision,
        advertisement: RunnerAdvertisement,
        correlation: RunnerLeaseCorrelation,
    ) -> Result<RunnerLease, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        crate::tool_loop::lock_tool_session(transaction.as_mut(), correlation.dispatch.session())
            .await
            .map_err(map_runner_tool_loop_error)?;
        let receipt = self
            .authenticate_resume_in(
                &mut transaction,
                request,
                observed,
                prior_revision,
                advertisement,
            )
            .await?;
        if correlation.runner != receipt.enrollment().runner()
            || correlation.registration_revision.get() != receipt.registration().revision().get()
        {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::CorrelationMismatch,
            ));
        }
        let lease = self
            .load_lease_in(&mut transaction, correlation.lease, correlation.generation)
            .await?
            .ok_or(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ))?;
        if lease.state() != RunnerLeaseState::Claimed {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        if lease.correlation() != correlation {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::CorrelationMismatch,
            ));
        }
        transaction.commit().await?;
        Ok(lease)
    }

    async fn authenticate_resume_in(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        request: RunnerEnrollmentRequestId,
        observed: IssuedRunnerEnrollmentIdentities,
        prior_revision: RunnerRegistrationRevision,
        advertisement: RunnerAdvertisement,
    ) -> Result<RunnerEnrollmentReceipt, RunnerProtocolStoreError> {
        let stored = load_enrollment_request_facts(transaction.as_mut(), request)
            .await?
            .ok_or(RunnerEnrollmentRequestFailure::UnknownRequest { request })?;
        let locked = sqlx::query(RUNNER_ENROLLMENT)
            .bind(stored.identities.enrollment().into_uuid())
            .fetch_optional(transaction.as_mut())
            .await?;
        if locked.is_none() {
            return Err(RunnerProtocolCorruption::MissingCanonicalEnrollment.into());
        }
        if stored.identities != observed {
            return Err(RunnerEnrollmentRequestFailure::ResumeIdentityMismatch {
                request,
                expected: stored.identities,
                observed,
            }
            .into());
        }
        let enrollment = load_enrollment_in(transaction.as_mut(), stored.identities.enrollment())
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        if enrollment.state() == RunnerEnrollmentState::Revoked {
            return Err(RunnerEnrollmentRequestFailure::EnrollmentRevoked {
                request,
                enrollment: enrollment.enrollment(),
            }
            .into());
        }
        let current: Option<Decimal> = sqlx::query_scalar(RUNNER_REGISTRATION_HEAD)
            .bind(enrollment.enrollment().into_uuid())
            .fetch_optional(transaction.as_mut())
            .await?;
        let current = current.ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        let current = decode_registration_revision(current)?;
        let registration = load_registration_in(
            transaction.as_mut(),
            enrollment.enrollment(),
            current,
            Some(&enrollment),
            &self.catalog,
        )
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        let authority = match enrollment.state() {
            RunnerEnrollmentState::Pending => RunnerEnrollmentAuthority::ReplacementPending,
            RunnerEnrollmentState::Active | RunnerEnrollmentState::Revoked => {
                RunnerEnrollmentAuthority::Active
            }
        };
        let receipt = RunnerEnrollmentReceipt {
            request,
            authority,
            enrollment,
            registration,
        };
        match prior_revision.cmp(&current) {
            Ordering::Less if receipt.advertisement() != advertisement => {
                return Err(RunnerEnrollmentRequestFailure::StaleResumeAdvertisement {
                    request,
                    prior: prior_revision,
                    current,
                }
                .into());
            }
            Ordering::Greater => {
                return Err(RunnerEnrollmentRequestFailure::ResumeRevisionMismatch {
                    request,
                    expected: current,
                    observed: prior_revision,
                }
                .into());
            }
            Ordering::Less | Ordering::Equal => {}
        }
        Ok(receipt)
    }

    async fn commit_lease_result_in(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        correlation: RunnerLeaseCorrelation,
        observation: signalbox_domain::ToolAttemptObservation,
    ) -> Result<(RunnerLeaseCompletion, LeaseResultCommitEffect), RunnerProtocolStoreError> {
        let lease_head = sqlx::query(RUNNER_LEASE_HEAD)
            .bind(correlation.lease.into_uuid())
            .bind(Decimal::from(correlation.generation.get()))
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ))?;
        let state: String = lease_head.decode_column("state_kind")?;
        let lease = self
            .load_lease_in(transaction, correlation.lease, correlation.generation)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalLoss)?;
        if state == "completed" {
            let attempt_id = correlation.dispatch.attempt();
            let mut attempts =
                crate::tool_loop::load_attempts_by_id(transaction.as_mut(), &[attempt_id])
                    .await
                    .map_err(map_runner_tool_loop_error)?;
            let attempt = attempts
                .remove(&attempt_id)
                .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
            let signalbox_domain::ReconstitutedToolAttempt::Ended(attempt) = attempt else {
                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
            };
            let completion = lease
                .verify_completed_observation(attempt, correlation, observation)
                .map_err(RunnerProtocolStoreError::Domain)?;
            return Ok((completion, LeaseResultCommitEffect::Replay));
        }
        if state != "claimed" {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let batch = crate::tool_loop::load_active_batch_from_connection(
            transaction.as_mut(),
            correlation.dispatch.session(),
            correlation.dispatch.turn(),
        )
        .await
        .map_err(map_runner_tool_loop_error)?
        .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
        let authorization = batch
            .resume_runner_result_attempt(correlation.dispatch.attempt())
            .map_err(|_| RunnerProtocolStoreError::Domain(RunnerDomainError::InvalidState))?;
        let completion = lease
            .complete_with_observation(authorization, correlation, observation)
            .map_err(RunnerProtocolStoreError::Domain)?;
        crate::tool_loop::persist_ended_attempt(transaction.as_mut(), completion.attempt())
            .await
            .map_err(map_runner_tool_loop_error)?;
        if completion.attempt().end() == &ToolAttemptEnd::Ambiguous {
            crate::tool_loop::persist_ambiguous_tool_recovery_wait(
                transaction.as_mut(),
                completion.attempt(),
            )
            .await
            .map_err(map_runner_tool_loop_error)?;
        }
        append_lease_event_in(transaction, completion.lease()).await?;
        Ok((completion, LeaseResultCommitEffect::Applied))
    }

    /// Atomically pins one workspace-free exact-directory placement, marks its
    /// prepared attempt in flight, and stores the first offered lease.
    pub async fn authorize_initial_dispatch(
        &self,
        request: InitialRunnerDispatchRequest,
        lease: RunnerLeaseId,
    ) -> Result<PinnedRunnerLeaseOffer, RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        crate::tool_loop::lock_tool_session(transaction.as_mut(), request.session())
            .await
            .map_err(map_runner_tool_loop_error)?;
        let requested_registration =
            RunnerRegistrationRevision::try_from_u64(request.registration_revision().get())
                .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        let enrollment_id = sqlx::query_scalar::<_, Uuid>(
            "SELECT enrollment_id
               FROM runner_registration
              WHERE runner_id = $1
                AND registration_revision = $2",
        )
        .bind(request.runner().into_uuid())
        .bind(Decimal::from(requested_registration.get()))
        .fetch_optional(&mut *transaction)
        .await?
        .map(RunnerEnrollmentId::from_uuid)
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        let enrollment_locked = sqlx::query(RUNNER_ENROLLMENT)
            .bind(enrollment_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        if enrollment_locked.is_none() {
            return Err(RunnerProtocolCorruption::MissingCanonicalEnrollment.into());
        }
        let enrollment = load_enrollment_in(transaction.as_mut(), enrollment_id)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        if enrollment.state() != RunnerEnrollmentState::Active {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let connection_authority =
            sqlx::query_scalar::<_, Decimal>(RUNNER_PLACEMENT_CONNECTION_AUTHORITY)
                .bind(enrollment_id.into_uuid())
                .fetch_optional(&mut *transaction)
                .await?;
        if connection_authority.is_none() {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        sqlx::query_scalar::<_, Decimal>(RUNNER_PLACEMENT_CURRENT_LOSS)
            .bind(enrollment_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        let connection = load_connection_head_in(transaction.as_mut(), enrollment_id)
            .await?
            .ok_or(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ))?;
        if connection.state() != RunnerConnectionState::Connected {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let current_registration: Decimal = sqlx::query_scalar(RUNNER_REGISTRATION_HEAD)
            .bind(enrollment_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        if decode_registration_revision(current_registration)? != requested_registration {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::RegistrationChanged,
            ));
        }
        let registration = load_registration_in(
            transaction.as_mut(),
            enrollment_id,
            requested_registration,
            Some(&enrollment),
            &self.catalog,
        )
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        let placement_row = sqlx::query(RUNNER_PLACEMENT_HEAD)
            .bind(request.session().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
        let stored_placement = self
            .decode_stored_placement_in(&mut transaction, &placement_row)
            .await?;
        let (_, placement, prior_registration, grant, interrupted) = stored_placement.into_parts();
        if prior_registration.is_some()
            || grant.is_some()
            || interrupted.is_some()
            || placement.state() != &SessionRunnerPlacementState::Unpinned
            || placement.request().workspace != WorkspaceRequirement::None
        {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let WorkingDirectorySelection::Exact(directory) =
            placement.request().working_directory.clone()
        else {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        };
        let authorization = crate::tool_loop::authorize_runner_attempt_from_connection(
            transaction.as_mut(),
            request.session(),
            request.turn(),
            request.attempt(),
        )
        .await
        .map_err(map_runner_tool_loop_error)?;
        let tool = authorization.tool().clone();
        let pin = placement
            .pin_and_offer_lease(
                &enrollment,
                registration.registration(),
                directory,
                None,
                authorization,
                RunnerLeaseOfferRequest { lease, tool },
            )
            .map_err(RunnerProtocolStoreError::Domain)?;
        insert_initial_pin_rows(
            &mut transaction,
            &placement_row,
            &pin,
            &registration,
            &self.catalog,
        )
        .await?;
        commit_mutation(transaction).await?;
        Ok(PinnedRunnerLeaseOffer::new(enrollment_id, pin.lease))
    }

    /// Atomically authorizes one prepared tool attempt and stores its offered
    /// lease against an existing pinned placement.
    pub async fn authorize_pinned_dispatch(
        &self,
        request: PinnedRunnerDispatchRequest,
        lease: RunnerLeaseId,
    ) -> Result<PinnedRunnerLeaseOffer, RunnerProtocolStoreError> {
        let mut transaction = self.pool.begin().await?;
        crate::tool_loop::lock_tool_session(transaction.as_mut(), request.session())
            .await
            .map_err(map_runner_tool_loop_error)?;
        let requested_registration =
            RunnerRegistrationRevision::try_from_u64(request.registration_revision().get())
                .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        let enrollment_id = sqlx::query_scalar::<_, Uuid>(
            "SELECT enrollment_id
               FROM runner_registration
              WHERE runner_id = $1
                AND registration_revision = $2",
        )
        .bind(request.runner().into_uuid())
        .bind(Decimal::from(requested_registration.get()))
        .fetch_optional(&mut *transaction)
        .await?
        .map(RunnerEnrollmentId::from_uuid)
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        let enrollment_locked = sqlx::query(RUNNER_ENROLLMENT)
            .bind(enrollment_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        if enrollment_locked.is_none() {
            return Err(RunnerProtocolCorruption::MissingCanonicalEnrollment.into());
        }
        let enrollment = load_enrollment_in(transaction.as_mut(), enrollment_id)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
        match enrollment.state() {
            RunnerEnrollmentState::Active => {}
            RunnerEnrollmentState::Pending => {
                return Err(RunnerProtocolStoreError::Domain(
                    RunnerDomainError::InvalidState,
                ));
            }
            RunnerEnrollmentState::Revoked => {
                return Err(RunnerProtocolStoreError::Domain(
                    RunnerDomainError::EnrollmentRevoked,
                ));
            }
        }
        let connection_authority =
            sqlx::query_scalar::<_, Decimal>(RUNNER_PLACEMENT_CONNECTION_AUTHORITY)
                .bind(enrollment_id.into_uuid())
                .fetch_optional(&mut *transaction)
                .await?;
        if connection_authority.is_none() {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        sqlx::query_scalar::<_, Decimal>(RUNNER_PLACEMENT_CURRENT_LOSS)
            .bind(enrollment_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?;
        let connection = load_connection_head_in(transaction.as_mut(), enrollment_id)
            .await?
            .ok_or(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ))?;
        if connection.state() != RunnerConnectionState::Connected {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let current_registration: Decimal = sqlx::query_scalar(RUNNER_REGISTRATION_HEAD)
            .bind(enrollment_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        if decode_registration_revision(current_registration)? != requested_registration {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::RegistrationChanged,
            ));
        }
        let registration = load_registration_in(
            transaction.as_mut(),
            enrollment_id,
            requested_registration,
            Some(&enrollment),
            &self.catalog,
        )
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        let placement_row = sqlx::query(RUNNER_PLACEMENT_HEAD)
            .bind(request.session().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
        let stored_placement = self
            .decode_stored_placement_in(&mut transaction, &placement_row)
            .await?;
        let (_, placement, _, grant, _) = stored_placement.into_parts();
        if pinned_placement(placement.state()).is_none() {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        if let Some(grant) = grant.as_ref() {
            let origin = placement_row
                .decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
            let locked_profile: Option<String> = sqlx::query_scalar(RUNNER_LEASE_GRANT_AUTHORITY)
                .bind(grant.session().into_uuid())
                .bind(origin)
                .bind(grant.runner().into_uuid())
                .bind(Decimal::from(grant.revision().get()))
                .fetch_optional(&mut *transaction)
                .await?;
            if locked_profile.as_deref() != Some(grant.profile().as_str()) {
                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
            }
        }
        let authorization = crate::tool_loop::authorize_runner_attempt_from_connection(
            transaction.as_mut(),
            request.session(),
            request.turn(),
            request.attempt(),
        )
        .await
        .map_err(map_runner_tool_loop_error)?;
        let tool = authorization.tool().clone();
        let offered = placement
            .offer_lease(
                &enrollment,
                registration.registration(),
                grant.as_ref(),
                authorization,
                RunnerLeaseOfferRequest { lease, tool },
            )
            .map_err(RunnerProtocolStoreError::Domain)?;
        append_lease_event_in(&mut transaction, &offered).await?;
        commit_mutation(transaction).await?;
        Ok(PinnedRunnerLeaseOffer::new(enrollment_id, offered))
    }

    async fn load_current_loss_lease_in(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        session: SessionId,
        placement_event_ordinal: u64,
    ) -> Result<Option<RunnerLease>, RunnerProtocolStoreError> {
        let candidates = sqlx::query(
            "SELECT generation.lease_id, generation.generation
               FROM runner_lease_generation AS generation
               JOIN runner_current_lease_event AS current_event
                 ON current_event.lease_id = generation.lease_id
                AND current_event.generation = generation.generation
               JOIN runner_lease_event AS event
                 ON event.lease_id = current_event.lease_id
                AND event.generation = current_event.generation
                AND event.event_ordinal = current_event.event_ordinal
               JOIN runner_current_tool_attempt AS current_attempt
                 ON current_attempt.attempt_id = generation.attempt_id
              WHERE generation.session_id = $1
                AND generation.placement_event_ordinal = $2
                AND event.state_kind IN ('offered', 'claimed')
              ORDER BY generation.lease_id, generation.generation
              LIMIT 2",
        )
        .bind(session.into_uuid())
        .bind(Decimal::from(placement_event_ordinal))
        .fetch_all(&mut **transaction)
        .await?;
        let [candidate] = candidates.as_slice() else {
            if candidates.is_empty() {
                return Ok(None);
            }
            return Err(RunnerProtocolCorruption::IncompleteInventory.into());
        };
        let lease_id = runner_lease_id(candidate.decode_column("lease_id")?);
        let generation = decode_generation(candidate.decode_column("generation")?)?;
        let locked = sqlx::query(RUNNER_LEASE_HEAD)
            .bind(lease_id.into_uuid())
            .bind(Decimal::from(generation.get()))
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalLoss)?;
        let state: String = locked.decode_column("state_kind")?;
        if !matches!(state.as_str(), "offered" | "claimed") {
            return Ok(None);
        }
        self.load_lease_in(transaction, lease_id, generation)
            .await?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalLoss.into())
            .map(Some)
    }

    /// Stores one sealed lease loss that does not claim independent no-execution proof.
    pub async fn store_lease_loss(
        &self,
        loss: &RunnerLeaseLoss,
    ) -> Result<(), RunnerProtocolStoreError> {
        if loss.no_execution_proof().is_some() {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        self.store_lease_without_proof(loss.lost()).await
    }

    /// Durably reserves one retryable claimed loss for an exact replacement attempt.
    /// Replaying the same reservation is idempotent after an interrupted write sequence.
    pub async fn store_claimed_retry_attempt_authority(
        &self,
        loss: &RunnerLeaseLoss,
        replacement: &RunnerClaimedAttemptReplacement,
    ) -> Result<(), RunnerProtocolStoreError> {
        let source = loss.lost().correlation();
        if loss.retry().is_none()
            || !matches!(
                loss.lost().state(),
                RunnerLeaseState::LostExecutionPossible | RunnerLeaseState::LostClaimed
            )
            || replacement.source() != &source
        {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::CorrelationMismatch,
            ));
        }
        let replacement = replacement.replacement();
        let mut transaction = self.pool.begin().await?;
        let scheduler_exists = sqlx::query_scalar::<_, Uuid>(RUNNER_RETRY_REPLACEMENT_SCHEDULER)
            .bind(source.dispatch.session().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .is_some();
        if !scheduler_exists {
            transaction.rollback().await?;
            return Err(RunnerProtocolStoreError::Corruption(
                RunnerProtocolCorruption::CrossWiredReference,
            ));
        }
        let source_is_live: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1
                   FROM tool_attempt AS attempt
                   JOIN runner_lease_generation AS lease
                     ON lease.attempt_id = attempt.attempt_id
                    AND lease.session_id = attempt.session_id
                   JOIN runner_current_lease_event AS lease_head
                     ON lease_head.lease_id = lease.lease_id
                    AND lease_head.generation = lease.generation
                   JOIN runner_lease_event AS lease_event
                     ON lease_event.lease_id = lease_head.lease_id
                    AND lease_event.generation = lease_head.generation
                    AND lease_event.event_ordinal = lease_head.event_ordinal
                  WHERE attempt.attempt_id = $1
                    AND attempt.session_id = $2
                    AND attempt.turn_id = $3
                    AND attempt.issuing_turn_attempt_id = $4
                    AND attempt.request_id = $5
                    AND attempt.dispatch_generation = $6
                    AND attempt.state_kind = 'in_flight'
                    AND lease.lease_id = $7
                    AND lease.generation = $8
                    AND lease.runner_id = $9
                    AND lease.tool_name = $10
                    AND lease_event.state_kind IN (
                        'lost_execution_possible', 'lost_claimed'
                    )
             )",
        )
        .bind(source.dispatch.attempt().into_uuid())
        .bind(source.dispatch.session().into_uuid())
        .bind(source.dispatch.turn().into_uuid())
        .bind(source.dispatch.issuing_attempt().into_uuid())
        .bind(source.dispatch.request().into_uuid())
        .bind(Decimal::from(source.dispatch.generation().as_u64()))
        .bind(source.lease.into_uuid())
        .bind(Decimal::from(source.generation.get()))
        .bind(source.runner.into_uuid())
        .bind(source.tool.as_str())
        .fetch_one(&mut *transaction)
        .await?;
        if !source_is_live {
            transaction.rollback().await?;
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let inserted = sqlx::query(
            "INSERT INTO runner_claimed_retry_attempt_authority
                (source_lease_id, source_generation,
                 replacement_attempt_id, replacement_session_id,
                 replacement_turn_id, replacement_issuing_turn_attempt_id,
                 replacement_request_id, replacement_dispatch_generation)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (source_lease_id, source_generation) DO NOTHING",
        )
        .bind(source.lease.into_uuid())
        .bind(Decimal::from(source.generation.get()))
        .bind(replacement.attempt().into_uuid())
        .bind(replacement.session().into_uuid())
        .bind(replacement.turn().into_uuid())
        .bind(replacement.issuing_attempt().into_uuid())
        .bind(replacement.request().into_uuid())
        .bind(Decimal::from(replacement.generation().as_u64()))
        .execute(&mut *transaction)
        .await?;
        if inserted.rows_affected() == 0 {
            let reserved = sqlx::query(
                "SELECT replacement_attempt_id, replacement_session_id,
                        replacement_turn_id, replacement_issuing_turn_attempt_id,
                        replacement_request_id, replacement_dispatch_generation
                   FROM runner_claimed_retry_attempt_authority
                  WHERE source_lease_id = $1 AND source_generation = $2",
            )
            .bind(source.lease.into_uuid())
            .bind(Decimal::from(source.generation.get()))
            .fetch_optional(&mut *transaction)
            .await?;
            let exact = if let Some(row) = reserved {
                row.decode_column::<Uuid>("replacement_attempt_id")?
                    == replacement.attempt().into_uuid()
                    && row.decode_column::<Uuid>("replacement_session_id")?
                        == replacement.session().into_uuid()
                    && row.decode_column::<Uuid>("replacement_turn_id")?
                        == replacement.turn().into_uuid()
                    && row.decode_column::<Uuid>("replacement_issuing_turn_attempt_id")?
                        == replacement.issuing_attempt().into_uuid()
                    && row.decode_column::<Uuid>("replacement_request_id")?
                        == replacement.request().into_uuid()
                    && row.decode_column::<Decimal>("replacement_dispatch_generation")?
                        == Decimal::from(replacement.generation().as_u64())
            } else {
                false
            };
            if !exact {
                transaction.rollback().await?;
                return Err(RunnerProtocolStoreError::Domain(
                    RunnerDomainError::CorrelationMismatch,
                ));
            }
        }
        commit_mutation(transaction).await
    }

    /// Loads an exact claimed-retry reservation for crash-resumable replay.
    pub async fn load_claimed_retry_attempt_reservation(
        &self,
        lease: RunnerLeaseId,
        generation: RunnerGeneration,
    ) -> Result<Option<ToolAttemptDispatchCorrelation>, RunnerProtocolStoreError> {
        let row = sqlx::query(
            "SELECT replacement_attempt_id, replacement_session_id,
                    replacement_turn_id, replacement_issuing_turn_attempt_id,
                    replacement_request_id, replacement_dispatch_generation
               FROM runner_claimed_retry_attempt_authority AS authority
               JOIN runner_lease_generation AS lease
                 ON lease.lease_id = authority.source_lease_id
                AND lease.generation = authority.source_generation
               JOIN tool_attempt AS source_attempt
                 ON source_attempt.attempt_id = lease.attempt_id
                AND source_attempt.session_id = lease.session_id
              WHERE authority.source_lease_id = $1
                AND authority.source_generation = $2
                AND source_attempt.state_kind = 'in_flight'",
        )
        .bind(lease.into_uuid())
        .bind(Decimal::from(generation.get()))
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok::<_, RunnerProtocolStoreError>(ToolAttemptDispatchCorrelation::reconstitute(
                ToolAttemptDispatchCorrelationReconstitutionInput {
                    session: session_id(row.decode_column("replacement_session_id")?),
                    turn: TurnId::from_uuid(row.decode_column("replacement_turn_id")?),
                    issuing_attempt: TurnAttemptId::from_uuid(
                        row.decode_column("replacement_issuing_turn_attempt_id")?,
                    ),
                    request: ToolRequestId::from_uuid(row.decode_column("replacement_request_id")?),
                    attempt: tool_attempt_id(row.decode_column("replacement_attempt_id")?),
                    generation: decode_dispatch_generation(
                        row.decode_column("replacement_dispatch_generation")?,
                    )?,
                },
            ))
        })
        .transpose()
    }

    /// Atomically retires the in-flight source attempt to its effect-correct
    /// terminal history and persists the exact replacement attempt together
    /// with its successor lease generation, after `offer_retry` validated the
    /// private claimed-retry evidence. Committing all three in one
    /// transaction leaves only two durable claimed-retry states: the loss
    /// with its still-in-flight source (with or without the replayable
    /// reservation), or the complete consumed retry, whose successor lease is
    /// already offered. The schema rejects a replacement attempt committed
    /// without its successor generation, so a crash can no longer strand the
    /// retry between them, and a reloaded batch always carries either the
    /// live source the checked replacement requires or the retired identity
    /// inventory. The retired attempt is the exact predecessor the claimed
    /// replacement produced; the reservation and lease-generation triggers
    /// independently reject any other pairing.
    pub async fn store_claimed_retry_replacement(
        &self,
        retired: &EndedToolAttempt,
        retry: &RunnerLease,
    ) -> Result<(), RunnerProtocolStoreError> {
        if retry.state() != RunnerLeaseState::Offered
            || retry.generation() == RunnerGeneration::one()
        {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let dispatch = retry.correlation().dispatch;
        let effect_matches = matches!(
            (retry.effect(), retired.effect_class()),
            (RunnerToolEffectClass::Pure, ToolEffectClass::EffectFree)
                | (
                    RunnerToolEffectClass::Idempotent,
                    ToolEffectClass::ExternalEffect
                )
        );
        if retired.session() != dispatch.session()
            || retired.turn() != dispatch.turn()
            || retired.issuing_attempt() != dispatch.issuing_attempt()
            || retired.request() != dispatch.request()
            || retired.attempt() == dispatch.attempt()
            || !effect_matches
        {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::CorrelationMismatch,
            ));
        }
        let (retired_disposition, retired_error) = match (retired.effect_class(), retired.end()) {
            (ToolEffectClass::EffectFree, ToolAttemptEnd::KnownFailed { error })
                if error.kind() == ToolExecutionErrorKind::CrashLost
                    && error.detail().is_none() =>
            {
                (
                    tool_attempt_disposition_to_str(ToolAttemptDispositionStorageKind::KnownFailed),
                    Some("crash_lost"),
                )
            }
            (ToolEffectClass::ExternalEffect, ToolAttemptEnd::Ambiguous) => (
                tool_attempt_disposition_to_str(ToolAttemptDispositionStorageKind::Ambiguous),
                None,
            ),
            _ => {
                return Err(RunnerProtocolStoreError::Domain(
                    RunnerDomainError::CorrelationMismatch,
                ));
            }
        };
        let mut transaction = self.pool.begin().await?;
        let scheduler_exists = sqlx::query_scalar::<_, Uuid>(RUNNER_RETRY_REPLACEMENT_SCHEDULER)
            .bind(retired.session().into_uuid())
            .fetch_optional(&mut *transaction)
            .await?
            .is_some();
        if !scheduler_exists {
            transaction.rollback().await?;
            return Err(RunnerProtocolStoreError::Corruption(
                RunnerProtocolCorruption::CrossWiredReference,
            ));
        }
        let retired_rows = sqlx::query(
            "UPDATE tool_attempt
                SET state_kind = 'terminal',
                    terminal_disposition_kind = $1,
                    error_kind = $2
              WHERE attempt_id = $3
                AND request_id = $4
                AND session_id = $5
                AND turn_id = $6
                AND issuing_turn_attempt_id = $7
                AND state_kind = 'in_flight'
                AND terminal_disposition_kind IS NULL",
        )
        .bind(retired_disposition)
        .bind(retired_error)
        .bind(retired.attempt().into_uuid())
        .bind(retired.request().into_uuid())
        .bind(retired.session().into_uuid())
        .bind(retired.turn().into_uuid())
        .bind(retired.issuing_attempt().into_uuid())
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        if retired_rows != 1 {
            transaction.rollback().await?;
            return Err(RunnerProtocolStoreError::Corruption(
                RunnerProtocolCorruption::CrossWiredReference,
            ));
        }
        sqlx::query(
            "INSERT INTO tool_attempt
                (attempt_id, request_id, session_id, turn_id,
                 issuing_turn_attempt_id, effect_class, dispatch_generation,
                 state_kind)
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'in_flight')",
        )
        .bind(dispatch.attempt().into_uuid())
        .bind(dispatch.request().into_uuid())
        .bind(dispatch.session().into_uuid())
        .bind(dispatch.turn().into_uuid())
        .bind(dispatch.issuing_attempt().into_uuid())
        .bind(match retired.effect_class() {
            ToolEffectClass::EffectFree => "effect_free",
            ToolEffectClass::ExternalEffect => "external_effect",
        })
        .bind(Decimal::from(dispatch.generation().as_u64()))
        .execute(&mut *transaction)
        .await?;
        append_lease_event_in(&mut transaction, retry).await?;
        commit_mutation(transaction).await
    }

    async fn store_lease_without_proof(
        &self,
        lease: &RunnerLease,
    ) -> Result<(), RunnerProtocolStoreError> {
        if lease.state() == RunnerLeaseState::LostUnclaimed {
            return Err(RunnerProtocolStoreError::Domain(
                RunnerDomainError::InvalidState,
            ));
        }
        let mut transaction = self.pool.begin().await?;
        append_lease_event_in(&mut transaction, lease).await?;
        commit_mutation(transaction).await
    }

    /// Loads the latest durable lease generation bound to one physical attempt.
    pub async fn load_current_lease_for_attempt(
        &self,
        attempt: ToolAttemptId,
    ) -> Result<Option<RunnerLease>, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let correlation = sqlx::query_as::<_, (Uuid, Decimal)>(
            "SELECT binding.lease_id, max(generation.generation)
               FROM runner_physical_attempt_lease_binding AS binding
               JOIN runner_lease_generation AS generation
                 ON generation.lease_id = binding.lease_id
                AND generation.attempt_id = binding.attempt_id
              WHERE binding.attempt_id = $1
              GROUP BY binding.lease_id",
        )
        .bind(attempt.into_uuid())
        .fetch_optional(transaction.as_mut())
        .await?;
        let loaded = match correlation {
            Some((lease, generation)) => {
                self.load_lease_in(
                    &mut transaction,
                    runner_lease_id(lease),
                    decode_generation(generation)?,
                )
                .await?
            }
            None => None,
        };
        transaction.commit().await?;
        Ok(loaded)
    }

    /// Loads one exact lease generation and independently joined fence.
    pub async fn load_lease(
        &self,
        lease: RunnerLeaseId,
        generation: RunnerGeneration,
    ) -> Result<Option<RunnerLease>, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let lease = self
            .load_lease_in(&mut transaction, lease, generation)
            .await?;
        transaction.commit().await?;
        Ok(lease)
    }

    async fn load_lease_in(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        lease: RunnerLeaseId,
        generation: RunnerGeneration,
    ) -> Result<Option<RunnerLease>, RunnerProtocolStoreError> {
        let row = sqlx::query(
            "SELECT lease_generation.*, event.state_kind,
                    request.tool_name AS canonical_attempt_tool,
                    request.arguments_kind AS canonical_arguments_kind,
                    request.arguments_text AS canonical_arguments_text,
                    attempt.turn_id AS canonical_attempt_turn,
                    attempt.issuing_turn_attempt_id
                        AS canonical_issuing_attempt,
                    attempt.request_id AS canonical_attempt_request,
                    attempt.dispatch_generation
                        AS canonical_dispatch_generation,
                    placement.state_kind AS canonical_placement_state,
                    placement.pinned_runner_id AS canonical_placement_runner,
                    placement.placement_revision
                        AS canonical_placement_revision,
                    placement.pinned_working_directory
                        AS canonical_working_directory,
                    placement.requested_sandbox_profile
                        AS canonical_sandbox_profile,
                    placement.registration_enrollment_id
                        AS canonical_registration_enrollment,
                    placement.registration_revision
                        AS canonical_registration_revision,
                    grant_tool.tool_name AS canonical_grant_tool,
                    grant_tool.approval_kind AS canonical_grant_approval
               FROM runner_lease_generation AS lease_generation
               JOIN runner_current_lease_event AS current_event
                 ON current_event.lease_id = lease_generation.lease_id
                AND current_event.generation = lease_generation.generation
               JOIN runner_lease_event AS event
                 ON event.lease_id = current_event.lease_id
                AND event.generation = current_event.generation
                AND event.event_ordinal = current_event.event_ordinal
               LEFT JOIN tool_attempt AS attempt
                 ON attempt.attempt_id = lease_generation.attempt_id
                AND attempt.session_id = lease_generation.session_id
               LEFT JOIN tool_request AS request
                 ON request.request_id = attempt.request_id
               LEFT JOIN runner_session_placement_record AS placement
                 ON placement.session_id = lease_generation.session_id
                AND placement.event_ordinal =
                    lease_generation.placement_event_ordinal
               LEFT JOIN runner_credential_grant_tool AS grant_tool
                 ON grant_tool.session_id = lease_generation.session_id
                AND grant_tool.lineage_origin_event_ordinal =
                    lease_generation.credential_grant_lineage_origin_ordinal
                AND grant_tool.runner_id = lease_generation.runner_id
                AND grant_tool.grant_revision =
                    lease_generation.credential_grant_revision
                AND grant_tool.credential_profile_name =
                    lease_generation.credential_profile_name
                AND grant_tool.tool_name = lease_generation.tool_name
              WHERE lease_generation.lease_id = $1
                AND lease_generation.generation = $2",
        )
        .bind(lease.into_uuid())
        .bind(Decimal::from(generation.get()))
        .fetch_optional(transaction.as_mut())
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let registration = load_registration_in(
            transaction.as_mut(),
            runner_enrollment_id(row.decode_column("registration_enrollment_id")?),
            decode_registration_revision(row.decode_column("offer_registration_revision")?)?,
            None,
            &self.catalog,
        )
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        let lease = decode_lease(&row, registration.registration())?;
        Ok(Some(lease))
    }

    /// Loads one durable loss generation and rebuilds its sealed retry authority.
    pub async fn load_lease_loss(
        &self,
        lease: RunnerLeaseId,
        generation: RunnerGeneration,
    ) -> Result<Option<RunnerLeaseLoss>, RunnerProtocolStoreError> {
        let mut transaction = begin_repeatable_read(&self.pool).await?;
        let Some(loaded) = self
            .load_lease_in(&mut transaction, lease, generation)
            .await?
        else {
            transaction.commit().await?;
            return Ok(None);
        };
        let row = sqlx::query(
            "SELECT *
               FROM runner_lease_no_execution_proof
              WHERE lease_id = $1 AND generation = $2",
        )
        .bind(lease.into_uuid())
        .bind(Decimal::from(generation.get()))
        .fetch_optional(transaction.as_mut())
        .await?;
        let lease_correlation = loaded.correlation();
        let no_execution = row
            .map(|row| {
                let dispatch = ToolAttemptDispatchCorrelation::reconstitute(
                    ToolAttemptDispatchCorrelationReconstitutionInput {
                        session: session_id(row.decode_column("session_id")?),
                        turn: TurnId::from_uuid(row.decode_column("turn_id")?),
                        issuing_attempt: TurnAttemptId::from_uuid(
                            row.decode_column("issuing_turn_attempt_id")?,
                        ),
                        request: ToolRequestId::from_uuid(row.decode_column("request_id")?),
                        attempt: tool_attempt_id(row.decode_column("attempt_id")?),
                        generation: decode_dispatch_generation(
                            row.decode_column("dispatch_generation")?,
                        )?,
                    },
                );
                let stored = RunnerLeaseCorrelation {
                    lease: runner_lease_id(row.decode_column("lease_id")?),
                    runner: runner_id(row.decode_column("runner_id")?),
                    registration_revision: lease_correlation.registration_revision,
                    placement_revision: lease_correlation.placement_revision,
                    working_directory: lease_correlation.working_directory.clone(),
                    sandbox: lease_correlation.sandbox,
                    tool: tool_name(row.decode_column("tool_name")?)?,
                    dispatch,
                    generation: decode_generation(row.decode_column("generation")?)?,
                };
                if stored != lease_correlation {
                    return Err(RunnerProtocolCorruption::CrossWiredReference.into());
                }
                Ok::<_, RunnerProtocolStoreError>(stored)
            })
            .transpose()?;
        let retry_prepared: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1
                   FROM runner_claimed_retry_attempt_authority AS authority
                   JOIN tool_attempt AS replacement
                     ON replacement.attempt_id = authority.replacement_attempt_id
                    AND replacement.session_id = authority.replacement_session_id
                    AND replacement.turn_id = authority.replacement_turn_id
                    AND replacement.issuing_turn_attempt_id =
                        authority.replacement_issuing_turn_attempt_id
                    AND replacement.request_id = authority.replacement_request_id
                    AND replacement.dispatch_generation =
                        authority.replacement_dispatch_generation
                  WHERE authority.source_lease_id = $1
                    AND authority.source_generation = $2
                 UNION ALL
                 SELECT 1
                   FROM runner_lease_generation
                  WHERE lease_id = $1 AND predecessor_generation = $2
                 UNION ALL
                 SELECT 1
                   FROM turn_runner_recovery_interrupt_effect AS stopped
                   JOIN tool_attempt AS source_attempt
                     ON source_attempt.attempt_id =
                        stopped.interrupted_tool_attempt_id
                    AND source_attempt.session_id = stopped.session_id
                    AND source_attempt.turn_id = stopped.turn_id
                   JOIN runner_lease_generation AS source_lease
                     ON source_lease.attempt_id = source_attempt.attempt_id
                    AND source_lease.session_id = source_attempt.session_id
                  WHERE source_lease.lease_id = $1
                    AND source_lease.generation = $2
             )",
        )
        .bind(lease.into_uuid())
        .bind(Decimal::from(generation.get()))
        .fetch_one(transaction.as_mut())
        .await?;
        transaction.commit().await?;
        let retry_preparation = match retry_prepared {
            true => RunnerLeaseRetryPreparation::Prepared,
            false => RunnerLeaseRetryPreparation::Available,
        };
        loaded
            .into_reconstituted_loss(no_execution, retry_preparation)
            .map(Some)
            .map_err(RunnerProtocolStoreError::Domain)
    }
}

fn merge_executable_tool_snapshot(
    runner_catalog: &RunnerCatalog,
    runner_tools: Box<[RunnerExecutableTool]>,
    daemon_catalog: &dyn ToolCatalog,
    dangerous_tool_auto_approval: signalbox_domain::DangerousToolAutoApproval,
) -> Result<Box<[ExecutableToolSnapshotEntry]>, PostgresExecutableToolSnapshotFailure> {
    let daemon_tools: BTreeMap<_, _> = daemon_catalog
        .definitions()
        .into_vec()
        .into_iter()
        .map(|definition| (definition.name().clone(), definition))
        .collect();
    let mut snapshot = BTreeMap::new();
    for (name, definition) in &daemon_tools {
        match runner_catalog.tool(name) {
            Some(declaration) => {
                validate_shared_tool_definition(definition, declaration)?;
                if declaration.loci().allows_daemon() {
                    snapshot.insert(
                        name.clone(),
                        ExecutableToolSnapshotEntry::daemon(
                            definition.clone(),
                            dangerous_tool_auto_approval,
                        ),
                    );
                }
            }
            None => {
                snapshot.insert(
                    name.clone(),
                    ExecutableToolSnapshotEntry::daemon(
                        definition.clone(),
                        dangerous_tool_auto_approval,
                    ),
                );
            }
        }
    }
    for runner_tool in runner_tools {
        let declaration = runner_tool.declaration();
        let daemon_available = daemon_tools.contains_key(declaration.name());
        if declaration.loci().allows_daemon() && daemon_available {
            continue;
        }
        let definition = application_tool_definition(declaration)?;
        let approval = match runner_tool.approval() {
            CredentialToolApproval::Automatic => InitialToolApproval::PolicyAuto,
            CredentialToolApproval::SessionPolicy => InitialToolApproval::Confirm,
        };
        let entry =
            ExecutableToolSnapshotEntry::runner(definition, runner_tool.locus().clone(), approval)
                .ok_or_else(|| {
                    PostgresExecutableToolSnapshotFailure::InvalidRunnerDefinition(
                        declaration.name().clone(),
                    )
                })?;
        snapshot.insert(declaration.name().clone(), entry);
    }
    Ok(snapshot
        .into_values()
        .collect::<Vec<_>>()
        .into_boxed_slice())
}

fn application_tool_definition(
    declaration: &RunnerToolDeclaration,
) -> Result<ToolDefinition, PostgresExecutableToolSnapshotFailure> {
    let schema = ToolInputSchema::try_new(declaration.model().input_schema().as_str().to_owned())
        .map_err(|_| {
        PostgresExecutableToolSnapshotFailure::InvalidRunnerDefinition(declaration.name().clone())
    })?;
    let effect = match declaration.effect() {
        RunnerToolEffectClass::Pure => ToolEffectClass::EffectFree,
        RunnerToolEffectClass::Idempotent | RunnerToolEffectClass::SideEffecting => {
            ToolEffectClass::ExternalEffect
        }
    };
    Ok(ToolDefinition::new(
        declaration.name().clone(),
        declaration.model().description().to_owned(),
        schema,
        declaration.permission(),
        effect,
    ))
}

fn validate_shared_tool_definition(
    daemon: &ToolDefinition,
    runner: &RunnerToolDeclaration,
) -> Result<(), PostgresExecutableToolSnapshotFailure> {
    let expected = application_tool_definition(runner)?;
    if daemon.name() == expected.name()
        && daemon.description() == expected.description()
        && daemon.input_schema() == expected.input_schema()
        && daemon.permission_default() == expected.permission_default()
        && daemon.effect_class() == expected.effect_class()
    {
        Ok(())
    } else {
        Err(
            PostgresExecutableToolSnapshotFailure::IncompatibleDaemonDefinition(
                runner.name().clone(),
            ),
        )
    }
}

#[derive(Clone, Copy)]
struct LockedRunnerLossPropagation {
    propagated_through: Option<SessionId>,
    complete: bool,
}

#[derive(Clone, Copy)]
struct LockedRunnerRegistrationReconciliation {
    propagated_through: Option<SessionId>,
    complete: bool,
}

async fn lock_registration_reconciliation(
    transaction: &mut Transaction<'_, Postgres>,
    reconciliation: RunnerRegistrationReconciliationSnapshot,
) -> Result<LockedRunnerRegistrationReconciliation, RunnerProtocolStoreError> {
    let row = sqlx::query(RUNNER_REGISTRATION_RECONCILIATION)
        .bind(reconciliation.enrollment().into_uuid())
        .bind(Decimal::from(reconciliation.registration_revision().get()))
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
    let state: String = row.decode_column("state_kind")?;
    let complete = match state.as_str() {
        "pending" => false,
        "completed" => true,
        _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    };
    Ok(LockedRunnerRegistrationReconciliation {
        propagated_through: row
            .decode_column::<Option<Uuid>>("propagated_through_session_id")?
            .map(session_id),
        complete,
    })
}

fn placement_is_registration_reconciliation_candidate(
    placement: &PgRow,
    reconciliation: RunnerRegistrationReconciliationSnapshot,
) -> Result<bool, RunnerProtocolStoreError> {
    let state: String = placement.decode_column("state_kind")?;
    let enrollment = placement.decode_column::<Option<Uuid>>("registration_enrollment_id")?;
    let revision = placement
        .decode_column::<Option<Decimal>>("registration_revision")?
        .map(decode_registration_revision)
        .transpose()?;
    Ok(state == "pinned"
        && enrollment == Some(reconciliation.enrollment().into_uuid())
        && revision.is_some_and(|revision| revision < reconciliation.registration_revision()))
}

async fn insert_registration_reconciliation_observation(
    transaction: &mut Transaction<'_, Postgres>,
    reconciliation: RunnerRegistrationReconciliationSnapshot,
    session: SessionId,
    placement_event_ordinal: u64,
    disposition: &str,
) -> Result<(), RunnerProtocolStoreError> {
    sqlx::query(
        "INSERT INTO runner_registration_reconciliation_observation
            (enrollment_id, registration_revision, session_id,
             placement_event_ordinal, disposition_kind)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(reconciliation.enrollment().into_uuid())
    .bind(Decimal::from(reconciliation.registration_revision().get()))
    .bind(session.into_uuid())
    .bind(Decimal::from(placement_event_ordinal))
    .bind(disposition)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn advance_registration_reconciliation_cursor(
    transaction: &mut Transaction<'_, Postgres>,
    reconciliation: RunnerRegistrationReconciliationSnapshot,
    session: SessionId,
) -> Result<(), RunnerProtocolStoreError> {
    let changed = sqlx::query(
        "UPDATE runner_registration_reconciliation
            SET propagated_through_session_id = $3
          WHERE enrollment_id = $1 AND registration_revision = $2
            AND state_kind = 'pending'",
    )
    .bind(reconciliation.enrollment().into_uuid())
    .bind(Decimal::from(reconciliation.registration_revision().get()))
    .bind(session.into_uuid())
    .execute(&mut **transaction)
    .await?
    .rows_affected();
    if changed != 1 {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    Ok(())
}

impl AbandonLostRunnerTransaction for RunnerProtocolStore {
    type Error = RunnerProtocolStoreError;

    async fn handle(
        &mut self,
        command: AbandonLostRunner,
    ) -> Result<AbandonLostRunnerOutcome, Self::Error> {
        RunnerProtocolStore::abandon_lost_runner(self, command).await
    }
}

impl ReplaceLostRunnerBeforePinTransaction for RunnerProtocolStore {
    type Error = RunnerProtocolStoreError;

    async fn handle(
        &mut self,
        command: ReplaceLostRunner,
    ) -> Result<ReplaceLostRunnerBeforePinOutcome, Self::Error> {
        RunnerProtocolStore::replace_lost_runner_before_pin(self, command).await
    }
}

impl RunnerReplacementProvisioningTransaction for RunnerProtocolStore {
    type Error = RunnerProtocolStoreError;

    async fn stage(
        &mut self,
        command: ReplaceLostRunner,
        authorization: WorkspaceProvisioningAuthorizationId,
    ) -> Result<RunnerReplacementProvisioningOutcome, Self::Error> {
        self.stage_runner_replacement_provisioning(command, authorization)
            .await
    }
}

impl RunnerWorkspaceReadyTransaction for RunnerProtocolStore {
    type Error = RunnerProtocolStoreError;

    async fn record(
        &mut self,
        receipt: RunnerWorkspaceReadyReceipt,
    ) -> Result<RunnerWorkspaceReadyReceipt, Self::Error> {
        self.record_workspace_ready_receipt(receipt).await
    }
}

impl RunnerWorkspaceReleaseTransaction for RunnerProtocolStore {
    type Error = RunnerProtocolStoreError;

    async fn record_release(
        &mut self,
        acknowledgement: RunnerWorkspaceReleaseAcknowledgement,
    ) -> Result<RunnerWorkspaceReleaseAcknowledgement, Self::Error> {
        self.record_workspace_release_acknowledgement(acknowledgement)
            .await
    }
}

impl RunnerWorkspaceCleanupFailureTransaction for RunnerProtocolStore {
    type Error = RunnerProtocolStoreError;

    async fn record_cleanup_failure(
        &mut self,
        failure: RunnerWorkspaceCleanupFailure,
    ) -> Result<RunnerWorkspaceCleanupFailure, Self::Error> {
        self.record_workspace_cleanup_failure(failure).await
    }
}

impl PinnedRunnerReplacementTransaction for RunnerProtocolStore {
    type Error = RunnerProtocolStoreError;

    async fn complete(
        &mut self,
        command: ReplaceLostRunner,
        identities: PinnedRunnerReplacementIdentities,
    ) -> Result<PinnedRunnerReplacementOutcome, Self::Error> {
        self.complete_workspace_free_pinned_replacement(command, identities)
            .await
    }
}

impl PinnedRunnerDispatchTransaction for RunnerProtocolStore {
    type Error = RunnerProtocolStoreError;

    async fn authorize(
        &mut self,
        request: PinnedRunnerDispatchRequest,
        lease: RunnerLeaseId,
    ) -> Result<PinnedRunnerLeaseOffer, Self::Error> {
        self.authorize_pinned_dispatch(request, lease).await
    }
}

impl InitialRunnerDispatchTransaction for RunnerProtocolStore {
    type Error = RunnerProtocolStoreError;

    async fn authorize_initial(
        &mut self,
        request: InitialRunnerDispatchRequest,
        lease: RunnerLeaseId,
    ) -> Result<PinnedRunnerLeaseOffer, Self::Error> {
        self.authorize_initial_dispatch(request, lease).await
    }
}

impl RunnerLeaseClaimTransaction for RunnerProtocolStore {
    type Error = RunnerProtocolStoreError;

    async fn claim(
        &mut self,
        request: RunnerLeaseClaimRequest,
    ) -> Result<RunnerLease, Self::Error> {
        self.claim_lease(request).await
    }
}

impl RunnerLeaseResultTransaction for RunnerProtocolStore {
    type Error = RunnerProtocolStoreError;

    async fn commit_result(
        &mut self,
        request: RunnerLeaseResultRequest,
    ) -> Result<RunnerLeaseCompletion, Self::Error> {
        self.commit_lease_result(request).await
    }
}

fn validate_workspace_ready_receipt(
    receipt: &RunnerWorkspaceReadyReceipt,
    authority: &StoredWorkspaceProvisioningAuthorization,
) -> Result<(), RunnerProtocolStoreError> {
    let expected_relative_path = format!(
        "sessions/{}/{}/repo",
        receipt.session().as_uuid(),
        receipt.placement_revision().get()
    );
    if receipt.session() != authority.session()
        || receipt.placement_revision() != authority.successor_placement_revision()
        || receipt.runner() != authority.runner()
        || receipt.repository() != authority.repository()
        || receipt.credential_profile() != authority.credential_profile()
        || receipt.sandbox() != authority.sandbox()
        || receipt.relative_path().as_str() != expected_relative_path
    {
        return Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::CorrelationMismatch,
        ));
    }
    Ok(())
}

fn exact_workspace_ready_replay(
    supplied: RunnerWorkspaceReadyReceipt,
    stored: StoredWorkspaceProvisioningReceipt,
) -> Result<RunnerWorkspaceReadyReceipt, RunnerProtocolStoreError> {
    let recorded = RunnerWorkspaceReadyReceipt::new(
        stored.authorization,
        stored.session,
        stored.placement_revision,
        stored.runner,
        stored.manifest,
        RunnerReadyManifestDigest::try_new(stored.manifest_digest)
            .map_err(|_| RunnerProtocolCorruption::InvalidEncoding)?,
        stored.repository,
        stored.canonical_clone_url_digest,
        stored.credential_profile,
        stored.sandbox,
        stored.relative_path,
        stored.recovery,
    );
    if supplied != recorded {
        return Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::CorrelationMismatch,
        ));
    }
    Ok(recorded)
}

fn exact_workspace_release_acknowledgement_replay(
    supplied: RunnerWorkspaceReleaseAcknowledgement,
    recorded: RunnerWorkspaceReleaseAcknowledgement,
) -> Result<RunnerWorkspaceReleaseAcknowledgement, RunnerProtocolStoreError> {
    if supplied != recorded {
        return Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::CorrelationMismatch,
        ));
    }
    Ok(recorded)
}

fn exact_workspace_cleanup_failure_replay(
    supplied: RunnerWorkspaceCleanupFailure,
    recorded: RunnerWorkspaceCleanupFailure,
) -> Result<RunnerWorkspaceCleanupFailure, RunnerProtocolStoreError> {
    if supplied != recorded {
        return Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::CorrelationMismatch,
        ));
    }
    Ok(recorded)
}

async fn load_workspace_cleanup_failure_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    session: SessionId,
    placement_revision: RunnerGeneration,
) -> Result<Option<RunnerWorkspaceCleanupFailure>, RunnerProtocolStoreError> {
    let row = sqlx::query(
        "SELECT runner_id, release_manifest_id, detail_code,
                detail_message, detail_payload_json
           FROM runner_operation_failure
          WHERE operation_kind = $3
            AND release_session_id = $1
            AND release_placement_revision = $2",
    )
    .bind(session.into_uuid())
    .bind(Decimal::from(placement_revision.get()))
    .bind(runner_operation_failure_operation_to_str(
        RunnerOperationFailureOperationStorageKind::WorkspaceRelease,
    ))
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let detail = RunnerOperationFailureDetail::try_new(RunnerOperationFailureDetailInput {
        code: row.decode_column("detail_code")?,
        message: row.decode_column("detail_message")?,
        payload_json: row.decode_column("detail_payload_json")?,
    })
    .map_err(|_| RunnerProtocolCorruption::InvalidEncoding)?;
    Ok(Some(RunnerWorkspaceCleanupFailure::new(
        session,
        placement_revision,
        runner_id(row.decode_column("runner_id")?),
        WorkspaceManifestId::from_uuid(row.decode_column("release_manifest_id")?),
        detail,
    )))
}

async fn insert_workspace_ready_receipt(
    connection: &mut PgConnection,
    receipt: &RunnerWorkspaceReadyReceipt,
) -> Result<(), RunnerProtocolStoreError> {
    let (recovery_kind, branch_name, revision) = match receipt.recovery() {
        WorkspaceRecovery::Commit { revision } => ("commit", None, Some(revision.as_str())),
        WorkspaceRecovery::Branch { name, revision } => {
            ("branch", Some(name.as_str()), Some(revision.as_str()))
        }
        WorkspaceRecovery::UnbornBranch { name } => ("unborn_branch", Some(name.as_str()), None),
    };
    sqlx::query(
        "INSERT INTO runner_replacement_workspace_receipt
            (authorization_id, session_id, placement_revision, runner_id,
             manifest_id, manifest_digest, repository_key,
             canonical_clone_url_digest, credential_profile_name,
             sandbox_profile, relative_path, recovery_kind, branch_name,
             revision)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11,
                 $12, $13, $14)",
    )
    .bind(receipt.authorization().into_uuid())
    .bind(receipt.session().into_uuid())
    .bind(Decimal::from(receipt.placement_revision().get()))
    .bind(receipt.runner().into_uuid())
    .bind(receipt.manifest_id().into_uuid())
    .bind(receipt.manifest_digest().as_str())
    .bind(receipt.repository().as_str())
    .bind(receipt.canonical_clone_url_digest().as_str())
    .bind(
        receipt
            .credential_profile()
            .map(CredentialProfileName::as_str),
    )
    .bind(runner_sandbox_to_str(receipt.sandbox()))
    .bind(receipt.relative_path().as_str())
    .bind(recovery_kind)
    .bind(branch_name)
    .bind(revision)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

fn map_runner_tool_loop_error(
    error: crate::tool_loop::ToolLoopRepositoryError,
) -> RunnerProtocolStoreError {
    match error {
        crate::tool_loop::ToolLoopRepositoryError::Database { source, .. } => {
            RunnerProtocolStoreError::Database(source)
        }
        crate::tool_loop::ToolLoopRepositoryError::Corruption(_) => {
            RunnerProtocolCorruption::CrossWiredReference.into()
        }
        crate::tool_loop::ToolLoopRepositoryError::IdentityCollision
        | crate::tool_loop::ToolLoopRepositoryError::DifferentCommandKind
        | crate::tool_loop::ToolLoopRepositoryError::ConflictingCommandReuse
        | crate::tool_loop::ToolLoopRepositoryError::InvalidTransition(_) => {
            RunnerProtocolStoreError::Domain(RunnerDomainError::InvalidState)
        }
    }
}

enum ReplacementTargetAuthority {
    Current(Box<ReplacementTargetEvidence>),
    Unavailable {
        reason: RunnerReplacementTargetUnavailableReason,
        runner: Option<RunnerId>,
        predecessor_runner: Option<RunnerId>,
    },
}

impl ReplacementTargetAuthority {
    const fn runner(&self) -> Option<RunnerId> {
        match self {
            Self::Current(evidence) => Some(evidence.runner),
            Self::Unavailable { runner, .. } => *runner,
        }
    }

    const fn predecessor_runner(&self) -> Option<RunnerId> {
        match self {
            Self::Current(evidence) => evidence.predecessor_runner(),
            Self::Unavailable {
                predecessor_runner, ..
            } => *predecessor_runner,
        }
    }

    const fn unavailable_reason(&self) -> Option<RunnerReplacementTargetUnavailableReason> {
        match self {
            Self::Current(_) => None,
            Self::Unavailable { reason, .. } => Some(*reason),
        }
    }
}

struct ReplacementTargetEvidence {
    runner: RunnerId,
    enrollment: RunnerEnrollmentId,
    registration_revision: RunnerRegistrationRevision,
    connection_epoch: RunnerConnectionEpoch,
    connection_event_ordinal: u64,
    registration: StoredValidatedRunnerRegistration,
    pending_activation: Option<PendingReplacementActivation>,
}

impl ReplacementTargetEvidence {
    const fn predecessor_runner(&self) -> Option<RunnerId> {
        match &self.pending_activation {
            Some(activation) => Some(activation.predecessor.runner),
            None => None,
        }
    }
}

#[derive(Clone)]
struct ReplacementEnrollmentRow {
    enrollment: RunnerEnrollmentId,
    runner: RunnerId,
    authentication: Uuid,
    allowed_class_count: Decimal,
    revision: u64,
    state: RunnerEnrollmentState,
}

struct PendingReplacementActivation {
    request: RunnerEnrollmentRequestId,
    candidate: ReplacementEnrollmentRow,
    predecessor: ReplacementEnrollmentRow,
    predecessor_loss_epoch: RunnerConnectionLossEpoch,
}

fn decode_replacement_enrollment_row(
    row: PgRow,
) -> Result<ReplacementEnrollmentRow, RunnerProtocolStoreError> {
    Ok(ReplacementEnrollmentRow {
        enrollment: runner_enrollment_id(row.decode_column("enrollment_id")?),
        runner: runner_id(row.decode_column("runner_id")?),
        authentication: row.decode_column("authentication_reference_id")?,
        allowed_class_count: row.decode_column("allowed_class_count")?,
        revision: decode_u64(row.decode_column("revision")?)?,
        state: runner_enrollment_state_from_str(&row.decode_column::<String>("state_kind")?)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding)?,
    })
}

async fn activate_pending_replacement_target(
    connection: &mut PgConnection,
    activation: &PendingReplacementActivation,
) -> Result<(), RunnerProtocolStoreError> {
    let relation_is_current: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1
              FROM runner_pending_enrollment AS pending
              JOIN runner_connection_loss_epoch AS loss
                ON loss.enrollment_id = pending.predecessor_enrollment_id
               AND loss.loss_epoch = pending.predecessor_loss_epoch
             WHERE pending.request_id = $1
               AND pending.enrollment_id = $2
               AND pending.predecessor_enrollment_id = $3
               AND pending.predecessor_loss_epoch = $4
        )",
    )
    .bind(activation.request.into_uuid())
    .bind(activation.candidate.enrollment.into_uuid())
    .bind(activation.predecessor.enrollment.into_uuid())
    .bind(Decimal::from(activation.predecessor_loss_epoch.get()))
    .fetch_one(&mut *connection)
    .await?;
    if !relation_is_current {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let predecessor_revision = activation
        .predecessor
        .revision
        .checked_add(1)
        .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
    append_replacement_enrollment_audit(
        connection,
        &activation.predecessor,
        predecessor_revision,
        RunnerEnrollmentState::Revoked,
    )
    .await?;
    append_replacement_enrollment_audit(
        connection,
        &activation.candidate,
        2,
        RunnerEnrollmentState::Active,
    )
    .await?;
    let predecessor_updated = sqlx::query(
        "UPDATE runner_enrollment
            SET revision = $2, state_kind = $4
          WHERE enrollment_id = $1 AND revision = $3 AND state_kind = $5",
    )
    .bind(activation.predecessor.enrollment.into_uuid())
    .bind(Decimal::from(predecessor_revision))
    .bind(Decimal::from(activation.predecessor.revision))
    .bind(runner_enrollment_state_to_str(
        RunnerEnrollmentState::Revoked,
    ))
    .bind(runner_enrollment_state_to_str(
        RunnerEnrollmentState::Active,
    ))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    let candidate_updated = sqlx::query(
        "UPDATE runner_enrollment
            SET revision = 2, state_kind = $2
          WHERE enrollment_id = $1 AND revision = 1 AND state_kind = $3",
    )
    .bind(activation.candidate.enrollment.into_uuid())
    .bind(runner_enrollment_state_to_str(
        RunnerEnrollmentState::Active,
    ))
    .bind(runner_enrollment_state_to_str(
        RunnerEnrollmentState::Pending,
    ))
    .execute(&mut *connection)
    .await?
    .rows_affected();
    if predecessor_updated != 1 || candidate_updated != 1 {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    Ok(())
}

async fn append_replacement_enrollment_audit(
    connection: &mut PgConnection,
    enrollment: &ReplacementEnrollmentRow,
    revision: u64,
    state: RunnerEnrollmentState,
) -> Result<(), RunnerProtocolStoreError> {
    sqlx::query(
        "INSERT INTO runner_enrollment_audit
            (enrollment_id, revision, runner_id, authentication_reference_id,
             allowed_class_count, state_kind)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(enrollment.enrollment.into_uuid())
    .bind(Decimal::from(revision))
    .bind(enrollment.runner.into_uuid())
    .bind(enrollment.authentication)
    .bind(enrollment.allowed_class_count)
    .bind(runner_enrollment_state_to_str(state))
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO runner_enrollment_audit_allowed_class
            (enrollment_id, revision, capability_class)
         SELECT enrollment_id, $2, capability_class
           FROM runner_enrollment_allowed_class
          WHERE enrollment_id = $1",
    )
    .bind(enrollment.enrollment.into_uuid())
    .bind(Decimal::from(revision))
    .execute(&mut *connection)
    .await?;
    Ok(())
}

#[derive(Default)]
struct ReplacementRecordEvidence {
    placement_event_ordinal: Option<u64>,
    placement_revision: Option<RunnerGeneration>,
    placement_state_kind: Option<String>,
    prior_runner: Option<RunnerId>,
    new_runner: Option<RunnerId>,
    sandbox: Option<RunnerSandboxProfile>,
    target_enrollment: Option<RunnerEnrollmentId>,
    target_registration_revision: Option<RunnerRegistrationRevision>,
    target_connection_epoch: Option<RunnerConnectionEpoch>,
    target_connection_event_ordinal: Option<u64>,
}

fn replacement_target_is_unavailable(error: &RunnerDomainError) -> bool {
    matches!(
        error,
        RunnerDomainError::SelectorMismatch
            | RunnerDomainError::CredentialProfileUnavailable
            | RunnerDomainError::WorkspaceCapabilityUnavailable
            | RunnerDomainError::SandboxProfileUnavailable
            | RunnerDomainError::RepositoryUnavailable
            | RunnerDomainError::ToolUnavailable
            | RunnerDomainError::ToolUndeclared(_)
    )
}

fn placement_recovery_state(
    state: &SessionRunnerPlacementState,
) -> Option<RunnerPlacementRecoveryState> {
    match state {
        SessionRunnerPlacementState::Unpinned => Some(RunnerPlacementRecoveryState::Unpinned),
        SessionRunnerPlacementState::Pinned(_) => Some(RunnerPlacementRecoveryState::Pinned),
        SessionRunnerPlacementState::RunnerLostBeforePin(_)
        | SessionRunnerPlacementState::RunnerLost(_) => None,
        SessionRunnerPlacementState::RunnerAbandoned(_) => {
            Some(RunnerPlacementRecoveryState::RunnerAbandoned)
        }
    }
}

async fn inspect_replacement_registry(
    connection: &mut PgConnection,
    command: DurableCommandId,
) -> Result<Option<CommandKind>, RunnerProtocolStoreError> {
    command_registry::inspect(connection, command)
        .await
        .map_err(|error| match error {
            RegistryInspectionError::Database(error) => RunnerProtocolStoreError::Database(error),
            RegistryInspectionError::Corruption(_) => {
                RunnerProtocolStoreError::Corruption(RunnerProtocolCorruption::CrossWiredReference)
            }
        })
}

async fn resolve_replacement_claim_winner(
    transaction: &mut Transaction<'_, Postgres>,
    command: ReplaceLostRunner,
) -> Result<ReplaceLostRunnerBeforePinOutcome, RunnerProtocolStoreError> {
    match inspect_replacement_registry(transaction.as_mut(), command.command()).await? {
        Some(CommandKind::ReplaceLostRunner) => {
            let (recorded, result) =
                load_replacement_record(transaction.as_mut(), command.command())
                    .await?
                    .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
            Ok(if recorded == command {
                ReplaceLostRunnerBeforePinOutcome::Recorded(result)
            } else {
                ReplaceLostRunnerBeforePinOutcome::ConflictingReuse {
                    command: command.command(),
                }
            })
        }
        Some(_) => Ok(ReplaceLostRunnerBeforePinOutcome::ConflictingReuse {
            command: command.command(),
        }),
        None => Err(RunnerProtocolCorruption::CrossWiredReference.into()),
    }
}

async fn insert_replacement_command(
    connection: &mut PgConnection,
    command: ReplaceLostRunner,
) -> Result<(), RunnerProtocolStoreError> {
    let (target_kind, target_runner, target_pending_request) = match command.replacement() {
        RunnerReplacementTarget::Runner(runner) => ("runner", Some(runner.into_uuid()), None),
        RunnerReplacementTarget::PendingEnrollment(request) => {
            ("pending_enrollment", None, Some(request.into_uuid()))
        }
        RunnerReplacementTarget::SameRunnerReenrollment(runner) => {
            ("same_runner_reenrollment", Some(runner.into_uuid()), None)
        }
    };
    sqlx::query(
        "INSERT INTO replace_lost_runner_command
            (command_id, command_kind, storage_version, session_id,
             expected_placement_revision, target_kind, target_runner_id,
             target_pending_request_id)
         VALUES ($1, $2, 1, $3, $4, $5, $6, $7)",
    )
    .bind(command.command().into_uuid())
    .bind(REPLACE_LOST_RUNNER_KIND)
    .bind(command.session().into_uuid())
    .bind(Decimal::from(command.expected_placement_revision().get()))
    .bind(target_kind)
    .bind(target_runner)
    .bind(target_pending_request)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn insert_workspace_free_replacement_stage(
    connection: &mut PgConnection,
    command: ReplaceLostRunner,
    lost_event_ordinal: u64,
    requested_working_directory: &RunnerWorkingDirectory,
    identities: PinnedRunnerReplacementIdentities,
) -> Result<(), RunnerProtocolStoreError> {
    sqlx::query(
        "INSERT INTO runner_workspace_free_replacement_stage
            (command_id, session_id, lost_placement_event_ordinal,
             lost_placement_revision, requested_working_directory,
             boundary_entry_id, boundary_frontier_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(command.command().into_uuid())
    .bind(command.session().into_uuid())
    .bind(Decimal::from(lost_event_ordinal))
    .bind(Decimal::from(command.expected_placement_revision().get()))
    .bind(requested_working_directory.as_str())
    .bind(identities.semantic_entry().into_uuid())
    .bind(identities.context_frontier().into_uuid())
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn insert_pinned_replacement_result(
    connection: &mut PgConnection,
    command: ReplaceLostRunner,
    applied: &ReplacedPinnedRunner,
    placement_event_ordinal: u64,
    target: &ReplacementTargetEvidence,
) -> Result<(), RunnerProtocolStoreError> {
    sqlx::query(
        "INSERT INTO replace_lost_runner_result
            (command_id, session_id, result_kind, placement_event_ordinal,
             placement_revision, placement_state_kind, prior_runner_id,
             new_runner_id, sandbox_profile, working_directory,
             target_enrollment_id, target_registration_revision,
             target_connection_epoch, target_connection_event_ordinal)
         VALUES ($1, $2, 'applied', $3, $4, 'pinned', $5, $6, $7, $8,
                 $9, $10, $11, $12)",
    )
    .bind(command.command().into_uuid())
    .bind(applied.session().into_uuid())
    .bind(Decimal::from(placement_event_ordinal))
    .bind(Decimal::from(applied.placement_revision().get()))
    .bind(applied.prior_runner().into_uuid())
    .bind(applied.new_runner().into_uuid())
    .bind(runner_sandbox_to_str(applied.sandbox()))
    .bind(applied.working_directory().as_str())
    .bind(target.enrollment.into_uuid())
    .bind(Decimal::from(target.registration_revision.get()))
    .bind(Decimal::from(target.connection_epoch.get()))
    .bind(Decimal::from(target.connection_event_ordinal))
    .execute(&mut *connection)
    .await?;
    Ok(())
}

fn map_snapshot_append_error(
    error: crate::model_execution::SnapshotAppendError,
) -> RunnerProtocolStoreError {
    match error {
        crate::model_execution::SnapshotAppendError::FrontierInsert(error)
        | crate::model_execution::SnapshotAppendError::MemberInsert(error) => {
            RunnerProtocolStoreError::Database(error)
        }
        crate::model_execution::SnapshotAppendError::MemberPositionOverflow => {
            RunnerProtocolStoreError::Corruption(RunnerProtocolCorruption::GenerationExhausted)
        }
    }
}

async fn insert_workspace_provisioning_authorization(
    connection: &mut PgConnection,
    command: ReplaceLostRunner,
    lost_event_ordinal: u64,
    target: &ReplacementTargetEvidence,
    authorization: &signalbox_domain::WorkspaceProvisioningAuthorization,
) -> Result<(), RunnerProtocolStoreError> {
    sqlx::query(
        "INSERT INTO runner_workspace_provisioning_authorization
            (authorization_id, command_id, session_id,
             lost_placement_event_ordinal, lost_placement_revision,
             successor_placement_revision, enrollment_id, runner_id,
             registration_revision, connection_epoch,
             connection_event_ordinal, repository_key, sandbox_profile,
             credential_profile_name)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                 $13, $14)",
    )
    .bind(authorization.authorization().into_uuid())
    .bind(command.command().into_uuid())
    .bind(command.session().into_uuid())
    .bind(Decimal::from(lost_event_ordinal))
    .bind(Decimal::from(command.expected_placement_revision().get()))
    .bind(Decimal::from(authorization.placement_revision().get()))
    .bind(target.enrollment.into_uuid())
    .bind(target.runner.into_uuid())
    .bind(Decimal::from(target.registration_revision.get()))
    .bind(Decimal::from(target.connection_epoch.get()))
    .bind(Decimal::from(target.connection_event_ordinal))
    .bind(authorization.repository().as_str())
    .bind(runner_sandbox_to_str(authorization.sandbox()))
    .bind(
        authorization
            .credential_profile()
            .map(CredentialProfileName::as_str),
    )
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn insert_replacement_provisioning_rejection(
    connection: &mut PgConnection,
    command: ReplaceLostRunner,
    rejection: RunnerReplacementProvisioningRejection,
    evidence: ReplacementRecordEvidence,
) -> Result<(), RunnerProtocolStoreError> {
    let rejection = match rejection {
        RunnerReplacementProvisioningRejection::SessionNotFound { session } => {
            ReplaceLostRunnerBeforePinRejection::SessionNotFound { session }
        }
        RunnerReplacementProvisioningRejection::RunnerPlacementNotFound { session } => {
            ReplaceLostRunnerBeforePinRejection::RunnerPlacementNotFound { session }
        }
        RunnerReplacementProvisioningRejection::PlacementRevisionMismatch {
            session,
            expected,
            current,
        } => ReplaceLostRunnerBeforePinRejection::PlacementRevisionMismatch {
            session,
            expected,
            current,
        },
        RunnerReplacementProvisioningRejection::PlacementNotLost {
            session,
            placement_revision,
            state,
        } => ReplaceLostRunnerBeforePinRejection::PlacementNotLost {
            session,
            placement_revision,
            state,
        },
        RunnerReplacementProvisioningRejection::ReplacementSameRunner { session, runner } => {
            ReplaceLostRunnerBeforePinRejection::ReplacementSameRunner { session, runner }
        }
        RunnerReplacementProvisioningRejection::ReplacementTargetUnavailable {
            session,
            target,
            reason,
        } => ReplaceLostRunnerBeforePinRejection::ReplacementTargetUnavailable {
            session,
            target,
            reason,
        },
    };
    insert_replacement_result(
        connection,
        command,
        ReplaceLostRunnerBeforePinResult::Rejected(rejection),
        evidence,
    )
    .await
}

async fn load_replacement_provisioning_outcome(
    connection: &mut PgConnection,
    command: ReplaceLostRunner,
) -> Result<Option<RunnerReplacementProvisioningOutcome>, RunnerProtocolStoreError> {
    let row = sqlx::query(
        "SELECT command.session_id, command.expected_placement_revision,
                command.target_kind, command.target_runner_id,
                command.target_pending_request_id,
                staged.authorization_id, staged.successor_placement_revision,
                staged.enrollment_id, staged.runner_id,
                staged.registration_revision, staged.repository_key,
                staged.sandbox_profile, staged.credential_profile_name,
                workspace_free.command_id AS workspace_free_stage_command_id,
                result.rejection_kind, result.target_unavailable_reason,
                result.placement_revision, result.placement_state_kind,
                result.new_runner_id
           FROM replace_lost_runner_command AS command
           LEFT JOIN runner_workspace_provisioning_authorization AS staged
             ON staged.command_id = command.command_id
            AND staged.session_id = command.session_id
           LEFT JOIN runner_workspace_free_replacement_stage AS workspace_free
             ON workspace_free.command_id = command.command_id
            AND workspace_free.session_id = command.session_id
           LEFT JOIN replace_lost_runner_result AS result
             ON result.command_id = command.command_id
            AND result.session_id = command.session_id
          WHERE command.command_id = $1",
    )
    .bind(command.command().into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let stored = ReplaceLostRunner::new(
        command.command(),
        session_id(row.decode_column("session_id")?),
        decode_runner_generation(row.decode_column("expected_placement_revision")?)?,
        decode_replacement_target(&row)?,
    );
    if stored != command {
        return Ok(Some(
            RunnerReplacementProvisioningOutcome::ConflictingReuse {
                command: command.command(),
            },
        ));
    }
    let staged: Option<Uuid> = row.decode_column("authorization_id")?;
    let workspace_free: Option<Uuid> = row.decode_column("workspace_free_stage_command_id")?;
    if staged.is_some() && workspace_free.is_some() {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    if let Some(staged) = staged {
        let profile: Option<String> = row.decode_column("credential_profile_name")?;
        let stage = RunnerReplacementProvisioningStage::from_stored(
            WorkspaceProvisioningAuthorizationId::from_uuid(staged),
            command.session(),
            decode_runner_generation(row.decode_column("successor_placement_revision")?)?,
            runner_enrollment_id(row.decode_column("enrollment_id")?),
            runner_id(row.decode_column("runner_id")?),
            decode_runner_generation(row.decode_column("registration_revision")?)?,
            repository_key(row.decode_column("repository_key")?)?,
            runner_sandbox_from_str(&row.decode_column::<String>("sandbox_profile")?)
                .ok_or(RunnerProtocolCorruption::InvalidEncoding)?,
            profile.map(profile_name).transpose()?,
        );
        return Ok(Some(RunnerReplacementProvisioningOutcome::Staged(stage)));
    }
    let rejection: Option<String> = row.decode_column("rejection_kind")?;
    let Some(rejection) = rejection else {
        if workspace_free == Some(command.command().into_uuid()) {
            return Ok(Some(RunnerReplacementProvisioningOutcome::NotApplicable));
        }
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    };
    let rejection = replace_lost_runner_rejection_from_str(&rejection)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
    let rejection = match rejection {
        ReplaceLostRunnerRejectionStorageKind::SessionNotFound => {
            RunnerReplacementProvisioningRejection::SessionNotFound {
                session: command.session(),
            }
        }
        ReplaceLostRunnerRejectionStorageKind::RunnerPlacementNotFound => {
            RunnerReplacementProvisioningRejection::RunnerPlacementNotFound {
                session: command.session(),
            }
        }
        ReplaceLostRunnerRejectionStorageKind::PlacementRevisionMismatch => {
            RunnerReplacementProvisioningRejection::PlacementRevisionMismatch {
                session: command.session(),
                expected: command.expected_placement_revision(),
                current: decode_runner_generation(row.decode_column("placement_revision")?)?,
            }
        }
        ReplaceLostRunnerRejectionStorageKind::PlacementNotLost => {
            let state: String = row.decode_column("placement_state_kind")?;
            RunnerReplacementProvisioningRejection::PlacementNotLost {
                session: command.session(),
                placement_revision: decode_runner_generation(
                    row.decode_column("placement_revision")?,
                )?,
                state: placement_recovery_state_from_str(&state)?,
            }
        }
        ReplaceLostRunnerRejectionStorageKind::ReplacementSameRunner => {
            RunnerReplacementProvisioningRejection::ReplacementSameRunner {
                session: command.session(),
                runner: runner_id(row.decode_column("new_runner_id")?),
            }
        }
        ReplaceLostRunnerRejectionStorageKind::ReplacementTargetUnavailable => {
            RunnerReplacementProvisioningRejection::ReplacementTargetUnavailable {
                session: command.session(),
                target: command.replacement(),
                reason: replacement_target_reason_from_str(
                    &row.decode_column::<String>("target_unavailable_reason")?,
                )?,
            }
        }
    };
    Ok(Some(RunnerReplacementProvisioningOutcome::Rejected(
        rejection,
    )))
}

async fn load_workspace_free_replacement_outcome(
    connection: &mut PgConnection,
    command: ReplaceLostRunner,
) -> Result<Option<PinnedRunnerReplacementOutcome>, RunnerProtocolStoreError> {
    let row = sqlx::query(
        "SELECT command.session_id, command.expected_placement_revision,
                command.target_kind, command.target_runner_id,
                command.target_pending_request_id,
                EXISTS (
                    SELECT 1
                      FROM runner_workspace_provisioning_authorization AS provisioning
                     WHERE provisioning.command_id = command.command_id
                       AND provisioning.session_id = command.session_id
                ) AS has_provisioning_stage,
                workspace_free.command_id AS workspace_free_stage_command_id,
                workspace_free.lost_placement_event_ordinal,
                workspace_free.lost_placement_revision,
                workspace_free.requested_working_directory,
                workspace_free.boundary_entry_id,
                workspace_free.boundary_frontier_id,
                EXISTS (
                    SELECT 1
                      FROM replace_lost_runner_result AS result
                     WHERE result.command_id = command.command_id
                       AND result.session_id = command.session_id
                ) AS has_result
           FROM replace_lost_runner_command AS command
           LEFT JOIN runner_workspace_free_replacement_stage AS workspace_free
             ON workspace_free.command_id = command.command_id
            AND workspace_free.session_id = command.session_id
          WHERE command.command_id = $1",
    )
    .bind(command.command().into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let stored = ReplaceLostRunner::new(
        command.command(),
        session_id(row.decode_column("session_id")?),
        decode_runner_generation(row.decode_column("expected_placement_revision")?)?,
        decode_replacement_target(&row)?,
    );
    if stored != command {
        return Ok(Some(PinnedRunnerReplacementOutcome::ConflictingReuse {
            command: command.command(),
        }));
    }
    let has_provisioning_stage: bool = row.decode_column("has_provisioning_stage")?;
    let workspace_free_stage: Option<Uuid> =
        row.decode_column("workspace_free_stage_command_id")?;
    let has_result: bool = row.decode_column("has_result")?;
    if has_provisioning_stage && workspace_free_stage.is_some() {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    if has_provisioning_stage {
        return Ok(Some(PinnedRunnerReplacementOutcome::NotApplicable));
    }
    if workspace_free_stage.is_some() {
        let stage_command = DurableCommandId::from_uuid(
            workspace_free_stage.ok_or(RunnerProtocolCorruption::CrossWiredReference)?,
        );
        let stage_revision =
            decode_runner_generation(row.decode_column("lost_placement_revision")?)?;
        let _stage_event_ordinal = decode_u64(row.decode_column("lost_placement_event_ordinal")?)?;
        let _stage_directory =
            RunnerWorkingDirectory::try_new(row.decode_column("requested_working_directory")?)
                .map_err(|_| RunnerProtocolCorruption::InvalidEncoding)?;
        let _: Uuid = row.decode_column("boundary_entry_id")?;
        let _: Uuid = row.decode_column("boundary_frontier_id")?;
        if stage_command != command.command()
            || stage_revision != command.expected_placement_revision()
        {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        if !has_result {
            return Ok(Some(PinnedRunnerReplacementOutcome::Staged {
                command: command.command(),
            }));
        }
    } else if !has_result {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let (recorded, result) = load_pinned_replacement_record(connection, command.command())
        .await?
        .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
    if recorded != command {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    Ok(Some(PinnedRunnerReplacementOutcome::Recorded(result)))
}

async fn load_pinned_replacement_record(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<(ReplaceLostRunner, PinnedRunnerReplacementResult)>, RunnerProtocolStoreError> {
    let row = sqlx::query(
        "SELECT command.session_id, command.expected_placement_revision,
                command.target_kind, command.target_runner_id,
                command.target_pending_request_id, result.result_kind,
                result.placement_event_ordinal, result.placement_state_kind,
                result.placement_revision,
                result.prior_runner_id, result.new_runner_id,
                result.working_directory, result.sandbox_profile,
                result.target_enrollment_id,
                result.target_registration_revision,
                result.target_connection_epoch,
                result.target_connection_event_ordinal,
                target_registration.enrollment_id AS target_registration_enrollment_id,
                target_registration.runner_id AS target_registration_runner_id,
                target_registration.authentication_reference_id
                    AS target_registration_authentication_id,
                target_enrollment.runner_id AS target_enrollment_runner_id,
                target_enrollment.state_kind AS target_enrollment_state_kind,
                target_connection.state_kind AS target_connection_state_kind,
                pending.request_id AS pending_request_id,
                pending.enrollment_id AS pending_enrollment_id,
                pending.predecessor_enrollment_id,
                pending.predecessor_loss_epoch,
                pending_predecessor.runner_id AS pending_predecessor_runner_id,
                pending_predecessor.state_kind AS pending_predecessor_state_kind,
                pending_loss.enrollment_id AS pending_loss_enrollment_id,
                pending_loss.loss_epoch AS pending_loss_epoch,
                pending_candidate_audit.state_kind AS pending_candidate_state_kind,
                active_candidate_audit.state_kind AS active_candidate_state_kind,
                stage.lost_placement_event_ordinal,
                stage.lost_placement_revision,
                stage.requested_working_directory AS stage_working_directory,
                stage.boundary_entry_id AS stage_boundary_entry_id,
                stage.boundary_frontier_id AS stage_boundary_frontier_id,
                lost.event_kind AS lost_event_kind,
                lost.state_kind AS lost_state_kind,
                lost.lost_runner_id AS lost_runner_id,
                lost.loss_source_kind AS lost_loss_source_kind,
                lost.loss_registration_revision,
                lost.loss_fence_enrollment_id,
                lost.observed_runner_loss_epoch,
                loss_registration.enrollment_id AS loss_registration_enrollment_id,
                loss_registration.runner_id AS loss_registration_runner_id,
                loss_registration.authentication_reference_id
                    AS loss_registration_authentication_id,
                placement.event_kind AS placement_event_kind,
                placement.state_kind AS stored_placement_state_kind,
                placement.pinned_runner_id AS placement_runner_id,
                placement.requested_working_directory AS placement_requested_directory,
                placement.pinned_working_directory AS placement_working_directory,
                placement.requested_sandbox_profile AS placement_sandbox_profile,
                boundary.semantic_entry_id AS boundary_entry_id,
                pointer.context_frontier_id AS boundary_frontier_id
           FROM replace_lost_runner_command AS command
           JOIN replace_lost_runner_result AS result
             ON result.command_id = command.command_id
            AND result.session_id = command.session_id
           LEFT JOIN runner_workspace_free_replacement_stage AS stage
             ON stage.command_id = result.command_id
            AND stage.session_id = result.session_id
           LEFT JOIN runner_session_placement_record AS lost
             ON lost.session_id = stage.session_id
            AND lost.event_ordinal = stage.lost_placement_event_ordinal
            AND lost.placement_revision = stage.lost_placement_revision
           LEFT JOIN runner_session_placement_record AS placement
             ON placement.session_id = result.session_id
            AND placement.event_ordinal = result.placement_event_ordinal
            AND placement.placement_revision = result.placement_revision
           LEFT JOIN session_runner_placement_frontier AS pointer
             ON pointer.session_id = result.session_id
            AND pointer.placement_revision = result.placement_revision
           LEFT JOIN semantic_transcript_entry AS boundary
             ON boundary.source_session_id = pointer.session_id
            AND boundary.semantic_entry_id = pointer.semantic_entry_id
            AND boundary.payload_kind = 'runner_placement_changed'
            AND boundary.runner_placement_revision = result.placement_revision
            AND boundary.runner_placement_event_ordinal =
                result.placement_event_ordinal
           LEFT JOIN runner_registration AS target_registration
             ON target_registration.enrollment_id = result.target_enrollment_id
            AND target_registration.registration_revision =
                result.target_registration_revision
           LEFT JOIN runner_enrollment AS target_enrollment
             ON target_enrollment.enrollment_id = result.target_enrollment_id
           LEFT JOIN runner_registration AS loss_registration
             ON loss_registration.enrollment_id =
                    lost.registration_enrollment_id
            AND loss_registration.registration_revision =
                    lost.loss_registration_revision
           LEFT JOIN runner_connection_event AS target_connection
             ON target_connection.enrollment_id = result.target_enrollment_id
            AND target_connection.connection_epoch = result.target_connection_epoch
            AND target_connection.event_ordinal =
                result.target_connection_event_ordinal
           LEFT JOIN runner_pending_enrollment AS pending
             ON pending.request_id = command.target_pending_request_id
           LEFT JOIN runner_enrollment AS pending_predecessor
             ON pending_predecessor.enrollment_id = pending.predecessor_enrollment_id
           LEFT JOIN runner_connection_loss_epoch AS pending_loss
             ON pending_loss.enrollment_id = pending.predecessor_enrollment_id
            AND pending_loss.loss_epoch = pending.predecessor_loss_epoch
           LEFT JOIN runner_enrollment_audit AS pending_candidate_audit
             ON pending_candidate_audit.enrollment_id = pending.enrollment_id
            AND pending_candidate_audit.revision = 1
           LEFT JOIN runner_enrollment_audit AS active_candidate_audit
             ON active_candidate_audit.enrollment_id = pending.enrollment_id
            AND active_candidate_audit.revision = 2
          WHERE command.command_id = $1",
    )
    .bind(command_id.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let session = session_id(row.decode_column("session_id")?);
    let expected = decode_runner_generation(row.decode_column("expected_placement_revision")?)?;
    let replacement = decode_replacement_target(&row)?;
    let command = ReplaceLostRunner::new(command_id, session, expected, replacement);
    let result_kind: String = row.decode_column("result_kind")?;
    if replace_lost_runner_result_from_str(&result_kind)
        == Some(ReplaceLostRunnerResultStorageKind::Rejected)
    {
        let (_, result) = load_replacement_record(connection, command_id)
            .await?
            .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
        let ReplaceLostRunnerBeforePinResult::Rejected(rejection) = result else {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        };
        return Ok(Some((
            command,
            PinnedRunnerReplacementResult::Rejected(pinned_replacement_rejection(rejection)),
        )));
    }
    if replace_lost_runner_result_from_str(&result_kind)
        != Some(ReplaceLostRunnerResultStorageKind::Applied)
    {
        return Err(RunnerProtocolCorruption::InvalidEncoding.into());
    }
    let placement_state: String = row.decode_column("placement_state_kind")?;
    if placement_state != "pinned" {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let prior_runner = runner_id(row.decode_column("prior_runner_id")?);
    let new_runner = runner_id(row.decode_column("new_runner_id")?);
    let revision = decode_runner_generation(row.decode_column("placement_revision")?)?;
    let event_ordinal = decode_u64(row.decode_column("placement_event_ordinal")?)?;
    let working_directory =
        RunnerWorkingDirectory::try_new(row.decode_column("working_directory")?)
            .map_err(|_| RunnerProtocolCorruption::InvalidEncoding)?;
    let sandbox: String = row.decode_column("sandbox_profile")?;
    let sandbox =
        runner_sandbox_from_str(&sandbox).ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
    let target_enrollment: Uuid = row.decode_column("target_enrollment_id")?;
    let target_registration_revision =
        decode_registration_revision(row.decode_column("target_registration_revision")?)?;
    let _ = RunnerConnectionEpoch::try_from_u64(decode_u64(
        row.decode_column("target_connection_epoch")?,
    )?)
    .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
    let _ = decode_u64(row.decode_column("target_connection_event_ordinal")?)?;
    let target_registration_enrollment: Uuid =
        row.decode_column("target_registration_enrollment_id")?;
    let target_registration_runner = runner_id(row.decode_column("target_registration_runner_id")?);
    let target_registration_authentication: Uuid =
        row.decode_column("target_registration_authentication_id")?;
    let target_enrollment_runner = runner_id(row.decode_column("target_enrollment_runner_id")?);
    let target_enrollment_state: String = row.decode_column("target_enrollment_state_kind")?;
    let target_connection_state: String = row.decode_column("target_connection_state_kind")?;
    let pending_request: Option<Uuid> = row.decode_column("pending_request_id")?;
    let pending_enrollment: Option<Uuid> = row.decode_column("pending_enrollment_id")?;
    let pending_predecessor_enrollment: Option<Uuid> =
        row.decode_column("predecessor_enrollment_id")?;
    let pending_predecessor_loss_epoch: Option<Decimal> =
        row.decode_column("predecessor_loss_epoch")?;
    let pending_predecessor_runner: Option<Uuid> =
        row.decode_column("pending_predecessor_runner_id")?;
    let pending_predecessor_state: Option<String> =
        row.decode_column("pending_predecessor_state_kind")?;
    let pending_loss_enrollment: Option<Uuid> = row.decode_column("pending_loss_enrollment_id")?;
    let pending_loss_epoch: Option<Decimal> = row.decode_column("pending_loss_epoch")?;
    let pending_candidate_state: Option<String> =
        row.decode_column("pending_candidate_state_kind")?;
    let active_candidate_state: Option<String> =
        row.decode_column("active_candidate_state_kind")?;
    let lost_event_ordinal = decode_u64(row.decode_column("lost_placement_event_ordinal")?)?;
    let lost_revision = decode_runner_generation(row.decode_column("lost_placement_revision")?)?;
    let stage_working_directory: String = row.decode_column("stage_working_directory")?;
    let lost_event_kind: String = row.decode_column("lost_event_kind")?;
    let lost_state_kind: String = row.decode_column("lost_state_kind")?;
    let lost_runner = runner_id(row.decode_column("lost_runner_id")?);
    let lost_loss_source: String = row.decode_column("lost_loss_source_kind")?;
    let loss_registration_revision: Option<Decimal> =
        row.decode_column("loss_registration_revision")?;
    let lost_loss_fence_enrollment: Option<Uuid> = row.decode_column("loss_fence_enrollment_id")?;
    let lost_observed_loss_epoch: Option<Decimal> =
        row.decode_column("observed_runner_loss_epoch")?;
    let loss_registration_enrollment: Option<Uuid> =
        row.decode_column("loss_registration_enrollment_id")?;
    let loss_registration_runner: Option<Uuid> =
        row.decode_column("loss_registration_runner_id")?;
    let loss_registration_authentication: Option<Uuid> =
        row.decode_column("loss_registration_authentication_id")?;
    let placement_event_kind: String = row.decode_column("placement_event_kind")?;
    let stored_placement_state_kind: String = row.decode_column("stored_placement_state_kind")?;
    let placement_runner = runner_id(row.decode_column("placement_runner_id")?);
    let placement_requested_directory: String =
        row.decode_column("placement_requested_directory")?;
    let placement_working_directory: String = row.decode_column("placement_working_directory")?;
    let placement_sandbox_profile: String = row.decode_column("placement_sandbox_profile")?;
    let stage_boundary_entry: Uuid = row.decode_column("stage_boundary_entry_id")?;
    let stage_boundary_frontier: Uuid = row.decode_column("stage_boundary_frontier_id")?;
    let boundary_entry: Uuid = row.decode_column("boundary_entry_id")?;
    let boundary_frontier: Uuid = row.decode_column("boundary_frontier_id")?;
    let target_matches = match command.replacement() {
        RunnerReplacementTarget::Runner(runner) => {
            runner == new_runner && prior_runner != new_runner
        }
        RunnerReplacementTarget::SameRunnerReenrollment(runner) => {
            runner == new_runner
                && prior_runner == new_runner
                && lost_loss_source == "registration"
                && loss_registration_revision
                    .map(decode_registration_revision)
                    .transpose()?
                    .is_some_and(|revision| revision <= target_registration_revision)
                && loss_registration_enrollment == Some(target_enrollment)
                && loss_registration_runner == Some(new_runner.into_uuid())
                && loss_registration_authentication == Some(target_registration_authentication)
        }
        RunnerReplacementTarget::PendingEnrollment(request) => {
            pending_request == Some(request.into_uuid())
                && pending_enrollment == Some(target_enrollment)
                && pending_predecessor_enrollment == lost_loss_fence_enrollment
                && lost_observed_loss_epoch
                    .is_none_or(|observed| Some(observed) < pending_predecessor_loss_epoch)
                && pending_predecessor_runner == Some(prior_runner.into_uuid())
                && pending_predecessor_state.as_deref() == Some("revoked")
                && pending_loss_enrollment == pending_predecessor_enrollment
                && pending_loss_epoch == pending_predecessor_loss_epoch
                && pending_candidate_state.as_deref() == Some("pending")
                && active_candidate_state.as_deref() == Some("active")
                && lost_loss_source == "connection"
                && prior_runner != new_runner
        }
    };
    if !target_matches
        || target_registration_enrollment != target_enrollment
        || target_registration_runner != new_runner
        || target_enrollment_runner != new_runner
        || target_enrollment_state != "active"
        || runner_connection_state_from_str(&target_connection_state)
            != Some(RunnerConnectionState::Connected)
        || lost_event_kind != "runner_lost"
        || lost_state_kind != "runner_lost"
        || lost_runner != prior_runner
        || lost_revision != expected
        || lost_event_ordinal.checked_add(1) != Some(event_ordinal)
        || stage_working_directory != working_directory.as_str()
        || placement_event_kind != "runner_replaced"
        || stored_placement_state_kind != "pinned"
        || placement_runner != new_runner
        || placement_requested_directory != working_directory.as_str()
        || placement_working_directory != working_directory.as_str()
        || runner_sandbox_from_str(&placement_sandbox_profile) != Some(sandbox)
        || boundary_entry != stage_boundary_entry
        || boundary_frontier != stage_boundary_frontier
    {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    Ok(Some((
        command,
        PinnedRunnerReplacementResult::Applied(ReplacedPinnedRunner::new(
            session,
            prior_runner,
            new_runner,
            revision,
            working_directory,
            sandbox,
        )),
    )))
}

const fn pinned_replacement_rejection(
    rejection: ReplaceLostRunnerBeforePinRejection,
) -> RunnerReplacementProvisioningRejection {
    match rejection {
        ReplaceLostRunnerBeforePinRejection::SessionNotFound { session } => {
            RunnerReplacementProvisioningRejection::SessionNotFound { session }
        }
        ReplaceLostRunnerBeforePinRejection::RunnerPlacementNotFound { session } => {
            RunnerReplacementProvisioningRejection::RunnerPlacementNotFound { session }
        }
        ReplaceLostRunnerBeforePinRejection::PlacementRevisionMismatch {
            session,
            expected,
            current,
        } => RunnerReplacementProvisioningRejection::PlacementRevisionMismatch {
            session,
            expected,
            current,
        },
        ReplaceLostRunnerBeforePinRejection::PlacementNotLost {
            session,
            placement_revision,
            state,
        } => RunnerReplacementProvisioningRejection::PlacementNotLost {
            session,
            placement_revision,
            state,
        },
        ReplaceLostRunnerBeforePinRejection::ReplacementSameRunner { session, runner } => {
            RunnerReplacementProvisioningRejection::ReplacementSameRunner { session, runner }
        }
        ReplaceLostRunnerBeforePinRejection::ReplacementTargetUnavailable {
            session,
            target,
            reason,
        } => RunnerReplacementProvisioningRejection::ReplacementTargetUnavailable {
            session,
            target,
            reason,
        },
    }
}

async fn insert_replacement_result(
    connection: &mut PgConnection,
    command: ReplaceLostRunner,
    result: ReplaceLostRunnerBeforePinResult,
    evidence: ReplacementRecordEvidence,
) -> Result<(), RunnerProtocolStoreError> {
    let mut rejection_kind = None;
    let mut target_reason = None;
    let result_kind = match result {
        ReplaceLostRunnerBeforePinResult::Applied(_) => ReplaceLostRunnerResultStorageKind::Applied,
        ReplaceLostRunnerBeforePinResult::Rejected(rejection) => {
            rejection_kind = Some(replace_lost_runner_rejection_to_str(match rejection {
                ReplaceLostRunnerBeforePinRejection::SessionNotFound { .. } => {
                    ReplaceLostRunnerRejectionStorageKind::SessionNotFound
                }
                ReplaceLostRunnerBeforePinRejection::RunnerPlacementNotFound { .. } => {
                    ReplaceLostRunnerRejectionStorageKind::RunnerPlacementNotFound
                }
                ReplaceLostRunnerBeforePinRejection::PlacementRevisionMismatch { .. } => {
                    ReplaceLostRunnerRejectionStorageKind::PlacementRevisionMismatch
                }
                ReplaceLostRunnerBeforePinRejection::PlacementNotLost { .. } => {
                    ReplaceLostRunnerRejectionStorageKind::PlacementNotLost
                }
                ReplaceLostRunnerBeforePinRejection::ReplacementSameRunner { .. } => {
                    ReplaceLostRunnerRejectionStorageKind::ReplacementSameRunner
                }
                ReplaceLostRunnerBeforePinRejection::ReplacementTargetUnavailable {
                    reason,
                    ..
                } => {
                    target_reason = Some(replacement_target_reason_to_str(reason));
                    ReplaceLostRunnerRejectionStorageKind::ReplacementTargetUnavailable
                }
            }));
            ReplaceLostRunnerResultStorageKind::Rejected
        }
    };
    sqlx::query(
        "INSERT INTO replace_lost_runner_result
            (command_id, session_id, result_kind, rejection_kind,
             target_unavailable_reason, placement_event_ordinal,
             placement_revision, placement_state_kind, prior_runner_id,
             new_runner_id, sandbox_profile, target_enrollment_id,
             target_registration_revision, target_connection_epoch,
             target_connection_event_ordinal)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                 $14, $15)",
    )
    .bind(command.command().into_uuid())
    .bind(command.session().into_uuid())
    .bind(replace_lost_runner_result_to_str(result_kind))
    .bind(rejection_kind)
    .bind(target_reason)
    .bind(evidence.placement_event_ordinal.map(Decimal::from))
    .bind(
        evidence
            .placement_revision
            .map(|revision| Decimal::from(revision.get())),
    )
    .bind(evidence.placement_state_kind)
    .bind(evidence.prior_runner.map(RunnerId::into_uuid))
    .bind(evidence.new_runner.map(RunnerId::into_uuid))
    .bind(evidence.sandbox.map(runner_sandbox_to_str))
    .bind(
        evidence
            .target_enrollment
            .map(RunnerEnrollmentId::into_uuid),
    )
    .bind(
        evidence
            .target_registration_revision
            .map(|revision| Decimal::from(revision.get())),
    )
    .bind(
        evidence
            .target_connection_epoch
            .map(|epoch| Decimal::from(epoch.get())),
    )
    .bind(evidence.target_connection_event_ordinal.map(Decimal::from))
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn load_replacement_record(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<(ReplaceLostRunner, ReplaceLostRunnerBeforePinResult)>, RunnerProtocolStoreError>
{
    let row = sqlx::query(
        "SELECT command.session_id, command.expected_placement_revision,
                command.target_kind, command.target_runner_id,
                command.target_pending_request_id, result.result_kind,
                result.rejection_kind, result.target_unavailable_reason,
                result.placement_revision, result.placement_state_kind,
                result.prior_runner_id, result.new_runner_id,
                result.sandbox_profile, result.target_enrollment_id,
                result.target_registration_revision,
                result.target_connection_epoch,
                result.target_connection_event_ordinal,
                target_registration.runner_id AS target_registration_runner_id,
                target_connection.state_kind AS target_connection_state_kind
           FROM replace_lost_runner_command AS command
           JOIN replace_lost_runner_result AS result
             ON result.command_id = command.command_id
            AND result.session_id = command.session_id
           LEFT JOIN runner_registration AS target_registration
             ON target_registration.enrollment_id = result.target_enrollment_id
            AND target_registration.registration_revision =
                result.target_registration_revision
           LEFT JOIN runner_connection_event AS target_connection
             ON target_connection.enrollment_id = result.target_enrollment_id
            AND target_connection.connection_epoch = result.target_connection_epoch
            AND target_connection.event_ordinal =
                result.target_connection_event_ordinal
          WHERE command.command_id = $1",
    )
    .bind(command_id.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let session = session_id(row.decode_column("session_id")?);
    let expected = decode_runner_generation(row.decode_column("expected_placement_revision")?)?;
    let replacement = decode_replacement_target(&row)?;
    let command = ReplaceLostRunner::new(command_id, session, expected, replacement);
    let result_kind: String = row.decode_column("result_kind")?;
    let result = match replace_lost_runner_result_from_str(&result_kind)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?
    {
        ReplaceLostRunnerResultStorageKind::Applied => {
            let prior_runner = runner_id(row.decode_column("prior_runner_id")?);
            let new_runner = runner_id(row.decode_column("new_runner_id")?);
            let revision = decode_runner_generation(row.decode_column("placement_revision")?)?;
            let sandbox: String = row.decode_column("sandbox_profile")?;
            let sandbox = runner_sandbox_from_str(&sandbox)
                .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
            let _: Uuid = row.decode_column("target_enrollment_id")?;
            let _ =
                decode_registration_revision(row.decode_column("target_registration_revision")?)?;
            let _ = RunnerConnectionEpoch::try_from_u64(decode_u64(
                row.decode_column("target_connection_epoch")?,
            )?)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
            let _ = decode_u64(row.decode_column("target_connection_event_ordinal")?)?;
            let target_registration_runner =
                runner_id(row.decode_column("target_registration_runner_id")?);
            let target_connection_state: String =
                row.decode_column("target_connection_state_kind")?;
            if target_registration_runner != new_runner
                || runner_connection_state_from_str(&target_connection_state)
                    != Some(RunnerConnectionState::Connected)
            {
                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
            }
            ReplaceLostRunnerBeforePinResult::Applied(ReplacedLostRunnerBeforePin::new(
                session,
                prior_runner,
                new_runner,
                revision,
                sandbox,
            ))
        }
        ReplaceLostRunnerResultStorageKind::Rejected => {
            let rejection: String = row.decode_column("rejection_kind")?;
            let rejection = replace_lost_runner_rejection_from_str(&rejection)
                .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
            ReplaceLostRunnerBeforePinResult::Rejected(match rejection {
                ReplaceLostRunnerRejectionStorageKind::SessionNotFound => {
                    ReplaceLostRunnerBeforePinRejection::SessionNotFound { session }
                }
                ReplaceLostRunnerRejectionStorageKind::RunnerPlacementNotFound => {
                    ReplaceLostRunnerBeforePinRejection::RunnerPlacementNotFound { session }
                }
                ReplaceLostRunnerRejectionStorageKind::PlacementRevisionMismatch => {
                    ReplaceLostRunnerBeforePinRejection::PlacementRevisionMismatch {
                        session,
                        expected,
                        current: decode_runner_generation(
                            row.decode_column("placement_revision")?,
                        )?,
                    }
                }
                ReplaceLostRunnerRejectionStorageKind::PlacementNotLost => {
                    let state: String = row.decode_column("placement_state_kind")?;
                    ReplaceLostRunnerBeforePinRejection::PlacementNotLost {
                        session,
                        placement_revision: decode_runner_generation(
                            row.decode_column("placement_revision")?,
                        )?,
                        state: placement_recovery_state_from_str(&state)?,
                    }
                }
                ReplaceLostRunnerRejectionStorageKind::ReplacementSameRunner => {
                    ReplaceLostRunnerBeforePinRejection::ReplacementSameRunner {
                        session,
                        runner: runner_id(row.decode_column("new_runner_id")?),
                    }
                }
                ReplaceLostRunnerRejectionStorageKind::ReplacementTargetUnavailable => {
                    let reason: String = row.decode_column("target_unavailable_reason")?;
                    ReplaceLostRunnerBeforePinRejection::ReplacementTargetUnavailable {
                        session,
                        target: replacement,
                        reason: replacement_target_reason_from_str(&reason)?,
                    }
                }
            })
        }
    };
    Ok(Some((command, result)))
}

fn decode_replacement_target(
    row: &PgRow,
) -> Result<RunnerReplacementTarget, RunnerProtocolStoreError> {
    let target_kind: String = row.decode_column("target_kind")?;
    match target_kind.as_str() {
        "runner" => Ok(RunnerReplacementTarget::Runner(runner_id(
            row.decode_column("target_runner_id")?,
        ))),
        "pending_enrollment" => Ok(RunnerReplacementTarget::PendingEnrollment(
            RunnerEnrollmentRequestId::from_uuid(row.decode_column("target_pending_request_id")?),
        )),
        "same_runner_reenrollment" => Ok(RunnerReplacementTarget::SameRunnerReenrollment(
            runner_id(row.decode_column("target_runner_id")?),
        )),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

const fn replacement_target_reason_to_str(
    reason: RunnerReplacementTargetUnavailableReason,
) -> &'static str {
    match reason {
        RunnerReplacementTargetUnavailableReason::NotConnected => "not_connected",
        RunnerReplacementTargetUnavailableReason::NotCurrent => "not_current",
        RunnerReplacementTargetUnavailableReason::NotAdvertised => "not_advertised",
        RunnerReplacementTargetUnavailableReason::PendingRequestMismatch => {
            "pending_request_mismatch"
        }
        RunnerReplacementTargetUnavailableReason::PendingRequestDisconnected => {
            "pending_request_disconnected"
        }
    }
}

fn replacement_target_reason_from_str(
    reason: &str,
) -> Result<RunnerReplacementTargetUnavailableReason, RunnerProtocolStoreError> {
    match reason {
        "not_connected" => Ok(RunnerReplacementTargetUnavailableReason::NotConnected),
        "not_current" => Ok(RunnerReplacementTargetUnavailableReason::NotCurrent),
        "not_advertised" => Ok(RunnerReplacementTargetUnavailableReason::NotAdvertised),
        "pending_request_mismatch" => {
            Ok(RunnerReplacementTargetUnavailableReason::PendingRequestMismatch)
        }
        "pending_request_disconnected" => {
            Ok(RunnerReplacementTargetUnavailableReason::PendingRequestDisconnected)
        }
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

fn placement_recovery_state_from_str(
    state: &str,
) -> Result<RunnerPlacementRecoveryState, RunnerProtocolStoreError> {
    match state {
        "unpinned" => Ok(RunnerPlacementRecoveryState::Unpinned),
        "pinned" => Ok(RunnerPlacementRecoveryState::Pinned),
        "runner_abandoned" => Ok(RunnerPlacementRecoveryState::RunnerAbandoned),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

async fn require_runner_loss_session_scheduler(
    transaction: &mut Transaction<'_, Postgres>,
    session: SessionId,
) -> Result<(), RunnerProtocolStoreError> {
    let scheduler = sqlx::query_scalar::<_, Uuid>(RUNNER_RETRY_REPLACEMENT_SCHEDULER)
        .bind(session.into_uuid())
        .fetch_optional(&mut **transaction)
        .await?;
    if scheduler.is_none() {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    Ok(())
}

#[derive(Default)]
struct AbandonmentRecordEvidence {
    placement_event_ordinal: Option<u64>,
    placement_revision: Option<RunnerGeneration>,
    placement_state_kind: Option<String>,
    active_turn: Option<TurnId>,
}

async fn inspect_abandonment_registry(
    connection: &mut PgConnection,
    command: DurableCommandId,
) -> Result<Option<CommandKind>, RunnerProtocolStoreError> {
    command_registry::inspect(connection, command)
        .await
        .map_err(|error| match error {
            RegistryInspectionError::Database(error) => RunnerProtocolStoreError::Database(error),
            RegistryInspectionError::Corruption(_) => {
                RunnerProtocolStoreError::Corruption(RunnerProtocolCorruption::CrossWiredReference)
            }
        })
}

async fn resolve_abandonment_claim_winner(
    transaction: &mut Transaction<'_, Postgres>,
    command: AbandonLostRunner,
) -> Result<AbandonLostRunnerOutcome, RunnerProtocolStoreError> {
    match inspect_abandonment_registry(transaction.as_mut(), command.command()).await? {
        Some(CommandKind::AbandonLostRunner) => {
            let (recorded, result) =
                load_abandonment_record(transaction.as_mut(), command.command())
                    .await?
                    .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
            Ok(if recorded == command {
                AbandonLostRunnerOutcome::Recorded(result)
            } else {
                AbandonLostRunnerOutcome::ConflictingReuse {
                    command: command.command(),
                }
            })
        }
        Some(_) => Ok(AbandonLostRunnerOutcome::ConflictingReuse {
            command: command.command(),
        }),
        None => Err(RunnerProtocolCorruption::CrossWiredReference.into()),
    }
}

async fn insert_abandonment_record(
    connection: &mut PgConnection,
    command: AbandonLostRunner,
    result: AbandonLostRunnerResult,
    evidence: AbandonmentRecordEvidence,
) -> Result<(), RunnerProtocolStoreError> {
    let (result_kind, rejection_kind) = match result {
        AbandonLostRunnerResult::Applied(_) => (AbandonLostRunnerResultStorageKind::Applied, None),
        AbandonLostRunnerResult::Rejected(rejection) => (
            AbandonLostRunnerResultStorageKind::Rejected,
            Some(abandon_lost_runner_rejection_to_str(match rejection {
                AbandonLostRunnerRejection::SessionNotFound { .. } => {
                    AbandonLostRunnerRejectionStorageKind::SessionNotFound
                }
                AbandonLostRunnerRejection::RunnerPlacementNotFound { .. } => {
                    AbandonLostRunnerRejectionStorageKind::RunnerPlacementNotFound
                }
                AbandonLostRunnerRejection::PlacementRevisionMismatch { .. } => {
                    AbandonLostRunnerRejectionStorageKind::PlacementRevisionMismatch
                }
                AbandonLostRunnerRejection::PlacementNotLost { .. } => {
                    AbandonLostRunnerRejectionStorageKind::PlacementNotLost
                }
                AbandonLostRunnerRejection::ActiveTurnRequiresExistingControl { .. } => {
                    AbandonLostRunnerRejectionStorageKind::ActiveTurnRequiresExistingControl
                }
            })),
        ),
    };
    sqlx::query(
        "INSERT INTO abandon_lost_runner_command
            (command_id, command_kind, storage_version, session_id,
             expected_placement_revision, result_kind, rejection_kind,
             placement_event_ordinal, placement_revision,
             placement_state_kind, active_turn_id)
         VALUES ($1, $2, 1, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(command.command().into_uuid())
    .bind(ABANDON_LOST_RUNNER_KIND)
    .bind(command.session().into_uuid())
    .bind(Decimal::from(command.expected_placement_revision().get()))
    .bind(abandon_lost_runner_result_to_str(result_kind))
    .bind(rejection_kind)
    .bind(evidence.placement_event_ordinal.map(Decimal::from))
    .bind(
        evidence
            .placement_revision
            .map(|revision| Decimal::from(revision.get())),
    )
    .bind(evidence.placement_state_kind)
    .bind(evidence.active_turn.map(TurnId::into_uuid))
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn load_abandonment_record(
    connection: &mut PgConnection,
    command: DurableCommandId,
) -> Result<Option<(AbandonLostRunner, AbandonLostRunnerResult)>, RunnerProtocolStoreError> {
    let row = sqlx::query(
        "SELECT session_id, expected_placement_revision, result_kind,
                rejection_kind, placement_revision, placement_state_kind,
                active_turn_id
           FROM abandon_lost_runner_command
          WHERE command_id = $1",
    )
    .bind(command.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let session = session_id(row.decode_column("session_id")?);
    let expected = decode_runner_generation(row.decode_column("expected_placement_revision")?)?;
    let recorded = AbandonLostRunner::new(command, session, expected);
    let result_kind =
        abandon_lost_runner_result_from_str(&row.decode_column::<String>("result_kind")?)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
    let rejection_kind = match row.decode_column::<Option<String>>("rejection_kind")? {
        Some(value) => Some(
            abandon_lost_runner_rejection_from_str(&value)
                .ok_or(RunnerProtocolCorruption::InvalidEncoding)?,
        ),
        None => None,
    };
    let placement_revision = row
        .decode_column::<Option<Decimal>>("placement_revision")?
        .map(decode_runner_generation)
        .transpose()?;
    let placement_state: Option<String> = row.decode_column("placement_state_kind")?;
    let active_turn = row
        .decode_column::<Option<Uuid>>("active_turn_id")?
        .map(TurnId::from_uuid);
    let result =
        match (result_kind, rejection_kind) {
            (AbandonLostRunnerResultStorageKind::Applied, None) => {
                AbandonLostRunnerResult::Applied(AbandonedLostRunner::new(
                    session,
                    placement_revision.ok_or(RunnerProtocolCorruption::InvalidEncoding)?,
                ))
            }
            (
                AbandonLostRunnerResultStorageKind::Rejected,
                Some(AbandonLostRunnerRejectionStorageKind::SessionNotFound),
            ) => AbandonLostRunnerResult::Rejected(AbandonLostRunnerRejection::SessionNotFound {
                session,
            }),
            (
                AbandonLostRunnerResultStorageKind::Rejected,
                Some(AbandonLostRunnerRejectionStorageKind::RunnerPlacementNotFound),
            ) => AbandonLostRunnerResult::Rejected(
                AbandonLostRunnerRejection::RunnerPlacementNotFound { session },
            ),
            (
                AbandonLostRunnerResultStorageKind::Rejected,
                Some(AbandonLostRunnerRejectionStorageKind::PlacementRevisionMismatch),
            ) => AbandonLostRunnerResult::Rejected(
                AbandonLostRunnerRejection::PlacementRevisionMismatch {
                    session,
                    expected,
                    current: placement_revision.ok_or(RunnerProtocolCorruption::InvalidEncoding)?,
                },
            ),
            (
                AbandonLostRunnerResultStorageKind::Rejected,
                Some(AbandonLostRunnerRejectionStorageKind::PlacementNotLost),
            ) => AbandonLostRunnerResult::Rejected(AbandonLostRunnerRejection::PlacementNotLost {
                session,
                placement_revision: placement_revision
                    .ok_or(RunnerProtocolCorruption::InvalidEncoding)?,
                state: decode_runner_placement_recovery_state(
                    placement_state
                        .as_deref()
                        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?,
                )?,
            }),
            (
                AbandonLostRunnerResultStorageKind::Rejected,
                Some(AbandonLostRunnerRejectionStorageKind::ActiveTurnRequiresExistingControl),
            ) => AbandonLostRunnerResult::Rejected(
                AbandonLostRunnerRejection::ActiveTurnRequiresExistingControl {
                    session,
                    active_turn: active_turn.ok_or(RunnerProtocolCorruption::InvalidEncoding)?,
                },
            ),
            _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
        };
    Ok(Some((recorded, result)))
}

fn decode_runner_generation(value: Decimal) -> Result<RunnerGeneration, RunnerProtocolStoreError> {
    RunnerGeneration::try_from_u64(decode_u64(value)?)
        .ok_or_else(|| RunnerProtocolCorruption::InvalidEncoding.into())
}

fn is_lower_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn decode_runner_placement_recovery_state(
    value: &str,
) -> Result<RunnerPlacementRecoveryState, RunnerProtocolStoreError> {
    match value {
        "unpinned" => Ok(RunnerPlacementRecoveryState::Unpinned),
        "pinned" => Ok(RunnerPlacementRecoveryState::Pinned),
        "runner_abandoned" => Ok(RunnerPlacementRecoveryState::RunnerAbandoned),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

async fn require_runner_loss_authority(
    transaction: &mut Transaction<'_, Postgres>,
    loss: RunnerConnectionLossSnapshot,
) -> Result<(), RunnerProtocolStoreError> {
    let enrollment = sqlx::query_scalar::<_, Uuid>(RUNNER_ENROLLMENT)
        .bind(loss.enrollment().into_uuid())
        .fetch_optional(&mut **transaction)
        .await?;
    if enrollment.is_none() {
        return Err(RunnerProtocolCorruption::MissingCanonicalEnrollment.into());
    }
    sqlx::query_scalar::<_, Decimal>(RUNNER_PLACEMENT_CONNECTION_AUTHORITY)
        .bind(loss.enrollment().into_uuid())
        .fetch_optional(&mut **transaction)
        .await?;
    let current_loss = sqlx::query_scalar::<_, Decimal>(RUNNER_CONNECTION_LOSS_HEAD)
        .bind(loss.enrollment().into_uuid())
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalLoss)?;
    if decode_u64(current_loss)? < loss.loss_epoch().get() {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let source = sqlx::query(
        "SELECT connection_epoch, connection_event_ordinal
           FROM runner_connection_loss_epoch
          WHERE enrollment_id = $1 AND loss_epoch = $2",
    )
    .bind(loss.enrollment().into_uuid())
    .bind(Decimal::from(loss.loss_epoch().get()))
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(RunnerProtocolCorruption::MissingCanonicalLoss)?;
    if decode_u64(source.decode_column("connection_epoch")?)? != loss.connection_epoch().get()
        || decode_u64(source.decode_column("connection_event_ordinal")?)?
            != loss.connection_event_ordinal()
    {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    Ok(())
}

async fn lock_runner_loss_propagation(
    transaction: &mut Transaction<'_, Postgres>,
    loss: RunnerConnectionLossSnapshot,
) -> Result<LockedRunnerLossPropagation, RunnerProtocolStoreError> {
    let row = sqlx::query(RUNNER_CONNECTION_LOSS_PROPAGATION)
        .bind(loss.enrollment().into_uuid())
        .bind(Decimal::from(loss.loss_epoch().get()))
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalLoss)?;
    if decode_u64(row.decode_column("connection_epoch")?)? != loss.connection_epoch().get()
        || decode_u64(row.decode_column("connection_event_ordinal")?)?
            != loss.connection_event_ordinal()
    {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let state: String = row.decode_column("state_kind")?;
    let complete = match state.as_str() {
        "pending" => false,
        "completed" => true,
        _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    };
    Ok(LockedRunnerLossPropagation {
        propagated_through: row
            .decode_column::<Option<Uuid>>("propagated_through_session_id")?
            .map(session_id),
        complete,
    })
}

fn placement_is_affected_by_loss(
    placement: &PgRow,
    loss: RunnerConnectionLossSnapshot,
) -> Result<bool, RunnerProtocolStoreError> {
    let enrollment = placement.decode_column::<Option<Uuid>>("loss_fence_enrollment_id")?;
    let observed = placement
        .decode_column::<Option<Decimal>>("observed_runner_loss_epoch")?
        .map(decode_u64)
        .transpose()?;
    let state: String = placement.decode_column("state_kind")?;
    let selector: String = placement.decode_column("selector_kind")?;
    Ok(enrollment == Some(loss.enrollment().into_uuid())
        && observed.is_none_or(|epoch| epoch < loss.loss_epoch().get())
        && (state == "pinned" || (state == "unpinned" && selector == "identity")))
}

async fn advance_runner_loss_cursor(
    transaction: &mut Transaction<'_, Postgres>,
    loss: RunnerConnectionLossSnapshot,
    session: SessionId,
) -> Result<(), RunnerProtocolStoreError> {
    let changed = sqlx::query(
        "UPDATE runner_connection_loss_propagation
            SET propagated_through_session_id = $3
          WHERE enrollment_id = $1 AND loss_epoch = $2
            AND state_kind = 'pending'",
    )
    .bind(loss.enrollment().into_uuid())
    .bind(Decimal::from(loss.loss_epoch().get()))
    .bind(session.into_uuid())
    .execute(&mut **transaction)
    .await?
    .rows_affected();
    if changed != 1 {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    Ok(())
}

async fn retire_workspace_releases_for_connection_loss(
    transaction: &mut Transaction<'_, Postgres>,
    loss: RunnerConnectionLossSnapshot,
    session: SessionId,
) -> Result<(), RunnerProtocolStoreError> {
    sqlx::query(
        "SELECT placement_revision
           FROM runner_workspace_release
          WHERE session_id = $1
            AND enrollment_id = $2
            AND connection_epoch = $3
          FOR UPDATE",
    )
    .bind(session.into_uuid())
    .bind(loss.enrollment().into_uuid())
    .bind(Decimal::from(loss.connection_epoch().get()))
    .fetch_all(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_workspace_release_loss_retirement
            (session_id, placement_revision, runner_id, manifest_id,
             enrollment_id, connection_epoch, loss_epoch,
             connection_event_ordinal)
         SELECT release.session_id, release.placement_revision,
                release.runner_id, release.manifest_id,
                release.enrollment_id, release.connection_epoch,
                $4, $5
           FROM runner_workspace_release AS release
          WHERE release.session_id = $1
            AND release.enrollment_id = $2
            AND release.connection_epoch = $3
            AND NOT EXISTS (
                SELECT 1
                  FROM runner_workspace_release_acknowledgement AS acknowledgement
                 WHERE acknowledgement.session_id = release.session_id
                   AND acknowledgement.placement_revision =
                        release.placement_revision
            )
            AND NOT EXISTS (
                SELECT 1
                  FROM runner_workspace_release_loss_retirement AS retirement
                 WHERE retirement.session_id = release.session_id
                   AND retirement.placement_revision =
                        release.placement_revision
            )
            AND NOT EXISTS (
                SELECT 1
                  FROM runner_operation_failure AS failure
                 WHERE failure.operation_kind = 'workspace_release'
                   AND failure.release_session_id = release.session_id
                   AND failure.release_placement_revision =
                        release.placement_revision
            )",
    )
    .bind(session.into_uuid())
    .bind(loss.enrollment().into_uuid())
    .bind(Decimal::from(loss.connection_epoch().get()))
    .bind(Decimal::from(loss.loss_epoch().get()))
    .bind(Decimal::from(loss.connection_event_ordinal()))
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn lost_runner_working_directory(
    placement: &SessionRunnerPlacement,
) -> Option<RunnerWorkingDirectory> {
    match &placement.request().working_directory {
        WorkingDirectorySelection::Exact(directory) => Some(directory.clone()),
        WorkingDirectorySelection::RunnerDefault => None,
    }
}

async fn persist_runner_loss_lease_and_wait(
    transaction: &mut Transaction<'_, Postgres>,
    placement: &SessionRunnerPlacement,
    lease: RunnerLease,
) -> Result<(), RunnerProtocolStoreError> {
    let correlation = lease.correlation();
    if correlation.dispatch.session() != placement.session()
        || placement_loss_fence_runner(placement) != Some(correlation.runner)
    {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let side_effecting = lease.effect() == RunnerToolEffectClass::SideEffecting;
    let execution_possible = lease.state() == RunnerLeaseState::Claimed;
    match lease.state() {
        RunnerLeaseState::Offered => {
            append_lost_unclaimed_lease_event(transaction, &lease).await?;
        }
        RunnerLeaseState::Claimed => {
            let loss = lease.lose().map_err(RunnerProtocolStoreError::Domain)?;
            append_lease_event_in(transaction, loss.lost()).await?;
            if side_effecting && loss.crash_attempt() != Some(correlation.dispatch.attempt()) {
                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
            }
        }
        RunnerLeaseState::Completed
        | RunnerLeaseState::LostUnclaimed
        | RunnerLeaseState::LostExecutionPossible
        | RunnerLeaseState::LostClaimed => {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
    }
    if side_effecting && execution_possible {
        terminalize_runner_loss_attempt_ambiguous(transaction, &correlation).await?;
    }
    yield_turn_to_runner_recovery(transaction, placement, &correlation).await
}

async fn append_lost_unclaimed_lease_event(
    transaction: &mut Transaction<'_, Postgres>,
    lease: &RunnerLease,
) -> Result<(), RunnerProtocolStoreError> {
    let correlation = lease.correlation();
    let current = sqlx::query(RUNNER_LEASE_HEAD)
        .bind(correlation.lease.into_uuid())
        .bind(Decimal::from(correlation.generation.get()))
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalLoss)?;
    require_stored_lease_identity(&current, lease)?;
    let state: String = current.decode_column("state_kind")?;
    if state != "offered" {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let event_ordinal = decode_u64(current.decode_column("event_ordinal")?)?
        .checked_add(1)
        .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
    sqlx::query(
        "INSERT INTO runner_lease_event
            (lease_id, generation, event_ordinal, state_kind)
         VALUES ($1, $2, $3, 'lost_unclaimed')",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .bind(Decimal::from(event_ordinal))
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "UPDATE runner_current_lease_event
            SET event_ordinal = $3
          WHERE lease_id = $1 AND generation = $2",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .bind(Decimal::from(event_ordinal))
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_lease_no_execution_proof
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, turn_id, issuing_turn_attempt_id, request_id,
             dispatch_generation)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .bind(correlation.dispatch.attempt().into_uuid())
    .bind(correlation.dispatch.session().into_uuid())
    .bind(correlation.runner.into_uuid())
    .bind(correlation.tool.as_str())
    .bind(correlation.dispatch.turn().into_uuid())
    .bind(correlation.dispatch.issuing_attempt().into_uuid())
    .bind(correlation.dispatch.request().into_uuid())
    .bind(Decimal::from(correlation.dispatch.generation().as_u64()))
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn terminalize_runner_loss_attempt_ambiguous(
    transaction: &mut Transaction<'_, Postgres>,
    correlation: &RunnerLeaseCorrelation,
) -> Result<(), RunnerProtocolStoreError> {
    let changed = sqlx::query(
        "UPDATE tool_attempt
            SET state_kind = 'terminal',
                terminal_disposition_kind = 'ambiguous'
          WHERE attempt_id = $1 AND request_id = $2 AND session_id = $3
            AND turn_id = $4 AND issuing_turn_attempt_id = $5
            AND dispatch_generation = $6 AND state_kind = 'in_flight'",
    )
    .bind(correlation.dispatch.attempt().into_uuid())
    .bind(correlation.dispatch.request().into_uuid())
    .bind(correlation.dispatch.session().into_uuid())
    .bind(correlation.dispatch.turn().into_uuid())
    .bind(correlation.dispatch.issuing_attempt().into_uuid())
    .bind(Decimal::from(correlation.dispatch.generation().as_u64()))
    .execute(&mut **transaction)
    .await?
    .rows_affected();
    if changed != 1 {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    Ok(())
}

async fn yield_turn_to_runner_recovery(
    transaction: &mut Transaction<'_, Postgres>,
    placement: &SessionRunnerPlacement,
    correlation: &RunnerLeaseCorrelation,
) -> Result<(), RunnerProtocolStoreError> {
    let yielded = sqlx::query(
        "UPDATE turn_attempt
            SET state_kind = 'ended', end_variant = 'without_stop',
                end_disposition = 'yielded_to_durable_wait'
          WHERE turn_attempt_id = $1 AND turn_id = $2 AND session_id = $3
            AND state_kind = 'running' AND end_variant IS NULL
            AND end_disposition IS NULL",
    )
    .bind(correlation.dispatch.issuing_attempt().into_uuid())
    .bind(correlation.dispatch.turn().into_uuid())
    .bind(correlation.dispatch.session().into_uuid())
    .execute(&mut **transaction)
    .await?
    .rows_affected();
    if yielded != 1 {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let changed = sqlx::query(
        "UPDATE turn_lifecycle AS lifecycle
            SET active_phase_kind = 'awaiting_runner_recovery',
                current_attempt_id = NULL,
                runner_recovery_runner_id = $4,
                runner_recovery_placement_revision = $5,
                runner_recovery_tool_attempt_id = $6
           FROM tool_request AS request
          WHERE lifecycle.turn_id = $1 AND lifecycle.session_id = $2
            AND lifecycle.state_kind = 'active'
            AND lifecycle.active_phase_kind = 'running'
            AND lifecycle.current_attempt_id = $3
            AND lifecycle.active_tool_round_call_id =
                request.producing_model_call_id
            AND request.request_id = $7 AND request.turn_id = $1
            AND request.session_id = $2",
    )
    .bind(correlation.dispatch.turn().into_uuid())
    .bind(correlation.dispatch.session().into_uuid())
    .bind(correlation.dispatch.issuing_attempt().into_uuid())
    .bind(correlation.runner.into_uuid())
    .bind(Decimal::from(placement.revision().get()))
    .bind(correlation.dispatch.attempt().into_uuid())
    .bind(correlation.dispatch.request().into_uuid())
    .execute(&mut **transaction)
    .await?
    .rows_affected();
    if changed != 1 {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    Ok(())
}

async fn append_runner_connection_health_events(
    connection: &mut PgConnection,
    enrollment: RunnerEnrollmentId,
    snapshot: RunnerConnectionSnapshot,
) -> Result<(), RunnerProtocolStoreError> {
    let expected_state = match snapshot.cause() {
        RunnerConnectionCause::Established | RunnerConnectionCause::HeartbeatRecovered => {
            RunnerConnectionState::Connected
        }
        RunnerConnectionCause::HeartbeatMissed => RunnerConnectionState::Suspect,
        RunnerConnectionCause::DaemonShutdown | RunnerConnectionCause::RunnerShutdown => {
            RunnerConnectionState::Shutdown
        }
        RunnerConnectionCause::HeartbeatTimeout
        | RunnerConnectionCause::TransportClosed
        | RunnerConnectionCause::ProtocolFailure
        | RunnerConnectionCause::EnrollmentRevoked => RunnerConnectionState::Lost,
    };
    if snapshot.state() != expected_state {
        return Err(RunnerProtocolCorruption::InvalidEncoding.into());
    }
    let state = match snapshot.cause() {
        RunnerConnectionCause::HeartbeatMissed => DispatchedRunnerState::Suspect,
        RunnerConnectionCause::Established | RunnerConnectionCause::HeartbeatRecovered => {
            DispatchedRunnerState::Connected
        }
        RunnerConnectionCause::DaemonShutdown
        | RunnerConnectionCause::RunnerShutdown
        | RunnerConnectionCause::HeartbeatTimeout
        | RunnerConnectionCause::TransportClosed
        | RunnerConnectionCause::ProtocolFailure
        | RunnerConnectionCause::EnrollmentRevoked => return Ok(()),
    };
    let placements = sqlx::query(
        "SELECT placement.session_id, placement.event_ordinal,
                placement.placement_revision, placement.pinned_runner_id,
                placement.requested_sandbox_profile,
                placement.requested_working_directory
           FROM runner_current_session_placement AS current_placement
           JOIN runner_session_placement_record AS placement
             ON placement.session_id = current_placement.session_id
            AND placement.event_ordinal = current_placement.event_ordinal
          WHERE placement.state_kind = 'pinned'
            AND placement.registration_enrollment_id = $1
          ORDER BY placement.session_id",
    )
    .bind(enrollment.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    for placement in placements {
        let session = session_id(placement.decode_column("session_id")?);
        let runner = runner_id(placement.decode_column("pinned_runner_id")?);
        let placement_revision = decode_generation(placement.decode_column("placement_revision")?)?;
        let sandbox = decode_sandbox(placement.decode_column("requested_sandbox_profile")?)?;
        let working_directory = placement
            .decode_column::<Option<String>>("requested_working_directory")?
            .map(working_directory)
            .transpose()?;
        let placement_event_ordinal = decode_u64(placement.decode_column("event_ordinal")?)?;
        outbox::append(
            connection,
            OutboxEvent::RunnerStateTransition(RunnerStateOutboxEvent {
                session,
                runner,
                placement_revision,
                sandbox,
                working_directory,
                state,
                source: RunnerStateOutboxSource {
                    placement_event_ordinal,
                    connection: Some(RunnerConnectionOutboxSource {
                        enrollment,
                        epoch: snapshot.epoch().get(),
                        event_ordinal: snapshot.event_ordinal(),
                    }),
                },
            }),
        )
        .await?;
    }
    Ok(())
}

async fn append_runner_connection_loss_epoch(
    connection: &mut PgConnection,
    enrollment: RunnerEnrollmentId,
    snapshot: RunnerConnectionSnapshot,
) -> Result<Option<RunnerConnectionLossSnapshot>, RunnerProtocolStoreError> {
    if snapshot.state() != RunnerConnectionState::Lost {
        return Ok(None);
    }
    let prior: Option<Decimal> = sqlx::query_scalar(RUNNER_CONNECTION_LOSS_HEAD)
        .bind(enrollment.into_uuid())
        .fetch_optional(&mut *connection)
        .await?;
    let (loss_epoch, head_statement) = match prior {
        Some(prior) => (
            RunnerConnectionLossEpoch::try_from_u64(decode_u64(prior)?)
                .ok_or(RunnerProtocolCorruption::InvalidEncoding)?
                .checked_next()
                .ok_or(RunnerProtocolCorruption::GenerationExhausted)?,
            "UPDATE runner_current_connection_loss
                SET loss_epoch = $2
              WHERE enrollment_id = $1",
        ),
        None => (
            RunnerConnectionLossEpoch::try_from_u64(1)
                .ok_or(RunnerProtocolCorruption::InvalidEncoding)?,
            "INSERT INTO runner_current_connection_loss
                (enrollment_id, loss_epoch)
             VALUES ($1, $2)",
        ),
    };
    sqlx::query(
        "INSERT INTO runner_connection_loss_epoch
            (enrollment_id, loss_epoch, connection_epoch,
             connection_event_ordinal)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(loss_epoch.get()))
    .bind(Decimal::from(snapshot.epoch().get()))
    .bind(Decimal::from(snapshot.event_ordinal()))
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO runner_connection_loss_propagation
            (enrollment_id, loss_epoch, propagated_through_session_id,
             state_kind)
         VALUES ($1, $2, NULL, $3)",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(loss_epoch.get()))
    .bind(runner_loss_propagation_state_to_str(
        RunnerLossPropagationStateStorageKind::Pending,
    ))
    .execute(&mut *connection)
    .await?;
    sqlx::query(head_statement)
        .bind(enrollment.into_uuid())
        .bind(Decimal::from(loss_epoch.get()))
        .execute(&mut *connection)
        .await?;
    let connection_event_ordinal = NonZeroU64::new(snapshot.event_ordinal())
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
    Ok(Some(RunnerConnectionLossSnapshot {
        enrollment,
        loss_epoch,
        connection_epoch: snapshot.epoch(),
        connection_event_ordinal,
    }))
}

async fn advance_runner_connection_authority_head(
    connection: &mut PgConnection,
    enrollment: RunnerEnrollmentId,
    prior: Option<RunnerConnectionSnapshot>,
    current: RunnerConnectionSnapshot,
    loss: Option<RunnerConnectionLossSnapshot>,
) -> Result<(), RunnerProtocolStoreError> {
    let rows = match prior {
        Some(prior) => sqlx::query(
            "UPDATE runner_connection_authority_head
                    SET connection_epoch = $2,
                        connection_event_ordinal = $3,
                        latest_loss_epoch = COALESCE($4, latest_loss_epoch)
                  WHERE enrollment_id = $1
                    AND connection_epoch = $5
                    AND connection_event_ordinal = $6",
        )
        .bind(enrollment.into_uuid())
        .bind(Decimal::from(current.epoch().get()))
        .bind(Decimal::from(current.event_ordinal()))
        .bind(loss.map(|loss| Decimal::from(loss.loss_epoch().get())))
        .bind(Decimal::from(prior.epoch().get()))
        .bind(Decimal::from(prior.event_ordinal()))
        .execute(&mut *connection)
        .await?
        .rows_affected(),
        None => sqlx::query(
            "INSERT INTO runner_connection_authority_head
                    (enrollment_id, connection_epoch,
                     connection_event_ordinal, latest_loss_epoch)
                 VALUES ($1, $2, $3, $4)",
        )
        .bind(enrollment.into_uuid())
        .bind(Decimal::from(current.epoch().get()))
        .bind(Decimal::from(current.event_ordinal()))
        .bind(loss.map(|loss| Decimal::from(loss.loss_epoch().get())))
        .execute(&mut *connection)
        .await?
        .rows_affected(),
    };
    if rows != 1 {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StoredEnrollmentRequestFacts {
    identities: IssuedRunnerEnrollmentIdentities,
    registration_revision: RunnerRegistrationRevision,
    authority: RunnerEnrollmentAuthority,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PristineEnrollmentAdmission {
    Active,
    ReplacementPending {
        predecessor: RunnerEnrollmentId,
        loss_epoch: RunnerConnectionLossEpoch,
    },
}

fn connection_transition_values(
    transition: RunnerConnectionTransition,
) -> Option<(RunnerConnectionState, RunnerConnectionCause, &'static str)> {
    match transition {
        RunnerConnectionTransition::Observe => None,
        RunnerConnectionTransition::HeartbeatRecovered => Some((
            RunnerConnectionState::Connected,
            RunnerConnectionCause::HeartbeatRecovered,
            "heartbeat_recovered",
        )),
        RunnerConnectionTransition::HeartbeatMissed => Some((
            RunnerConnectionState::Suspect,
            RunnerConnectionCause::HeartbeatMissed,
            "heartbeat_missed",
        )),
        RunnerConnectionTransition::DaemonShutdown => Some((
            RunnerConnectionState::Shutdown,
            RunnerConnectionCause::DaemonShutdown,
            "daemon_shutdown",
        )),
        RunnerConnectionTransition::RunnerShutdown => Some((
            RunnerConnectionState::Shutdown,
            RunnerConnectionCause::RunnerShutdown,
            "runner_shutdown",
        )),
        RunnerConnectionTransition::HeartbeatTimeout => Some((
            RunnerConnectionState::Lost,
            RunnerConnectionCause::HeartbeatTimeout,
            "heartbeat_timeout",
        )),
        RunnerConnectionTransition::TransportClosed => Some((
            RunnerConnectionState::Lost,
            RunnerConnectionCause::TransportClosed,
            "transport_closed",
        )),
        RunnerConnectionTransition::ProtocolFailure => Some((
            RunnerConnectionState::Lost,
            RunnerConnectionCause::ProtocolFailure,
            "protocol_failure",
        )),
    }
}

async fn load_connection_head_in(
    connection: &mut PgConnection,
    enrollment: RunnerEnrollmentId,
) -> Result<Option<RunnerConnectionSnapshot>, RunnerProtocolStoreError> {
    let row = sqlx::query(
        "SELECT connection_epoch, event_ordinal, state_kind, cause_kind
           FROM runner_connection_event
          WHERE enrollment_id = $1
          ORDER BY connection_epoch DESC, event_ordinal DESC
          LIMIT 1",
    )
    .bind(enrollment.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    row.map(|row| {
        let epoch = RunnerConnectionEpoch::try_from_u64(decode_u64(
            row.decode_column("connection_epoch")?,
        )?)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        let event_ordinal = NonZeroU64::new(decode_u64(row.decode_column("event_ordinal")?)?)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        let state_kind: String = row.decode_column("state_kind")?;
        let cause_kind: String = row.decode_column("cause_kind")?;
        let state = runner_connection_state_from_str(&state_kind)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        let cause = match cause_kind.as_str() {
            "established" => RunnerConnectionCause::Established,
            "heartbeat_recovered" => RunnerConnectionCause::HeartbeatRecovered,
            "heartbeat_missed" => RunnerConnectionCause::HeartbeatMissed,
            "daemon_shutdown" => RunnerConnectionCause::DaemonShutdown,
            "runner_shutdown" => RunnerConnectionCause::RunnerShutdown,
            "heartbeat_timeout" => RunnerConnectionCause::HeartbeatTimeout,
            "transport_closed" => RunnerConnectionCause::TransportClosed,
            "protocol_failure" => RunnerConnectionCause::ProtocolFailure,
            "enrollment_revoked" => RunnerConnectionCause::EnrollmentRevoked,
            _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
        };
        Ok(RunnerConnectionSnapshot {
            epoch,
            event_ordinal,
            state,
            cause,
        })
    })
    .transpose()
}

async fn terminalize_connection_for_revocation(
    transaction: &mut Transaction<'_, Postgres>,
    enrollment: RunnerEnrollmentId,
) -> Result<(), RunnerProtocolStoreError> {
    let Some(current) = load_connection_head_in(transaction.as_mut(), enrollment).await? else {
        return Ok(());
    };
    if matches!(
        current.state(),
        RunnerConnectionState::Shutdown | RunnerConnectionState::Lost
    ) {
        return Ok(());
    }
    let event_ordinal = NonZeroU64::new(
        current
            .event_ordinal()
            .checked_add(1)
            .ok_or(RunnerProtocolCorruption::GenerationExhausted)?,
    )
    .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
    sqlx::query(
        "INSERT INTO runner_connection_event
            (enrollment_id, connection_epoch, event_ordinal,
             state_kind, cause_kind)
         VALUES ($1, $2, $3, 'lost', 'enrollment_revoked')",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(current.epoch().get()))
    .bind(Decimal::from(event_ordinal.get()))
    .execute(&mut **transaction)
    .await?;
    let snapshot = RunnerConnectionSnapshot {
        epoch: current.epoch(),
        event_ordinal,
        state: RunnerConnectionState::Lost,
        cause: RunnerConnectionCause::EnrollmentRevoked,
    };
    let loss =
        append_runner_connection_loss_epoch(transaction.as_mut(), enrollment, snapshot).await?;
    advance_runner_connection_authority_head(
        transaction.as_mut(),
        enrollment,
        Some(current),
        snapshot,
        loss,
    )
    .await?;
    append_runner_connection_health_events(transaction.as_mut(), enrollment, snapshot).await?;
    Ok(())
}

async fn load_enrollment_request_facts(
    connection: &mut PgConnection,
    request: RunnerEnrollmentRequestId,
) -> Result<Option<StoredEnrollmentRequestFacts>, RunnerProtocolStoreError> {
    let row = sqlx::query(
        "SELECT enrollment_id, runner_id, authentication_reference_id,
                registration_revision, authority_kind
           FROM runner_enrollment_request_receipt
          WHERE request_id = $1
          FOR SHARE",
    )
    .bind(request.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    row.map(|row| {
        Ok(StoredEnrollmentRequestFacts {
            identities: IssuedRunnerEnrollmentIdentities::new(
                runner_enrollment_id(row.decode_column("enrollment_id")?),
                runner_id(row.decode_column("runner_id")?),
                runner_authentication_id(row.decode_column("authentication_reference_id")?),
            ),
            registration_revision: decode_registration_revision(
                row.decode_column("registration_revision")?,
            )?,
            authority: decode_enrollment_authority(row.decode_column("authority_kind")?)?,
        })
    })
    .transpose()
}

async fn load_enrollment_request_receipt_in(
    connection: &mut PgConnection,
    request: RunnerEnrollmentRequestId,
    catalog: &RunnerCatalog,
) -> Result<Option<RunnerEnrollmentReceipt>, RunnerProtocolStoreError> {
    let Some(stored) = load_enrollment_request_facts(connection, request).await? else {
        return Ok(None);
    };
    let locked = sqlx::query(RUNNER_ENROLLMENT)
        .bind(stored.identities.enrollment().into_uuid())
        .fetch_optional(&mut *connection)
        .await?;
    if locked.is_none() {
        return Err(RunnerProtocolCorruption::MissingCanonicalEnrollment.into());
    }
    let enrollment = load_enrollment_in(connection, stored.identities.enrollment())
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
    if stored.identities
        != IssuedRunnerEnrollmentIdentities::new(
            enrollment.enrollment(),
            enrollment.runner(),
            enrollment.authentication(),
        )
    {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let pending_relation: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1
              FROM runner_pending_enrollment AS pending
              JOIN runner_connection_loss_epoch AS loss
                ON loss.enrollment_id = pending.predecessor_enrollment_id
               AND loss.loss_epoch = pending.predecessor_loss_epoch
              JOIN runner_connection_event AS source
                ON source.enrollment_id = loss.enrollment_id
               AND source.connection_epoch = loss.connection_epoch
               AND source.event_ordinal = loss.connection_event_ordinal
             WHERE pending.request_id = $1
               AND pending.enrollment_id = $2
               AND source.state_kind = 'lost'
        )",
    )
    .bind(request.into_uuid())
    .bind(enrollment.enrollment().into_uuid())
    .fetch_one(&mut *connection)
    .await?;
    if !matches!(
        (stored.authority, enrollment.state(), pending_relation),
        (
            RunnerEnrollmentAuthority::Active,
            RunnerEnrollmentState::Active | RunnerEnrollmentState::Revoked,
            false
        ) | (
            RunnerEnrollmentAuthority::ReplacementPending,
            RunnerEnrollmentState::Pending,
            true
        ) | (
            RunnerEnrollmentAuthority::ReplacementPending,
            RunnerEnrollmentState::Active | RunnerEnrollmentState::Revoked,
            true
        )
    ) {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let registration = load_registration_in(
        connection,
        enrollment.enrollment(),
        stored.registration_revision,
        Some(&enrollment),
        catalog,
    )
    .await?
    .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
    Ok(Some(RunnerEnrollmentReceipt {
        request,
        authority: match enrollment.state() {
            RunnerEnrollmentState::Pending => RunnerEnrollmentAuthority::ReplacementPending,
            RunnerEnrollmentState::Active | RunnerEnrollmentState::Revoked => {
                RunnerEnrollmentAuthority::Active
            }
        },
        enrollment,
        registration,
    }))
}

async fn select_pristine_enrollment_admission(
    transaction: &mut Transaction<'_, Postgres>,
    request: RunnerEnrollmentRequestId,
) -> Result<PristineEnrollmentAdmission, RunnerProtocolStoreError> {
    let active_rows: Vec<Uuid> = sqlx::query_scalar(RUNNER_PRISTINE_ACTIVE_ENROLLMENTS)
        .fetch_all(&mut **transaction)
        .await?;
    if active_rows.len() > 1 {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let pending_rows: Vec<Uuid> = sqlx::query_scalar(RUNNER_PRISTINE_PENDING_ENROLLMENTS)
        .fetch_all(&mut **transaction)
        .await?;
    if pending_rows.len() > 1 {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    if let Some(pending) = pending_rows.first().copied() {
        return Err(RunnerEnrollmentRequestFailure::PendingEnrollmentExists {
            request,
            pending_enrollment: runner_enrollment_id(pending),
        }
        .into());
    }
    let Some(active) = active_rows.first().copied() else {
        return Ok(PristineEnrollmentAdmission::Active);
    };
    let loss_epoch: Option<Decimal> = sqlx::query_scalar(
        "SELECT authority.latest_loss_epoch
           FROM runner_connection_authority_head AS authority
           JOIN runner_connection_event AS connection
             ON connection.enrollment_id = authority.enrollment_id
            AND connection.connection_epoch = authority.connection_epoch
            AND connection.event_ordinal = authority.connection_event_ordinal
          WHERE authority.enrollment_id = $1
            AND authority.latest_loss_epoch IS NOT NULL
            AND connection.state_kind = 'lost'
          FOR SHARE OF authority",
    )
    .bind(active)
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(loss_epoch) = loss_epoch else {
        return Err(RunnerEnrollmentRequestFailure::ActiveEnrollmentExists {
            request,
            active_enrollment: runner_enrollment_id(active),
        }
        .into());
    };
    let loss_epoch = RunnerConnectionLossEpoch::try_from_u64(decode_u64(loss_epoch)?)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
    Ok(PristineEnrollmentAdmission::ReplacementPending {
        predecessor: runner_enrollment_id(active),
        loss_epoch,
    })
}

async fn insert_enrollment_rows(
    transaction: &mut Transaction<'_, Postgres>,
    enrollment: &RunnerEnrollment,
) -> Result<(), RunnerProtocolStoreError> {
    let classes: Vec<_> = enrollment.allowed_classes().collect();
    let state = runner_enrollment_state_to_str(enrollment.state());
    sqlx::query(
        "INSERT INTO runner_enrollment_audit
            (enrollment_id, revision, runner_id,
             authentication_reference_id, allowed_class_count, state_kind)
         VALUES ($1, 1, $2, $3, $4, $5)",
    )
    .bind(enrollment.enrollment().into_uuid())
    .bind(enrollment.runner().into_uuid())
    .bind(enrollment.authentication().into_uuid())
    .bind(count_decimal(classes.len())?)
    .bind(state)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_enrollment
            (enrollment_id, runner_id, authentication_reference_id,
             allowed_class_count, revision, state_kind)
         VALUES ($1, $2, $3, $4, 1, $5)",
    )
    .bind(enrollment.enrollment().into_uuid())
    .bind(enrollment.runner().into_uuid())
    .bind(enrollment.authentication().into_uuid())
    .bind(count_decimal(classes.len())?)
    .bind(state)
    .execute(&mut **transaction)
    .await?;
    for class in classes {
        sqlx::query(
            "INSERT INTO runner_enrollment_allowed_class
                (enrollment_id, capability_class)
             VALUES ($1, $2)",
        )
        .bind(enrollment.enrollment().into_uuid())
        .bind(class.as_str())
        .execute(&mut **transaction)
        .await?;
        sqlx::query(
            "INSERT INTO runner_enrollment_audit_allowed_class
                (enrollment_id, revision, capability_class)
             VALUES ($1, 1, $2)",
        )
        .bind(enrollment.enrollment().into_uuid())
        .bind(class.as_str())
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

async fn load_enrollment_in(
    connection: &mut PgConnection,
    enrollment: RunnerEnrollmentId,
) -> Result<Option<RunnerEnrollment>, RunnerProtocolStoreError> {
    let row = sqlx::query(
        "SELECT enrollment.enrollment_id, enrollment.runner_id,
                enrollment.authentication_reference_id,
                enrollment.allowed_class_count, enrollment.state_kind,
                audit.runner_id AS audit_runner_id,
                audit.authentication_reference_id AS audit_authentication_reference_id,
                audit.allowed_class_count AS audit_allowed_class_count,
                audit.state_kind AS audit_state_kind,
                current_registration.registration_revision
           FROM runner_enrollment AS enrollment
           LEFT JOIN runner_enrollment_audit AS audit
             ON audit.enrollment_id = enrollment.enrollment_id
            AND audit.revision = enrollment.revision
           LEFT JOIN runner_current_registration AS current_registration
             ON current_registration.enrollment_id = enrollment.enrollment_id
          WHERE enrollment.enrollment_id = $1",
    )
    .bind(enrollment.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let class_rows = sqlx::query(
        "SELECT capability_class
           FROM runner_enrollment_allowed_class
          WHERE enrollment_id = $1
          ORDER BY capability_class",
    )
    .bind(enrollment.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    let audit_class_rows = sqlx::query(
        "SELECT audited.capability_class
           FROM runner_enrollment AS enrollment
           JOIN runner_enrollment_audit_allowed_class AS audited
             ON audited.enrollment_id = enrollment.enrollment_id
            AND audited.revision = enrollment.revision
          WHERE enrollment.enrollment_id = $1
          ORDER BY audited.capability_class",
    )
    .bind(enrollment.into_uuid())
    .fetch_all(&mut *connection)
    .await?;
    if Decimal::from(class_rows.len()) != row.decode_column::<Decimal>("allowed_class_count")? {
        return Err(RunnerProtocolCorruption::IncompleteInventory.into());
    }
    let audit_count = row
        .decode_column::<Option<Decimal>>("audit_allowed_class_count")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalAudit)?;
    if Decimal::from(audit_class_rows.len()) != audit_count {
        return Err(RunnerProtocolCorruption::IncompleteInventory.into());
    }
    let classes = decode_classes(&class_rows)?;
    let audit_classes = decode_classes(&audit_class_rows)?;
    let state = runner_enrollment_state_from_str(row.decode_column("state_kind")?)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
    let audit_state = row
        .decode_column::<Option<String>>("audit_state_kind")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalAudit)?;
    let audit_state = runner_enrollment_state_from_str(&audit_state)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
    let registration_revision = row
        .decode_column::<Option<Decimal>>("registration_revision")?
        .map(decode_generation)
        .transpose()?;
    RunnerEnrollment::reconstitute(RunnerEnrollmentReconstitutionInput {
        enrollment,
        recorded_enrollment: runner_enrollment_id(row.decode_column("enrollment_id")?),
        runner: runner_id(row.decode_column("runner_id")?),
        recorded_runner: runner_id(
            row.decode_column::<Option<Uuid>>("audit_runner_id")?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalAudit)?,
        ),
        authentication: runner_authentication_id(row.decode_column("authentication_reference_id")?),
        recorded_authentication: runner_authentication_id(
            row.decode_column::<Option<Uuid>>("audit_authentication_reference_id")?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalAudit)?,
        ),
        allowed_classes: classes,
        recorded_allowed_classes: audit_classes,
        state,
        recorded_state: audit_state,
        registration_revision,
        recorded_registration_revision: registration_revision,
    })
    .map(Some)
    .map_err(RunnerProtocolStoreError::Domain)
}

async fn insert_registration_reconciliation(
    transaction: &mut Transaction<'_, Postgres>,
    enrollment: RunnerEnrollmentId,
    revision: RunnerRegistrationRevision,
) -> Result<(), RunnerProtocolStoreError> {
    if revision == RunnerRegistrationRevision::first() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO runner_registration_reconciliation
            (enrollment_id, registration_revision,
             propagated_through_session_id, state_kind)
         VALUES ($1, $2, NULL, 'pending')",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(revision.get()))
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn require_completed_registration_reconciliation(
    transaction: &mut Transaction<'_, Postgres>,
    enrollment: RunnerEnrollmentId,
    revision: RunnerRegistrationRevision,
) -> Result<(), RunnerProtocolStoreError> {
    if revision == RunnerRegistrationRevision::first() {
        return Ok(());
    }
    let state = sqlx::query_scalar::<_, String>(RUNNER_REGISTRATION_RECONCILIATION_STATE)
        .bind(enrollment.into_uuid())
        .bind(Decimal::from(revision.get()))
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
    match state.as_str() {
        "completed" => return Ok(()),
        "pending" => {}
        _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
    let has_candidate = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM runner_current_session_placement AS current_placement
              JOIN runner_session_placement_record AS placement
                ON placement.session_id = current_placement.session_id
               AND placement.event_ordinal = current_placement.event_ordinal
              LEFT JOIN runner_registration_reconciliation_observation AS observed
                ON observed.enrollment_id = $1
               AND observed.registration_revision = $2
               AND observed.session_id = placement.session_id
             WHERE placement.state_kind = 'pinned'
               AND placement.registration_enrollment_id = $1
               AND placement.registration_revision < $2
               AND observed.session_id IS NULL
        )",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(revision.get()))
    .fetch_one(&mut **transaction)
    .await?;
    if has_candidate {
        return Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::RegistrationInProgress,
        ));
    }
    sqlx::query(
        "UPDATE runner_registration_reconciliation
            SET state_kind = 'completed'
          WHERE enrollment_id = $1 AND registration_revision = $2",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(revision.get()))
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn insert_registration(
    transaction: &mut Transaction<'_, Postgres>,
    revision: RunnerRegistrationRevision,
    registration: &ValidatedRunnerRegistration,
) -> Result<(), RunnerProtocolStoreError> {
    let classes: Vec<_> = registration.classes().collect();
    let tools: Vec<_> = registration.tools().collect();
    let profiles: Vec<_> = registration.profiles().collect();
    let workspaces: Vec<_> = registration.workspaces().collect();
    let sandboxes: Vec<_> = registration.sandboxes().collect();
    let repositories: Vec<_> = registration.repositories().collect();
    sqlx::query(
        "INSERT INTO runner_registration
            (enrollment_id, registration_revision, runner_id,
             authentication_reference_id, class_count, tool_count,
             profile_count, workspace_count, repository_count, sandbox_count)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(registration.enrollment().into_uuid())
    .bind(Decimal::from(revision.get()))
    .bind(registration.runner().into_uuid())
    .bind(registration.authentication().into_uuid())
    .bind(count_decimal(classes.len())?)
    .bind(count_decimal(tools.len())?)
    .bind(count_decimal(profiles.len())?)
    .bind(count_decimal(workspaces.len())?)
    .bind(count_decimal(repositories.len())?)
    .bind(count_decimal(sandboxes.len())?)
    .execute(&mut **transaction)
    .await?;
    for class in classes {
        sqlx::query(
            "INSERT INTO runner_registration_class
                (enrollment_id, registration_revision, capability_class)
             VALUES ($1, $2, $3)",
        )
        .bind(registration.enrollment().into_uuid())
        .bind(Decimal::from(revision.get()))
        .bind(class.as_str())
        .execute(&mut **transaction)
        .await?;
    }
    for tool in tools {
        let loci = encode_loci(tool.loci())?;
        sqlx::query(
            "INSERT INTO runner_registration_tool
                (enrollment_id, registration_revision, tool_name,
                 model_description, model_input_schema, permission_kind,
                 effect_class, loci_kind, selector_kind, selector_runner_id,
                 selector_capability_class)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(registration.enrollment().into_uuid())
        .bind(Decimal::from(revision.get()))
        .bind(tool.name().as_str())
        .bind(tool.model().description())
        .bind(tool.model().input_schema().as_str())
        .bind(tool_permission_default_to_str(tool.permission()))
        .bind(encode_effect(tool.effect()))
        .bind(loci.kind)
        .bind(loci.selector_kind)
        .bind(loci.selector_runner)
        .bind(loci.selector_class)
        .execute(&mut **transaction)
        .await?;
    }
    for profile in profiles {
        let approvals: Vec<_> = profile.approvals().collect();
        sqlx::query(
            "INSERT INTO runner_registration_profile
                (enrollment_id, registration_revision,
                 credential_profile_name, approval_count)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(registration.enrollment().into_uuid())
        .bind(Decimal::from(revision.get()))
        .bind(profile.name().as_str())
        .bind(count_decimal(approvals.len())?)
        .execute(&mut **transaction)
        .await?;
        for (tool, approval) in approvals {
            sqlx::query(
                "INSERT INTO runner_registration_profile_approval
                    (enrollment_id, registration_revision,
                     credential_profile_name, tool_name, approval_kind)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(registration.enrollment().into_uuid())
            .bind(Decimal::from(revision.get()))
            .bind(profile.name().as_str())
            .bind(tool.as_str())
            .bind(encode_approval(approval))
            .execute(&mut **transaction)
            .await?;
        }
    }
    for workspace in workspaces {
        sqlx::query(
            "INSERT INTO runner_registration_workspace
                (enrollment_id, registration_revision, workspace_kind)
             VALUES ($1, $2, $3)",
        )
        .bind(registration.enrollment().into_uuid())
        .bind(Decimal::from(revision.get()))
        .bind(encode_workspace(workspace))
        .execute(&mut **transaction)
        .await?;
    }
    for sandbox in sandboxes {
        sqlx::query(
            "INSERT INTO runner_registration_sandbox
                (enrollment_id, registration_revision, sandbox_profile)
             VALUES ($1, $2, $3)",
        )
        .bind(registration.enrollment().into_uuid())
        .bind(Decimal::from(revision.get()))
        .bind(runner_sandbox_to_str(sandbox))
        .execute(&mut **transaction)
        .await?;
    }
    for repository in repositories {
        sqlx::query(
            "INSERT INTO runner_registration_repository
                (enrollment_id, registration_revision, repository_key,
                 credential_profile_name)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(registration.enrollment().into_uuid())
        .bind(Decimal::from(revision.get()))
        .bind(repository.key().as_str())
        .bind(
            repository
                .credential_profile()
                .map(CredentialProfileName::as_str),
        )
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

async fn load_registration_in(
    connection: &mut PgConnection,
    enrollment: RunnerEnrollmentId,
    revision: RunnerRegistrationRevision,
    authority: Option<&RunnerEnrollment>,
    catalog: &RunnerCatalog,
) -> Result<Option<StoredValidatedRunnerRegistration>, RunnerProtocolStoreError> {
    let row = sqlx::query(
        "SELECT *
           FROM runner_registration
          WHERE enrollment_id = $1
            AND registration_revision = $2",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(revision.get()))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let canonical = load_enrollment_in(connection, enrollment)
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalEnrollment)?;
    let authority = authority.unwrap_or(&canonical);
    if canonical != *authority {
        return Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::CorruptStoredFacts,
        ));
    }
    let class_rows = sqlx::query(
        "SELECT capability_class
           FROM runner_registration_class
          WHERE enrollment_id = $1 AND registration_revision = $2
          ORDER BY capability_class",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(revision.get()))
    .fetch_all(&mut *connection)
    .await?;
    let tool_rows = sqlx::query(
        "SELECT *
           FROM runner_registration_tool
          WHERE enrollment_id = $1 AND registration_revision = $2
          ORDER BY tool_name",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(revision.get()))
    .fetch_all(&mut *connection)
    .await?;
    let profile_rows = sqlx::query(
        "SELECT *
           FROM runner_registration_profile
          WHERE enrollment_id = $1 AND registration_revision = $2
          ORDER BY credential_profile_name",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(revision.get()))
    .fetch_all(&mut *connection)
    .await?;
    let workspace_rows = sqlx::query(
        "SELECT workspace_kind
           FROM runner_registration_workspace
          WHERE enrollment_id = $1 AND registration_revision = $2
          ORDER BY workspace_kind",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(revision.get()))
    .fetch_all(&mut *connection)
    .await?;
    let sandbox_rows = sqlx::query(
        "SELECT sandbox_profile
           FROM runner_registration_sandbox
          WHERE enrollment_id = $1 AND registration_revision = $2
          ORDER BY sandbox_profile",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(revision.get()))
    .fetch_all(&mut *connection)
    .await?;
    let repository_rows = sqlx::query(
        "SELECT repository_key, credential_profile_name
           FROM runner_registration_repository
          WHERE enrollment_id = $1 AND registration_revision = $2
          ORDER BY repository_key",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(revision.get()))
    .fetch_all(&mut *connection)
    .await?;
    require_count(&row, "class_count", class_rows.len())?;
    require_count(&row, "tool_count", tool_rows.len())?;
    require_count(&row, "profile_count", profile_rows.len())?;
    require_count(&row, "workspace_count", workspace_rows.len())?;
    require_count(&row, "sandbox_count", sandbox_rows.len())?;
    require_count(&row, "repository_count", repository_rows.len())?;
    let classes = decode_classes(&class_rows)?;
    let tools = tool_rows
        .iter()
        .map(decode_tool_declaration)
        .collect::<Result<Vec<_>, _>>()?;
    let mut profiles = Vec::with_capacity(profile_rows.len());
    for profile in profile_rows {
        let name = profile_name(profile.decode_column("credential_profile_name")?)?;
        let approval_rows = sqlx::query(
            "SELECT tool_name, approval_kind
               FROM runner_registration_profile_approval
              WHERE enrollment_id = $1
                AND registration_revision = $2
                AND credential_profile_name = $3
              ORDER BY tool_name",
        )
        .bind(enrollment.into_uuid())
        .bind(Decimal::from(revision.get()))
        .bind(name.as_str())
        .fetch_all(&mut *connection)
        .await?;
        require_count(&profile, "approval_count", approval_rows.len())?;
        let approvals = approval_rows
            .iter()
            .map(|row| {
                Ok((
                    tool_name(row.decode_column("tool_name")?)?,
                    decode_approval(row.decode_column("approval_kind")?)?,
                ))
            })
            .collect::<Result<Vec<_>, RunnerProtocolStoreError>>()?;
        profiles.push(
            CredentialProfilePolicy::try_new(name, approvals)
                .map_err(RunnerProtocolStoreError::Domain)?,
        );
    }
    let workspaces = workspace_rows
        .iter()
        .map(|row| decode_workspace(row.decode_column("workspace_kind")?))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let sandboxes = sandbox_rows
        .iter()
        .map(|row| decode_sandbox(row.decode_column("sandbox_profile")?))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let repositories = repository_rows
        .iter()
        .map(|row| {
            Ok(RunnerRepositoryEntry::new(
                repository_key(row.decode_column("repository_key")?)?,
                row.decode_column::<Option<String>>("credential_profile_name")?
                    .map(profile_name)
                    .transpose()?,
            ))
        })
        .collect::<Result<Vec<_>, RunnerProtocolStoreError>>()?;
    let registration = ValidatedRunnerRegistration::reconstitute(
        authority,
        catalog,
        ValidatedRunnerRegistrationReconstitutionInput {
            enrollment: runner_enrollment_id(row.decode_column("enrollment_id")?),
            revision: RunnerGeneration::try_from_u64(revision.get())
                .ok_or(RunnerProtocolCorruption::GenerationExhausted)?,
            runner: runner_id(row.decode_column("runner_id")?),
            authentication: runner_authentication_id(
                row.decode_column("authentication_reference_id")?,
            ),
            classes,
            tools,
            profiles,
            workspaces,
            sandboxes,
            repositories,
        },
    )
    .map_err(RunnerProtocolStoreError::Domain)?;
    Ok(Some(StoredValidatedRunnerRegistration {
        revision,
        registration,
    }))
}

fn classify_placement_event(
    prior: Option<&PgRow>,
    placement: &SessionRunnerPlacement,
) -> Result<&'static str, RunnerProtocolStoreError> {
    let Some(prior) = prior else {
        if matches!(placement.state(), SessionRunnerPlacementState::Unpinned)
            && placement.revision() == RunnerGeneration::one()
        {
            return Ok("created");
        }
        return Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::InvalidState,
        ));
    };
    let prior_revision = decode_generation(prior.decode_column("placement_revision")?)?;
    let prior_state: String = prior.decode_column("state_kind")?;
    let invalid = || {
        Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::InvalidState,
        ))
    };
    match placement.state() {
        SessionRunnerPlacementState::Unpinned => match prior_state.as_str() {
            "runner_lost_before_pin" => match prior_revision.checked_next() {
                Some(revision) if placement.revision() == revision => Ok("pre_pin_replaced"),
                Some(_) => invalid(),
                None => Err(RunnerProtocolCorruption::GenerationExhausted.into()),
            },
            _ => invalid(),
        },
        SessionRunnerPlacementState::Pinned(_) => match prior_state.as_str() {
            "unpinned" if placement.revision() == prior_revision => Ok("pinned"),
            "runner_lost" => match prior_revision.checked_next() {
                Some(revision) if placement.revision() == revision => Ok("runner_replaced"),
                Some(_) => invalid(),
                None => Err(RunnerProtocolCorruption::GenerationExhausted.into()),
            },
            "pinned" => match prior_revision.checked_next() {
                Some(revision) if placement.revision() == revision => Ok("profile_replaced"),
                Some(_) => invalid(),
                None => Err(RunnerProtocolCorruption::GenerationExhausted.into()),
            },
            _ => invalid(),
        },
        SessionRunnerPlacementState::RunnerLostBeforePin(_) => match prior_state.as_str() {
            "unpinned" if placement.revision() == prior_revision => Ok("runner_lost_before_pin"),
            _ => invalid(),
        },
        SessionRunnerPlacementState::RunnerLost(_) => match prior_state.as_str() {
            "pinned" if placement.revision() == prior_revision => Ok("runner_lost"),
            _ => invalid(),
        },
        SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::BeforePin(_)) => {
            match prior_state.as_str() {
                "runner_lost_before_pin" if placement.revision() == prior_revision => {
                    Ok("abandoned")
                }
                _ => invalid(),
            }
        }
        SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::Pinned(_)) => {
            match prior_state.as_str() {
                "runner_lost" if placement.revision() == prior_revision => Ok("abandoned"),
                _ => invalid(),
            }
        }
    }
}

async fn insert_initial_pin_rows(
    transaction: &mut Transaction<'_, Postgres>,
    prior: &PgRow,
    pin: &SessionRunnerPin,
    registration: &StoredValidatedRunnerRegistration,
    catalog: &RunnerCatalog,
) -> Result<(), RunnerProtocolStoreError> {
    let prior_event_ordinal = decode_u64(prior.decode_column("event_ordinal")?)?;
    let event_ordinal = prior_event_ordinal
        .checked_add(1)
        .ok_or(RunnerProtocolCorruption::GenerationExhausted)?;
    let event_kind = classify_placement_event(Some(prior), &pin.placement)?;
    if event_kind != "pinned" {
        return Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::InvalidState,
        ));
    }
    let grant_origin = placement_grant_origin(Some(prior), event_ordinal, &pin.placement)?;
    insert_placement_record(
        transaction,
        event_ordinal,
        event_kind,
        &pin.placement,
        PlacementRecordEvidence {
            registration_identity: stored_registration_identity(Some(registration)),
            grant_origin,
            interrupted_tool_attempt: None,
            loss_registration_revision: None,
        },
    )
    .await?;
    if let Some(grant) = pin.grant.as_ref() {
        insert_grant_if_new(
            transaction,
            Some(prior),
            event_ordinal,
            &pin.placement,
            grant,
            RegistrationAuthority {
                stored: registration,
                catalog,
            },
            grant_origin.ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?,
        )
        .await?;
    }
    sqlx::query(
        "INSERT INTO runner_current_session_placement
            (session_id, event_ordinal)
         VALUES ($1, $2)
         ON CONFLICT (session_id)
         DO UPDATE SET event_ordinal = EXCLUDED.event_ordinal",
    )
    .bind(pin.placement.session().into_uuid())
    .bind(Decimal::from(event_ordinal))
    .execute(&mut **transaction)
    .await?;
    let pinned = pinned_placement(pin.placement.state()).ok_or(
        RunnerProtocolStoreError::Domain(RunnerDomainError::InvalidState),
    )?;
    outbox::append(
        transaction.as_mut(),
        OutboxEvent::RunnerStateTransition(RunnerStateOutboxEvent {
            session: pin.placement.session(),
            runner: pinned.runner,
            placement_revision: pin.placement.revision(),
            sandbox: pinned.sandbox,
            working_directory: Some(pinned.working_directory.clone()),
            state: DispatchedRunnerState::Pinned,
            source: RunnerStateOutboxSource {
                placement_event_ordinal: event_ordinal,
                connection: None,
            },
        }),
    )
    .await?;
    insert_lease_generation(transaction, &pin.lease).await?;
    let correlation = pin.lease.correlation();
    sqlx::query(
        "INSERT INTO runner_lease_event
            (lease_id, generation, event_ordinal, state_kind)
         VALUES ($1, $2, 1, 'offered')",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_current_lease_event
            (lease_id, generation, event_ordinal)
         VALUES ($1, $2, 1)",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn placement_grant_origin(
    prior: Option<&PgRow>,
    event_ordinal: u64,
    placement: &SessionRunnerPlacement,
) -> Result<Option<Decimal>, RunnerProtocolStoreError> {
    let lineage = match placement.state() {
        SessionRunnerPlacementState::Pinned(pinned) => pinned.grant_lineage,
        SessionRunnerPlacementState::RunnerLost(lost) => lost.pinned().grant_lineage,
        SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::Pinned(lost)) => {
            lost.pinned().grant_lineage
        }
        SessionRunnerPlacementState::Unpinned
        | SessionRunnerPlacementState::RunnerLostBeforePin(_)
        | SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::BeforePin(_)) => {
            None
        }
    };
    let Some(lineage) = lineage else {
        return Ok(None);
    };
    if let Some(prior) = prior {
        let prior_origin =
            prior.decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?;
        let prior_runner = prior.decode_column::<Option<Uuid>>("credential_grant_runner_id")?;
        let prior_revision = prior.decode_column::<Option<Decimal>>("credential_grant_revision")?;
        match (prior_origin, prior_runner, prior_revision) {
            (Some(origin), Some(runner), Some(revision)) => {
                let revision = decode_generation(revision)?;
                let same_grant =
                    revision == lineage.revision && runner_id(runner) == lineage.runner;
                let successor = revision.checked_next() == Some(lineage.revision);
                if same_grant || successor {
                    return Ok(Some(origin));
                }
            }
            (None, None, None) => {}
            _ => return Err(RunnerProtocolCorruption::CrossWiredReference.into()),
        }
    }
    if lineage.revision == RunnerGeneration::one() {
        Ok(Some(Decimal::from(event_ordinal)))
    } else {
        Err(RunnerProtocolCorruption::MissingCanonicalGrant.into())
    }
}

fn stored_registration_identity(
    registration: Option<&StoredValidatedRunnerRegistration>,
) -> (Option<Uuid>, Option<Decimal>) {
    registration
        .map(|registration| {
            (
                Some(registration.registration.enrollment().into_uuid()),
                Some(Decimal::from(registration.revision.get())),
            )
        })
        .unwrap_or((None, None))
}

struct PlacementRecordEvidence {
    registration_identity: (Option<Uuid>, Option<Decimal>),
    grant_origin: Option<Decimal>,
    interrupted_tool_attempt: Option<ToolAttemptId>,
    loss_registration_revision: Option<RunnerRegistrationRevision>,
}

async fn insert_placement_record(
    connection: &mut PgConnection,
    event_ordinal: u64,
    event_kind: &str,
    placement: &SessionRunnerPlacement,
    evidence: PlacementRecordEvidence,
) -> Result<(), RunnerProtocolStoreError> {
    let request = placement.request();
    let (selector_kind, selector_runner, selector_class) = encode_selector(&request.selector);
    let (directory_kind, requested_directory) = encode_directory(&request.working_directory);
    let (workspace_kind, requested_repository) = encode_workspace_requirement(&request.workspace);
    let state = encode_placement_state(placement.state());
    let permission_overrides: Vec<_> = request.permission_overrides.iter().collect();
    let (registration_enrollment, registration_revision) = evidence.registration_identity;
    let mut insert = QueryBuilder::<Postgres>::new(
        "INSERT INTO runner_session_placement_record
            (session_id, event_ordinal, placement_revision, event_kind,
             selector_kind, selector_runner_id, selector_capability_class,
             directory_selection_kind, requested_working_directory,
             requested_credential_profile_name, workspace_requirement_kind,
             requested_repository_key, requested_sandbox_profile,
             permission_override_count, state_kind, lost_runner_id,
             loss_source_kind",
    );
    if evidence.loss_registration_revision.is_some() {
        insert.push(", loss_registration_revision");
    }
    insert.push(
        ", pinned_runner_id,
             interrupted_tool_attempt_id,
             pinned_working_directory, pinned_credential_profile_name,
             registration_enrollment_id, registration_revision,
             pinned_tool_count, workspace_repository_key,
             workspace_working_directory, workspace_manifest_id,
             workspace_placement_revision,
             workspace_clone_url_digest, workspace_credential_profile_name,
             workspace_sandbox_profile, workspace_relative_path,
             workspace_recovery_kind, workspace_branch_name, workspace_revision,
             credential_grant_runner_id,
             credential_grant_lineage_origin_ordinal, credential_grant_revision)
         VALUES (",
    );
    let mut values = insert.separated(", ");
    values.push_bind(placement.session().into_uuid());
    values.push_bind(Decimal::from(event_ordinal));
    values.push_bind(Decimal::from(placement.revision().get()));
    values.push_bind(event_kind);
    values.push_bind(selector_kind);
    values.push_bind(selector_runner);
    values.push_bind(selector_class);
    values.push_bind(directory_kind);
    values.push_bind(requested_directory);
    values.push_bind(
        request
            .credential_profile
            .as_ref()
            .map(CredentialProfileName::as_str),
    );
    values.push_bind(workspace_kind);
    values.push_bind(requested_repository);
    values.push_bind(runner_sandbox_to_str(request.sandbox));
    values.push_bind(count_decimal(permission_overrides.len())?);
    values.push_bind(state.kind);
    values.push_bind(state.lost_runner);
    values.push_bind(state.loss_source);
    if let Some(revision) = evidence.loss_registration_revision {
        values.push_bind(Decimal::from(revision.get()));
    }
    values.push_bind(state.pinned_runner);
    values.push_bind(
        evidence
            .interrupted_tool_attempt
            .map(ToolAttemptId::into_uuid),
    );
    values.push_bind(state.pinned_directory);
    values.push_bind(state.pinned_profile);
    values.push_bind(registration_enrollment);
    values.push_bind(registration_revision);
    values.push_bind(count_decimal(state.tools.len())?);
    values.push_bind(state.workspace_repository);
    values.push_bind(state.workspace_directory);
    values.push_bind(state.workspace_manifest);
    values.push_bind(state.workspace_placement_revision);
    values.push_bind(state.workspace_clone_url_digest);
    values.push_bind(state.workspace_credential_profile);
    values.push_bind(state.workspace_sandbox);
    values.push_bind(state.workspace_relative_path);
    values.push_bind(state.workspace_recovery_kind);
    values.push_bind(state.workspace_branch_name);
    values.push_bind(state.workspace_revision);
    values.push_bind(
        state
            .grant_lineage
            .map(|lineage| lineage.runner.into_uuid()),
    );
    values.push_bind(evidence.grant_origin);
    values.push_bind(
        state
            .grant_lineage
            .map(|lineage| Decimal::from(lineage.revision.get())),
    );
    values.push_unseparated(")");
    insert.build().execute(&mut *connection).await?;
    for tool in state.tools {
        sqlx::query(
            "INSERT INTO runner_session_placement_tool
                (session_id, event_ordinal, tool_name, runner_required)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(placement.session().into_uuid())
        .bind(Decimal::from(event_ordinal))
        .bind(tool.as_str())
        .bind(state.runner_required_tools.contains(tool))
        .execute(&mut *connection)
        .await?;
    }
    for (tool, permission) in permission_overrides {
        sqlx::query(
            "INSERT INTO runner_session_placement_permission_override
                (session_id, event_ordinal, tool_name, permission_kind)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(placement.session().into_uuid())
        .bind(Decimal::from(event_ordinal))
        .bind(tool.as_str())
        .bind(encode_permission_override(permission))
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

pub(crate) async fn insert_initial_session_runner_placement(
    connection: &mut PgConnection,
    placement: &SessionRunnerPlacement,
) -> Result<(), RunnerProtocolStoreError> {
    if placement.revision() != RunnerGeneration::one()
        || !matches!(placement.state(), SessionRunnerPlacementState::Unpinned)
    {
        return Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::InvalidState,
        ));
    }
    insert_placement_record(
        connection,
        1,
        "created",
        placement,
        PlacementRecordEvidence {
            registration_identity: (None, None),
            grant_origin: None,
            interrupted_tool_attempt: None,
            loss_registration_revision: None,
        },
    )
    .await?;
    sqlx::query(
        "INSERT INTO runner_current_session_placement (session_id, event_ordinal)
         VALUES ($1, 1)",
    )
    .bind(placement.session().into_uuid())
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn insert_grant_if_new(
    transaction: &mut Transaction<'_, Postgres>,
    prior_placement: Option<&PgRow>,
    placement_event: u64,
    placement: &SessionRunnerPlacement,
    grant: &CredentialProfileGrant,
    authority: RegistrationAuthority<'_>,
    grant_origin: Decimal,
) -> Result<(), RunnerProtocolStoreError> {
    let registration = authority.stored;
    let catalog = authority.catalog;
    let historical_registration;
    let mut tombstone_policy_event = None;
    let tombstone = matches!(
        pinned_placement(placement.state()),
        Some(pinned) if pinned.credential_profile.is_none()
    );
    let grant_registration = if !tombstone {
        registration
    } else {
        let prior_revision = grant
            .revision()
            .get()
            .checked_sub(1)
            .filter(|revision| *revision > 0)
            .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
        let row = sqlx::query(
            "WITH RECURSIVE grant_line AS (
                 SELECT grant_record.*
                   FROM runner_credential_grant AS grant_record
                  WHERE grant_record.session_id = $1
                    AND grant_record.lineage_origin_event_ordinal = $2
                    AND grant_record.runner_id = $3
                    AND grant_record.grant_revision = $4
                 UNION ALL
                 SELECT predecessor.*
                   FROM grant_line AS successor
                   JOIN runner_credential_grant AS predecessor
                     ON predecessor.session_id = successor.session_id
                    AND predecessor.lineage_origin_event_ordinal =
                        successor.lineage_origin_event_ordinal
                    AND predecessor.runner_id = successor.prior_runner_id
                    AND predecessor.grant_revision = successor.prior_grant_revision
             )
             SELECT grant_line.registration_enrollment_id,
                    grant_line.registration_revision,
                    grant_line.placement_event_ordinal
               FROM grant_line
               JOIN runner_session_placement_record AS placement
                 ON placement.session_id = grant_line.session_id
                AND placement.event_ordinal = grant_line.placement_event_ordinal
              WHERE placement.pinned_credential_profile_name IS NOT NULL
              ORDER BY grant_line.grant_revision DESC
              LIMIT 1",
        )
        .bind(grant.session().into_uuid())
        .bind(grant_origin)
        .bind(grant.runner().into_uuid())
        .bind(Decimal::from(prior_revision))
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
        tombstone_policy_event = Some(row.decode_column::<Decimal>("placement_event_ordinal")?);
        historical_registration = load_registration_in(
            transaction.as_mut(),
            runner_enrollment_id(row.decode_column("registration_enrollment_id")?),
            decode_registration_revision(row.decode_column("registration_revision")?)?,
            None,
            catalog,
        )
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
        &historical_registration
    };
    let (grant_sandbox, grant_permission_overrides) = if tombstone {
        let policy_event =
            tombstone_policy_event.ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
        let prior_record = sqlx::query(
            "SELECT *
               FROM runner_session_placement_record
              WHERE session_id = $1 AND event_ordinal = $2",
        )
        .bind(placement.session().into_uuid())
        .bind(policy_event)
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
        (
            decode_sandbox(prior_record.decode_column("requested_sandbox_profile")?)?,
            load_permission_overrides(transaction.as_mut(), &prior_record).await?,
        )
    } else {
        (
            placement.request().sandbox,
            placement.request().permission_overrides.clone(),
        )
    };
    CredentialProfileGrant::reconstitute(
        grant_input(grant),
        grant.session(),
        grant_registration.registration(),
        grant_sandbox,
        &grant_permission_overrides,
    )
    .map_err(RunnerProtocolStoreError::Domain)?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM runner_credential_grant
              WHERE session_id = $1
                AND lineage_origin_event_ordinal = $2
                AND runner_id = $3
                AND grant_revision = $4
         )",
    )
    .bind(grant.session().into_uuid())
    .bind(grant_origin)
    .bind(grant.runner().into_uuid())
    .bind(Decimal::from(grant.revision().get()))
    .fetch_one(&mut **transaction)
    .await?;
    if exists {
        let row = sqlx::query(
            "SELECT grant_record.*,
                    EXISTS (
                        SELECT 1
                          FROM runner_credential_grant_audit AS audit
                         WHERE audit.session_id = grant_record.session_id
                           AND audit.lineage_origin_event_ordinal =
                                grant_record.lineage_origin_event_ordinal
                           AND audit.runner_id = grant_record.runner_id
                           AND audit.grant_revision =
                                grant_record.grant_revision
                           AND audit.event_kind = 'revoked'
                    ) AS revoked
               FROM runner_credential_grant AS grant_record
              WHERE grant_record.session_id = $1
                AND grant_record.lineage_origin_event_ordinal = $2
                AND grant_record.runner_id = $3
                AND grant_record.grant_revision = $4",
        )
        .bind(grant.session().into_uuid())
        .bind(grant_origin)
        .bind(grant.runner().into_uuid())
        .bind(Decimal::from(grant.revision().get()))
        .fetch_one(&mut **transaction)
        .await?;
        let tool_rows = sqlx::query(
            "SELECT tool_name, approval_kind
               FROM runner_credential_grant_tool
              WHERE session_id = $1
                AND lineage_origin_event_ordinal = $2
                AND runner_id = $3
                AND grant_revision = $4
              ORDER BY tool_name",
        )
        .bind(grant.session().into_uuid())
        .bind(grant_origin)
        .bind(grant.runner().into_uuid())
        .bind(Decimal::from(grant.revision().get()))
        .fetch_all(&mut **transaction)
        .await?;
        require_count(&row, "tool_count", tool_rows.len())?;
        let mut approvals = BTreeMap::new();
        for tool_row in tool_rows {
            approvals.insert(
                tool_name(tool_row.decode_column("tool_name")?)?,
                decode_approval(tool_row.decode_column("approval_kind")?)?,
            );
        }
        let expected_approvals: BTreeMap<_, _> = grant
            .approvals()
            .map(|(tool, approval)| (tool.clone(), approval))
            .collect();
        let stored_state =
            match decode_stored_grant_revocation(row.decode_column::<bool>("revoked")?) {
                StoredGrantRevocation::Active => CredentialProfileGrantState::Active,
                StoredGrantRevocation::Revoked => CredentialProfileGrantState::Revoked,
            };
        if row.decode_column::<String>("credential_profile_name")? != grant.profile().as_str()
            || runner_enrollment_id(row.decode_column("registration_enrollment_id")?)
                != grant_registration.registration.enrollment()
            || decode_registration_revision(row.decode_column("registration_revision")?)?
                != grant_registration.revision
            || approvals != expected_approvals
            || stored_state != grant.state()
        {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        return Ok(());
    }
    let tools: Vec<_> = grant.approvals().collect();
    let prior = grant
        .revision()
        .get()
        .checked_sub(1)
        .filter(|value| *value > 0)
        .map(Decimal::from);
    let prior_runner: Option<Uuid> = match (prior, prior_placement) {
        (Some(expected_revision), Some(prior_placement)) => {
            let runner = prior_placement
                .decode_column::<Option<Uuid>>("credential_grant_runner_id")?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
            let origin = prior_placement
                .decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
            let revision = prior_placement
                .decode_column::<Option<Decimal>>("credential_grant_revision")?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
            if origin != grant_origin || revision != expected_revision {
                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
            }
            Some(runner)
        }
        (Some(_), None) => return Err(RunnerProtocolCorruption::MissingCanonicalGrant.into()),
        (None, _) => None,
    };
    sqlx::query(
        "INSERT INTO runner_credential_grant
            (session_id, lineage_origin_event_ordinal,
             runner_id, grant_revision, credential_profile_name,
             registration_enrollment_id, registration_revision,
             placement_event_ordinal, prior_runner_id,
             prior_grant_revision, tool_count)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(grant.session().into_uuid())
    .bind(grant_origin)
    .bind(grant.runner().into_uuid())
    .bind(Decimal::from(grant.revision().get()))
    .bind(grant.profile().as_str())
    .bind(grant_registration.registration.enrollment().into_uuid())
    .bind(Decimal::from(grant_registration.revision.get()))
    .bind(Decimal::from(placement_event))
    .bind(prior_runner)
    .bind(prior)
    .bind(count_decimal(tools.len())?)
    .execute(&mut **transaction)
    .await?;
    for (tool, approval) in tools {
        sqlx::query(
            "INSERT INTO runner_credential_grant_tool
                (session_id, lineage_origin_event_ordinal,
                 runner_id, grant_revision, credential_profile_name,
                 tool_name, approval_kind)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(grant.session().into_uuid())
        .bind(grant_origin)
        .bind(grant.runner().into_uuid())
        .bind(Decimal::from(grant.revision().get()))
        .bind(grant.profile().as_str())
        .bind(tool.as_str())
        .bind(encode_approval(approval))
        .execute(&mut **transaction)
        .await?;
    }
    sqlx::query(
        "INSERT INTO runner_credential_grant_audit
            (session_id, lineage_origin_event_ordinal,
             runner_id, grant_revision, audit_ordinal,
             event_kind, credential_profile_name)
         VALUES ($1, $2, $3, $4, 1, $5, $6)",
    )
    .bind(grant.session().into_uuid())
    .bind(grant_origin)
    .bind(grant.runner().into_uuid())
    .bind(Decimal::from(grant.revision().get()))
    .bind(if grant.revision() == RunnerGeneration::one() {
        "issued"
    } else {
        "replaced"
    })
    .bind(grant.profile().as_str())
    .execute(&mut **transaction)
    .await?;
    if credential_grant_is_revoked(grant.state()) {
        sqlx::query(
            "INSERT INTO runner_credential_grant_audit
                (session_id, lineage_origin_event_ordinal,
                 runner_id, grant_revision, audit_ordinal,
                 event_kind, credential_profile_name)
             VALUES ($1, $2, $3, $4, 2, 'revoked', $5)",
        )
        .bind(grant.session().into_uuid())
        .bind(grant_origin)
        .bind(grant.runner().into_uuid())
        .bind(Decimal::from(grant.revision().get()))
        .bind(grant.profile().as_str())
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

async fn load_placement_registration(
    connection: &mut PgConnection,
    row: &PgRow,
    catalog: &RunnerCatalog,
) -> Result<Option<StoredValidatedRunnerRegistration>, RunnerProtocolStoreError> {
    let enrollment = row.decode_column::<Option<Uuid>>("registration_enrollment_id")?;
    let revision = row.decode_column::<Option<Decimal>>("registration_revision")?;
    match (enrollment, revision) {
        (None, None) => Ok(None),
        (Some(enrollment), Some(revision)) => load_registration_in(
            connection,
            runner_enrollment_id(enrollment),
            decode_registration_revision(revision)?,
            None,
            catalog,
        )
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)
        .map(Some)
        .map_err(Into::into),
        _ => Err(RunnerProtocolCorruption::CrossWiredReference.into()),
    }
}

async fn load_permission_overrides(
    connection: &mut PgConnection,
    row: &PgRow,
) -> Result<RunnerToolPermissionOverrides, RunnerProtocolStoreError> {
    let session = row.decode_column::<Uuid>("session_id")?;
    let event = row.decode_column::<Decimal>("event_ordinal")?;
    let override_rows = sqlx::query(
        "SELECT tool_name, permission_kind
           FROM runner_session_placement_permission_override
          WHERE session_id = $1 AND event_ordinal = $2
          ORDER BY tool_name",
    )
    .bind(session)
    .bind(event)
    .fetch_all(&mut *connection)
    .await?;
    require_count(row, "permission_override_count", override_rows.len())?;
    RunnerToolPermissionOverrides::try_new(
        override_rows
            .iter()
            .map(|override_row| {
                Ok((
                    tool_name(override_row.decode_column("tool_name")?)?,
                    decode_permission_override(override_row.decode_column("permission_kind")?)?,
                ))
            })
            .collect::<Result<Vec<_>, RunnerProtocolStoreError>>()?,
    )
    .map_err(RunnerProtocolStoreError::Domain)
}

async fn decode_placement(
    connection: &mut PgConnection,
    row: &PgRow,
    catalog: &RunnerCatalog,
    grant_policies: &GrantPolicyIndex,
    registration: Option<&ValidatedRunnerRegistration>,
    profileless_tombstone: Option<&CredentialProfileGrant>,
) -> Result<SessionRunnerPlacement, RunnerProtocolStoreError> {
    let session = session_id(row.decode_column("session_id")?);
    let event_ordinal = decode_u64(row.decode_column("event_ordinal")?)?;
    let placement_revision = decode_generation(row.decode_column("placement_revision")?)?;
    let request = decode_placement_request(connection, row).await?;
    let permission_overrides = request.permission_overrides.clone();
    let event_kind: String = row.decode_column("event_kind")?;
    let state_kind: String = row.decode_column("state_kind")?;
    let event_matches_state = match state_kind.as_str() {
        "unpinned" => match event_kind.as_str() {
            "created" => event_ordinal == 1 && placement_revision == RunnerGeneration::one(),
            "pre_pin_replaced" => {
                event_ordinal > 1 && placement_revision != RunnerGeneration::one()
            }
            _ => false,
        },
        "pinned" => matches!(
            event_kind.as_str(),
            "pinned" | "runner_replaced" | "profile_replaced"
        ),
        "runner_lost_before_pin" => event_kind == "runner_lost_before_pin",
        "runner_lost" => event_kind == "runner_lost",
        "runner_abandoned" => event_kind == "abandoned",
        _ => false,
    };
    if !event_matches_state {
        return Err(RunnerProtocolCorruption::InvalidEncoding.into());
    }
    let lost_runner = row
        .decode_column::<Option<Uuid>>("lost_runner_id")?
        .map(runner_id);
    let loss_source = row
        .decode_column::<Option<String>>("loss_source_kind")?
        .map(|source| {
            runner_placement_loss_source_from_str(&source)
                .ok_or(RunnerProtocolCorruption::InvalidEncoding)
        })
        .transpose()?;
    let state = if state_kind == "unpinned" {
        if placement_row_has_invalid_unpinned_facts(row)? {
            return Err(RunnerProtocolCorruption::InvalidEncoding.into());
        }
        SessionRunnerPlacementState::Unpinned
    } else if state_kind == "runner_lost_before_pin" {
        let runner = lost_runner.ok_or(RunnerProtocolCorruption::IncompleteInventory)?;
        if placement_row_has_invalid_pre_pin_loss_facts(row)? {
            return Err(RunnerProtocolCorruption::InvalidEncoding.into());
        }
        SessionRunnerPlacementState::RunnerLostBeforePin(RunnerLostBeforePin::from_stored(runner))
    } else if state_kind == "runner_abandoned"
        && row
            .decode_column::<Option<Uuid>>("pinned_runner_id")?
            .is_none()
    {
        let runner = lost_runner.ok_or(RunnerProtocolCorruption::IncompleteInventory)?;
        if placement_row_has_invalid_pre_pin_loss_facts(row)? {
            return Err(RunnerProtocolCorruption::InvalidEncoding.into());
        }
        SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::BeforePin(
            RunnerLostBeforePin::from_stored(runner),
        ))
    } else {
        let loss_registration_revision =
            if matches!(state_kind.as_str(), "runner_lost" | "runner_abandoned") {
                row.decode_column::<Option<Decimal>>("loss_registration_revision")?
                    .map(decode_generation)
                    .transpose()?
            } else {
                None
            };
        let pinned = decode_pinned_placement(
            connection,
            row,
            session,
            request.sandbox,
            permission_overrides,
        )
        .await?;
        let runner = pinned.runner;
        let loss_registration = match (loss_source, loss_registration_revision) {
            (Some(RunnerPlacementLossSource::Registration), Some(revision)) => {
                let pinned_registration =
                    registration.ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
                let revision = RunnerRegistrationRevision::try_from_u64(revision.get())
                    .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
                load_registration_in(
                    connection,
                    pinned_registration.enrollment(),
                    revision,
                    None,
                    catalog,
                )
                .await?
            }
            (Some(RunnerPlacementLossSource::Connection), _)
            | (Some(RunnerPlacementLossSource::Registration), None)
            | (None, _) => None,
        };
        let registration_loss = match (loss_source, registration, loss_registration.as_ref()) {
            (
                Some(RunnerPlacementLossSource::Registration),
                Some(pinned_registration),
                Some(loss_registration),
            ) => Some(StoredRunnerRegistrationLossEvidence {
                pinned_registration,
                loss_registration: loss_registration.registration(),
            }),
            (Some(RunnerPlacementLossSource::Connection), _, _)
            | (Some(RunnerPlacementLossSource::Registration), None, _)
            | (Some(RunnerPlacementLossSource::Registration), Some(_), None)
            | (None, _, _) => None,
        };
        match (state_kind.as_str(), lost_runner, loss_source) {
            ("pinned", None, None) => SessionRunnerPlacementState::Pinned(pinned),
            ("runner_lost", Some(lost), Some(source)) if lost == runner => {
                SessionRunnerPlacementState::RunnerLost(LostPinnedRunnerPlacement::from_stored(
                    pinned,
                    source,
                    registration_loss,
                ))
            }
            ("runner_abandoned", Some(lost), Some(source)) if lost == runner => {
                SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::Pinned(
                    Box::new(LostPinnedRunnerPlacement::from_stored(
                        pinned,
                        source,
                        registration_loss,
                    )),
                ))
            }
            _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
        }
    };
    authenticate_pinned_predecessor(connection, row, catalog, grant_policies, &request, &state)
        .await?;
    authenticate_loss_predecessor(connection, row, catalog, grant_policies, &request, &state)
        .await?;
    authenticate_abandonment_predecessor(
        connection,
        row,
        catalog,
        grant_policies,
        &request,
        &state,
    )
    .await?;
    let history = match &state {
        SessionRunnerPlacementState::Unpinned
        | SessionRunnerPlacementState::RunnerLostBeforePin(_)
        | SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::BeforePin(_)) => {
            load_placement_reconstitution_history(connection, row).await?
        }
        SessionRunnerPlacementState::Pinned(_)
        | SessionRunnerPlacementState::RunnerLost(_)
        | SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::Pinned(_)) => {
            RunnerPlacementReconstitutionHistory::Initial
        }
    };
    SessionRunnerPlacement::reconstitute(
        SessionRunnerPlacementReconstitutionInput {
            session,
            revision: placement_revision,
            request,
            state,
            history,
        },
        session,
        registration,
        profileless_tombstone,
    )
    .map_err(RunnerProtocolStoreError::Domain)
}

async fn authenticate_loss_predecessor(
    connection: &mut PgConnection,
    row: &PgRow,
    catalog: &RunnerCatalog,
    grant_policies: &GrantPolicyIndex,
    request: &SessionRunnerPlacementRequest,
    state: &SessionRunnerPlacementState,
) -> Result<(), RunnerProtocolStoreError> {
    match state {
        SessionRunnerPlacementState::RunnerLostBeforePin(_) => {
            return authenticate_pre_pin_loss_predecessor(connection, row, request).await;
        }
        SessionRunnerPlacementState::RunnerLost(_) => {}
        SessionRunnerPlacementState::Unpinned
        | SessionRunnerPlacementState::Pinned(_)
        | SessionRunnerPlacementState::RunnerAbandoned(_) => return Ok(()),
    }
    let (predecessor, predecessor_request, pinned) =
        load_authenticated_pinned_loss_predecessor(connection, row, request, state).await?;
    authenticate_registration_loss_cause(connection, row, &predecessor).await?;
    authenticate_pinned_predecessor(
        connection,
        &predecessor,
        catalog,
        grant_policies,
        &predecessor_request,
        &SessionRunnerPlacementState::Pinned(pinned),
    )
    .await
}

async fn load_authenticated_pinned_loss_predecessor(
    connection: &mut PgConnection,
    row: &PgRow,
    request: &SessionRunnerPlacementRequest,
    state: &SessionRunnerPlacementState,
) -> Result<(PgRow, SessionRunnerPlacementRequest, PinnedRunnerPlacement), RunnerProtocolStoreError>
{
    let SessionRunnerPlacementState::RunnerLost(lost) = state else {
        return Err(RunnerProtocolCorruption::InvalidEncoding.into());
    };
    let session = session_id(row.decode_column("session_id")?);
    let predecessor_ordinal = decode_u64(row.decode_column("event_ordinal")?)?
        .checked_sub(1)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
    let predecessor = sqlx::query(
        "SELECT *
           FROM runner_session_placement_record
          WHERE session_id = $1 AND event_ordinal = $2",
    )
    .bind(session.into_uuid())
    .bind(Decimal::from(predecessor_ordinal))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
    let predecessor_request = decode_placement_request(connection, &predecessor).await?;
    let predecessor_revision = decode_generation(predecessor.decode_column("placement_revision")?)?;
    if predecessor_revision != decode_generation(row.decode_column("placement_revision")?)?
        || &predecessor_request != request
    {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let predecessor_event: String = predecessor.decode_column("event_kind")?;
    let predecessor_state: String = predecessor.decode_column("state_kind")?;
    if predecessor_state != "pinned"
        || !matches!(
            predecessor_event.as_str(),
            "pinned" | "runner_replaced" | "profile_replaced"
        )
        || placement_row_has_loss_facts(&predecessor)?
    {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let pinned = decode_pinned_placement(
        connection,
        &predecessor,
        session,
        predecessor_request.sandbox,
        predecessor_request.permission_overrides.clone(),
    )
    .await?;
    let predecessor_registration = decode_pinned_registration_identity(&predecessor)?;
    let loss_registration = decode_pinned_registration_identity(row)?;
    if pinned != *lost.pinned() || predecessor_registration != loss_registration {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    Ok((predecessor, predecessor_request, pinned))
}

async fn authenticate_registration_loss_cause(
    connection: &mut PgConnection,
    loss: &PgRow,
    predecessor: &PgRow,
) -> Result<(), RunnerProtocolStoreError> {
    let source = loss
        .decode_column::<Option<String>>("loss_source_kind")?
        .map(|source| {
            runner_placement_loss_source_from_str(&source)
                .ok_or(RunnerProtocolCorruption::InvalidEncoding)
        })
        .transpose()?
        .ok_or(RunnerProtocolCorruption::IncompleteInventory)?;
    let cause = loss
        .decode_column::<Option<Decimal>>("loss_registration_revision")?
        .map(decode_registration_revision)
        .transpose()?;
    if source == RunnerPlacementLossSource::Connection {
        return match cause {
            None => Ok(()),
            Some(_) => Err(RunnerProtocolCorruption::CrossWiredReference.into()),
        };
    }
    let cause = cause.ok_or(RunnerProtocolCorruption::IncompleteInventory)?;
    let (enrollment, pinned_revision) = decode_pinned_registration_identity(loss)?;
    if cause <= pinned_revision {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let session = session_id(loss.decode_column("session_id")?);
    let predecessor_event = predecessor.decode_column::<Decimal>("event_ordinal")?;
    let preserves = sqlx::query_scalar::<_, bool>(
        "SELECT runner_registration_preserves_placement($1, $2, $3, $4)",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(cause.get()))
    .bind(session.into_uuid())
    .bind(predecessor_event)
    .fetch_one(&mut *connection)
    .await?;
    let observed = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM runner_registration_reconciliation_observation
             WHERE enrollment_id = $1
               AND registration_revision = $2
               AND session_id = $3
               AND placement_event_ordinal = $4
               AND disposition_kind = 'runner_lost'
        )",
    )
    .bind(enrollment.into_uuid())
    .bind(Decimal::from(cause.get()))
    .bind(session.into_uuid())
    .bind(loss.decode_column::<Decimal>("event_ordinal")?)
    .fetch_one(&mut *connection)
    .await?;
    if preserves || !observed {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    Ok(())
}

async fn authenticate_pre_pin_loss_predecessor(
    connection: &mut PgConnection,
    row: &PgRow,
    request: &SessionRunnerPlacementRequest,
) -> Result<(), RunnerProtocolStoreError> {
    let session = row.decode_column::<Uuid>("session_id")?;
    let predecessor_ordinal = decode_u64(row.decode_column("event_ordinal")?)?
        .checked_sub(1)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
    let predecessor = sqlx::query(
        "SELECT *
           FROM runner_session_placement_record
          WHERE session_id = $1 AND event_ordinal = $2",
    )
    .bind(session)
    .bind(Decimal::from(predecessor_ordinal))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
    let predecessor_revision = decode_generation(predecessor.decode_column("placement_revision")?)?;
    let predecessor_event: String = predecessor.decode_column("event_kind")?;
    let predecessor_state: String = predecessor.decode_column("state_kind")?;
    let canonical_origin = match predecessor_event.as_str() {
        "created" => predecessor_ordinal == 1 && predecessor_revision == RunnerGeneration::one(),
        "pre_pin_replaced" => {
            predecessor_ordinal > 1 && predecessor_revision != RunnerGeneration::one()
        }
        _ => false,
    };
    if predecessor_state != "unpinned"
        || !canonical_origin
        || predecessor_revision != decode_generation(row.decode_column("placement_revision")?)?
        || decode_placement_request(connection, &predecessor).await? != *request
        || placement_row_has_invalid_unpinned_facts(&predecessor)?
    {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    Ok(())
}

async fn authenticate_pinned_predecessor(
    connection: &mut PgConnection,
    initial_row: &PgRow,
    catalog: &RunnerCatalog,
    grant_policies: &GrantPolicyIndex,
    request: &SessionRunnerPlacementRequest,
    state: &SessionRunnerPlacementState,
) -> Result<(), RunnerProtocolStoreError> {
    let SessionRunnerPlacementState::Pinned(pinned) = state else {
        return Ok(());
    };
    let mut current_row = None;
    let mut current_request = request.clone();
    let mut current_pinned = pinned.clone();
    loop {
        let row = current_row.as_ref().unwrap_or(initial_row);
        authenticate_pinned_registration(
            connection,
            row,
            catalog,
            grant_policies,
            &current_request,
            &current_pinned,
        )
        .await?;
        let session = session_id(row.decode_column("session_id")?);
        let predecessor_ordinal = decode_u64(row.decode_column("event_ordinal")?)?
            .checked_sub(1)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        let predecessor = sqlx::query(
            "SELECT *
               FROM runner_session_placement_record
              WHERE session_id = $1 AND event_ordinal = $2",
        )
        .bind(session.into_uuid())
        .bind(Decimal::from(predecessor_ordinal))
        .fetch_optional(&mut *connection)
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
        let event: String = row.decode_column("event_kind")?;
        let predecessor_event: String = predecessor.decode_column("event_kind")?;
        let predecessor_state: String = predecessor.decode_column("state_kind")?;
        let revision = decode_generation(row.decode_column("placement_revision")?)?;
        let predecessor_revision =
            decode_generation(predecessor.decode_column("placement_revision")?)?;
        let predecessor_request = decode_placement_request(connection, &predecessor).await?;
        let workspace_is_fresh = current_pinned
            .workspace
            .as_ref()
            .is_none_or(|workspace| workspace.placement_revision == revision);
        match event.as_str() {
            "pinned"
                if predecessor_state == "unpinned"
                    && predecessor_revision == revision
                    && predecessor_request == current_request
                    && workspace_is_fresh
                    && match predecessor_event.as_str() {
                        "created" => {
                            predecessor_ordinal == 1
                                && predecessor_revision == RunnerGeneration::one()
                        }
                        "pre_pin_replaced" => {
                            predecessor_ordinal > 1
                                && predecessor_revision != RunnerGeneration::one()
                        }
                        _ => false,
                    }
                    && !placement_row_has_invalid_unpinned_facts(&predecessor)? =>
            {
                if predecessor_event == "pre_pin_replaced" {
                    load_placement_reconstitution_history(connection, &predecessor).await?;
                }
                return Ok(());
            }
            "runner_replaced"
                if predecessor_event == "runner_lost"
                    && predecessor_state == "runner_lost"
                    && predecessor_revision.checked_next() == Some(revision) =>
            {
                if placement_row_has_loss_facts(row)? {
                    return Err(RunnerProtocolCorruption::InvalidEncoding.into());
                }
                let prior_pinned = decode_pinned_placement(
                    connection,
                    &predecessor,
                    session,
                    predecessor_request.sandbox,
                    predecessor_request.permission_overrides.clone(),
                )
                .await?;
                let source = predecessor
                    .decode_column::<Option<String>>("loss_source_kind")?
                    .map(|source| {
                        runner_placement_loss_source_from_str(&source)
                            .ok_or(RunnerProtocolCorruption::InvalidEncoding)
                    })
                    .transpose()?
                    .ok_or(RunnerProtocolCorruption::IncompleteInventory)?;
                let loss_registration_revision = predecessor
                    .decode_column::<Option<Decimal>>("loss_registration_revision")?
                    .map(decode_generation)
                    .transpose()?;
                let lost_runner = predecessor
                    .decode_column::<Option<Uuid>>("lost_runner_id")?
                    .map(runner_id)
                    .ok_or(RunnerProtocolCorruption::IncompleteInventory)?;
                let durable_grant =
                    durable_grant_predecessor_matches_placement(connection, &predecessor, row)
                        .await?;
                let grant_succeeds = runner_replacement_grant_is_successor(&predecessor, row)?
                    && durable_grant.matches;
                let pinned_registration =
                    load_placement_registration(connection, &predecessor, catalog)
                        .await?
                        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
                let successor_registration = load_placement_registration(connection, row, catalog)
                    .await?
                    .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
                let loss_registration = match source {
                    RunnerPlacementLossSource::Connection => None,
                    RunnerPlacementLossSource::Registration => {
                        let revision = loss_registration_revision
                            .ok_or(RunnerProtocolCorruption::IncompleteInventory)?;
                        let revision = RunnerRegistrationRevision::try_from_u64(revision.get())
                            .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
                        load_registration_in(
                            connection,
                            pinned_registration.registration().enrollment(),
                            revision,
                            None,
                            catalog,
                        )
                        .await?
                    }
                };
                let same_runner_replacement_admitted = match source {
                    RunnerPlacementLossSource::Connection => false,
                    RunnerPlacementLossSource::Registration => loss_registration_revision
                        .is_some_and(|loss| {
                            pinned_registration.registration().enrollment()
                                == successor_registration.registration().enrollment()
                                && pinned_registration.registration().authentication()
                                    == successor_registration.registration().authentication()
                                && successor_registration.revision().get() >= loss.get()
                        }),
                };
                if lost_runner != prior_pinned.runner
                    || (current_pinned.runner == lost_runner && !same_runner_replacement_admitted)
                    || !grant_succeeds
                    || !workspace_is_fresh
                {
                    return Err(RunnerProtocolCorruption::CrossWiredReference.into());
                }
                let registration_loss = match (source, loss_registration.as_ref()) {
                    (RunnerPlacementLossSource::Connection, _) => None,
                    (RunnerPlacementLossSource::Registration, Some(loss_registration)) => {
                        Some(StoredRunnerRegistrationLossEvidence {
                            pinned_registration: pinned_registration.registration(),
                            loss_registration: loss_registration.registration(),
                        })
                    }
                    (RunnerPlacementLossSource::Registration, None) => None,
                };
                let lost = SessionRunnerPlacementState::RunnerLost(
                    LostPinnedRunnerPlacement::from_stored(prior_pinned, source, registration_loss),
                );
                let (prior_row, prior_request, prior_pinned) =
                    load_authenticated_pinned_loss_predecessor(
                        connection,
                        &predecessor,
                        &predecessor_request,
                        &lost,
                    )
                    .await?;
                authenticate_registration_loss_cause(connection, &predecessor, &prior_row).await?;
                current_row = Some(prior_row);
                current_request = prior_request;
                current_pinned = prior_pinned;
            }
            "profile_replaced"
                if predecessor_state == "pinned"
                    && predecessor_revision.checked_next() == Some(revision) =>
            {
                let prior_pinned = decode_pinned_placement(
                    connection,
                    &predecessor,
                    session,
                    predecessor_request.sandbox,
                    predecessor_request.permission_overrides.clone(),
                )
                .await?;
                let same_request_axes = predecessor_request.selector == current_request.selector
                    && predecessor_request.working_directory == current_request.working_directory
                    && predecessor_request.workspace == current_request.workspace
                    && predecessor_request.sandbox == current_request.sandbox
                    && predecessor_request.permission_overrides
                        == current_request.permission_overrides;
                let same_pinned_axes = prior_pinned.runner == current_pinned.runner
                    && prior_pinned.working_directory == current_pinned.working_directory
                    && prior_pinned.tools == current_pinned.tools
                    && prior_pinned.runner_required_tools == current_pinned.runner_required_tools
                    && prior_pinned.workspace == current_pinned.workspace
                    && prior_pinned.sandbox == current_pinned.sandbox
                    && prior_pinned.permission_overrides == current_pinned.permission_overrides;
                let same_registration = decode_pinned_registration_identity(&predecessor)?
                    == decode_pinned_registration_identity(row)?;
                let grant_advances =
                    match (prior_pinned.grant_lineage, current_pinned.grant_lineage) {
                        (Some(before), Some(after)) => {
                            before.runner == after.runner
                                && before.revision.checked_next() == Some(after.revision)
                        }
                        (None, None) | (None, Some(_)) | (Some(_), None) => false,
                    };
                let durable_grant =
                    durable_grant_predecessor_matches_placement(connection, &predecessor, row)
                        .await?;
                if same_request_axes
                    && same_pinned_axes
                    && same_registration
                    && grant_advances
                    && durable_grant.matches
                    && !durable_grant.predecessor_revoked
                {
                    current_row = Some(predecessor);
                    current_request = predecessor_request;
                    current_pinned = prior_pinned;
                } else {
                    return Err(RunnerProtocolCorruption::CrossWiredReference.into());
                }
            }
            "pinned" | "runner_replaced" | "profile_replaced" => {
                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
            }
            _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
        }
    }
}

async fn authenticate_pinned_registration(
    connection: &mut PgConnection,
    row: &PgRow,
    catalog: &RunnerCatalog,
    grant_policies: &GrantPolicyIndex,
    request: &SessionRunnerPlacementRequest,
    pinned: &PinnedRunnerPlacement,
) -> Result<(), RunnerProtocolStoreError> {
    let session = session_id(row.decode_column("session_id")?);
    let registration = load_placement_registration(connection, row, catalog)
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
    let grant = load_grant_for_placement(connection, row, catalog, grant_policies).await?;
    let pinned_profile = row.decode_column::<Option<String>>("pinned_credential_profile_name")?;
    let profileless_tombstone = grant
        .as_ref()
        .filter(|grant| credential_grant_is_revoked(grant.state()) && pinned_profile.is_none());
    SessionRunnerPlacement::reconstitute(
        SessionRunnerPlacementReconstitutionInput {
            session,
            revision: decode_generation(row.decode_column("placement_revision")?)?,
            request: request.clone(),
            state: SessionRunnerPlacementState::Pinned(pinned.clone()),
            history: RunnerPlacementReconstitutionHistory::Initial,
        },
        session,
        Some(registration.registration()),
        profileless_tombstone,
    )
    .map(|_| ())
    .map_err(RunnerProtocolStoreError::Domain)
}

fn runner_replacement_grant_is_successor(
    predecessor: &PgRow,
    replacement: &PgRow,
) -> Result<bool, RunnerProtocolStoreError> {
    let predecessor_revision = predecessor
        .decode_column::<Option<Decimal>>("credential_grant_revision")?
        .map(decode_generation)
        .transpose()?;
    let replacement_revision = replacement
        .decode_column::<Option<Decimal>>("credential_grant_revision")?
        .map(decode_generation)
        .transpose()?;
    let predecessor_origin =
        predecessor.decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?;
    let replacement_origin =
        replacement.decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?;
    let replacement_event = replacement.decode_column::<Decimal>("event_ordinal")?;
    Ok(match (predecessor_revision, replacement_revision) {
        (None, None) => predecessor_origin.is_none() && replacement_origin.is_none(),
        (None, Some(revision)) => {
            predecessor_origin.is_none()
                && revision == RunnerGeneration::one()
                && replacement_origin == Some(replacement_event)
        }
        (Some(before), Some(after)) => {
            before.checked_next() == Some(after) && predecessor_origin == replacement_origin
        }
        (Some(_), None) => false,
    })
}

struct DurableGrantPredecessorEvidence {
    matches: bool,
    predecessor_revoked: bool,
}

async fn durable_grant_predecessor_matches_placement(
    connection: &mut PgConnection,
    predecessor: &PgRow,
    successor: &PgRow,
) -> Result<DurableGrantPredecessorEvidence, RunnerProtocolStoreError> {
    let predecessor_origin =
        predecessor.decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?;
    let predecessor_runner =
        predecessor.decode_column::<Option<Uuid>>("credential_grant_runner_id")?;
    let predecessor_revision =
        predecessor.decode_column::<Option<Decimal>>("credential_grant_revision")?;
    let successor_origin =
        successor.decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?;
    let successor_runner = successor.decode_column::<Option<Uuid>>("credential_grant_runner_id")?;
    let successor_revision =
        successor.decode_column::<Option<Decimal>>("credential_grant_revision")?;
    let predecessor_identity = match (predecessor_origin, predecessor_runner, predecessor_revision)
    {
        (None, None, None) => None,
        (Some(origin), Some(runner), Some(revision)) => Some((origin, runner, revision)),
        _ => {
            return Ok(DurableGrantPredecessorEvidence {
                matches: false,
                predecessor_revoked: false,
            });
        }
    };
    let successor_identity = match (successor_origin, successor_runner, successor_revision) {
        (None, None, None) => {
            return Ok(DurableGrantPredecessorEvidence {
                matches: predecessor_identity.is_none(),
                predecessor_revoked: false,
            });
        }
        (Some(origin), Some(runner), Some(revision)) => (origin, runner, revision),
        _ => {
            return Ok(DurableGrantPredecessorEvidence {
                matches: false,
                predecessor_revoked: false,
            });
        }
    };
    let grant = sqlx::query(
        "SELECT grant_record.placement_event_ordinal,
                grant_record.prior_runner_id,
                grant_record.prior_grant_revision,
                EXISTS (
                    SELECT 1
                      FROM runner_credential_grant_audit AS audit
                     WHERE audit.session_id = grant_record.session_id
                       AND audit.lineage_origin_event_ordinal =
                            grant_record.lineage_origin_event_ordinal
                       AND audit.runner_id = grant_record.prior_runner_id
                       AND audit.grant_revision = grant_record.prior_grant_revision
                       AND audit.event_kind = 'revoked'
                ) AS predecessor_revoked
           FROM runner_credential_grant AS grant_record
          WHERE grant_record.session_id = $1
            AND grant_record.lineage_origin_event_ordinal = $2
            AND grant_record.runner_id = $3
            AND grant_record.grant_revision = $4",
    )
    .bind(successor.decode_column::<Uuid>("session_id")?)
    .bind(successor_identity.0)
    .bind(successor_identity.1)
    .bind(successor_identity.2)
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
    let placement_event_ordinal = grant.decode_column::<Decimal>("placement_event_ordinal")?;
    let successor_event_ordinal = successor.decode_column::<Decimal>("event_ordinal")?;
    if placement_event_ordinal != successor_event_ordinal {
        return Ok(DurableGrantPredecessorEvidence {
            matches: false,
            predecessor_revoked: false,
        });
    }
    let predecessor_revoked = grant.decode_column::<bool>("predecessor_revoked")?;
    let durable_predecessor = match (
        grant.decode_column::<Option<Uuid>>("prior_runner_id")?,
        grant.decode_column::<Option<Decimal>>("prior_grant_revision")?,
    ) {
        (None, None) => None,
        (Some(runner), Some(revision)) => Some((successor_identity.0, runner, revision)),
        _ => {
            return Ok(DurableGrantPredecessorEvidence {
                matches: false,
                predecessor_revoked,
            });
        }
    };
    Ok(DurableGrantPredecessorEvidence {
        matches: durable_predecessor == predecessor_identity,
        predecessor_revoked,
    })
}

async fn decode_pinned_placement(
    connection: &mut PgConnection,
    row: &PgRow,
    session: SessionId,
    sandbox: RunnerSandboxProfile,
    permission_overrides: RunnerToolPermissionOverrides,
) -> Result<PinnedRunnerPlacement, RunnerProtocolStoreError> {
    let event = row.decode_column::<Decimal>("event_ordinal")?;
    let tool_rows = sqlx::query(
        "SELECT tool_name, runner_required
           FROM runner_session_placement_tool
          WHERE session_id = $1 AND event_ordinal = $2
          ORDER BY tool_name",
    )
    .bind(session.into_uuid())
    .bind(event)
    .fetch_all(&mut *connection)
    .await?;
    require_count(row, "pinned_tool_count", tool_rows.len())?;
    let tools = tool_rows
        .iter()
        .map(|row| tool_name(row.decode_column("tool_name")?))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let mut runner_required_tools = BTreeSet::new();
    for row in &tool_rows {
        match decode_stored_runner_requirement(row.decode_column::<bool>("runner_required")?) {
            StoredRunnerRequirement::Optional => {}
            StoredRunnerRequirement::Required => {
                runner_required_tools.insert(tool_name(row.decode_column("tool_name")?)?);
            }
        }
    }
    let runner = row
        .decode_column::<Option<Uuid>>("pinned_runner_id")?
        .map(runner_id)
        .ok_or(RunnerProtocolCorruption::IncompleteInventory)?;
    let working_directory = row
        .decode_column::<Option<String>>("pinned_working_directory")?
        .map(working_directory)
        .transpose()?
        .ok_or(RunnerProtocolCorruption::IncompleteInventory)?;
    Ok(PinnedRunnerPlacement {
        runner,
        working_directory,
        credential_profile: row
            .decode_column::<Option<String>>("pinned_credential_profile_name")?
            .map(profile_name)
            .transpose()?,
        grant_lineage: decode_grant_lineage(row)?,
        tools,
        runner_required_tools,
        workspace: decode_provisioned_workspace(row, session, runner)?,
        sandbox,
        permission_overrides,
    })
}

async fn authenticate_abandonment_predecessor(
    connection: &mut PgConnection,
    row: &PgRow,
    catalog: &RunnerCatalog,
    grant_policies: &GrantPolicyIndex,
    request: &SessionRunnerPlacementRequest,
    state: &SessionRunnerPlacementState,
) -> Result<(), RunnerProtocolStoreError> {
    let SessionRunnerPlacementState::RunnerAbandoned(abandoned) = state else {
        return Ok(());
    };
    let session = session_id(row.decode_column("session_id")?);
    let predecessor_ordinal = decode_u64(row.decode_column("event_ordinal")?)?
        .checked_sub(1)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
    let predecessor = sqlx::query(
        "SELECT *
           FROM runner_session_placement_record
          WHERE session_id = $1 AND event_ordinal = $2",
    )
    .bind(session.into_uuid())
    .bind(Decimal::from(predecessor_ordinal))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
    let predecessor_request = decode_placement_request(connection, &predecessor).await?;
    if decode_generation(predecessor.decode_column("placement_revision")?)?
        != decode_generation(row.decode_column("placement_revision")?)?
        || &predecessor_request != request
    {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let predecessor_event: String = predecessor.decode_column("event_kind")?;
    let predecessor_state: String = predecessor.decode_column("state_kind")?;
    match abandoned {
        AbandonedRunnerPlacement::BeforePin(lost)
            if predecessor_event == "runner_lost_before_pin"
                && predecessor_state == "runner_lost_before_pin"
                && predecessor
                    .decode_column::<Option<Uuid>>("lost_runner_id")?
                    .map(runner_id)
                    == Some(lost.runner())
                && !placement_row_has_invalid_pre_pin_loss_facts(&predecessor)? =>
        {
            authenticate_pre_pin_loss_predecessor(connection, &predecessor, &predecessor_request)
                .await
        }
        AbandonedRunnerPlacement::Pinned(lost)
            if predecessor_event == "runner_lost"
                && predecessor_state == "runner_lost"
                && predecessor
                    .decode_column::<Option<Uuid>>("lost_runner_id")?
                    .map(runner_id)
                    == Some(lost.pinned().runner)
                && predecessor
                    .decode_column::<Option<String>>("loss_source_kind")?
                    .map(|source| {
                        runner_placement_loss_source_from_str(&source)
                            .ok_or(RunnerProtocolCorruption::InvalidEncoding)
                    })
                    .transpose()?
                    == Some(lost.source()) =>
        {
            let pinned = decode_pinned_placement(
                connection,
                &predecessor,
                session,
                predecessor_request.sandbox,
                predecessor_request.permission_overrides.clone(),
            )
            .await?;
            let predecessor_registration = decode_pinned_registration_identity(&predecessor)?;
            let abandonment_registration = decode_pinned_registration_identity(row)?;
            let predecessor_loss_registration =
                predecessor.decode_column::<Option<Decimal>>("loss_registration_revision")?;
            let abandonment_loss_registration =
                row.decode_column::<Option<Decimal>>("loss_registration_revision")?;
            if pinned != *lost.pinned()
                || predecessor_registration != abandonment_registration
                || predecessor_loss_registration != abandonment_loss_registration
            {
                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
            }
            let loss = SessionRunnerPlacementState::RunnerLost(lost.as_ref().clone());
            let (prior_row, prior_request, prior_pinned) =
                load_authenticated_pinned_loss_predecessor(
                    connection,
                    &predecessor,
                    &predecessor_request,
                    &loss,
                )
                .await?;
            authenticate_registration_loss_cause(connection, &predecessor, &prior_row).await?;
            authenticate_pinned_predecessor(
                connection,
                &prior_row,
                catalog,
                grant_policies,
                &prior_request,
                &SessionRunnerPlacementState::Pinned(prior_pinned),
            )
            .await
        }
        AbandonedRunnerPlacement::BeforePin(_) | AbandonedRunnerPlacement::Pinned(_) => {
            Err(RunnerProtocolCorruption::CrossWiredReference.into())
        }
    }
}

fn decode_pinned_registration_identity(
    row: &PgRow,
) -> Result<(RunnerEnrollmentId, RunnerRegistrationRevision), RunnerProtocolStoreError> {
    let enrollment = row
        .decode_column::<Option<Uuid>>("registration_enrollment_id")?
        .ok_or(RunnerProtocolCorruption::IncompleteInventory)?;
    let revision = row
        .decode_column::<Option<Decimal>>("registration_revision")?
        .ok_or(RunnerProtocolCorruption::IncompleteInventory)?;
    Ok((
        runner_enrollment_id(enrollment),
        decode_registration_revision(revision)?,
    ))
}

fn placement_row_has_pinned_facts(row: &PgRow) -> Result<bool, RunnerProtocolStoreError> {
    Ok(row
        .decode_column::<Option<Uuid>>("pinned_runner_id")?
        .is_some()
        || row
            .decode_column::<Option<String>>("pinned_working_directory")?
            .is_some()
        || row
            .decode_column::<Option<String>>("pinned_credential_profile_name")?
            .is_some()
        || row
            .decode_column::<Option<Uuid>>("registration_enrollment_id")?
            .is_some()
        || row
            .decode_column::<Option<Decimal>>("registration_revision")?
            .is_some()
        || decode_u64(row.decode_column::<Decimal>("pinned_tool_count")?)? != 0
        || row
            .decode_column::<Option<String>>("workspace_repository_key")?
            .is_some()
        || row
            .decode_column::<Option<String>>("workspace_working_directory")?
            .is_some()
        || row
            .decode_column::<Option<Uuid>>("workspace_manifest_id")?
            .is_some()
        || row
            .decode_column::<Option<Decimal>>("workspace_placement_revision")?
            .is_some()
        || row
            .decode_column::<Option<String>>("workspace_clone_url_digest")?
            .is_some()
        || row
            .decode_column::<Option<String>>("workspace_credential_profile_name")?
            .is_some()
        || row
            .decode_column::<Option<String>>("workspace_sandbox_profile")?
            .is_some()
        || row
            .decode_column::<Option<String>>("workspace_relative_path")?
            .is_some()
        || row
            .decode_column::<Option<String>>("workspace_recovery_kind")?
            .is_some()
        || row
            .decode_column::<Option<String>>("workspace_branch_name")?
            .is_some()
        || row
            .decode_column::<Option<String>>("workspace_revision")?
            .is_some()
        || row
            .decode_column::<Option<Uuid>>("credential_grant_runner_id")?
            .is_some()
        || row
            .decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?
            .is_some()
        || row
            .decode_column::<Option<Decimal>>("credential_grant_revision")?
            .is_some())
}

fn placement_row_has_invalid_pre_pin_loss_facts(
    row: &PgRow,
) -> Result<bool, RunnerProtocolStoreError> {
    Ok(row
        .decode_column::<Option<String>>("loss_source_kind")?
        .is_some()
        || placement_row_has_pinned_facts(row)?)
}

fn placement_row_has_loss_facts(row: &PgRow) -> Result<bool, RunnerProtocolStoreError> {
    Ok(row
        .decode_column::<Option<Uuid>>("lost_runner_id")?
        .is_some()
        || row
            .decode_column::<Option<String>>("loss_source_kind")?
            .is_some())
}

fn placement_row_has_invalid_unpinned_facts(row: &PgRow) -> Result<bool, RunnerProtocolStoreError> {
    Ok(placement_row_has_loss_facts(row)? || placement_row_has_pinned_facts(row)?)
}

async fn decode_placement_request(
    connection: &mut PgConnection,
    row: &PgRow,
) -> Result<SessionRunnerPlacementRequest, RunnerProtocolStoreError> {
    Ok(SessionRunnerPlacementRequest {
        selector: decode_selector(row)?,
        working_directory: decode_directory(row)?,
        credential_profile: row
            .decode_column::<Option<String>>("requested_credential_profile_name")?
            .map(profile_name)
            .transpose()?,
        workspace: decode_workspace_requirement(row)?,
        sandbox: decode_sandbox(row.decode_column("requested_sandbox_profile")?)?,
        permission_overrides: load_permission_overrides(connection, row).await?,
    })
}

async fn load_placement_reconstitution_history(
    connection: &mut PgConnection,
    row: &PgRow,
) -> Result<RunnerPlacementReconstitutionHistory, RunnerProtocolStoreError> {
    let revision = decode_generation(row.decode_column("placement_revision")?)?;
    let session = row.decode_column::<Uuid>("session_id")?;
    let head_event_ordinal = decode_u64(row.decode_column("event_ordinal")?)?;
    let initial = sqlx::query(
        "SELECT *
           FROM runner_session_placement_record
          WHERE session_id = $1
            AND event_ordinal = 1",
    )
    .bind(session)
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
    let initial_revision = decode_generation(initial.decode_column("placement_revision")?)?;
    let initial_event: String = initial.decode_column("event_kind")?;
    let initial_state: String = initial.decode_column("state_kind")?;
    if initial_revision != RunnerGeneration::one()
        || initial_event != "created"
        || initial_state != "unpinned"
        || placement_row_has_invalid_unpinned_facts(&initial)?
    {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let initial_request = decode_placement_request(connection, &initial).await?;
    if revision == RunnerGeneration::one() {
        if decode_placement_request(connection, row).await? != initial_request {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        return Ok(RunnerPlacementReconstitutionHistory::Initial);
    }
    let origins = sqlx::query(
        "SELECT *
           FROM runner_session_placement_record
          WHERE session_id = $1
            AND placement_revision <= $2
            AND event_ordinal <= $3
            AND event_kind = 'pre_pin_replaced'
          ORDER BY placement_revision, event_ordinal",
    )
    .bind(session)
    .bind(Decimal::from(revision.get()))
    .bind(Decimal::from(head_event_ordinal))
    .fetch_all(&mut *connection)
    .await?;
    let expected_origins = usize::try_from(revision.get() - 1)
        .map_err(|_| RunnerProtocolCorruption::InvalidEncoding)?;
    if origins.len() != expected_origins {
        return Err(RunnerProtocolCorruption::MissingCanonicalPlacement.into());
    }
    let mut replacements = Vec::with_capacity(origins.len());
    for (index, origin) in origins.iter().enumerate() {
        let current_revision = u64::try_from(index)
            .ok()
            .and_then(|index| index.checked_add(2))
            .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        if decode_generation(origin.decode_column("placement_revision")?)?.get() != current_revision
        {
            return Err(RunnerProtocolCorruption::MissingCanonicalPlacement.into());
        }
        let origin_state: String = origin.decode_column("state_kind")?;
        if origin_state != "unpinned" {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        if placement_row_has_invalid_unpinned_facts(origin)? {
            return Err(RunnerProtocolCorruption::InvalidEncoding.into());
        }
        let origin_ordinal = decode_u64(origin.decode_column("event_ordinal")?)?;
        let predecessor_ordinal = origin_ordinal
            .checked_sub(1)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
        let predecessor = sqlx::query(
            "SELECT *
              FROM runner_session_placement_record
              WHERE session_id = $1
                AND event_ordinal = $2
                AND event_ordinal < $3",
        )
        .bind(session)
        .bind(Decimal::from(predecessor_ordinal))
        .bind(Decimal::from(head_event_ordinal))
        .fetch_optional(&mut *connection)
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
        let predecessor_kind: String = predecessor.decode_column("event_kind")?;
        let predecessor_state: String = predecessor.decode_column("state_kind")?;
        if predecessor_kind != "runner_lost_before_pin"
            || predecessor_state != "runner_lost_before_pin"
        {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        if placement_row_has_invalid_pre_pin_loss_facts(&predecessor)? {
            return Err(RunnerProtocolCorruption::InvalidEncoding.into());
        }
        let prior_revision = decode_generation(predecessor.decode_column("placement_revision")?)?;
        let lost_runner = predecessor
            .decode_column::<Option<Uuid>>("lost_runner_id")?
            .map(runner_id)
            .ok_or(RunnerProtocolCorruption::IncompleteInventory)?;
        let prior_request = decode_placement_request(connection, &predecessor).await?;
        authenticate_pre_pin_loss_predecessor(connection, &predecessor, &prior_request).await?;
        if index == 0 && prior_request != initial_request {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
        let replacement_request = decode_placement_request(connection, origin).await?;
        replacements.push(RunnerPrePinReplacementHistory {
            prior_revision,
            lost_runner,
            prior_request,
            replacement_request,
        });
    }
    Ok(RunnerPlacementReconstitutionHistory::PrePinReplacements(
        replacements,
    ))
}

fn decode_provisioned_workspace(
    row: &PgRow,
    session: SessionId,
    runner: RunnerId,
) -> Result<Option<ProvisionedWorkspace>, RunnerProtocolStoreError> {
    let repository = row.decode_column::<Option<String>>("workspace_repository_key")?;
    let directory = row.decode_column::<Option<String>>("workspace_working_directory")?;
    let manifest = row.decode_column::<Option<Uuid>>("workspace_manifest_id")?;
    let placement_revision =
        row.decode_column::<Option<Decimal>>("workspace_placement_revision")?;
    let clone_url_digest = row.decode_column::<Option<String>>("workspace_clone_url_digest")?;
    let credential_profile =
        row.decode_column::<Option<String>>("workspace_credential_profile_name")?;
    let sandbox = row.decode_column::<Option<String>>("workspace_sandbox_profile")?;
    let relative_path = row.decode_column::<Option<String>>("workspace_relative_path")?;
    let recovery_kind = row.decode_column::<Option<String>>("workspace_recovery_kind")?;
    let branch_name = row.decode_column::<Option<String>>("workspace_branch_name")?;
    let revision = row.decode_column::<Option<String>>("workspace_revision")?;
    let any_present = repository.is_some()
        || directory.is_some()
        || manifest.is_some()
        || placement_revision.is_some()
        || clone_url_digest.is_some()
        || credential_profile.is_some()
        || sandbox.is_some()
        || relative_path.is_some()
        || recovery_kind.is_some()
        || branch_name.is_some()
        || revision.is_some();
    if !any_present {
        return Ok(None);
    }
    let recovery = match (recovery_kind.as_deref(), branch_name, revision) {
        (None, None, None) => None,
        (Some("commit"), None, Some(revision)) => Some(WorkspaceRecovery::Commit {
            revision: WorkspaceRevision::try_new(revision)
                .map_err(RunnerProtocolStoreError::Domain)?,
        }),
        (Some("branch"), Some(name), Some(revision)) => Some(WorkspaceRecovery::Branch {
            name: WorkspaceBranchName::try_new(name).map_err(RunnerProtocolStoreError::Domain)?,
            revision: WorkspaceRevision::try_new(revision)
                .map_err(RunnerProtocolStoreError::Domain)?,
        }),
        (Some("unborn_branch"), Some(name), None) => Some(WorkspaceRecovery::UnbornBranch {
            name: WorkspaceBranchName::try_new(name).map_err(RunnerProtocolStoreError::Domain)?,
        }),
        _ => return Err(RunnerProtocolCorruption::CrossWiredReference.into()),
    };
    Ok(Some(ProvisionedWorkspace {
        session,
        placement_revision: decode_generation(
            placement_revision.ok_or(RunnerProtocolCorruption::IncompleteInventory)?,
        )?,
        runner,
        repository: repository.map(repository_key).transpose()?,
        canonical_clone_url_digest: clone_url_digest
            .map(CanonicalCloneUrlDigest::try_new)
            .transpose()
            .map_err(RunnerProtocolStoreError::Domain)?,
        credential_profile: credential_profile.map(profile_name).transpose()?,
        sandbox: decode_sandbox(sandbox.ok_or(RunnerProtocolCorruption::IncompleteInventory)?)?,
        working_directory: working_directory(
            directory.ok_or(RunnerProtocolCorruption::IncompleteInventory)?,
        )?,
        relative_path: WorkspaceRelativePath::try_new(
            relative_path.ok_or(RunnerProtocolCorruption::IncompleteInventory)?,
        )
        .map_err(RunnerProtocolStoreError::Domain)?,
        manifest_id: WorkspaceManifestId::from_uuid(
            manifest.ok_or(RunnerProtocolCorruption::IncompleteInventory)?,
        ),
        recovery,
    }))
}

fn decode_grant_lineage(
    placement: &PgRow,
) -> Result<Option<RunnerCredentialGrantLineage>, RunnerProtocolStoreError> {
    let origin =
        placement.decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?;
    let revision = placement.decode_column::<Option<Decimal>>("credential_grant_revision")?;
    let runner = placement.decode_column::<Option<Uuid>>("credential_grant_runner_id")?;
    match (origin, runner, revision) {
        (None, None, None) => Ok(None),
        (Some(_), Some(runner), Some(revision)) => Ok(Some(RunnerCredentialGrantLineage {
            runner: runner_id(runner),
            revision: decode_generation(revision)?,
        })),
        _ => Err(RunnerProtocolCorruption::CrossWiredReference.into()),
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct StoredGrantIdentity {
    lineage_origin: Decimal,
    runner: Uuid,
    revision: Decimal,
}

#[derive(Default)]
struct GrantPolicyIndex {
    events: BTreeMap<StoredGrantIdentity, Decimal>,
}

impl GrantPolicyIndex {
    fn policy_event_for(&self, placement: &PgRow) -> Result<Decimal, RunnerProtocolStoreError> {
        let identity = decode_stored_grant_identity(placement)?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
        self.events
            .get(&identity)
            .copied()
            .ok_or_else(|| RunnerProtocolCorruption::MissingCanonicalGrant.into())
    }
}

fn decode_stored_grant_identity(
    placement: &PgRow,
) -> Result<Option<StoredGrantIdentity>, RunnerProtocolStoreError> {
    let lineage_origin =
        placement.decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?;
    let runner = placement.decode_column::<Option<Uuid>>("credential_grant_runner_id")?;
    let revision = placement.decode_column::<Option<Decimal>>("credential_grant_revision")?;
    match (lineage_origin, runner, revision) {
        (None, None, None) => Ok(None),
        (Some(lineage_origin), Some(runner), Some(revision)) => Ok(Some(StoredGrantIdentity {
            lineage_origin,
            runner,
            revision,
        })),
        _ => Err(RunnerProtocolCorruption::CrossWiredReference.into()),
    }
}

/// Loads one grant's exact predecessor closure once, excluding unrelated
/// grants that merely share its lineage origin. Placement-history
/// authentication reuses the resulting policy map instead of recursively
/// revisiting the chain for every historical pin.
async fn load_grant_policy_index(
    connection: &mut PgConnection,
    placement: &PgRow,
) -> Result<GrantPolicyIndex, RunnerProtocolStoreError> {
    let Some(current) = decode_stored_grant_identity(placement)? else {
        return Ok(GrantPolicyIndex::default());
    };
    let session = placement.decode_column::<Uuid>("session_id")?;
    let rows = sqlx::query(
        "WITH RECURSIVE grant_line AS (
             SELECT grant_record.*
               FROM runner_credential_grant AS grant_record
              WHERE grant_record.session_id = $1
                AND grant_record.lineage_origin_event_ordinal = $2
                AND grant_record.runner_id = $3
                AND grant_record.grant_revision = $4
             UNION
             SELECT predecessor.*
               FROM grant_line AS successor
               JOIN runner_credential_grant AS predecessor
                 ON predecessor.session_id = successor.session_id
                AND predecessor.lineage_origin_event_ordinal =
                    successor.lineage_origin_event_ordinal
                AND predecessor.runner_id = successor.prior_runner_id
                AND predecessor.grant_revision = successor.prior_grant_revision
         )
         SELECT grant_line.lineage_origin_event_ordinal,
                grant_line.runner_id,
                grant_line.grant_revision,
                grant_line.prior_runner_id,
                grant_line.prior_grant_revision,
                grant_line.placement_event_ordinal,
                policy_placement.event_ordinal IS NOT NULL
                    AS has_policy_placement,
                policy_placement.pinned_credential_profile_name IS NOT NULL
                    AS defines_policy,
                EXISTS (
                    SELECT 1
                      FROM runner_credential_grant_audit AS issuance
                     WHERE issuance.session_id = grant_line.session_id
                       AND issuance.lineage_origin_event_ordinal =
                            grant_line.lineage_origin_event_ordinal
                       AND issuance.runner_id = grant_line.runner_id
                       AND issuance.grant_revision = grant_line.grant_revision
                       AND issuance.audit_ordinal = 1
                       AND issuance.credential_profile_name =
                            grant_line.credential_profile_name
                       AND issuance.event_kind = CASE
                           WHEN grant_line.grant_revision = 1 THEN 'issued'
                           ELSE 'replaced'
                       END
                ) AS has_canonical_issuance
           FROM grant_line
           LEFT JOIN runner_session_placement_record AS policy_placement
             ON policy_placement.session_id = grant_line.session_id
            AND policy_placement.event_ordinal = grant_line.placement_event_ordinal
            AND policy_placement.credential_grant_lineage_origin_ordinal =
                grant_line.lineage_origin_event_ordinal
            AND policy_placement.credential_grant_runner_id = grant_line.runner_id
            AND policy_placement.credential_grant_revision = grant_line.grant_revision
          ORDER BY grant_line.placement_event_ordinal",
    )
    .bind(session)
    .bind(current.lineage_origin)
    .bind(current.runner)
    .bind(current.revision)
    .fetch_all(&mut *connection)
    .await?;
    let mut events = BTreeMap::new();
    let mut inherited_policy = None;
    let mut reached_base = false;
    for row in rows {
        let revision = validate_grant_predecessor_shape(&row)?;
        reached_base |= revision == RunnerGeneration::one();
        if !row.decode_column::<bool>("has_policy_placement")? {
            return Err(RunnerProtocolCorruption::MissingCanonicalGrant.into());
        }
        if !row.decode_column::<bool>("has_canonical_issuance")? {
            return Err(RunnerProtocolCorruption::MissingCanonicalGrant.into());
        }
        let identity = StoredGrantIdentity {
            lineage_origin: row.decode_column("lineage_origin_event_ordinal")?,
            runner: row.decode_column("runner_id")?,
            revision: row.decode_column("grant_revision")?,
        };
        let placement_event = row.decode_column::<Decimal>("placement_event_ordinal")?;
        if row.decode_column::<bool>("defines_policy")? {
            inherited_policy = Some(placement_event);
        }
        let policy_event =
            inherited_policy.ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
        events.insert(identity, policy_event);
    }
    if !events.contains_key(&current) {
        return Err(RunnerProtocolCorruption::MissingCanonicalGrant.into());
    }
    if !reached_base {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    Ok(GrantPolicyIndex { events })
}

fn validate_grant_predecessor_shape(
    row: &PgRow,
) -> Result<RunnerGeneration, RunnerProtocolStoreError> {
    let revision = decode_generation(row.decode_column("grant_revision")?)?;
    let prior_runner = row.decode_column::<Option<Uuid>>("prior_runner_id")?;
    let prior_revision = row
        .decode_column::<Option<Decimal>>("prior_grant_revision")?
        .map(decode_generation)
        .transpose()?;
    let valid = match (revision, prior_runner, prior_revision) {
        (revision, None, None) => revision == RunnerGeneration::one(),
        (revision, Some(_), Some(prior)) => prior.checked_next() == Some(revision),
        _ => false,
    };
    if !valid {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    Ok(revision)
}

async fn load_grant_for_placement(
    connection: &mut PgConnection,
    placement: &PgRow,
    catalog: &RunnerCatalog,
    grant_policies: &GrantPolicyIndex,
) -> Result<Option<CredentialProfileGrant>, RunnerProtocolStoreError> {
    let origin =
        placement.decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?;
    let revision = placement.decode_column::<Option<Decimal>>("credential_grant_revision")?;
    let runner = placement.decode_column::<Option<Uuid>>("credential_grant_runner_id")?;
    if origin.is_none() && revision.is_none() && runner.is_none() {
        return Ok(None);
    }
    let (Some(origin), Some(revision), Some(runner)) = (origin, revision, runner) else {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    };
    let session = session_id(placement.decode_column("session_id")?);
    let revision = decode_generation(revision)?;
    let row = sqlx::query(
        "SELECT grant_record.*,
                EXISTS (
                    SELECT 1
                      FROM runner_credential_grant_audit AS audit
                     WHERE audit.session_id = grant_record.session_id
                       AND audit.lineage_origin_event_ordinal =
                            grant_record.lineage_origin_event_ordinal
                       AND audit.runner_id = grant_record.runner_id
                       AND audit.grant_revision =
                            grant_record.grant_revision
                       AND audit.event_kind = 'revoked'
                ) AS revoked
           FROM runner_credential_grant AS grant_record
          WHERE grant_record.session_id = $1
            AND grant_record.lineage_origin_event_ordinal = $2
            AND grant_record.runner_id = $3
            AND grant_record.grant_revision = $4",
    )
    .bind(session.into_uuid())
    .bind(origin)
    .bind(runner)
    .bind(Decimal::from(revision.get()))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
    let profile = row.decode_column::<String>("credential_profile_name")?;
    let revoked = decode_stored_grant_revocation(row.decode_column::<bool>("revoked")?);
    let pinned_profile =
        placement.decode_column::<Option<String>>("pinned_credential_profile_name")?;
    if pinned_profile
        .as_ref()
        .is_some_and(|pinned| pinned != &profile)
    {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let policy_event = grant_policies.policy_event_for(placement)?;
    let policy_placement = sqlx::query(
        "SELECT *
           FROM runner_session_placement_record
          WHERE session_id = $1 AND event_ordinal = $2",
    )
    .bind(session.into_uuid())
    .bind(policy_event)
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
    let grant_sandbox =
        decode_sandbox(policy_placement.decode_column("requested_sandbox_profile")?)?;
    let grant_permission_overrides =
        load_permission_overrides(connection, &policy_placement).await?;
    let grant_registration = load_registration_in(
        connection,
        runner_enrollment_id(row.decode_column("registration_enrollment_id")?),
        decode_registration_revision(row.decode_column("registration_revision")?)?,
        None,
        catalog,
    )
    .await?
    .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
    let tool_rows = sqlx::query(
        "SELECT tool_name, approval_kind
           FROM runner_credential_grant_tool
          WHERE session_id = $1
            AND lineage_origin_event_ordinal = $2
            AND runner_id = $3
            AND grant_revision = $4
          ORDER BY tool_name",
    )
    .bind(session.into_uuid())
    .bind(origin)
    .bind(runner)
    .bind(Decimal::from(revision.get()))
    .fetch_all(&mut *connection)
    .await?;
    require_count(&row, "tool_count", tool_rows.len())?;
    let mut tools = BTreeSet::new();
    let mut approvals = BTreeMap::new();
    for tool_row in tool_rows {
        let tool = tool_name(tool_row.decode_column("tool_name")?)?;
        tools.insert(tool.clone());
        approvals.insert(
            tool,
            decode_approval(tool_row.decode_column("approval_kind")?)?,
        );
    }
    CredentialProfileGrant::reconstitute(
        CredentialProfileGrantReconstitutionInput {
            session,
            runner: runner_id(runner),
            revision,
            profile: profile_name(profile)?,
            tools,
            approvals,
            state: match revoked {
                StoredGrantRevocation::Active => CredentialProfileGrantState::Active,
                StoredGrantRevocation::Revoked => CredentialProfileGrantState::Revoked,
            },
        },
        session,
        grant_registration.registration(),
        grant_sandbox,
        &grant_permission_overrides,
    )
    .map(Some)
    .map_err(RunnerProtocolStoreError::Domain)
}

/// Appends one lease generation or state event inside the caller's
/// transaction, exactly as the standalone lease store does.
async fn append_lease_event_in(
    transaction: &mut Transaction<'_, Postgres>,
    lease: &RunnerLease,
) -> Result<(), RunnerProtocolStoreError> {
    let correlation = lease.correlation();
    lock_runner_session_scheduler(transaction, correlation.dispatch.session()).await?;
    if lease.state() == RunnerLeaseState::Claimed {
        lock_runner_lease_claim_connection_authority(transaction, &correlation).await?;
    }
    let current_event = sqlx::query(RUNNER_LEASE_HEAD)
        .bind(correlation.lease.into_uuid())
        .bind(Decimal::from(correlation.generation.get()))
        .fetch_optional(&mut **transaction)
        .await?;
    let event_ordinal = match current_event {
        None => {
            if lease.state() != RunnerLeaseState::Offered {
                return Err(RunnerProtocolStoreError::Domain(
                    RunnerDomainError::InvalidState,
                ));
            }
            insert_lease_generation(transaction, lease).await?;
            1
        }
        Some(row) => {
            require_stored_lease_identity(&row, lease)?;
            decode_u64(row.decode_column("event_ordinal")?)?
                .checked_add(1)
                .ok_or(RunnerProtocolCorruption::GenerationExhausted)?
        }
    };
    let state = encode_lease_state(lease.state());
    sqlx::query(
        "INSERT INTO runner_lease_event
            (lease_id, generation, event_ordinal, state_kind)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .bind(Decimal::from(event_ordinal))
    .bind(state)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO runner_current_lease_event
            (lease_id, generation, event_ordinal)
         VALUES ($1, $2, $3)
         ON CONFLICT (lease_id, generation)
         DO UPDATE SET event_ordinal = EXCLUDED.event_ordinal",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .bind(Decimal::from(event_ordinal))
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn lock_runner_lease_claim_connection_authority(
    transaction: &mut Transaction<'_, Postgres>,
    correlation: &RunnerLeaseCorrelation,
) -> Result<(), RunnerProtocolStoreError> {
    let enrollment: Uuid = sqlx::query_scalar(
        "SELECT registration_enrollment_id
           FROM runner_lease_generation
          WHERE lease_id = $1 AND generation = $2",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(RunnerProtocolStoreError::Domain(
        RunnerDomainError::InvalidState,
    ))?;
    sqlx::query(
        "SELECT enrollment_id
           FROM runner_enrollment
          WHERE enrollment_id = $1
          FOR SHARE",
    )
    .bind(enrollment)
    .fetch_one(&mut **transaction)
    .await?;
    sqlx::query(
        "SELECT enrollment_id
           FROM runner_connection_authority_head
          WHERE enrollment_id = $1
          FOR SHARE",
    )
    .bind(enrollment)
    .fetch_optional(&mut **transaction)
    .await?;
    Ok(())
}

async fn lock_runner_session_scheduler(
    transaction: &mut Transaction<'_, Postgres>,
    session: SessionId,
) -> Result<(), RunnerProtocolStoreError> {
    let scheduler_exists = sqlx::query_scalar::<_, Uuid>(RUNNER_RETRY_REPLACEMENT_SCHEDULER)
        .bind(session.into_uuid())
        .fetch_optional(&mut **transaction)
        .await?
        .is_some();
    if !scheduler_exists {
        return Err(RunnerProtocolStoreError::Corruption(
            RunnerProtocolCorruption::CrossWiredReference,
        ));
    }
    Ok(())
}

async fn insert_lease_generation(
    transaction: &mut Transaction<'_, Postgres>,
    lease: &RunnerLease,
) -> Result<(), RunnerProtocolStoreError> {
    let correlation = lease.correlation();
    let canonical_dispatch = sqlx::query(
        "SELECT attempt.session_id, attempt.turn_id,
                attempt.issuing_turn_attempt_id, attempt.request_id,
                attempt.dispatch_generation,
                request.tool_name AS canonical_attempt_tool,
                request.arguments_kind AS canonical_arguments_kind,
                request.arguments_text AS canonical_arguments_text
           FROM tool_attempt AS attempt
           JOIN tool_request AS request
             ON request.request_id = attempt.request_id
            AND request.session_id = attempt.session_id
            AND request.turn_id = attempt.turn_id
          WHERE attempt.attempt_id = $1",
    )
    .bind(correlation.dispatch.attempt().into_uuid())
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(RunnerProtocolCorruption::MissingCanonicalAttempt)?;
    if canonical_dispatch.decode_column::<Uuid>("session_id")?
        != correlation.dispatch.session().into_uuid()
        || canonical_dispatch.decode_column::<Uuid>("turn_id")?
            != correlation.dispatch.turn().into_uuid()
        || canonical_dispatch.decode_column::<Uuid>("issuing_turn_attempt_id")?
            != correlation.dispatch.issuing_attempt().into_uuid()
        || canonical_dispatch.decode_column::<Uuid>("request_id")?
            != correlation.dispatch.request().into_uuid()
        || canonical_dispatch.decode_column::<Decimal>("dispatch_generation")?
            != Decimal::from(correlation.dispatch.generation().as_u64())
        || canonical_dispatch.decode_column::<String>("canonical_attempt_tool")?
            != lease.tool().as_str()
        || decode_lease_arguments(&canonical_dispatch)? != *lease.arguments()
    {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let observed_enrollment: Uuid = sqlx::query_scalar(
        "SELECT record.registration_enrollment_id
           FROM runner_current_session_placement AS current_placement
           JOIN runner_session_placement_record AS record
             ON record.session_id = current_placement.session_id
            AND record.event_ordinal = current_placement.event_ordinal
          WHERE current_placement.session_id = $1",
    )
    .bind(lease.session().into_uuid())
    .fetch_optional(&mut **transaction)
    .await?
    .flatten()
    .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
    let enrollment_state: Option<String> = sqlx::query_scalar(RUNNER_LEASE_ENROLLMENT_AUTHORITY)
        .bind(observed_enrollment)
        .fetch_optional(&mut **transaction)
        .await?;
    if enrollment_state.is_none() {
        return Err(RunnerProtocolCorruption::MissingCanonicalEnrollment.into());
    }
    let placement = sqlx::query(RUNNER_LEASE_PLACEMENT)
        .bind(lease.session().into_uuid())
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
    let placement_runner = placement
        .decode_column::<Option<Uuid>>("pinned_runner_id")?
        .ok_or(RunnerProtocolCorruption::CrossWiredReference)?;
    let placement_registration_revision = decode_generation(
        placement
            .decode_column::<Option<Decimal>>("registration_revision")?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?,
    )?;
    let placement_revision =
        decode_generation(placement.decode_column::<Decimal>("placement_revision")?)?;
    let placement_working_directory = RunnerWorkingDirectory::try_new(
        placement
            .decode_column::<Option<String>>("pinned_working_directory")?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?,
    )
    .map_err(|_| RunnerProtocolCorruption::InvalidEncoding)?;
    let placement_sandbox =
        decode_sandbox(placement.decode_column::<String>("requested_sandbox_profile")?)?;
    if placement_runner != lease.runner().into_uuid()
        || placement_revision != correlation.placement_revision
        || placement_working_directory != correlation.working_directory
        || placement_sandbox != correlation.sandbox
    {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let enrollment = placement
        .decode_column::<Option<Uuid>>("registration_enrollment_id")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
    if enrollment != observed_enrollment {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let authorization = lease.credential_authorization();
    let authorization_origin = match authorization {
        Some(_) => Some(
            placement
                .decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?,
        ),
        None => None,
    };
    if let Some(authorization) = authorization {
        let profile: Option<String> = sqlx::query_scalar(RUNNER_LEASE_GRANT_AUTHORITY)
            .bind(authorization.session.into_uuid())
            .bind(authorization_origin)
            .bind(authorization.runner.into_uuid())
            .bind(Decimal::from(authorization.grant_revision.get()))
            .fetch_optional(&mut **transaction)
            .await?;
        if profile.as_deref() != Some(authorization.profile.as_str()) {
            return Err(RunnerProtocolCorruption::CrossWiredReference.into());
        }
    }
    sqlx::query(
        "INSERT INTO runner_lease_generation
            (lease_id, generation, attempt_id, session_id, runner_id,
             tool_name, effect_class, placement_event_ordinal,
             registration_enrollment_id, registration_revision,
             offer_registration_revision,
             credential_profile_name,
             credential_grant_lineage_origin_ordinal,
             credential_grant_revision, credential_approval_kind,
             predecessor_generation)
         VALUES (
             $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
             $15, $16
         )",
    )
    .bind(correlation.lease.into_uuid())
    .bind(Decimal::from(correlation.generation.get()))
    .bind(correlation.dispatch.attempt().into_uuid())
    .bind(lease.session().into_uuid())
    .bind(lease.runner().into_uuid())
    .bind(lease.tool().as_str())
    .bind(encode_effect(lease.effect()))
    .bind(placement.decode_column::<Decimal>("event_ordinal")?)
    .bind(
        placement
            .decode_column::<Option<Uuid>>("registration_enrollment_id")?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?,
    )
    .bind(Decimal::from(placement_registration_revision.get()))
    .bind(Decimal::from(correlation.registration_revision.get()))
    .bind(authorization.map(|authorization| authorization.profile.as_str()))
    .bind(authorization_origin)
    .bind(authorization.map(|authorization| Decimal::from(authorization.grant_revision.get())))
    .bind(authorization.map(|authorization| encode_approval(authorization.approval)))
    .bind(
        correlation
            .generation
            .get()
            .checked_sub(1)
            .filter(|value| *value > 0)
            .map(Decimal::from),
    )
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn decode_lease(
    row: &PgRow,
    registration: &ValidatedRunnerRegistration,
) -> Result<RunnerLease, RunnerProtocolStoreError> {
    let lease = runner_lease_id(row.decode_column("lease_id")?);
    let attempt = tool_attempt_id(row.decode_column("attempt_id")?);
    let session = session_id(row.decode_column("session_id")?);
    let runner = runner_id(row.decode_column("runner_id")?);
    let tool = tool_name(row.decode_column("tool_name")?)?;
    let generation = decode_generation(row.decode_column("generation")?)?;
    let canonical_tool = row
        .decode_column::<Option<String>>("canonical_attempt_tool")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalAttempt)?;
    let arguments = decode_lease_arguments(row)?;
    let dispatch = ToolAttemptDispatchCorrelation::reconstitute(
        ToolAttemptDispatchCorrelationReconstitutionInput {
            session,
            turn: TurnId::from_uuid(
                row.decode_column::<Option<Uuid>>("canonical_attempt_turn")?
                    .ok_or(RunnerProtocolCorruption::MissingCanonicalAttempt)?,
            ),
            issuing_attempt: TurnAttemptId::from_uuid(
                row.decode_column::<Option<Uuid>>("canonical_issuing_attempt")?
                    .ok_or(RunnerProtocolCorruption::MissingCanonicalAttempt)?,
            ),
            request: ToolRequestId::from_uuid(
                row.decode_column::<Option<Uuid>>("canonical_attempt_request")?
                    .ok_or(RunnerProtocolCorruption::MissingCanonicalAttempt)?,
            ),
            attempt,
            generation: decode_dispatch_generation(
                row.decode_column::<Option<Decimal>>("canonical_dispatch_generation")?
                    .ok_or(RunnerProtocolCorruption::MissingCanonicalAttempt)?,
            )?,
        },
    );
    let canonical_runner = row
        .decode_column::<Option<Uuid>>("canonical_placement_runner")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
    let canonical_placement_state = row
        .decode_column::<Option<String>>("canonical_placement_state")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
    let canonical_placement_revision = decode_generation(
        row.decode_column::<Option<Decimal>>("canonical_placement_revision")?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?,
    )?;
    let canonical_working_directory = RunnerWorkingDirectory::try_new(
        row.decode_column::<Option<String>>("canonical_working_directory")?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?,
    )
    .map_err(|_| RunnerProtocolCorruption::InvalidEncoding)?;
    let canonical_sandbox = decode_sandbox(
        row.decode_column::<Option<String>>("canonical_sandbox_profile")?
            .ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?,
    )?;
    let canonical_registration_enrollment = row
        .decode_column::<Option<Uuid>>("canonical_registration_enrollment")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
    let canonical_registration_revision = row
        .decode_column::<Option<Decimal>>("canonical_registration_revision")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalRegistration)?;
    if canonical_placement_state != "pinned"
        || canonical_runner != runner.into_uuid()
        || canonical_registration_enrollment
            != row.decode_column::<Uuid>("registration_enrollment_id")?
        || canonical_registration_revision
            != row.decode_column::<Decimal>("registration_revision")?
    {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let authorization = match (
        row.decode_column::<Option<String>>("credential_profile_name")?,
        row.decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?,
        row.decode_column::<Option<Decimal>>("credential_grant_revision")?,
        row.decode_column::<Option<String>>("credential_approval_kind")?,
    ) {
        (None, None, None, None) => {
            if row
                .decode_column::<Option<String>>("canonical_grant_tool")?
                .is_some()
                || row
                    .decode_column::<Option<String>>("canonical_grant_approval")?
                    .is_some()
            {
                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
            }
            None
        }
        (Some(profile), Some(_), Some(grant_revision), Some(approval)) => {
            let canonical_grant_tool = row
                .decode_column::<Option<String>>("canonical_grant_tool")?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
            let canonical_grant_approval = row
                .decode_column::<Option<String>>("canonical_grant_approval")?
                .ok_or(RunnerProtocolCorruption::MissingCanonicalGrant)?;
            if canonical_grant_tool != tool.as_str() || canonical_grant_approval != approval {
                return Err(RunnerProtocolCorruption::CrossWiredReference.into());
            }
            Some(CredentialDispatchAuthorization {
                session,
                runner,
                grant_revision: decode_generation(grant_revision)?,
                profile: profile_name(profile)?,
                tool: tool.clone(),
                approval: decode_approval(approval)?,
            })
        }
        _ => return Err(RunnerProtocolCorruption::CrossWiredReference.into()),
    };
    RunnerLease::reconstitute(
        RunnerLeaseReconstitutionInput {
            lease,
            dispatch,
            runner,
            registration_revision: decode_generation(
                row.decode_column("offer_registration_revision")?,
            )?,
            placement_revision: canonical_placement_revision,
            working_directory: canonical_working_directory.clone(),
            sandbox: canonical_sandbox,
            tool: tool.clone(),
            arguments: arguments.clone(),
            effect: decode_effect(row.decode_column("effect_class")?)?,
            credential_authorization: authorization.clone(),
            generation,
            state: decode_lease_state(row.decode_column("state_kind")?)?,
            recorded_correlation: RunnerLeaseCorrelation {
                lease,
                runner: runner_id(canonical_runner),
                registration_revision: decode_generation(
                    row.decode_column("offer_registration_revision")?,
                )?,
                placement_revision: canonical_placement_revision,
                working_directory: canonical_working_directory,
                sandbox: canonical_sandbox,
                tool: tool_name(canonical_tool)?,
                dispatch,
                generation,
            },
            recorded_session: session,
            recorded_effect: decode_effect(row.decode_column("effect_class")?)?,
            recorded_arguments: arguments,
            recorded_credential_authorization: authorization.clone(),
            recorded_state: decode_lease_state(row.decode_column("state_kind")?)?,
            retry_preparation: RunnerLeaseRetryPreparation::Available,
        },
        registration,
    )
    .map_err(RunnerProtocolStoreError::Domain)
}

fn decode_lease_arguments(
    row: &PgRow,
) -> Result<NormalizedToolArguments, RunnerProtocolStoreError> {
    let kind = row
        .decode_column::<Option<String>>("canonical_arguments_kind")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalAttempt)?;
    let kind = match kind.as_str() {
        "json" => ToolArgumentsKind::Json,
        "undecodable" => ToolArgumentsKind::Undecodable,
        _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    };
    let text = row
        .decode_column::<Option<String>>("canonical_arguments_text")?
        .ok_or(RunnerProtocolCorruption::MissingCanonicalAttempt)?;
    NormalizedToolArguments::try_from_stored(kind, text)
        .map_err(|_| RunnerProtocolCorruption::InvalidEncoding.into())
}

fn validate_placement_snapshot(
    placement: &SessionRunnerPlacement,
    registration: Option<&StoredValidatedRunnerRegistration>,
    grant: Option<&CredentialProfileGrant>,
    history: RunnerPlacementReconstitutionHistory,
) -> Result<(), RunnerProtocolStoreError> {
    let profileless_tombstone = match (pinned_placement(placement.state()), grant) {
        (Some(pinned), Some(grant))
            if pinned.credential_profile.is_none()
                && credential_grant_is_revoked(grant.state()) =>
        {
            Some(grant)
        }
        _ => None,
    };
    SessionRunnerPlacement::reconstitute(
        SessionRunnerPlacementReconstitutionInput {
            session: placement.session(),
            revision: placement.revision(),
            request: placement.request().clone(),
            state: placement.state().clone(),
            history,
        },
        placement.session(),
        registration.map(StoredValidatedRunnerRegistration::registration),
        profileless_tombstone,
    )
    .map_err(RunnerProtocolStoreError::Domain)?;
    if grant.is_some() && registration.is_none() {
        return Err(RunnerProtocolStoreError::Corruption(
            RunnerProtocolCorruption::MissingCanonicalRegistration,
        ));
    }
    let binding_matches = match (pinned_placement(placement.state()), grant) {
        (None, None) => true,
        (Some(pinned), Some(grant)) => match pinned.credential_profile.as_ref() {
            Some(profile) => {
                profile == grant.profile()
                    && placement.session() == grant.session()
                    && pinned.runner == grant.runner()
                    && pinned.grant_lineage == Some(grant.lineage())
            }
            None => {
                placement.session() == grant.session()
                    && credential_grant_is_revoked(grant.state())
                    && grant.revision() != RunnerGeneration::one()
                    && pinned.grant_lineage == Some(grant.lineage())
            }
        },
        (Some(pinned), None) => {
            pinned.credential_profile.is_none() && pinned.grant_lineage.is_none()
        }
        (None, Some(_)) => false,
    };
    if !binding_matches {
        return Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::CorruptStoredFacts,
        ));
    }
    Ok(())
}

fn credential_grant_is_revoked(state: CredentialProfileGrantState) -> bool {
    match state {
        CredentialProfileGrantState::Active => false,
        CredentialProfileGrantState::Revoked => true,
    }
}

fn pinned_placement(state: &SessionRunnerPlacementState) -> Option<&PinnedRunnerPlacement> {
    match state {
        SessionRunnerPlacementState::Pinned(pinned) => Some(pinned),
        SessionRunnerPlacementState::RunnerLost(lost) => Some(lost.pinned()),
        SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::Pinned(lost)) => {
            Some(lost.pinned())
        }
        SessionRunnerPlacementState::Unpinned
        | SessionRunnerPlacementState::RunnerLostBeforePin(_)
        | SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::BeforePin(_)) => {
            None
        }
    }
}

fn placement_loss_fence_runner(placement: &SessionRunnerPlacement) -> Option<RunnerId> {
    match placement.state() {
        SessionRunnerPlacementState::Unpinned => match &placement.request().selector {
            RunnerSelector::Identity(runner) => Some(*runner),
            RunnerSelector::CapabilityClass(_) => None,
        },
        SessionRunnerPlacementState::Pinned(pinned) => Some(pinned.runner),
        SessionRunnerPlacementState::RunnerLostBeforePin(lost) => Some(lost.runner()),
        SessionRunnerPlacementState::RunnerLost(lost) => Some(lost.pinned().runner),
        SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::BeforePin(lost)) => {
            Some(lost.runner())
        }
        SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::Pinned(lost)) => {
            Some(lost.pinned().runner)
        }
    }
}

async fn lock_runner_placement_loss_baseline(
    transaction: &mut Transaction<'_, Postgres>,
    placement: &SessionRunnerPlacement,
) -> Result<(), RunnerProtocolStoreError> {
    let scheduler = sqlx::query_scalar::<_, Uuid>(RUNNER_RETRY_REPLACEMENT_SCHEDULER)
        .bind(placement.session().into_uuid())
        .fetch_optional(&mut **transaction)
        .await?;
    if scheduler.is_none() {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let Some(runner) = placement_loss_fence_runner(placement) else {
        return Ok(());
    };
    let enrollment = sqlx::query_scalar::<_, Uuid>(RUNNER_PLACEMENT_ENROLLMENT_BY_RUNNER)
        .bind(runner.into_uuid())
        .fetch_optional(&mut **transaction)
        .await?;
    let Some(enrollment) = enrollment else {
        return Ok(());
    };
    sqlx::query_scalar::<_, Decimal>(RUNNER_PLACEMENT_CONNECTION_AUTHORITY)
        .bind(enrollment)
        .fetch_optional(&mut **transaction)
        .await?;
    sqlx::query_scalar::<_, Decimal>(RUNNER_PLACEMENT_CURRENT_LOSS)
        .bind(enrollment)
        .fetch_optional(&mut **transaction)
        .await?;
    Ok(())
}

async fn prospective_placement_reconstitution_history(
    connection: &mut PgConnection,
    prior: Option<&PgRow>,
    event_kind: &str,
    placement: &SessionRunnerPlacement,
) -> Result<RunnerPlacementReconstitutionHistory, RunnerProtocolStoreError> {
    let retains_pinned_history = match placement.state() {
        SessionRunnerPlacementState::Pinned(_)
        | SessionRunnerPlacementState::RunnerLost(_)
        | SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::Pinned(_)) => true,
        SessionRunnerPlacementState::Unpinned
        | SessionRunnerPlacementState::RunnerLostBeforePin(_)
        | SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::BeforePin(_)) => {
            false
        }
    };
    if placement.revision() == RunnerGeneration::one() || retains_pinned_history {
        return Ok(RunnerPlacementReconstitutionHistory::Initial);
    }
    let prior = prior.ok_or(RunnerProtocolCorruption::MissingCanonicalPlacement)?;
    if event_kind != "pre_pin_replaced" {
        return load_placement_reconstitution_history(connection, prior).await;
    }
    let prior_revision = decode_generation(prior.decode_column("placement_revision")?)?;
    let prior_kind: String = prior.decode_column("event_kind")?;
    let prior_state: String = prior.decode_column("state_kind")?;
    if prior_kind != "runner_lost_before_pin" || prior_state != "runner_lost_before_pin" {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    let lost_runner = prior
        .decode_column::<Option<Uuid>>("lost_runner_id")?
        .map(runner_id)
        .ok_or(RunnerProtocolCorruption::IncompleteInventory)?;
    let history = load_placement_reconstitution_history(connection, prior).await?;
    let mut replacements = match history {
        RunnerPlacementReconstitutionHistory::Initial => Vec::new(),
        RunnerPlacementReconstitutionHistory::PrePinReplacements(replacements) => replacements,
    };
    replacements.push(RunnerPrePinReplacementHistory {
        prior_revision,
        lost_runner,
        prior_request: decode_placement_request(connection, prior).await?,
        replacement_request: placement.request().clone(),
    });
    Ok(RunnerPlacementReconstitutionHistory::PrePinReplacements(
        replacements,
    ))
}

fn grant_input(grant: &CredentialProfileGrant) -> CredentialProfileGrantReconstitutionInput {
    CredentialProfileGrantReconstitutionInput {
        session: grant.session(),
        runner: grant.runner(),
        revision: grant.revision(),
        profile: grant.profile().clone(),
        tools: grant.tools().cloned().collect(),
        approvals: grant
            .approvals()
            .map(|(tool, approval)| (tool.clone(), approval))
            .collect(),
        state: grant.state(),
    }
}

fn require_stored_lease_identity(
    row: &PgRow,
    lease: &RunnerLease,
) -> Result<(), RunnerProtocolStoreError> {
    let correlation = lease.correlation();
    let stored_authorization = match (
        row.decode_column::<Option<String>>("credential_profile_name")?,
        row.decode_column::<Option<Decimal>>("credential_grant_lineage_origin_ordinal")?,
        row.decode_column::<Option<Decimal>>("credential_grant_revision")?,
        row.decode_column::<Option<String>>("credential_approval_kind")?,
    ) {
        (None, None, None, None) => None,
        (Some(profile), Some(_), Some(revision), Some(approval)) => {
            Some(CredentialDispatchAuthorization {
                session: lease.session(),
                runner: lease.runner(),
                grant_revision: decode_generation(revision)?,
                profile: profile_name(profile)?,
                tool: lease.tool().clone(),
                approval: decode_approval(approval)?,
            })
        }
        _ => return Err(RunnerProtocolCorruption::CrossWiredReference.into()),
    };
    if row.decode_column::<Uuid>("attempt_id")? != correlation.dispatch.attempt().into_uuid()
        || row.decode_column::<Uuid>("canonical_dispatch_session")?
            != correlation.dispatch.session().into_uuid()
        || row.decode_column::<Uuid>("canonical_dispatch_turn")?
            != correlation.dispatch.turn().into_uuid()
        || row.decode_column::<Uuid>("canonical_dispatch_issuing_attempt")?
            != correlation.dispatch.issuing_attempt().into_uuid()
        || row.decode_column::<Uuid>("canonical_dispatch_request")?
            != correlation.dispatch.request().into_uuid()
        || row.decode_column::<Decimal>("canonical_dispatch_generation")?
            != Decimal::from(correlation.dispatch.generation().as_u64())
        || row.decode_column::<Uuid>("session_id")? != lease.session().into_uuid()
        || row.decode_column::<Uuid>("runner_id")? != correlation.runner.into_uuid()
        || row.decode_column::<String>("tool_name")? != correlation.tool.as_str()
        || decode_lease_arguments(row)? != *lease.arguments()
        || decode_effect(row.decode_column("effect_class")?)? != lease.effect()
        || stored_authorization.as_ref() != lease.credential_authorization()
    {
        return Err(RunnerProtocolCorruption::CrossWiredReference.into());
    }
    Ok(())
}

struct EncodedPlacementState<'a> {
    kind: &'static str,
    lost_runner: Option<Uuid>,
    loss_source: Option<&'static str>,
    pinned_runner: Option<Uuid>,
    pinned_directory: Option<&'a str>,
    pinned_profile: Option<&'a str>,
    grant_lineage: Option<RunnerCredentialGrantLineage>,
    tools: Vec<&'a ToolName>,
    runner_required_tools: BTreeSet<&'a ToolName>,
    workspace_repository: Option<&'a str>,
    workspace_directory: Option<&'a str>,
    workspace_manifest: Option<Uuid>,
    workspace_placement_revision: Option<Decimal>,
    workspace_clone_url_digest: Option<&'a str>,
    workspace_credential_profile: Option<&'a str>,
    workspace_sandbox: Option<&'static str>,
    workspace_relative_path: Option<&'a str>,
    workspace_recovery_kind: Option<&'static str>,
    workspace_branch_name: Option<&'a str>,
    workspace_revision: Option<&'a str>,
}

fn encode_placement_state(state: &SessionRunnerPlacementState) -> EncodedPlacementState<'_> {
    let (state_kind, pinned, lost_runner, loss_source) = match state {
        SessionRunnerPlacementState::Unpinned => {
            return EncodedPlacementState {
                kind: "unpinned",
                lost_runner: None,
                loss_source: None,
                pinned_runner: None,
                pinned_directory: None,
                pinned_profile: None,
                grant_lineage: None,
                tools: Vec::new(),
                runner_required_tools: BTreeSet::new(),
                workspace_repository: None,
                workspace_directory: None,
                workspace_manifest: None,
                workspace_placement_revision: None,
                workspace_clone_url_digest: None,
                workspace_credential_profile: None,
                workspace_sandbox: None,
                workspace_relative_path: None,
                workspace_recovery_kind: None,
                workspace_branch_name: None,
                workspace_revision: None,
            };
        }
        SessionRunnerPlacementState::RunnerLostBeforePin(lost) => {
            return EncodedPlacementState {
                kind: "runner_lost_before_pin",
                lost_runner: Some(lost.runner().into_uuid()),
                loss_source: None,
                pinned_runner: None,
                pinned_directory: None,
                pinned_profile: None,
                grant_lineage: None,
                tools: Vec::new(),
                runner_required_tools: BTreeSet::new(),
                workspace_repository: None,
                workspace_directory: None,
                workspace_manifest: None,
                workspace_placement_revision: None,
                workspace_clone_url_digest: None,
                workspace_credential_profile: None,
                workspace_sandbox: None,
                workspace_relative_path: None,
                workspace_recovery_kind: None,
                workspace_branch_name: None,
                workspace_revision: None,
            };
        }
        SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::BeforePin(lost)) => {
            return EncodedPlacementState {
                kind: "runner_abandoned",
                lost_runner: Some(lost.runner().into_uuid()),
                loss_source: None,
                pinned_runner: None,
                pinned_directory: None,
                pinned_profile: None,
                grant_lineage: None,
                tools: Vec::new(),
                runner_required_tools: BTreeSet::new(),
                workspace_repository: None,
                workspace_directory: None,
                workspace_manifest: None,
                workspace_placement_revision: None,
                workspace_clone_url_digest: None,
                workspace_credential_profile: None,
                workspace_sandbox: None,
                workspace_relative_path: None,
                workspace_recovery_kind: None,
                workspace_branch_name: None,
                workspace_revision: None,
            };
        }
        SessionRunnerPlacementState::Pinned(pinned) => ("pinned", pinned, None, None),
        SessionRunnerPlacementState::RunnerLost(lost) => (
            "runner_lost",
            lost.pinned(),
            Some(lost.pinned().runner.into_uuid()),
            Some(runner_placement_loss_source_to_str(lost.source())),
        ),
        SessionRunnerPlacementState::RunnerAbandoned(AbandonedRunnerPlacement::Pinned(lost)) => (
            "runner_abandoned",
            lost.pinned(),
            Some(lost.pinned().runner.into_uuid()),
            Some(runner_placement_loss_source_to_str(lost.source())),
        ),
    };
    let workspace = pinned.workspace.as_ref();
    let (workspace_recovery_kind, workspace_branch_name, workspace_revision) = workspace
        .and_then(|workspace| workspace.recovery.as_ref())
        .map(encode_workspace_recovery)
        .unwrap_or((None, None, None));
    EncodedPlacementState {
        kind: state_kind,
        lost_runner,
        loss_source,
        pinned_runner: Some(pinned.runner.into_uuid()),
        pinned_directory: Some(pinned.working_directory.as_str()),
        pinned_profile: pinned
            .credential_profile
            .as_ref()
            .map(CredentialProfileName::as_str),
        grant_lineage: pinned.grant_lineage,
        tools: pinned.tools.iter().collect(),
        runner_required_tools: pinned.runner_required_tools.iter().collect(),
        workspace_repository: workspace
            .and_then(|workspace| workspace.repository.as_ref())
            .map(WorkspaceRepositoryKey::as_str),
        workspace_directory: workspace.map(|workspace| workspace.working_directory.as_str()),
        workspace_manifest: workspace.map(|workspace| workspace.manifest_id.into_uuid()),
        workspace_placement_revision: workspace
            .map(|workspace| Decimal::from(workspace.placement_revision.get())),
        workspace_clone_url_digest: workspace
            .and_then(|workspace| workspace.canonical_clone_url_digest.as_ref())
            .map(CanonicalCloneUrlDigest::as_str),
        workspace_credential_profile: workspace
            .and_then(|workspace| workspace.credential_profile.as_ref())
            .map(CredentialProfileName::as_str),
        workspace_sandbox: workspace.map(|workspace| runner_sandbox_to_str(workspace.sandbox)),
        workspace_relative_path: workspace.map(|workspace| workspace.relative_path.as_str()),
        workspace_recovery_kind,
        workspace_branch_name,
        workspace_revision,
    }
}

fn encode_workspace_recovery(
    recovery: &WorkspaceRecovery,
) -> (Option<&'static str>, Option<&str>, Option<&str>) {
    match recovery {
        WorkspaceRecovery::Commit { revision } => (Some("commit"), None, Some(revision.as_str())),
        WorkspaceRecovery::Branch { name, revision } => {
            (Some("branch"), Some(name.as_str()), Some(revision.as_str()))
        }
        WorkspaceRecovery::UnbornBranch { name } => {
            (Some("unborn_branch"), Some(name.as_str()), None)
        }
    }
}

fn encode_selector(selector: &RunnerSelector) -> (&'static str, Option<Uuid>, Option<&str>) {
    match selector {
        RunnerSelector::Identity(runner) => ("identity", Some(runner.into_uuid()), None),
        RunnerSelector::CapabilityClass(class) => ("capability_class", None, Some(class.as_str())),
    }
}

fn encode_directory(selection: &WorkingDirectorySelection) -> (&'static str, Option<&str>) {
    match selection {
        WorkingDirectorySelection::RunnerDefault => ("runner_default", None),
        WorkingDirectorySelection::Exact(directory) => ("exact", Some(directory.as_str())),
    }
}

fn encode_workspace_requirement(
    requirement: &WorkspaceRequirement,
) -> (&'static str, Option<&str>) {
    match requirement {
        WorkspaceRequirement::None => ("none", None),
        WorkspaceRequirement::RepositoryWorktree { repository } => {
            ("repository_worktree", Some(repository.as_str()))
        }
    }
}

fn decode_selector(row: &PgRow) -> Result<RunnerSelector, RunnerProtocolStoreError> {
    let kind: String = row.decode_column("selector_kind")?;
    match kind.as_str() {
        "identity" => row
            .decode_column::<Option<Uuid>>("selector_runner_id")?
            .map(runner_id)
            .map(RunnerSelector::Identity)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding.into()),
        "capability_class" => row
            .decode_column::<Option<String>>("selector_capability_class")?
            .map(capability_class)
            .transpose()?
            .map(RunnerSelector::CapabilityClass)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding.into()),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

fn decode_directory(row: &PgRow) -> Result<WorkingDirectorySelection, RunnerProtocolStoreError> {
    let kind: String = row.decode_column("directory_selection_kind")?;
    match kind.as_str() {
        "runner_default" => Ok(WorkingDirectorySelection::RunnerDefault),
        "exact" => row
            .decode_column::<Option<String>>("requested_working_directory")?
            .map(working_directory)
            .transpose()?
            .map(WorkingDirectorySelection::Exact)
            .ok_or(RunnerProtocolCorruption::InvalidEncoding.into()),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

fn decode_workspace_requirement(
    row: &PgRow,
) -> Result<WorkspaceRequirement, RunnerProtocolStoreError> {
    let kind: String = row.decode_column("workspace_requirement_kind")?;
    match kind.as_str() {
        "none" => Ok(WorkspaceRequirement::None),
        "repository_worktree" => row
            .decode_column::<Option<String>>("requested_repository_key")?
            .map(repository_key)
            .transpose()?
            .map(|repository| WorkspaceRequirement::RepositoryWorktree { repository })
            .ok_or(RunnerProtocolCorruption::InvalidEncoding.into()),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

struct EncodedLoci<'a> {
    kind: &'static str,
    selector_kind: &'static str,
    selector_runner: Option<Uuid>,
    selector_class: Option<&'a str>,
}

fn encode_loci(loci: &ToolAdmissibleLoci) -> Result<EncodedLoci<'_>, RunnerProtocolStoreError> {
    match loci {
        ToolAdmissibleLoci::DaemonOnly => Err(RunnerProtocolStoreError::Domain(
            RunnerDomainError::InvalidState,
        )),
        ToolAdmissibleLoci::RunnerOnly { selector } => {
            let (kind, runner, class) = encode_selector(selector);
            Ok(EncodedLoci {
                kind: "runner_only",
                selector_kind: kind,
                selector_runner: runner,
                selector_class: class,
            })
        }
        ToolAdmissibleLoci::DaemonOrRunner { selector } => {
            let (kind, runner, class) = encode_selector(selector);
            Ok(EncodedLoci {
                kind: "daemon_or_runner",
                selector_kind: kind,
                selector_runner: runner,
                selector_class: class,
            })
        }
    }
}

fn decode_tool_declaration(row: &PgRow) -> Result<RunnerToolDeclaration, RunnerProtocolStoreError> {
    let selector = decode_selector(row)?;
    let loci: String = row.decode_column("loci_kind")?;
    let loci = match loci.as_str() {
        "runner_only" => ToolAdmissibleLoci::RunnerOnly { selector },
        "daemon_or_runner" => ToolAdmissibleLoci::DaemonOrRunner { selector },
        _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    };
    let stored_schema: String = row.decode_column("model_input_schema")?;
    let model = RunnerToolModelDefinition::try_new(
        row.decode_column("model_description")?,
        stored_schema.clone(),
    )
    .map_err(RunnerProtocolStoreError::Domain)?;
    if model.input_schema().as_str() != stored_schema {
        return Err(RunnerProtocolCorruption::InvalidEncoding.into());
    }
    Ok(RunnerToolDeclaration::new(
        tool_name(row.decode_column("tool_name")?)?,
        model,
        decode_permission(row.decode_column("permission_kind")?)?,
        decode_effect(row.decode_column("effect_class")?)?,
        loci,
    ))
}

pub(crate) const fn encode_permission_override(
    permission: RunnerToolPermissionOverride,
) -> &'static str {
    match permission {
        RunnerToolPermissionOverride::Auto => "auto",
        RunnerToolPermissionOverride::Confirm => "confirm",
    }
}

pub(crate) struct EncodedRunnerPlacementRequest<'a> {
    pub(crate) selector_kind: Option<&'static str>,
    pub(crate) selector_runner: Option<Uuid>,
    pub(crate) selector_class: Option<&'a str>,
    pub(crate) directory_kind: Option<&'static str>,
    pub(crate) requested_directory: Option<&'a str>,
    pub(crate) credential_profile: Option<&'a str>,
    pub(crate) workspace_kind: Option<&'static str>,
    pub(crate) requested_repository: Option<&'a str>,
    pub(crate) sandbox: Option<&'static str>,
    pub(crate) permission_override_count: Decimal,
}

pub(crate) fn encode_runner_placement_request(
    request: Option<&SessionRunnerPlacementRequest>,
) -> Result<EncodedRunnerPlacementRequest<'_>, RunnerProtocolStoreError> {
    let Some(request) = request else {
        return Ok(EncodedRunnerPlacementRequest {
            selector_kind: None,
            selector_runner: None,
            selector_class: None,
            directory_kind: None,
            requested_directory: None,
            credential_profile: None,
            workspace_kind: None,
            requested_repository: None,
            sandbox: None,
            permission_override_count: Decimal::ZERO,
        });
    };
    let (selector_kind, selector_runner, selector_class) = encode_selector(&request.selector);
    let (directory_kind, requested_directory) = encode_directory(&request.working_directory);
    let (workspace_kind, requested_repository) = encode_workspace_requirement(&request.workspace);
    Ok(EncodedRunnerPlacementRequest {
        selector_kind: Some(selector_kind),
        selector_runner,
        selector_class,
        directory_kind: Some(directory_kind),
        requested_directory,
        credential_profile: request
            .credential_profile
            .as_ref()
            .map(CredentialProfileName::as_str),
        workspace_kind: Some(workspace_kind),
        requested_repository,
        sandbox: Some(runner_sandbox_to_str(request.sandbox)),
        permission_override_count: count_decimal(request.permission_overrides.iter().count())?,
    })
}

pub(crate) enum RunnerCreationCommandKind {
    Native,
    Imported,
}

pub(crate) async fn load_creation_permission_overrides(
    connection: &mut PgConnection,
    kind: RunnerCreationCommandKind,
    command_id: Uuid,
) -> Result<RunnerToolPermissionOverrides, RunnerProtocolStoreError> {
    let query = match kind {
        RunnerCreationCommandKind::Native => {
            "SELECT tool_name, permission_kind
               FROM create_session_runner_permission_override
              WHERE command_id = $1 ORDER BY tool_name"
        }
        RunnerCreationCommandKind::Imported => {
            "SELECT tool_name, permission_kind
               FROM imported_session_runner_permission_override
              WHERE command_id = $1 ORDER BY tool_name"
        }
    };
    let rows = sqlx::query(query)
        .bind(command_id)
        .fetch_all(&mut *connection)
        .await?;
    let overrides = rows
        .into_iter()
        .map(|row| {
            Ok((
                tool_name(row.decode_column("tool_name")?)?,
                decode_permission_override(row.decode_column("permission_kind")?)?,
            ))
        })
        .collect::<Result<Vec<_>, RunnerProtocolStoreError>>()?;
    RunnerToolPermissionOverrides::try_new(overrides).map_err(RunnerProtocolStoreError::Domain)
}

pub(crate) fn decode_creation_runner_placement_request(
    row: &PgRow,
    overrides: RunnerToolPermissionOverrides,
) -> Result<Option<SessionRunnerPlacementRequest>, RunnerProtocolStoreError> {
    let selector_kind: Option<String> = row.decode_column("runner_selector_kind")?;
    let Some(selector_kind) = selector_kind else {
        let expected_count =
            decode_u64(row.decode_column::<Decimal>("runner_permission_override_count")?)?;
        if expected_count != 0
            || overrides.iter().next().is_some()
            || row
                .decode_column::<Option<Uuid>>("runner_selector_runner_id")?
                .is_some()
            || row
                .decode_column::<Option<String>>("runner_selector_capability_class")?
                .is_some()
            || row
                .decode_column::<Option<String>>("runner_directory_selection_kind")?
                .is_some()
            || row
                .decode_column::<Option<String>>("runner_requested_working_directory")?
                .is_some()
            || row
                .decode_column::<Option<String>>("runner_credential_profile_name")?
                .is_some()
            || row
                .decode_column::<Option<String>>("runner_workspace_requirement_kind")?
                .is_some()
            || row
                .decode_column::<Option<String>>("runner_requested_repository_key")?
                .is_some()
            || row
                .decode_column::<Option<String>>("runner_sandbox_profile")?
                .is_some()
        {
            return Err(RunnerProtocolCorruption::InvalidEncoding.into());
        }
        return Ok(None);
    };
    let selector = match selector_kind.as_str() {
        "identity" => RunnerSelector::Identity(runner_id(
            row.decode_column::<Option<Uuid>>("runner_selector_runner_id")?
                .ok_or(RunnerProtocolCorruption::InvalidEncoding)?,
        )),
        "capability_class" => RunnerSelector::CapabilityClass(capability_class(
            row.decode_column::<Option<String>>("runner_selector_capability_class")?
                .ok_or(RunnerProtocolCorruption::InvalidEncoding)?,
        )?),
        _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    };
    let directory_kind = row
        .decode_column::<Option<String>>("runner_directory_selection_kind")?
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
    let working_directory = match directory_kind.as_str() {
        "runner_default" => WorkingDirectorySelection::RunnerDefault,
        "exact" => WorkingDirectorySelection::Exact(working_directory(
            row.decode_column::<Option<String>>("runner_requested_working_directory")?
                .ok_or(RunnerProtocolCorruption::InvalidEncoding)?,
        )?),
        _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    };
    let credential_profile = row
        .decode_column::<Option<String>>("runner_credential_profile_name")?
        .map(profile_name)
        .transpose()?;
    let workspace_kind = row
        .decode_column::<Option<String>>("runner_workspace_requirement_kind")?
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
    let workspace = match workspace_kind.as_str() {
        "none" => WorkspaceRequirement::None,
        "repository_worktree" => WorkspaceRequirement::RepositoryWorktree {
            repository: repository_key(
                row.decode_column::<Option<String>>("runner_requested_repository_key")?
                    .ok_or(RunnerProtocolCorruption::InvalidEncoding)?,
            )?,
        },
        _ => return Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    };
    let sandbox = runner_sandbox_from_str(
        &row.decode_column::<Option<String>>("runner_sandbox_profile")?
            .ok_or(RunnerProtocolCorruption::InvalidEncoding)?,
    )
    .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
    let expected_count =
        decode_u64(row.decode_column::<Decimal>("runner_permission_override_count")?)?;
    let actual_count = u64::try_from(overrides.iter().count())
        .map_err(|_| RunnerProtocolCorruption::InvalidEncoding)?;
    if expected_count != actual_count {
        return Err(RunnerProtocolCorruption::InvalidEncoding.into());
    }
    Ok(Some(SessionRunnerPlacementRequest {
        selector,
        working_directory,
        credential_profile,
        workspace,
        sandbox,
        permission_overrides: overrides,
    }))
}

pub(crate) async fn load_initial_session_runner_placement(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<Option<SessionRunnerPlacement>, RunnerProtocolStoreError> {
    let row = sqlx::query(
        "SELECT session_id, event_ordinal, placement_revision, event_kind, state_kind,
                selector_kind AS runner_selector_kind,
                selector_runner_id AS runner_selector_runner_id,
                selector_capability_class AS runner_selector_capability_class,
                directory_selection_kind AS runner_directory_selection_kind,
                requested_working_directory AS runner_requested_working_directory,
                requested_credential_profile_name AS runner_credential_profile_name,
                workspace_requirement_kind AS runner_workspace_requirement_kind,
                requested_repository_key AS runner_requested_repository_key,
                requested_sandbox_profile AS runner_sandbox_profile,
                permission_override_count AS runner_permission_override_count,
                permission_override_count
           FROM runner_session_placement_record
          WHERE session_id = $1 AND event_ordinal = 1",
    )
    .bind(session.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    if decode_u64(row.decode_column("event_ordinal")?)? != 1
        || decode_u64(row.decode_column("placement_revision")?)? != 1
        || row.decode_column::<String>("event_kind")? != "created"
        || row.decode_column::<String>("state_kind")? != "unpinned"
    {
        return Err(RunnerProtocolCorruption::InvalidEncoding.into());
    }
    let overrides = load_permission_overrides(connection, &row).await?;
    let request = decode_creation_runner_placement_request(&row, overrides)?
        .ok_or(RunnerProtocolCorruption::InvalidEncoding)?;
    Ok(Some(SessionRunnerPlacement::new(session, request)))
}

fn decode_permission_override(
    value: String,
) -> Result<RunnerToolPermissionOverride, RunnerProtocolStoreError> {
    match value.as_str() {
        "auto" => Ok(RunnerToolPermissionOverride::Auto),
        "confirm" => Ok(RunnerToolPermissionOverride::Confirm),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

fn decode_permission(value: String) -> Result<ToolPermissionDefault, RunnerProtocolStoreError> {
    tool_permission_default_from_str(&value)
        .ok_or_else(|| RunnerProtocolCorruption::InvalidEncoding.into())
}

const fn encode_effect(effect: RunnerToolEffectClass) -> &'static str {
    match effect {
        RunnerToolEffectClass::Pure => "pure",
        RunnerToolEffectClass::Idempotent => "idempotent",
        RunnerToolEffectClass::SideEffecting => "side_effecting",
    }
}

fn decode_effect(value: String) -> Result<RunnerToolEffectClass, RunnerProtocolStoreError> {
    match value.as_str() {
        "pure" => Ok(RunnerToolEffectClass::Pure),
        "idempotent" => Ok(RunnerToolEffectClass::Idempotent),
        "side_effecting" => Ok(RunnerToolEffectClass::SideEffecting),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

const fn encode_approval(approval: CredentialToolApproval) -> &'static str {
    match approval {
        CredentialToolApproval::Automatic => "automatic",
        CredentialToolApproval::SessionPolicy => "session_policy",
    }
}

fn decode_approval(value: String) -> Result<CredentialToolApproval, RunnerProtocolStoreError> {
    match value.as_str() {
        "automatic" => Ok(CredentialToolApproval::Automatic),
        "session_policy" => Ok(CredentialToolApproval::SessionPolicy),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

fn decode_sandbox(value: String) -> Result<RunnerSandboxProfile, RunnerProtocolStoreError> {
    runner_sandbox_from_str(&value).ok_or_else(|| RunnerProtocolCorruption::InvalidEncoding.into())
}

const fn encode_workspace(workspace: WorkspaceCapability) -> &'static str {
    match workspace {
        WorkspaceCapability::WorktreePerSession => "worktree_per_session",
    }
}

fn decode_workspace(value: String) -> Result<WorkspaceCapability, RunnerProtocolStoreError> {
    match value.as_str() {
        "worktree_per_session" => Ok(WorkspaceCapability::WorktreePerSession),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

const fn encode_lease_state(state: RunnerLeaseState) -> &'static str {
    match state {
        RunnerLeaseState::Offered => "offered",
        RunnerLeaseState::Claimed => "claimed",
        RunnerLeaseState::Completed => "completed",
        RunnerLeaseState::LostUnclaimed => "lost_unclaimed",
        RunnerLeaseState::LostClaimed => "lost_claimed",
        RunnerLeaseState::LostExecutionPossible => "lost_execution_possible",
    }
}

fn decode_lease_state(value: String) -> Result<RunnerLeaseState, RunnerProtocolStoreError> {
    match value.as_str() {
        "offered" => Ok(RunnerLeaseState::Offered),
        "claimed" => Ok(RunnerLeaseState::Claimed),
        "completed" => Ok(RunnerLeaseState::Completed),
        "lost_unclaimed" => Ok(RunnerLeaseState::LostUnclaimed),
        "lost_claimed" => Ok(RunnerLeaseState::LostClaimed),
        "lost_execution_possible" => Ok(RunnerLeaseState::LostExecutionPossible),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

fn decode_enrollment_authority(
    value: String,
) -> Result<RunnerEnrollmentAuthority, RunnerProtocolStoreError> {
    match value.as_str() {
        "active" => Ok(RunnerEnrollmentAuthority::Active),
        "replacement_pending" => Ok(RunnerEnrollmentAuthority::ReplacementPending),
        _ => Err(RunnerProtocolCorruption::InvalidEncoding.into()),
    }
}

pub(crate) const fn encode_enrollment_authority(
    authority: RunnerEnrollmentAuthority,
) -> &'static str {
    match authority {
        RunnerEnrollmentAuthority::Active => "active",
        RunnerEnrollmentAuthority::ReplacementPending => "replacement_pending",
    }
}

fn decode_classes(
    rows: &[PgRow],
) -> Result<BTreeSet<RunnerCapabilityClass>, RunnerProtocolStoreError> {
    rows.iter()
        .map(|row| capability_class(row.decode_column("capability_class")?))
        .collect()
}

fn capability_class(value: String) -> Result<RunnerCapabilityClass, RunnerProtocolStoreError> {
    RunnerCapabilityClass::try_new(value).map_err(RunnerProtocolStoreError::Domain)
}

fn profile_name(value: String) -> Result<CredentialProfileName, RunnerProtocolStoreError> {
    CredentialProfileName::try_new(value).map_err(RunnerProtocolStoreError::Domain)
}

fn working_directory(value: String) -> Result<RunnerWorkingDirectory, RunnerProtocolStoreError> {
    RunnerWorkingDirectory::try_new(value).map_err(RunnerProtocolStoreError::Domain)
}

fn repository_key(value: String) -> Result<WorkspaceRepositoryKey, RunnerProtocolStoreError> {
    WorkspaceRepositoryKey::try_new(value).map_err(RunnerProtocolStoreError::Domain)
}

fn tool_name(value: String) -> Result<ToolName, RunnerProtocolStoreError> {
    ToolName::try_new(value).map_err(|_| RunnerProtocolCorruption::InvalidEncoding.into())
}

fn require_count(
    row: &PgRow,
    column: &'static str,
    actual: usize,
) -> Result<(), RunnerProtocolStoreError> {
    if row.decode_column::<Decimal>(column)? == Decimal::from(actual) {
        Ok(())
    } else {
        Err(RunnerProtocolCorruption::IncompleteInventory.into())
    }
}

fn count_decimal(value: usize) -> Result<Decimal, RunnerProtocolStoreError> {
    let value = u64::try_from(value).map_err(|_| RunnerProtocolCorruption::GenerationExhausted)?;
    Ok(Decimal::from(value))
}

fn decode_u64(value: Decimal) -> Result<u64, RunnerProtocolStoreError> {
    value
        .to_u64()
        .filter(|decoded| Decimal::from(*decoded) == value)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding.into())
}

fn decode_generation(value: Decimal) -> Result<RunnerGeneration, RunnerProtocolStoreError> {
    RunnerGeneration::try_from_u64(decode_u64(value)?)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding.into())
}

fn decode_dispatch_generation(
    value: Decimal,
) -> Result<ToolDispatchGeneration, RunnerProtocolStoreError> {
    let value = decode_u64(value)?;
    ToolDispatchGeneration::try_from_u64(value)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding.into())
}

fn decode_registration_revision(
    value: Decimal,
) -> Result<RunnerRegistrationRevision, RunnerProtocolStoreError> {
    RunnerRegistrationRevision::try_from_u64(decode_u64(value)?)
        .ok_or(RunnerProtocolCorruption::InvalidEncoding.into())
}

async fn begin_repeatable_read(pool: &PgPool) -> Result<Transaction<'_, Postgres>, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *transaction)
        .await?;
    Ok(transaction)
}

async fn commit_mutation(
    transaction: Transaction<'_, Postgres>,
) -> Result<(), RunnerProtocolStoreError> {
    transaction
        .commit()
        .await
        .map_err(classify_mutating_commit_error)
}

fn classify_mutating_commit_error(error: sqlx::Error) -> RunnerProtocolStoreError {
    if crate::commit_failure_is_ambiguous(&error) {
        RunnerProtocolStoreError::CommitAmbiguous(error)
    } else {
        RunnerProtocolStoreError::Database(error)
    }
}

trait RunnerProtocolRow {
    fn decode_column<'row, T>(
        &'row self,
        column: &'static str,
    ) -> Result<T, RunnerProtocolStoreError>
    where
        T: sqlx::Decode<'row, Postgres> + sqlx::Type<Postgres>;
}

impl RunnerProtocolRow for PgRow {
    fn decode_column<'row, T>(
        &'row self,
        column: &'static str,
    ) -> Result<T, RunnerProtocolStoreError>
    where
        T: sqlx::Decode<'row, Postgres> + sqlx::Type<Postgres>,
    {
        match Row::try_get(self, column) {
            Ok(value) => Ok(value),
            Err(sqlx::Error::ColumnDecode { .. } | sqlx::Error::Decode(_)) => {
                Err(RunnerProtocolCorruption::InvalidColumn(column).into())
            }
            Err(error) => Err(error.into()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StoredGrantRevocation {
    Active,
    Revoked,
}

const fn decode_stored_grant_revocation(value: bool) -> StoredGrantRevocation {
    match value {
        false => StoredGrantRevocation::Active,
        true => StoredGrantRevocation::Revoked,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StoredRunnerRequirement {
    Optional,
    Required,
}

const fn decode_stored_runner_requirement(value: bool) -> StoredRunnerRequirement {
    match value {
        false => StoredRunnerRequirement::Optional,
        true => StoredRunnerRequirement::Required,
    }
}

const fn runner_enrollment_id(value: Uuid) -> RunnerEnrollmentId {
    RunnerEnrollmentId::from_uuid(value)
}

const fn runner_id(value: Uuid) -> RunnerId {
    RunnerId::from_uuid(value)
}

const fn runner_authentication_id(value: Uuid) -> RunnerAuthenticationId {
    RunnerAuthenticationId::from_uuid(value)
}

const fn runner_lease_id(value: Uuid) -> RunnerLeaseId {
    RunnerLeaseId::from_uuid(value)
}

const fn tool_attempt_id(value: Uuid) -> ToolAttemptId {
    ToolAttemptId::from_uuid(value)
}

const fn session_id(value: Uuid) -> SessionId {
    SessionId::from_uuid(value)
}

/// Why a pristine enrollment or registration resume fails before mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerEnrollmentRequestFailure {
    /// Another active version-one enrollment already occupies the singleton slot.
    ActiveEnrollmentExists {
        /// The rejected stable request identity.
        request: RunnerEnrollmentRequestId,
        /// The enrollment currently occupying the active slot.
        active_enrollment: RunnerEnrollmentId,
    },
    /// Another pending version-one successor already occupies the candidate slot.
    PendingEnrollmentExists {
        /// The rejected stable request identity.
        request: RunnerEnrollmentRequestId,
        /// The enrollment currently occupying the pending slot.
        pending_enrollment: RunnerEnrollmentId,
    },
    /// A replay changed the availability payload bound to its request identity.
    ReplayAdvertisementMismatch {
        /// The replayed stable request identity.
        request: RunnerEnrollmentRequestId,
    },
    /// A replay changed daemon-owned allowed classes bound to the enrollment.
    ReplayPolicyMismatch {
        /// The replayed stable request identity.
        request: RunnerEnrollmentRequestId,
    },
    /// Resume named no durable enrollment request.
    UnknownRequest {
        /// The unknown request identity.
        request: RunnerEnrollmentRequestId,
    },
    /// Resume supplied identities other than those durably issued for the request.
    ResumeIdentityMismatch {
        /// The stable enrollment request identity.
        request: RunnerEnrollmentRequestId,
        /// The identities stored by pristine enrollment.
        expected: IssuedRunnerEnrollmentIdentities,
        /// The identities supplied by the reconnecting runner.
        observed: IssuedRunnerEnrollmentIdentities,
    },
    /// Resume attempted to use terminally revoked enrollment authority.
    EnrollmentRevoked {
        /// The stable enrollment request identity.
        request: RunnerEnrollmentRequestId,
        /// The terminally revoked enrollment.
        enrollment: RunnerEnrollmentId,
    },
    /// Resume supplied a registration revision other than the durable current head.
    ResumeRevisionMismatch {
        /// The stable enrollment request identity.
        request: RunnerEnrollmentRequestId,
        /// The durable current registration revision.
        expected: RunnerRegistrationRevision,
        /// The revision supplied by the reconnecting runner.
        observed: RunnerRegistrationRevision,
    },
    /// A stale resume also diverged from the durable current advertisement.
    StaleResumeAdvertisement {
        /// The stable enrollment request identity.
        request: RunnerEnrollmentRequestId,
        /// The stale registration revision supplied by the runner.
        prior: RunnerRegistrationRevision,
        /// The durable current registration revision.
        current: RunnerRegistrationRevision,
    },
}

impl fmt::Display for RunnerEnrollmentRequestFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ActiveEnrollmentExists {
                request,
                active_enrollment,
            } => write!(
                formatter,
                "runner enrollment request {} conflicts with active enrollment {}",
                request.as_uuid(),
                active_enrollment.as_uuid()
            ),
            Self::PendingEnrollmentExists {
                request,
                pending_enrollment,
            } => write!(
                formatter,
                "runner enrollment request {} conflicts with pending enrollment {}",
                request.as_uuid(),
                pending_enrollment.as_uuid()
            ),
            Self::ReplayAdvertisementMismatch { request } => write!(
                formatter,
                "runner enrollment request {} replayed with different availability",
                request.as_uuid()
            ),
            Self::ReplayPolicyMismatch { request } => write!(
                formatter,
                "runner enrollment request {} replayed with different allowed classes",
                request.as_uuid()
            ),
            Self::UnknownRequest { request } => write!(
                formatter,
                "runner resume names unknown enrollment request {}",
                request.as_uuid()
            ),
            Self::ResumeIdentityMismatch {
                request,
                expected,
                observed,
            } => write!(
                formatter,
                "runner resume {} identity mismatch: expected enrollment {}, runner {}, authentication {}; observed enrollment {}, runner {}, authentication {}",
                request.as_uuid(),
                expected.enrollment().as_uuid(),
                expected.runner().as_uuid(),
                expected.authentication().as_uuid(),
                observed.enrollment().as_uuid(),
                observed.runner().as_uuid(),
                observed.authentication().as_uuid()
            ),
            Self::EnrollmentRevoked {
                request,
                enrollment,
            } => write!(
                formatter,
                "runner resume {} names revoked enrollment {}",
                request.as_uuid(),
                enrollment.as_uuid()
            ),
            Self::ResumeRevisionMismatch {
                request,
                expected,
                observed,
            } => write!(
                formatter,
                "runner resume {} revision mismatch: expected {}, observed {}",
                request.as_uuid(),
                expected.get(),
                observed.get()
            ),
            Self::StaleResumeAdvertisement {
                request,
                prior,
                current,
            } => write!(
                formatter,
                "runner resume {} at stale revision {} diverges from current registration revision {}",
                request.as_uuid(),
                prior.get(),
                current.get()
            ),
        }
    }
}

impl Error for RunnerEnrollmentRequestFailure {}

/// A durable runner-protocol shape that cannot reconstruct domain state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerProtocolCorruption {
    /// Canonical enrollment state is absent.
    MissingCanonicalEnrollment,
    /// Canonical audit evidence is absent.
    MissingCanonicalAudit,
    /// Canonical registration state is absent.
    MissingCanonicalRegistration,
    /// Canonical physical connection state is absent.
    MissingCanonicalConnection,
    /// Canonical connection-loss state or its propagation cursor is absent.
    MissingCanonicalLoss,
    /// Canonical placement state is absent.
    MissingCanonicalPlacement,
    /// Canonical credential-grant state is absent.
    MissingCanonicalGrant,
    /// Canonical tool-attempt state is absent.
    MissingCanonicalAttempt,
    /// A declared count disagrees with its durable members.
    IncompleteInventory,
    /// Correlated durable records identify different domain values.
    CrossWiredReference,
    /// A projected column cannot decode to its expected Rust type.
    InvalidColumn(&'static str),
    /// A stored scalar cannot construct its closed domain value.
    InvalidEncoding,
    /// A durable generation cannot advance without overflow.
    GenerationExhausted,
}

impl fmt::Display for RunnerProtocolCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCanonicalEnrollment => {
                formatter.write_str("canonical runner enrollment is missing")
            }
            Self::MissingCanonicalAudit => {
                formatter.write_str("canonical runner audit evidence is missing")
            }
            Self::MissingCanonicalRegistration => {
                formatter.write_str("canonical runner registration is missing")
            }
            Self::MissingCanonicalConnection => {
                formatter.write_str("canonical runner connection is missing")
            }
            Self::MissingCanonicalLoss => {
                formatter.write_str("canonical runner connection loss is missing")
            }
            Self::MissingCanonicalPlacement => {
                formatter.write_str("canonical runner placement is missing")
            }
            Self::MissingCanonicalGrant => {
                formatter.write_str("canonical credential grant is missing")
            }
            Self::MissingCanonicalAttempt => {
                formatter.write_str("canonical physical tool attempt is missing")
            }
            Self::IncompleteInventory => {
                formatter.write_str("stored runner inventory is incomplete")
            }
            Self::CrossWiredReference => {
                formatter.write_str("stored runner references are cross-wired")
            }
            Self::InvalidColumn(column) => {
                write!(
                    formatter,
                    "stored runner column {column} has an invalid value"
                )
            }
            Self::InvalidEncoding => formatter.write_str("stored runner encoding is invalid"),
            Self::GenerationExhausted => {
                formatter.write_str("stored runner generation is exhausted")
            }
        }
    }
}

impl Error for RunnerProtocolCorruption {}

/// A database, durable-shape, or domain-admission failure.
#[derive(Debug)]
pub enum RunnerProtocolStoreError {
    /// PostgreSQL failed before a commit could have succeeded.
    Database(sqlx::Error),
    /// PostgreSQL obscured whether the requested commit succeeded.
    CommitAmbiguous(sqlx::Error),
    /// Durable records cannot reconstruct the admitted runner state.
    Corruption(RunnerProtocolCorruption),
    /// Complete values fail a domain-owned runner transition or invariant.
    Domain(RunnerDomainError),
    /// Enrollment or resume input conflicts with durable request authority.
    EnrollmentRequest(RunnerEnrollmentRequestFailure),
}

impl fmt::Display for RunnerProtocolStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "runner-protocol database failure: {error}"),
            Self::CommitAmbiguous(error) => {
                write!(
                    formatter,
                    "runner-protocol commit outcome is ambiguous: {error}"
                )
            }
            Self::Corruption(error) => error.fmt(formatter),
            Self::Domain(error) => write!(formatter, "runner-protocol domain failure: {error:?}"),
            Self::EnrollmentRequest(error) => error.fmt(formatter),
        }
    }
}

impl Error for RunnerProtocolStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::Corruption(error) => Some(error),
            Self::EnrollmentRequest(error) => Some(error),
            Self::Domain(_) => None,
        }
    }
}

impl ClassifyOperatorFailure for RunnerProtocolStoreError {
    fn operator_failure_class(&self) -> OperatorFailureClass {
        match self {
            Self::Database(_) => OperatorFailureClass::Infrastructure {
                commit_ambiguous: false,
            },
            Self::CommitAmbiguous(_) => OperatorFailureClass::Infrastructure {
                commit_ambiguous: true,
            },
            Self::Corruption(_) => OperatorFailureClass::FailClosedCorruption,
            Self::Domain(_) | Self::EnrollmentRequest(_) => OperatorFailureClass::CallerOrHubBug,
        }
    }

    fn operator_failure_cause_code(&self) -> &'static str {
        "runner_protocol_persistence"
    }
}

impl From<sqlx::Error> for RunnerProtocolStoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<RunnerProtocolCorruption> for RunnerProtocolStoreError {
    fn from(error: RunnerProtocolCorruption) -> Self {
        Self::Corruption(error)
    }
}

impl From<RunnerEnrollmentRequestFailure> for RunnerProtocolStoreError {
    fn from(error: RunnerEnrollmentRequestFailure) -> Self {
        Self::EnrollmentRequest(error)
    }
}
