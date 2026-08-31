//! Hub-side registration, lifecycle, and durable-operation runtime for the local runner wire.

use std::{error::Error, fmt, future::Future, io, pin::Pin, sync::Arc, time::Duration};

use rustix::process::geteuid;
use signalbox_application::{
    RunnerLeaseClaimRequest, RunnerLeaseClaimService, RunnerLeaseResultRequest,
    RunnerLeaseResultService, RunnerReadyManifestDigest, RunnerWorkspaceReadyReceipt,
    RunnerWorkspaceReadyService,
};
use signalbox_domain::{
    CanonicalCloneUrlDigest, CredentialProfileName, CredentialProfilePolicy,
    RunnerAuthenticationId, RunnerCapabilityClass, RunnerCatalog, RunnerDomainError,
    RunnerEnrollmentId, RunnerEnrollmentRequestId, RunnerGeneration, RunnerId, RunnerLease,
    RunnerSandboxProfile, SessionId, WorkspaceBranchName, WorkspaceManifestId,
    WorkspaceProvisioningAuthorizationId, WorkspaceRecovery, WorkspaceRelativePath,
    WorkspaceRepositoryKey, WorkspaceRevision,
};
use signalbox_persistence::runner_protocol::{
    AppliedRunnerConnectionTransition, IssuedRunnerEnrollmentIdentities,
    PristineRunnerEnrollmentRequest, RunnerConnectionCause, RunnerConnectionEpoch,
    RunnerConnectionState, RunnerConnectionTransition, RunnerConnectionTransitionEffect,
    RunnerConnectionTransitionOutcome, RunnerEnrollmentAuthority, RunnerEnrollmentDisposition,
    RunnerEnrollmentRequestFailure, RunnerProtocolStore, RunnerProtocolStoreError,
    RunnerRegistrationRevision,
};
use signalbox_runner_wire::{
    Advertise, AvailableCorrelation, CanonicalUuid, DIGEST_VERSION, Directive, DirectiveAction,
    Enroll, Enrolled, Frame, FrameError, Heartbeat, HeartbeatAck, HeartbeatWorkspacePhase,
    LeaseClaim, LeaseCorrelation, LeasePhaseKind, MAX_FRAME_BYTES, Message, PositiveU64,
    ProvisionPhase, ReconnectDirectives, ReconnectInventory, Recovery as WireWorkspaceRecovery,
    Registered, Rejected, RejectionCode, ReplacementPending, ResultFrame, Resume, Resumed,
    SandboxProfile, Shutdown, ShutdownReason, WorkspaceFailureCorrelation, WorkspaceOperation,
    WorkspaceReady, WorkspaceRecorded, advertisement_digest, decode_line, encode_line,
};
use sqlx::PgPool;
use tokio::{
    io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
    net::{UnixStream, unix::OwnedReadHalf, unix::OwnedWriteHalf},
    sync::{Mutex, watch},
    task::{JoinError, JoinSet},
    time::{MissedTickBehavior, interval, timeout},
};

use crate::local_socket::LocalSocketError;
use crate::{
    LocalProcessListener,
    runner_connection_broker::{
        RunnerConnectionAddress, RunnerConnectionBroker, RunnerConnectionBrokerError,
    },
    runner_dispatch_wire::{RunnerDispatchWireAdapter, RunnerDispatchWireError},
};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const HEARTBEAT_MISSES_BEFORE_LOSS: u8 = 3;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECTION_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const MAXIMUM_CONCURRENT_CONNECTIONS: usize = 64;
const REGISTRATION_ONLY_CREDENTIAL_PROFILE: &str = "github-runner";

/// Boxed future returned by the injected durable registration service.
pub type RunnerRegistrationFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, RunnerRegistrationFailure>> + Send + 'a>>;

/// Boxed future returned by the injected durable lease-operation service.
pub type RunnerLeaseOperationFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, RunnerLeaseOperationFailure>> + Send + 'a>>;

/// Closed durable failure classification for one lease claim or result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunnerLeaseOperationFailure {
    code: RejectionCode,
    cause: RunnerRegistrationFailureCause,
}

impl RunnerLeaseOperationFailure {
    /// Constructs one classified durable-operation failure.
    pub const fn new(code: RejectionCode, cause: RunnerRegistrationFailureCause) -> Self {
        Self { code, cause }
    }

    fn into_registration_failure(
        self,
        offending_kind: RunnerInboundFrameKind,
        correlation: AvailableCorrelation,
    ) -> RunnerRegistrationFailure {
        RunnerRegistrationFailure::from_durable_cause(
            offending_kind,
            correlation,
            self.code,
            self.cause,
        )
    }
}

/// Durable-before-ack lease operation boundary consumed by an established connection.
pub trait RunnerLeaseOperationService: Clone + Send + Sync + 'static {
    /// Commits one exact offered lease claim.
    fn claim(
        &self,
        request: RunnerLeaseClaimRequest,
    ) -> RunnerLeaseOperationFuture<'_, RunnerLease>;

    /// Commits one exact claimed lease and terminal attempt observation.
    fn record_result(
        &self,
        request: RunnerLeaseResultRequest,
    ) -> RunnerLeaseOperationFuture<'_, RunnerLease>;
}

/// Fail-closed lease operation service retained by registration-only test composition.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableRunnerLeaseOperationService;

impl RunnerLeaseOperationService for UnavailableRunnerLeaseOperationService {
    fn claim(
        &self,
        _request: RunnerLeaseClaimRequest,
    ) -> RunnerLeaseOperationFuture<'_, RunnerLease> {
        Box::pin(std::future::ready(Err(RunnerLeaseOperationFailure::new(
            RejectionCode::Unavailable,
            RunnerRegistrationFailureCause::Policy,
        ))))
    }

    fn record_result(
        &self,
        _request: RunnerLeaseResultRequest,
    ) -> RunnerLeaseOperationFuture<'_, RunnerLease> {
        Box::pin(std::future::ready(Err(RunnerLeaseOperationFailure::new(
            RejectionCode::Unavailable,
            RunnerRegistrationFailureCause::Policy,
        ))))
    }
}

/// Boxed future returned by the injected durable workspace-ready service.
pub type RunnerWorkspaceReadyOperationFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, RunnerWorkspaceReadyOperationFailure>> + Send + 'a>>;

/// Closed durable failure classification for one workspace-ready receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunnerWorkspaceReadyOperationFailure {
    code: RejectionCode,
    cause: RunnerRegistrationFailureCause,
}

impl RunnerWorkspaceReadyOperationFailure {
    /// Constructs one classified durable-operation failure.
    pub const fn new(code: RejectionCode, cause: RunnerRegistrationFailureCause) -> Self {
        Self { code, cause }
    }

    fn into_registration_failure(
        self,
        correlation: AvailableCorrelation,
    ) -> RunnerRegistrationFailure {
        RunnerRegistrationFailure::from_durable_cause(
            RunnerInboundFrameKind::WorkspaceReady,
            correlation,
            self.code,
            self.cause,
        )
    }
}

/// Durable-before-ack workspace-ready boundary consumed by an established connection.
pub trait RunnerWorkspaceReadyOperationService: Clone + Send + Sync + 'static {
    /// Commits or exactly replays one validated ready-workspace receipt.
    fn record_workspace_ready(
        &self,
        receipt: RunnerWorkspaceReadyReceipt,
    ) -> RunnerWorkspaceReadyOperationFuture<'_, RunnerWorkspaceReadyReceipt>;
}

/// Fail-closed workspace-ready service retained by registration-only test composition.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableRunnerWorkspaceReadyOperationService;

impl RunnerWorkspaceReadyOperationService for UnavailableRunnerWorkspaceReadyOperationService {
    fn record_workspace_ready(
        &self,
        _receipt: RunnerWorkspaceReadyReceipt,
    ) -> RunnerWorkspaceReadyOperationFuture<'_, RunnerWorkspaceReadyReceipt> {
        Box::pin(std::future::ready(Err(
            RunnerWorkspaceReadyOperationFailure::new(
                RejectionCode::Unavailable,
                RunnerRegistrationFailureCause::Policy,
            ),
        )))
    }
}

/// One closed successful response to a pristine runner enrollment request.
#[derive(Clone, Debug)]
pub enum RunnerEnrollmentAccepted {
    /// The first deployment enrollment received active authority.
    Active(Enrolled),
    /// A successor admitted after durable predecessor loss remains provisioning-only.
    ReplacementPending(ReplacementPending),
}

impl RunnerEnrollmentAccepted {
    /// Returns the stable request identity from either accepted authority.
    pub const fn request_id(&self) -> CanonicalUuid {
        match self {
            Self::Active(response) => response.request_id,
            Self::ReplacementPending(response) => response.request_id,
        }
    }

    /// Returns the issued enrollment identity from either accepted authority.
    pub const fn enrollment_id(&self) -> CanonicalUuid {
        match self {
            Self::Active(response) => response.enrollment_id,
            Self::ReplacementPending(response) => response.enrollment_id,
        }
    }

    /// Returns the issued runner identity from either accepted authority.
    pub const fn runner_id(&self) -> CanonicalUuid {
        match self {
            Self::Active(response) => response.runner_id,
            Self::ReplacementPending(response) => response.runner_id,
        }
    }

    /// Returns the issued authentication-reference identity.
    pub const fn authentication_id(&self) -> CanonicalUuid {
        match self {
            Self::Active(response) => response.authentication_id,
            Self::ReplacementPending(response) => response.authentication_id,
        }
    }

    /// Returns the immutable initial registration revision.
    pub const fn registration_revision(&self) -> PositiveU64 {
        match self {
            Self::Active(response) => response.registration_revision,
            Self::ReplacementPending(response) => response.registration_revision,
        }
    }

    /// Returns the opened physical connection epoch.
    pub const fn connection_epoch(&self) -> PositiveU64 {
        match self {
            Self::Active(response) => response.connection_epoch,
            Self::ReplacementPending(response) => response.connection_epoch,
        }
    }

    fn into_message(self) -> Message {
        match self {
            Self::Active(response) => Message::Enrolled(response),
            Self::ReplacementPending(response) => Message::ReplacementPending(response),
        }
    }
}

/// Durable-before-ack boundary consumed by the runner socket runtime.
pub trait RunnerRegistrationService: Clone + Send + Sync + 'static {
    /// Atomically creates or exactly replays pristine enrollment authority.
    fn enroll(&self, request: Enroll) -> RunnerRegistrationFuture<'_, RunnerEnrollmentAccepted>;

    /// Validates one reconnect and returns canonical current registration facts.
    fn resume(&self, request: Resume) -> RunnerRegistrationFuture<'_, RunnerResumeAccepted>;

    /// Atomically appends one replacement availability advertisement.
    fn advertise(
        &self,
        connection_enrollment: CanonicalUuid,
        request: Advertise,
        epoch: PositiveU64,
    ) -> RunnerRegistrationFuture<'_, Registered>;

    /// Durably observes or advances one exact physical connection lifecycle.
    fn transition_connection(
        &self,
        enrollment: CanonicalUuid,
        epoch: PositiveU64,
        transition: RunnerConnectionTransition,
    ) -> RunnerRegistrationFuture<'_, RunnerConnectionTransitionOutcome>;
}

/// Durable resume receipt plus any canonical claimed lease to replay.
#[derive(Debug)]
pub struct RunnerResumeAccepted {
    response: Resumed,
    claimed_lease: Option<RunnerLease>,
}

impl RunnerResumeAccepted {
    /// Constructs a durable resume result and its optional canonical replay.
    pub fn new(response: Resumed, claimed_lease: Option<RunnerLease>) -> Self {
        Self {
            response,
            claimed_lease,
        }
    }

    /// Returns the opened physical connection epoch.
    pub fn connection_epoch(&self) -> PositiveU64 {
        self.response.connection_epoch
    }

    /// Returns the canonical registration revision accepted by resume.
    pub fn registration_revision(&self) -> PositiveU64 {
        self.response.registration_revision
    }

    /// Separates the wire receipt from its optional claimed-lease replay.
    pub fn into_parts(self) -> (Resumed, Option<RunnerLease>) {
        (self.response, self.claimed_lease)
    }
}

/// PostgreSQL-backed durable registration authority for the local runner wire.
#[derive(Clone, Debug)]
pub struct PostgresRunnerRegistrationService {
    store: RunnerProtocolStore,
    allowed_classes: Vec<RunnerCapabilityClass>,
    registration_admission: Arc<Mutex<()>>,
}

impl PostgresRunnerRegistrationService {
    /// Composes persistence with the daemon-owned capability-class allowlist.
    pub fn new(
        store: RunnerProtocolStore,
        allowed_classes: impl IntoIterator<Item = RunnerCapabilityClass>,
    ) -> Self {
        Self {
            store,
            allowed_classes: allowed_classes.into_iter().collect(),
            registration_admission: Arc::new(Mutex::new(())),
        }
    }

    /// Composes the registration-only catalog admitted by this daemon slice.
    pub fn registration_only(pool: PgPool) -> Result<Self, RunnerDomainError> {
        Ok(Self::new(
            RunnerProtocolStore::new(pool, registration_only_catalog()?),
            std::iter::empty(),
        ))
    }

    /// Returns the shared durable runner-protocol adapter for process commands.
    pub fn protocol_store(&self) -> RunnerProtocolStore {
        self.store.clone()
    }

    /// Classifies prior-process nonterminal connections as lost before admission.
    pub async fn mark_orphaned_connections_lost(
        &self,
    ) -> Result<Vec<AppliedRunnerConnectionTransition>, RunnerProtocolStoreError> {
        let _admission = self.registration_admission.lock().await;
        self.propagate_pending_registration_reconciliations()
            .await?;
        let connections = self.store.load_nonterminal_connection_heads().await?;
        let mut transitions = Vec::new();
        for connection in connections {
            let effect = self
                .store
                .transition_connection_with_effect(
                    connection.enrollment(),
                    connection.epoch(),
                    RunnerConnectionTransition::TransportClosed,
                )
                .await?;
            match effect {
                RunnerConnectionTransitionEffect::Applied(applied) => {
                    log_connection_transition(applied, RunnerConnectionTransition::TransportClosed);
                    transitions.push(applied);
                }
                RunnerConnectionTransitionEffect::Unchanged(
                    RunnerConnectionTransitionOutcome::Current(_)
                    | RunnerConnectionTransitionOutcome::Stale { .. },
                ) => {}
            }
        }
        self.propagate_pending_connection_losses().await?;
        Ok(transitions)
    }

    async fn propagate_pending_registration_reconciliations(
        &self,
    ) -> Result<(), RunnerProtocolStoreError> {
        for reconciliation in self
            .store
            .load_pending_registration_reconciliations()
            .await?
        {
            loop {
                let page = self
                    .store
                    .load_registration_reconciliation_page(reconciliation)
                    .await?;
                if page.is_complete() {
                    break;
                }
                if page.sessions().is_empty() {
                    self.store
                        .complete_registration_reconciliation(reconciliation)
                        .await?;
                    break;
                }
                for session in page.sessions() {
                    let disposition = self
                        .store
                        .reconcile_registration_session(reconciliation, *session)
                        .await?;
                    tracing::info!(
                        enrollment_id = %reconciliation.enrollment().into_uuid(),
                        registration_revision = reconciliation.registration_revision().get(),
                        session_id = %session.into_uuid(),
                        ?disposition,
                        "runner registration reconciled against session placement"
                    );
                }
            }
        }
        Ok(())
    }

    async fn propagate_pending_connection_losses(&self) -> Result<(), RunnerProtocolStoreError> {
        for loss in self.store.load_pending_connection_losses().await? {
            loop {
                let page = self
                    .store
                    .load_connection_loss_propagation_page(loss)
                    .await?;
                if page.is_complete() {
                    break;
                }
                if page.sessions().is_empty() {
                    self.store
                        .complete_connection_loss_propagation(loss)
                        .await?;
                    break;
                }
                for session in page.sessions() {
                    let disposition = self
                        .store
                        .propagate_connection_loss_session(loss, *session)
                        .await?;
                    tracing::info!(
                        enrollment_id = %loss.enrollment().into_uuid(),
                        loss_epoch = loss.loss_epoch().get(),
                        session_id = %session.into_uuid(),
                        ?disposition,
                        "runner connection loss propagated to session"
                    );
                }
            }
        }
        Ok(())
    }

    async fn enroll_durably(
        &self,
        request: Enroll,
    ) -> Result<RunnerEnrollmentAccepted, RunnerRegistrationFailure> {
        let _admission = self.registration_admission.lock().await;
        let correlation = AvailableCorrelation::Enrollment(request.request_id);
        if request.digest_version != DIGEST_VERSION {
            return Err(RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Enroll,
                correlation,
                RejectionCode::UnsupportedDigestVersion,
            ));
        }
        let digest = advertisement_digest(&request.advertisement).map_err(|_| {
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Enroll,
                correlation.clone(),
                RejectionCode::RegistrationRejected,
            )
        })?;
        let advertisement = request.advertisement.try_into_domain().map_err(|_| {
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Enroll,
                correlation.clone(),
                RejectionCode::RegistrationRejected,
            )
        })?;
        let issued = IssuedRunnerEnrollmentIdentities::new(
            RunnerEnrollmentId::from_uuid(uuid::Uuid::now_v7()),
            RunnerId::from_uuid(uuid::Uuid::now_v7()),
            RunnerAuthenticationId::from_uuid(uuid::Uuid::now_v7()),
        );
        let outcome = self
            .store
            .enroll_pristine(PristineRunnerEnrollmentRequest::new(
                RunnerEnrollmentRequestId::from_uuid(request.request_id.into_uuid()),
                issued,
                self.allowed_classes.iter().cloned(),
                advertisement,
            ))
            .await
            .map_err(|error| {
                store_failure(RunnerInboundFrameKind::Enroll, correlation.clone(), error)
            })?;
        let receipt = outcome.receipt();
        let identities = receipt.identities();
        tracing::info!(
            request_id = %receipt.request().into_uuid(),
            enrollment_id = %identities.enrollment().into_uuid(),
            runner_id = %identities.runner().into_uuid(),
            disposition = ?outcome.disposition(),
            "runner enrollment accepted"
        );
        match outcome.disposition() {
            RunnerEnrollmentDisposition::Created => tracing::info!(
                enrollment_id = %identities.enrollment().into_uuid(),
                runner_id = %identities.runner().into_uuid(),
                registration_revision = receipt.registration().revision().get(),
                "runner registration revision stored"
            ),
            RunnerEnrollmentDisposition::Replayed => {}
        }
        let connection = self
            .store
            .open_connection(identities.enrollment())
            .await
            .map_err(|error| {
                store_failure(RunnerInboundFrameKind::Enroll, correlation.clone(), error)
            })?;
        tracing::info!(
            enrollment_id = %identities.enrollment().into_uuid(),
            runner_id = %identities.runner().into_uuid(),
            connection_epoch = connection.epoch().get(),
            connection_cause = ?connection.cause(),
            "runner connection established"
        );
        let request_id = CanonicalUuid::from_uuid(receipt.request().into_uuid());
        let enrollment_id = CanonicalUuid::from_uuid(identities.enrollment().into_uuid());
        let runner_id = CanonicalUuid::from_uuid(identities.runner().into_uuid());
        let authentication_id = CanonicalUuid::from_uuid(identities.authentication().into_uuid());
        let registration_revision = positive_revision(receipt.registration().revision())?;
        let connection_epoch = positive_epoch(connection.epoch())?;
        Ok(match receipt.authority() {
            RunnerEnrollmentAuthority::Active => RunnerEnrollmentAccepted::Active(Enrolled {
                request_id,
                enrollment_id,
                runner_id,
                authentication_id,
                registration_revision,
                connection_epoch,
                advertisement_digest: digest,
            }),
            RunnerEnrollmentAuthority::ReplacementPending => {
                RunnerEnrollmentAccepted::ReplacementPending(ReplacementPending {
                    request_id,
                    enrollment_id,
                    runner_id,
                    authentication_id,
                    registration_revision,
                    connection_epoch,
                    advertisement_digest: digest,
                })
            }
        })
    }

    async fn resume_durably(
        &self,
        request: Resume,
    ) -> Result<RunnerResumeAccepted, RunnerRegistrationFailure> {
        let _admission = self.registration_admission.lock().await;
        let correlation = AvailableCorrelation::Enrollment(request.request_id);
        if request.digest_version != DIGEST_VERSION {
            return Err(RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Resume,
                correlation,
                RejectionCode::UnsupportedDigestVersion,
            ));
        }
        let resume_operation = classify_resume_inventory(&request.inventory).map_err(|code| {
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Resume,
                correlation.clone(),
                code,
            )
        })?;
        let prior =
            RunnerRegistrationRevision::try_from_u64(request.prior_registration_revision.get())
                .ok_or_else(|| {
                    RunnerRegistrationFailure::new(
                        RunnerInboundFrameKind::Resume,
                        correlation.clone(),
                        RejectionCode::RegistrationRejected,
                    )
                })?;
        let advertisement = request.advertisement.try_into_domain().map_err(|_| {
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Resume,
                correlation.clone(),
                RejectionCode::RegistrationRejected,
            )
        })?;
        let identities = IssuedRunnerEnrollmentIdentities::new(
            RunnerEnrollmentId::from_uuid(request.enrollment_id.into_uuid()),
            RunnerId::from_uuid(request.runner_id.into_uuid()),
            RunnerAuthenticationId::from_uuid(request.authentication_id.into_uuid()),
        );
        let (directives, claimed_correlation) = match resume_operation {
            ResumeOperation::RetainedResult(result) => {
                let result = RunnerDispatchWireAdapter::result_request(result).map_err(|_| {
                    RunnerRegistrationFailure::new(
                        RunnerInboundFrameKind::Resume,
                        correlation.clone(),
                        RejectionCode::CorrelationMismatch,
                    )
                })?;
                let action = match self
                    .store
                    .commit_retained_result_before_resume(
                        RunnerEnrollmentRequestId::from_uuid(request.request_id.into_uuid()),
                        identities,
                        prior,
                        advertisement.clone(),
                        result,
                    )
                    .await
                {
                    Ok(_) => DirectiveAction::DiscardAsRecorded,
                    Err(RunnerProtocolStoreError::Domain(
                        RunnerDomainError::InvalidState | RunnerDomainError::CorrelationMismatch,
                    )) => DirectiveAction::FailStale,
                    Err(error) => {
                        return Err(store_failure(
                            RunnerInboundFrameKind::Resume,
                            correlation.clone(),
                            error,
                        ));
                    }
                };
                (
                    retained_result_directives(&request.inventory, action).map_err(|code| {
                        RunnerRegistrationFailure::new(
                            RunnerInboundFrameKind::Resume,
                            correlation.clone(),
                            code,
                        )
                    })?,
                    None,
                )
            }
            ResumeOperation::ClaimedLease(wire_correlation) => {
                let claimed = RunnerDispatchWireAdapter::claim_request(LeaseClaim {
                    correlation: wire_correlation,
                })
                .map_err(|_| {
                    RunnerRegistrationFailure::new(
                        RunnerInboundFrameKind::Resume,
                        correlation.clone(),
                        RejectionCode::CorrelationMismatch,
                    )
                })?
                .into_correlation();
                let action = match self
                    .store
                    .load_claimed_lease_for_authenticated_resume(
                        RunnerEnrollmentRequestId::from_uuid(request.request_id.into_uuid()),
                        identities,
                        prior,
                        advertisement.clone(),
                        claimed.clone(),
                    )
                    .await
                {
                    Ok(_) => DirectiveAction::Await,
                    Err(RunnerProtocolStoreError::Domain(
                        RunnerDomainError::InvalidState | RunnerDomainError::CorrelationMismatch,
                    )) => DirectiveAction::FailStale,
                    Err(error) => {
                        return Err(store_failure(
                            RunnerInboundFrameKind::Resume,
                            correlation.clone(),
                            error,
                        ));
                    }
                };
                (
                    claimed_lease_directives(&request.inventory, action).map_err(|code| {
                        RunnerRegistrationFailure::new(
                            RunnerInboundFrameKind::Resume,
                            correlation.clone(),
                            code,
                        )
                    })?,
                    (action == DirectiveAction::Await).then_some(claimed),
                )
            }
            ResumeOperation::ReadyWorkspace(ready_correlation) => (
                ready_workspace_directives(
                    &request.inventory,
                    if ready_correlation.runner_id == request.runner_id
                        && ready_correlation.registration_revision.get() == prior.get()
                    {
                        DirectiveAction::Resend
                    } else {
                        DirectiveAction::FailStale
                    },
                )
                .map_err(|code| {
                    RunnerRegistrationFailure::new(
                        RunnerInboundFrameKind::Resume,
                        correlation.clone(),
                        code,
                    )
                })?,
                None,
            ),
            ResumeOperation::Empty => (ReconnectDirectives::default(), None),
        };
        let previous_registration_revision = match self
            .store
            .load_enrollment(identities.enrollment())
            .await
            .map_err(|error| {
                store_failure(RunnerInboundFrameKind::Resume, correlation.clone(), error)
            })? {
            Some(enrollment) => self
                .store
                .load_current_registration(&enrollment)
                .await
                .map_err(|error| {
                    store_failure(RunnerInboundFrameKind::Resume, correlation.clone(), error)
                })?
                .map(|registration| registration.revision()),
            None => None,
        };
        let receipt = self
            .store
            .resume_registration(
                RunnerEnrollmentRequestId::from_uuid(request.request_id.into_uuid()),
                identities,
                prior,
                advertisement,
            )
            .await
            .map_err(|error| {
                store_failure(RunnerInboundFrameKind::Resume, correlation.clone(), error)
            })?;
        self.propagate_pending_registration_reconciliations()
            .await
            .map_err(|error| {
                store_failure(RunnerInboundFrameKind::Resume, correlation.clone(), error)
            })?;
        if previous_registration_revision == Some(prior)
            && receipt.registration().revision() != prior
        {
            tracing::info!(
                enrollment_id = %receipt.enrollment().enrollment().into_uuid(),
                runner_id = %receipt.enrollment().runner().into_uuid(),
                registration_revision = receipt.registration().revision().get(),
                "runner registration revision stored"
            );
        }
        let connection = self
            .store
            .open_connection(receipt.enrollment().enrollment())
            .await
            .map_err(|error| {
                store_failure(RunnerInboundFrameKind::Resume, correlation.clone(), error)
            })?;
        tracing::info!(
            enrollment_id = %receipt.enrollment().enrollment().into_uuid(),
            runner_id = %receipt.enrollment().runner().into_uuid(),
            connection_epoch = connection.epoch().get(),
            connection_cause = ?connection.cause(),
            "runner connection established"
        );
        let response = Resumed {
            registration_revision: positive_revision(receipt.registration().revision())?,
            connection_epoch: positive_epoch(connection.epoch())?,
            directives,
        };
        let claimed_lease = match claimed_correlation {
            Some(claimed) => Some(
                self.store
                    .load_claimed_lease_for_authenticated_resume(
                        receipt.request(),
                        receipt.identities(),
                        receipt.registration().revision(),
                        receipt.advertisement(),
                        claimed,
                    )
                    .await
                    .map_err(|error| {
                        store_failure(RunnerInboundFrameKind::Resume, correlation, error)
                    })?,
            ),
            None => None,
        };
        Ok(RunnerResumeAccepted::new(response, claimed_lease))
    }

    async fn advertise_durably(
        &self,
        connection_enrollment: CanonicalUuid,
        request: Advertise,
        epoch: PositiveU64,
    ) -> Result<Registered, RunnerRegistrationFailure> {
        if request.enrollment_id != connection_enrollment {
            return Err(RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Advertise,
                AvailableCorrelation::Registration(request.registration_revision),
                RejectionCode::CorrelationMismatch,
            ));
        }
        let _admission = self.registration_admission.lock().await;
        let correlation = AvailableCorrelation::Registration(request.registration_revision);
        let observed_epoch = RunnerConnectionEpoch::try_from_u64(epoch.get()).ok_or_else(|| {
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Advertise,
                AvailableCorrelation::ConnectionEpoch(epoch),
                RejectionCode::CorrelationMismatch,
            )
        })?;
        let current = self
            .store
            .transition_connection(
                RunnerEnrollmentId::from_uuid(connection_enrollment.into_uuid()),
                observed_epoch,
                RunnerConnectionTransition::Observe,
            )
            .await
            .map_err(|error| {
                store_failure(
                    RunnerInboundFrameKind::Advertise,
                    AvailableCorrelation::ConnectionEpoch(epoch),
                    error,
                )
            })?;
        if let RunnerConnectionTransitionOutcome::Stale { .. } = current {
            return Err(RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Advertise,
                AvailableCorrelation::ConnectionEpoch(epoch),
                RejectionCode::StaleConnection,
            ));
        }
        let enrollment = self
            .store
            .load_enrollment(RunnerEnrollmentId::from_uuid(
                connection_enrollment.into_uuid(),
            ))
            .await
            .map_err(|error| {
                store_failure(
                    RunnerInboundFrameKind::Advertise,
                    correlation.clone(),
                    error,
                )
            })?
            .ok_or_else(|| {
                RunnerRegistrationFailure::new(
                    RunnerInboundFrameKind::Advertise,
                    correlation.clone(),
                    RejectionCode::CorrelationMismatch,
                )
            })?;
        if enrollment.runner().into_uuid() != request.runner_id.into_uuid()
            || enrollment.authentication().into_uuid() != request.authentication_id.into_uuid()
        {
            return Err(RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Advertise,
                correlation,
                RejectionCode::CorrelationMismatch,
            ));
        }
        let expected =
            RunnerRegistrationRevision::try_from_u64(request.registration_revision.get())
                .ok_or_else(|| {
                    RunnerRegistrationFailure::new(
                        RunnerInboundFrameKind::Advertise,
                        correlation.clone(),
                        RejectionCode::RegistrationRejected,
                    )
                })?;
        let digest = advertisement_digest(&request.advertisement).map_err(|_| {
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Advertise,
                correlation.clone(),
                RejectionCode::RegistrationRejected,
            )
        })?;
        let advertisement = request.advertisement.try_into_domain().map_err(|_| {
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Advertise,
                correlation.clone(),
                RejectionCode::RegistrationRejected,
            )
        })?;
        let registration = self
            .store
            .register_at_revision(&enrollment, expected, advertisement)
            .await
            .map_err(|error| {
                store_failure(RunnerInboundFrameKind::Advertise, correlation, error)
            })?;
        self.propagate_pending_registration_reconciliations()
            .await
            .map_err(|error| {
                store_failure(
                    RunnerInboundFrameKind::Advertise,
                    AvailableCorrelation::Registration(request.registration_revision),
                    error,
                )
            })?;
        tracing::info!(
            enrollment_id = %enrollment.enrollment().into_uuid(),
            runner_id = %enrollment.runner().into_uuid(),
            registration_revision = registration.revision().get(),
            "runner registration revision stored"
        );
        Ok(Registered {
            registration_revision: positive_revision(registration.revision())?,
            advertisement_digest: digest,
        })
    }

    async fn transition_connection_durably(
        &self,
        enrollment: CanonicalUuid,
        epoch: PositiveU64,
        transition: RunnerConnectionTransition,
    ) -> Result<RunnerConnectionTransitionOutcome, RunnerRegistrationFailure> {
        let wire_epoch = epoch;
        let operation_kind = transition_operation_kind(transition);
        let epoch = RunnerConnectionEpoch::try_from_u64(epoch.get()).ok_or_else(|| {
            RunnerRegistrationFailure::new(
                operation_kind,
                AvailableCorrelation::ConnectionEpoch(epoch),
                RejectionCode::CorrelationMismatch,
            )
        })?;
        let effect = self
            .store
            .transition_connection_with_effect(
                RunnerEnrollmentId::from_uuid(enrollment.into_uuid()),
                epoch,
                transition,
            )
            .await
            .map_err(|error| {
                store_failure(
                    operation_kind,
                    AvailableCorrelation::ConnectionEpoch(wire_epoch),
                    error,
                )
            })?;
        let outcome = match effect {
            RunnerConnectionTransitionEffect::Applied(applied) => {
                log_connection_transition(applied, transition);
                RunnerConnectionTransitionOutcome::Current(applied.snapshot())
            }
            RunnerConnectionTransitionEffect::Unchanged(outcome) => outcome,
        };
        if matches!(
            outcome,
            RunnerConnectionTransitionOutcome::Current(snapshot)
                if snapshot.state() == RunnerConnectionState::Lost
        ) {
            self.propagate_pending_connection_losses()
                .await
                .map_err(|error| {
                    store_failure(
                        operation_kind,
                        AvailableCorrelation::ConnectionEpoch(wire_epoch),
                        error,
                    )
                })?;
        }
        Ok(outcome)
    }
}

fn log_connection_transition(
    applied: AppliedRunnerConnectionTransition,
    transition: RunnerConnectionTransition,
) {
    let enrollment = applied.enrollment();
    let snapshot = applied.snapshot();
    match (transition, snapshot.cause()) {
        (RunnerConnectionTransition::DaemonShutdown, RunnerConnectionCause::DaemonShutdown)
        | (RunnerConnectionTransition::RunnerShutdown, RunnerConnectionCause::RunnerShutdown) => {
            tracing::info!(
                enrollment_id = %enrollment.into_uuid(),
                connection_epoch = snapshot.epoch().get(),
                shutdown_cause = ?snapshot.cause(),
                "runner shutdown recorded"
            )
        }
        (RunnerConnectionTransition::TransportClosed, RunnerConnectionCause::TransportClosed) => {
            tracing::info!(
                enrollment_id = %enrollment.into_uuid(),
                connection_epoch = snapshot.epoch().get(),
                loss_cause = ?snapshot.cause(),
                "runner transport loss recorded"
            )
        }
        (
            RunnerConnectionTransition::Observe
            | RunnerConnectionTransition::HeartbeatRecovered
            | RunnerConnectionTransition::HeartbeatMissed
            | RunnerConnectionTransition::HeartbeatTimeout
            | RunnerConnectionTransition::ProtocolFailure
            | RunnerConnectionTransition::DaemonShutdown
            | RunnerConnectionTransition::RunnerShutdown
            | RunnerConnectionTransition::TransportClosed,
            RunnerConnectionCause::Established
            | RunnerConnectionCause::HeartbeatRecovered
            | RunnerConnectionCause::HeartbeatMissed
            | RunnerConnectionCause::DaemonShutdown
            | RunnerConnectionCause::RunnerShutdown
            | RunnerConnectionCause::HeartbeatTimeout
            | RunnerConnectionCause::TransportClosed
            | RunnerConnectionCause::ProtocolFailure
            | RunnerConnectionCause::EnrollmentRevoked,
        ) => {}
    }
}

fn registration_only_catalog() -> Result<RunnerCatalog, RunnerDomainError> {
    let profile = CredentialProfileName::try_new(REGISTRATION_ONLY_CREDENTIAL_PROFILE.to_owned())?;
    let policy = CredentialProfilePolicy::try_new(profile, [])?;
    RunnerCatalog::try_new([], [], [policy], [], [])
}

impl RunnerRegistrationService for PostgresRunnerRegistrationService {
    fn enroll(&self, request: Enroll) -> RunnerRegistrationFuture<'_, RunnerEnrollmentAccepted> {
        Box::pin(self.enroll_durably(request))
    }

    fn resume(&self, request: Resume) -> RunnerRegistrationFuture<'_, RunnerResumeAccepted> {
        Box::pin(self.resume_durably(request))
    }

    fn advertise(
        &self,
        connection_enrollment: CanonicalUuid,
        request: Advertise,
        epoch: PositiveU64,
    ) -> RunnerRegistrationFuture<'_, Registered> {
        Box::pin(self.advertise_durably(connection_enrollment, request, epoch))
    }

    fn transition_connection(
        &self,
        enrollment: CanonicalUuid,
        epoch: PositiveU64,
        transition: RunnerConnectionTransition,
    ) -> RunnerRegistrationFuture<'_, RunnerConnectionTransitionOutcome> {
        Box::pin(self.transition_connection_durably(enrollment, epoch, transition))
    }
}

#[derive(Debug, PartialEq)]
enum ResumeOperation {
    Empty,
    ClaimedLease(LeaseCorrelation),
    RetainedResult(ResultFrame),
    ReadyWorkspace(signalbox_runner_wire::ProvisionCorrelation),
}

fn classify_resume_inventory(
    inventory: &ReconnectInventory,
) -> Result<ResumeOperation, RejectionCode> {
    if inventory == &ReconnectInventory::default() {
        return Ok(ResumeOperation::Empty);
    }
    if let Some(operation) = inventory.workspace_operation.as_ref() {
        if inventory.lease.is_some()
            || inventory.result.is_some()
            || inventory.operation_failure.is_some()
            || inventory.leak_page.is_some()
        {
            return Err(RejectionCode::CorrelationMismatch);
        }
        return match operation {
            WorkspaceOperation::Provision {
                correlation,
                phase: ProvisionPhase::ReadyUnrecorded,
            } => Ok(ResumeOperation::ReadyWorkspace(correlation.clone())),
            WorkspaceOperation::Provision {
                phase: ProvisionPhase::Provisioning,
                ..
            }
            | WorkspaceOperation::Release { .. } => Err(RejectionCode::Unavailable),
        };
    }
    if inventory.operation_failure.is_some() || inventory.leak_page.is_some() {
        return Err(RejectionCode::Unavailable);
    }
    let Some(lease) = inventory.lease.as_ref() else {
        return Err(RejectionCode::CorrelationMismatch);
    };
    let Some(result) = inventory.result.as_ref() else {
        return match lease.phase {
            LeasePhaseKind::WaitingDispatch | LeasePhaseKind::DispatchReceived => {
                Ok(ResumeOperation::ClaimedLease(lease.correlation.clone()))
            }
            LeasePhaseKind::ExecutionMayHaveStarted => Err(RejectionCode::Unavailable),
        };
    };
    if lease.phase != LeasePhaseKind::ExecutionMayHaveStarted
        || lease.correlation != result.correlation
    {
        return Err(RejectionCode::CorrelationMismatch);
    }
    Ok(ResumeOperation::RetainedResult(ResultFrame {
        correlation: result.correlation.clone(),
        result: result.result.clone(),
    }))
}

fn claimed_lease_directives(
    inventory: &ReconnectInventory,
    action: DirectiveAction,
) -> Result<ReconnectDirectives, RejectionCode> {
    let Some(lease) = inventory.lease.as_ref() else {
        return Err(RejectionCode::CorrelationMismatch);
    };
    let directives = ReconnectDirectives {
        lease: Some(Directive {
            correlation: lease.correlation.clone(),
            action,
        }),
        ..ReconnectDirectives::default()
    };
    directives
        .validate_against(inventory)
        .map_err(|_| RejectionCode::CorrelationMismatch)?;
    Ok(directives)
}

fn retained_result_directives(
    inventory: &ReconnectInventory,
    action: DirectiveAction,
) -> Result<ReconnectDirectives, RejectionCode> {
    let (Some(lease), Some(result)) = (inventory.lease.as_ref(), inventory.result.as_ref()) else {
        return Err(RejectionCode::CorrelationMismatch);
    };
    let directives = ReconnectDirectives {
        lease: Some(Directive {
            correlation: lease.correlation.clone(),
            action,
        }),
        result: Some(Directive {
            correlation: result.correlation.clone(),
            action,
        }),
        ..ReconnectDirectives::default()
    };
    directives
        .validate_against(inventory)
        .map_err(|_| RejectionCode::CorrelationMismatch)?;
    Ok(directives)
}

fn ready_workspace_directives(
    inventory: &ReconnectInventory,
    action: DirectiveAction,
) -> Result<ReconnectDirectives, RejectionCode> {
    let Some(WorkspaceOperation::Provision { correlation, .. }) =
        inventory.workspace_operation.as_ref()
    else {
        return Err(RejectionCode::CorrelationMismatch);
    };
    let directives = ReconnectDirectives {
        workspace_operation: Some(Directive {
            correlation: signalbox_runner_wire::OperationCorrelation::Provision(
                correlation.clone(),
            ),
            action,
        }),
        ..ReconnectDirectives::default()
    };
    directives
        .validate_against(inventory)
        .map_err(|_| RejectionCode::CorrelationMismatch)?;
    Ok(directives)
}

impl RunnerLeaseOperationService for RunnerProtocolStore {
    fn claim(
        &self,
        request: RunnerLeaseClaimRequest,
    ) -> RunnerLeaseOperationFuture<'_, RunnerLease> {
        let store = self.clone();
        Box::pin(async move {
            RunnerLeaseClaimService::new(store)
                .execute(request)
                .await
                .map_err(runner_lease_operation_failure)
        })
    }

    fn record_result(
        &self,
        request: RunnerLeaseResultRequest,
    ) -> RunnerLeaseOperationFuture<'_, RunnerLease> {
        let store = self.clone();
        Box::pin(async move {
            RunnerLeaseResultService::new(store)
                .execute(request)
                .await
                .map(|completion| completion.into_parts().0)
                .map_err(runner_lease_operation_failure)
        })
    }
}

impl RunnerWorkspaceReadyOperationService for RunnerProtocolStore {
    fn record_workspace_ready(
        &self,
        receipt: RunnerWorkspaceReadyReceipt,
    ) -> RunnerWorkspaceReadyOperationFuture<'_, RunnerWorkspaceReadyReceipt> {
        let store = self.clone();
        Box::pin(async move {
            RunnerWorkspaceReadyService::new(store)
                .execute(receipt)
                .await
                .map_err(runner_workspace_ready_operation_failure)
        })
    }
}

fn positive_epoch(epoch: RunnerConnectionEpoch) -> Result<PositiveU64, RunnerRegistrationFailure> {
    PositiveU64::try_new(epoch.get()).map_err(|_| {
        RunnerRegistrationFailure::new(
            RunnerInboundFrameKind::Registration,
            AvailableCorrelation::None,
            RejectionCode::Unavailable,
        )
    })
}

fn positive_revision(
    revision: RunnerRegistrationRevision,
) -> Result<PositiveU64, RunnerRegistrationFailure> {
    PositiveU64::try_new(revision.get()).map_err(|_| {
        RunnerRegistrationFailure::new(
            RunnerInboundFrameKind::Registration,
            AvailableCorrelation::None,
            RejectionCode::Unavailable,
        )
    })
}

fn store_failure(
    offending_kind: RunnerInboundFrameKind,
    correlation: AvailableCorrelation,
    error: RunnerProtocolStoreError,
) -> RunnerRegistrationFailure {
    let failure = runner_store_failure_classification(&error);
    if failure.cause.operator_actionable() {
        tracing::error!(
            error = %error,
            frame_kind = offending_kind.as_str(),
            cause = ?failure.cause,
            "durable runner registration failed"
        );
    }
    RunnerRegistrationFailure::from_durable_cause(
        offending_kind,
        correlation,
        failure.code,
        failure.cause,
    )
}

fn runner_lease_operation_failure(error: RunnerProtocolStoreError) -> RunnerLeaseOperationFailure {
    let failure = runner_store_failure_classification(&error);
    if failure.cause.operator_actionable() {
        tracing::error!(
            error = %error,
            cause = ?failure.cause,
            "durable runner lease operation failed"
        );
    }
    failure
}

fn runner_workspace_ready_operation_failure(
    error: RunnerProtocolStoreError,
) -> RunnerWorkspaceReadyOperationFailure {
    let failure = runner_store_failure_classification(&error);
    if failure.cause.operator_actionable() {
        tracing::error!(
            error = %error,
            cause = ?failure.cause,
            "durable runner workspace-ready operation failed"
        );
    }
    RunnerWorkspaceReadyOperationFailure::new(failure.code, failure.cause)
}

fn runner_store_failure_classification(
    error: &RunnerProtocolStoreError,
) -> RunnerLeaseOperationFailure {
    let (code, cause) = match error {
        RunnerProtocolStoreError::EnrollmentRequest(
            RunnerEnrollmentRequestFailure::ActiveEnrollmentExists { .. }
            | RunnerEnrollmentRequestFailure::PendingEnrollmentExists { .. }
            | RunnerEnrollmentRequestFailure::ReplayAdvertisementMismatch { .. }
            | RunnerEnrollmentRequestFailure::ReplayPolicyMismatch { .. },
        ) => (
            RejectionCode::EnrollmentConflict,
            RunnerRegistrationFailureCause::EnrollmentAuthority,
        ),
        RunnerProtocolStoreError::EnrollmentRequest(
            RunnerEnrollmentRequestFailure::EnrollmentRevoked { .. },
        )
        | RunnerProtocolStoreError::Domain(RunnerDomainError::EnrollmentRevoked) => (
            RejectionCode::EnrollmentRevoked,
            RunnerRegistrationFailureCause::EnrollmentAuthority,
        ),
        RunnerProtocolStoreError::EnrollmentRequest(
            RunnerEnrollmentRequestFailure::ResumeRevisionMismatch { .. }
            | RunnerEnrollmentRequestFailure::StaleResumeAdvertisement { .. },
        )
        | RunnerProtocolStoreError::Domain(RunnerDomainError::RegistrationChanged) => (
            RejectionCode::StaleConnection,
            RunnerRegistrationFailureCause::EnrollmentAuthority,
        ),
        RunnerProtocolStoreError::EnrollmentRequest(
            RunnerEnrollmentRequestFailure::UnknownRequest { .. }
            | RunnerEnrollmentRequestFailure::ResumeIdentityMismatch { .. },
        )
        | RunnerProtocolStoreError::Domain(RunnerDomainError::CorrelationMismatch) => (
            RejectionCode::CorrelationMismatch,
            RunnerRegistrationFailureCause::EnrollmentAuthority,
        ),
        RunnerProtocolStoreError::Domain(_) => (
            RejectionCode::PolicyRejected,
            RunnerRegistrationFailureCause::Policy,
        ),
        RunnerProtocolStoreError::Database(_) => (
            RejectionCode::Unavailable,
            RunnerRegistrationFailureCause::Database,
        ),
        RunnerProtocolStoreError::CommitAmbiguous(_) => (
            RejectionCode::Unavailable,
            RunnerRegistrationFailureCause::CommitAmbiguous,
        ),
        RunnerProtocolStoreError::Corruption(_) => (
            RejectionCode::Unavailable,
            RunnerRegistrationFailureCause::Corruption,
        ),
    };
    RunnerLeaseOperationFailure::new(code, cause)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunnerInboundFrameKind {
    Enroll,
    Enrolled,
    Resume,
    Resumed,
    ReplacementPending,
    Advertise,
    Registered,
    Heartbeat,
    HeartbeatAck,
    WorkspaceLeakPage,
    WorkspaceLeakRecorded,
    WorkspaceProvision,
    WorkspaceReady,
    WorkspaceRecorded,
    WorkspaceRelease,
    WorkspaceReleased,
    WorkspaceReleaseRecorded,
    LeaseOffer,
    LeaseClaim,
    LeaseClaimed,
    Dispatch,
    Result,
    ResultRecorded,
    OperationFailed,
    OperationFailureRecorded,
    Shutdown,
    Rejected,
    Registration,
    ConnectionObserve,
    HeartbeatMissed,
    HeartbeatRecovered,
    HeartbeatTimeout,
    TransportClosed,
    ProtocolFailure,
    DaemonShutdown,
    RunnerShutdown,
}

impl RunnerInboundFrameKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Enroll => "enroll",
            Self::Enrolled => "enrolled",
            Self::Resume => "resume",
            Self::Resumed => "resumed",
            Self::ReplacementPending => "replacement_pending",
            Self::Advertise => "advertise",
            Self::Registered => "registered",
            Self::Heartbeat => "heartbeat",
            Self::HeartbeatAck => "heartbeat_ack",
            Self::WorkspaceLeakPage => "workspace_leak_page",
            Self::WorkspaceLeakRecorded => "workspace_leak_recorded",
            Self::WorkspaceProvision => "workspace_provision",
            Self::WorkspaceReady => "workspace_ready",
            Self::WorkspaceRecorded => "workspace_recorded",
            Self::WorkspaceRelease => "workspace_release",
            Self::WorkspaceReleased => "workspace_released",
            Self::WorkspaceReleaseRecorded => "workspace_release_recorded",
            Self::LeaseOffer => "lease_offer",
            Self::LeaseClaim => "lease_claim",
            Self::LeaseClaimed => "lease_claimed",
            Self::Dispatch => "dispatch",
            Self::Result => "result",
            Self::ResultRecorded => "result_recorded",
            Self::OperationFailed => "operation_failed",
            Self::OperationFailureRecorded => "operation_failure_recorded",
            Self::Shutdown => "shutdown",
            Self::Rejected => "rejected",
            Self::Registration => "registration",
            Self::ConnectionObserve => "connection_observe",
            Self::HeartbeatMissed => "heartbeat_missed",
            Self::HeartbeatRecovered => "heartbeat_recovered",
            Self::HeartbeatTimeout => "heartbeat_timeout",
            Self::TransportClosed => "transport_closed",
            Self::ProtocolFailure => "protocol_failure",
            Self::DaemonShutdown => "daemon_shutdown",
            Self::RunnerShutdown => "runner_shutdown",
        }
    }
}

const fn transition_operation_kind(
    transition: RunnerConnectionTransition,
) -> RunnerInboundFrameKind {
    match transition {
        RunnerConnectionTransition::Observe => RunnerInboundFrameKind::ConnectionObserve,
        RunnerConnectionTransition::HeartbeatRecovered => {
            RunnerInboundFrameKind::HeartbeatRecovered
        }
        RunnerConnectionTransition::HeartbeatMissed => RunnerInboundFrameKind::HeartbeatMissed,
        RunnerConnectionTransition::DaemonShutdown => RunnerInboundFrameKind::DaemonShutdown,
        RunnerConnectionTransition::RunnerShutdown => RunnerInboundFrameKind::RunnerShutdown,
        RunnerConnectionTransition::HeartbeatTimeout => RunnerInboundFrameKind::HeartbeatTimeout,
        RunnerConnectionTransition::TransportClosed => RunnerInboundFrameKind::TransportClosed,
        RunnerConnectionTransition::ProtocolFailure => RunnerInboundFrameKind::ProtocolFailure,
    }
}

/// Closed provenance for one registration rejection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunnerRegistrationFailureCause {
    PeerInput,
    EnrollmentAuthority,
    Policy,
    Database,
    CommitAmbiguous,
    Corruption,
}

impl RunnerRegistrationFailureCause {
    const fn operator_actionable(self) -> bool {
        matches!(
            self,
            Self::Database | Self::CommitAmbiguous | Self::Corruption
        )
    }
}

/// Evidence-bearing rejection returned by durable registration admission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerRegistrationFailure {
    offending_kind: RunnerInboundFrameKind,
    available_correlation: Box<AvailableCorrelation>,
    code: RejectionCode,
    cause: RunnerRegistrationFailureCause,
}

impl RunnerRegistrationFailure {
    /// Constructs a peer-input rejection for an injected registration service.
    fn new(
        offending_kind: RunnerInboundFrameKind,
        available_correlation: AvailableCorrelation,
        code: RejectionCode,
    ) -> Self {
        Self {
            offending_kind,
            available_correlation: Box::new(available_correlation),
            code,
            cause: RunnerRegistrationFailureCause::PeerInput,
        }
    }

    /// Constructs a peer-input rejection for an injected enrollment service.
    pub fn enroll(available_correlation: AvailableCorrelation, code: RejectionCode) -> Self {
        Self::new(RunnerInboundFrameKind::Enroll, available_correlation, code)
    }

    /// Constructs a peer-input rejection for an injected resume service.
    pub fn resume(available_correlation: AvailableCorrelation, code: RejectionCode) -> Self {
        Self::new(RunnerInboundFrameKind::Resume, available_correlation, code)
    }

    /// Constructs a peer-input rejection for an injected advertisement service.
    pub fn advertise(available_correlation: AvailableCorrelation, code: RejectionCode) -> Self {
        Self::new(
            RunnerInboundFrameKind::Advertise,
            available_correlation,
            code,
        )
    }

    fn from_durable_cause(
        offending_kind: RunnerInboundFrameKind,
        available_correlation: AvailableCorrelation,
        code: RejectionCode,
        cause: RunnerRegistrationFailureCause,
    ) -> Self {
        Self {
            offending_kind,
            available_correlation: Box::new(available_correlation),
            code,
            cause,
        }
    }

    /// Returns the closed provenance of this failure.
    pub const fn cause(&self) -> RunnerRegistrationFailureCause {
        self.cause
    }

    fn into_rejected(self) -> Rejected {
        Rejected {
            offending_kind: self.offending_kind.as_str().to_owned(),
            available_correlation: *self.available_correlation,
            code: self.code,
        }
    }
}

impl fmt::Display for RunnerRegistrationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "runner frame {} was rejected with {:?}",
            self.offending_kind.as_str(),
            self.code
        )
    }
}

impl Error for RunnerRegistrationFailure {}

/// Dedicated local runner listener and its durable registration service.
#[derive(Debug)]
pub struct RunnerProtocolRuntime<
    S,
    O = UnavailableRunnerLeaseOperationService,
    W = UnavailableRunnerWorkspaceReadyOperationService,
> {
    listener: Option<LocalProcessListener>,
    service: Option<S>,
    operations: Option<O>,
    workspace_ready: Option<W>,
    broker: RunnerConnectionBroker,
}

struct RunnerListenerGuard {
    listener: Option<LocalProcessListener>,
}

impl RunnerListenerGuard {
    const fn new(listener: LocalProcessListener) -> Self {
        Self {
            listener: Some(listener),
        }
    }

    fn listener(&self) -> Result<&LocalProcessListener, RunnerProtocolRuntimeError> {
        self.listener
            .as_ref()
            .ok_or(RunnerProtocolRuntimeError::OwnershipUnavailable)
    }

    fn cleanup(mut self) -> Result<(), RunnerProtocolRuntimeError> {
        self.listener
            .take()
            .ok_or(RunnerProtocolRuntimeError::OwnershipUnavailable)?
            .cleanup()
            .map_err(RunnerProtocolRuntimeError::Cleanup)
    }
}

impl Drop for RunnerListenerGuard {
    fn drop(&mut self) {
        if let Some(listener) = self.listener.take()
            && let Err(error) = listener.cleanup()
        {
            tracing::error!(
                error = %error,
                "cancelled runner runtime listener cleanup failed"
            );
        }
    }
}

impl<S, O, W> Drop for RunnerProtocolRuntime<S, O, W> {
    fn drop(&mut self) {
        if let Some(listener) = self.listener.take()
            && let Err(error) = listener.cleanup()
        {
            tracing::error!(
                error = %error,
                "unpolled runner runtime listener cleanup failed"
            );
        }
    }
}

impl<S> RunnerProtocolRuntime<S, UnavailableRunnerLeaseOperationService>
where
    S: RunnerRegistrationService,
{
    /// Composes the dedicated guarded listener with durable runner authority.
    pub fn new(listener: LocalProcessListener, service: S) -> Self {
        Self {
            listener: Some(listener),
            service: Some(service),
            operations: Some(UnavailableRunnerLeaseOperationService),
            workspace_ready: Some(UnavailableRunnerWorkspaceReadyOperationService),
            broker: RunnerConnectionBroker::new(),
        }
    }
}

impl<S, O, W> RunnerProtocolRuntime<S, O, W>
where
    S: RunnerRegistrationService,
    O: RunnerLeaseOperationService,
    W: RunnerWorkspaceReadyOperationService,
{
    /// Installs the durable claim and result boundary used by established connections.
    pub fn with_lease_operation_service<Replacement>(
        mut self,
        operations: Replacement,
    ) -> RunnerProtocolRuntime<S, Replacement, W>
    where
        Replacement: RunnerLeaseOperationService,
    {
        RunnerProtocolRuntime {
            listener: self.listener.take(),
            service: self.service.take(),
            operations: Some(operations),
            workspace_ready: self.workspace_ready.take(),
            broker: self.broker.clone(),
        }
    }

    /// Installs the durable workspace-ready boundary used by established connections.
    pub fn with_workspace_ready_operation_service<Replacement>(
        mut self,
        workspace_ready: Replacement,
    ) -> RunnerProtocolRuntime<S, O, Replacement>
    where
        Replacement: RunnerWorkspaceReadyOperationService,
    {
        RunnerProtocolRuntime {
            listener: self.listener.take(),
            service: self.service.take(),
            operations: self.operations.take(),
            workspace_ready: Some(workspace_ready),
            broker: self.broker.clone(),
        }
    }

    /// Replaces the default empty broker with one shared by operation producers.
    pub fn with_connection_broker(mut self, broker: RunnerConnectionBroker) -> Self {
        self.broker = broker;
        self
    }

    /// Accepts runner connections until shutdown; each connection is serial.
    pub async fn run(
        mut self,
        shutdown: watch::Receiver<bool>,
    ) -> Result<(), RunnerProtocolRuntimeError> {
        let listener = self
            .listener
            .take()
            .ok_or(RunnerProtocolRuntimeError::OwnershipUnavailable)?;
        let service = self
            .service
            .take()
            .ok_or(RunnerProtocolRuntimeError::OwnershipUnavailable)?;
        let operations = self
            .operations
            .take()
            .ok_or(RunnerProtocolRuntimeError::OwnershipUnavailable)?;
        let workspace_ready = self
            .workspace_ready
            .take()
            .ok_or(RunnerProtocolRuntimeError::OwnershipUnavailable)?;
        let listener = RunnerListenerGuard::new(listener);
        let outcome = run_connections(
            listener.listener()?,
            service,
            operations,
            workspace_ready,
            self.broker.clone(),
            shutdown,
        )
        .await;
        let cleanup = listener.cleanup();
        match (outcome, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) => Err(error),
            (Ok(()), Err(error)) => Err(error),
            (Err(error), Err(cleanup_error)) => {
                tracing::error!(
                    error = %cleanup_error,
                    "runner listener cleanup also failed after a runtime failure"
                );
                Err(error)
            }
        }
    }
}

async fn run_connections<S, O, W>(
    listener: &LocalProcessListener,
    service: S,
    operations: O,
    workspace_ready: W,
    broker: RunnerConnectionBroker,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), RunnerProtocolRuntimeError>
where
    S: RunnerRegistrationService,
    O: RunnerLeaseOperationService,
    W: RunnerWorkspaceReadyOperationService,
{
    let mut connections = JoinSet::new();
    let (connection_shutdown_sender, connection_shutdown) = watch::channel(*shutdown.borrow());
    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    let _ = connection_shutdown_sender.send(true);
                    return drain_connection_tasks(&mut connections, None).await;
                }
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                match completed {
                    Some(Ok(Ok(()))) | None => {}
                    Some(Ok(Err(error @ RunnerProtocolRuntimeError::Lifecycle(_)))) => {
                        let _ = connection_shutdown_sender.send(true);
                        return drain_connection_tasks(&mut connections, Some(error)).await;
                    }
                    Some(Ok(Err(error))) => {
                        tracing::warn!(
                            error = %error,
                            "runner connection closed after a typed protocol failure"
                        );
                    }
                    Some(Err(error)) => {
                        let _ = connection_shutdown_sender.send(true);
                        return drain_connection_tasks(
                            &mut connections,
                            Some(RunnerProtocolRuntimeError::ConnectionTask(error)),
                        )
                        .await;
                    }
                }
            }
            accepted = listener.accept(), if connections.len() < MAXIMUM_CONCURRENT_CONNECTIONS => {
                let Some(stream) = accepted_stream_or_drain(
                    accepted.map(|(stream, _)| stream),
                    &connection_shutdown_sender,
                    &mut connections,
                ).await? else {
                    return Ok(());
                };
                if let Err(error) = verify_runner_peer(&stream) {
                    tracing::warn!(error = %error, "runner peer failed same-user admission");
                    continue;
                }
                connections.spawn(serve_connection_with_operations_and_broker(
                    stream,
                    service.clone(),
                    operations.clone(),
                    workspace_ready.clone(),
                    broker.clone(),
                    connection_shutdown.clone(),
                ));
            }
        }
    }
}

async fn accepted_stream_or_drain(
    accepted: io::Result<UnixStream>,
    shutdown: &watch::Sender<bool>,
    connections: &mut JoinSet<Result<(), RunnerProtocolRuntimeError>>,
) -> Result<Option<UnixStream>, RunnerProtocolRuntimeError> {
    match accepted {
        Ok(stream) => Ok(Some(stream)),
        Err(error) => {
            let _ = shutdown.send(true);
            drain_connection_tasks(connections, Some(RunnerProtocolRuntimeError::Accept(error)))
                .await?;
            Ok(None)
        }
    }
}

async fn drain_connection_tasks(
    connections: &mut JoinSet<Result<(), RunnerProtocolRuntimeError>>,
    failure: Option<RunnerProtocolRuntimeError>,
) -> Result<(), RunnerProtocolRuntimeError> {
    drain_connection_tasks_with_timeout(connections, failure, CONNECTION_DRAIN_TIMEOUT).await
}

async fn drain_connection_tasks_with_timeout(
    connections: &mut JoinSet<Result<(), RunnerProtocolRuntimeError>>,
    mut failure: Option<RunnerProtocolRuntimeError>,
    drain_timeout: Duration,
) -> Result<(), RunnerProtocolRuntimeError> {
    match timeout(
        drain_timeout,
        drain_connection_tasks_to_completion(connections, &mut failure),
    )
    .await
    {
        Ok(()) => match failure {
            Some(error) => Err(error),
            None => Ok(()),
        },
        Err(_) => {
            let remaining = connections.len();
            connections.abort_all();
            while connections.join_next().await.is_some() {}
            Err(RunnerProtocolRuntimeError::ConnectionDrainTimeout {
                remaining,
                initiating: failure.map(Box::new),
            })
        }
    }
}

async fn drain_connection_tasks_to_completion(
    connections: &mut JoinSet<Result<(), RunnerProtocolRuntimeError>>,
    failure: &mut Option<RunnerProtocolRuntimeError>,
) {
    while let Some(completed) = connections.join_next().await {
        let error = match completed {
            Ok(Ok(())) => continue,
            Ok(Err(error)) => error,
            Err(error) => RunnerProtocolRuntimeError::ConnectionTask(error),
        };
        if failure.is_none()
            && matches!(
                error,
                RunnerProtocolRuntimeError::Lifecycle(_)
                    | RunnerProtocolRuntimeError::ConnectionTask(_)
            )
        {
            *failure = Some(error);
        } else {
            tracing::warn!(
                error = %error,
                "runner connection closed while draining peer tasks"
            );
        }
    }
}

#[derive(Debug)]
enum RunnerPeerIdentityError {
    Inspect(io::Error),
    OwnerMismatch { expected: u32, observed: u32 },
}

impl fmt::Display for RunnerPeerIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inspect(_) => formatter.write_str("runner peer identity inspection failed"),
            Self::OwnerMismatch { expected, observed } => write!(
                formatter,
                "runner peer user {observed} differs from effective user {expected}"
            ),
        }
    }
}

impl Error for RunnerPeerIdentityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Inspect(error) => Some(error),
            Self::OwnerMismatch { .. } => None,
        }
    }
}

fn verify_runner_peer(stream: &UnixStream) -> Result<(), RunnerPeerIdentityError> {
    let expected = geteuid().as_raw();
    let observed = stream
        .peer_cred()
        .map_err(RunnerPeerIdentityError::Inspect)?
        .uid();
    if observed == expected {
        Ok(())
    } else {
        Err(RunnerPeerIdentityError::OwnerMismatch { expected, observed })
    }
}

#[cfg(test)]
async fn serve_connection<S>(
    stream: UnixStream,
    service: S,
    shutdown: watch::Receiver<bool>,
) -> Result<(), RunnerProtocolRuntimeError>
where
    S: RunnerRegistrationService,
{
    serve_connection_with_operations_and_broker(
        stream,
        service,
        UnavailableRunnerLeaseOperationService,
        UnavailableRunnerWorkspaceReadyOperationService,
        RunnerConnectionBroker::new(),
        shutdown,
    )
    .await
}

#[cfg(test)]
async fn serve_connection_with_broker<S>(
    stream: UnixStream,
    service: S,
    broker: RunnerConnectionBroker,
    shutdown: watch::Receiver<bool>,
) -> Result<(), RunnerProtocolRuntimeError>
where
    S: RunnerRegistrationService,
{
    serve_connection_with_handshake_timeout_operations_and_broker(
        stream,
        service,
        UnavailableRunnerLeaseOperationService,
        UnavailableRunnerWorkspaceReadyOperationService,
        broker,
        shutdown,
        HANDSHAKE_TIMEOUT,
    )
    .await
}

#[cfg(test)]
async fn serve_connection_with_handshake_timeout<S>(
    stream: UnixStream,
    service: S,
    shutdown: watch::Receiver<bool>,
    handshake_timeout: Duration,
) -> Result<(), RunnerProtocolRuntimeError>
where
    S: RunnerRegistrationService,
{
    serve_connection_with_handshake_timeout_operations_and_broker(
        stream,
        service,
        UnavailableRunnerLeaseOperationService,
        UnavailableRunnerWorkspaceReadyOperationService,
        RunnerConnectionBroker::new(),
        shutdown,
        handshake_timeout,
    )
    .await
}

async fn serve_connection_with_operations_and_broker<S, O, W>(
    stream: UnixStream,
    service: S,
    operations: O,
    workspace_ready: W,
    broker: RunnerConnectionBroker,
    shutdown: watch::Receiver<bool>,
) -> Result<(), RunnerProtocolRuntimeError>
where
    S: RunnerRegistrationService,
    O: RunnerLeaseOperationService,
    W: RunnerWorkspaceReadyOperationService,
{
    serve_connection_with_handshake_timeout_operations_and_broker(
        stream,
        service,
        operations,
        workspace_ready,
        broker,
        shutdown,
        HANDSHAKE_TIMEOUT,
    )
    .await
}

async fn serve_connection_with_handshake_timeout_operations_and_broker<S, O, W>(
    stream: UnixStream,
    service: S,
    operations: O,
    workspace_ready: W,
    broker: RunnerConnectionBroker,
    mut shutdown: watch::Receiver<bool>,
    handshake_timeout: Duration,
) -> Result<(), RunnerProtocolRuntimeError>
where
    S: RunnerRegistrationService,
    O: RunnerLeaseOperationService,
    W: RunnerWorkspaceReadyOperationService,
{
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let first = match timeout(handshake_timeout, async {
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(None);
                    }
                }
                frame = read_frame(&mut reader) => break frame.map(Some),
            }
        }
    })
    .await
    {
        Ok(Ok(Some(frame))) => frame,
        Ok(Ok(None)) => return Ok(()),
        Ok(Err(RunnerProtocolRuntimeError::Decode(error))) => {
            write_decode_rejected(&mut writer, &error).await?;
            return Ok(());
        }
        Ok(Err(error)) => return Err(error),
        Err(_) => return Err(RunnerProtocolRuntimeError::HandshakeTimeout),
    };
    let context = match first.message {
        Message::Enroll(request) => match service.enroll(request).await {
            Ok(response) => {
                let context = ConnectionContext {
                    enrollment: response.enrollment_id(),
                    runner: response.runner_id(),
                    registration_revision: response.registration_revision(),
                    epoch: response.connection_epoch(),
                };
                let attachment = broker
                    .attach(connection_address(context)?)
                    .map_err(RunnerProtocolRuntimeError::Broker)?;
                if let Err(error) = write_message(&mut writer, response.into_message()).await {
                    transition_is_current(
                        &service,
                        context,
                        RunnerConnectionTransition::TransportClosed,
                    )
                    .await?;
                    return Err(error);
                }
                (context, attachment)
            }
            Err(failure) => {
                write_rejected(&mut writer, failure).await?;
                return Ok(());
            }
        },
        Message::Resume(request) => {
            let enrollment = request.enrollment_id;
            let runner = request.runner_id;
            match service.resume(*request).await {
                Ok(accepted) => {
                    let context = ConnectionContext {
                        enrollment,
                        runner,
                        registration_revision: accepted.registration_revision(),
                        epoch: accepted.connection_epoch(),
                    };
                    let attachment = broker
                        .attach(connection_address(context)?)
                        .map_err(RunnerProtocolRuntimeError::Broker)?;
                    let (response, claimed_lease) = accepted.into_parts();
                    if let Err(error) =
                        write_message(&mut writer, Message::Resumed(Box::new(response))).await
                    {
                        transition_is_current(
                            &service,
                            context,
                            RunnerConnectionTransition::TransportClosed,
                        )
                        .await?;
                        return Err(error);
                    }
                    if let Some(claimed_lease) = claimed_lease {
                        let [claim_acknowledgement, dispatch] =
                            claimed_resume_messages(&claimed_lease)?;
                        if !transition_is_current(
                            &service,
                            context,
                            RunnerConnectionTransition::Observe,
                        )
                        .await?
                        {
                            return Ok(());
                        }
                        write_message(&mut writer, claim_acknowledgement).await?;
                        if !transition_is_current(
                            &service,
                            context,
                            RunnerConnectionTransition::Observe,
                        )
                        .await?
                        {
                            return Ok(());
                        }
                        write_message(&mut writer, dispatch).await?;
                    }
                    (context, attachment)
                }
                Err(failure) => {
                    write_rejected(&mut writer, failure).await?;
                    return Ok(());
                }
            }
        }
        message => {
            write_rejected(
                &mut writer,
                RunnerRegistrationFailure::new(
                    inbound_frame_kind(&message),
                    available_correlation(&message),
                    RejectionCode::CorrelationMismatch,
                ),
            )
            .await?;
            return Ok(());
        }
    };

    let (mut context, mut attachment) = context;

    let outcome = async {
        let mut heartbeat = interval(HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        heartbeat.tick().await;
        let mut heartbeat_state = HeartbeatState::new();
        loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    if transition_or_reject_not_current(
                        &service,
                        context,
                        &mut writer,
                        RunnerInboundFrameKind::Shutdown,
                        context.epoch,
                        RunnerConnectionTransition::DaemonShutdown,
                    ).await? {
                        write_message(&mut writer, Message::Shutdown(Shutdown {
                            connection_epoch: context.epoch,
                            reason: ShutdownReason::DaemonShutdown,
                        })).await?;
                    }
                    return Ok(());
                }
            }
            frame = read_frame(&mut reader) => {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(RunnerProtocolRuntimeError::Closed) => {
                        transition_is_current(
                            &service,
                            context,
                            RunnerConnectionTransition::TransportClosed,
                        ).await?;
                        return Ok(());
                    }
                    Err(RunnerProtocolRuntimeError::Decode(error)) => {
                        if !transition_or_reject_not_current(
                            &service,
                            context,
                            &mut writer,
                            RunnerInboundFrameKind::Registration,
                            context.epoch,
                            RunnerConnectionTransition::ProtocolFailure,
                        ).await? {
                            return Ok(());
                        }
                        write_decode_rejected(&mut writer, &error).await?;
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                };
                match frame.message {
                    Message::Advertise(request) => {
                        if !transition_or_reject_not_current(
                            &service,
                            context,
                            &mut writer,
                            RunnerInboundFrameKind::Advertise,
                            context.epoch,
                            RunnerConnectionTransition::Observe,
                        ).await? {
                            return Ok(());
                        }
                        match service
                            .advertise(context.enrollment, request, context.epoch)
                            .await
                        {
                            Ok(response) => {
                                context.registration_revision = response.registration_revision;
                                write_message(&mut writer, Message::Registered(response)).await?;
                            }
                            Err(failure) => {
                                let transition = rejection_terminal_transition(failure.cause());
                                if transition_or_reject_not_current(
                                    &service,
                                    context,
                                    &mut writer,
                                    RunnerInboundFrameKind::Advertise,
                                    context.epoch,
                                    transition,
                                ).await? {
                                    write_rejected(&mut writer, failure).await?;
                                }
                                return Ok(());
                            }
                        }
                    }
                    Message::HeartbeatAck(acknowledgement) => {
                        if let Err(failure) = heartbeat_state.accept(&acknowledgement) {
                            terminalize_protocol_rejection(
                                &service,
                                context,
                                &mut writer,
                                RunnerInboundFrameKind::HeartbeatAck,
                                context.epoch,
                                failure,
                            )
                            .await?;
                            return Ok(());
                        }
                        if !transition_or_reject_not_current(
                            &service,
                            context,
                            &mut writer,
                            RunnerInboundFrameKind::HeartbeatAck,
                            context.epoch,
                            RunnerConnectionTransition::HeartbeatRecovered,
                        ).await? {
                            return Ok(());
                        }
                    }
                    Message::WorkspaceReady(ready) => {
                        if !workspace_ready_matches_connection(&ready, context) {
                            terminalize_protocol_rejection(
                                &service,
                                context,
                                &mut writer,
                                RunnerInboundFrameKind::WorkspaceReady,
                                context.epoch,
                                RunnerRegistrationFailure::new(
                                    RunnerInboundFrameKind::WorkspaceReady,
                                    AvailableCorrelation::Provision(ready.correlation),
                                    RejectionCode::CorrelationMismatch,
                                ),
                            )
                            .await?;
                            return Ok(());
                        }
                        if !transition_or_reject_not_current(
                            &service,
                            context,
                            &mut writer,
                            RunnerInboundFrameKind::WorkspaceReady,
                            context.epoch,
                            RunnerConnectionTransition::Observe,
                        )
                        .await?
                        {
                            return Ok(());
                        }
                        match admit_workspace_ready(&workspace_ready, ready).await {
                            Ok(acknowledgement) => {
                                write_message(&mut writer, acknowledgement).await?;
                            }
                            Err(failure) => {
                                terminalize_operation_rejection(
                                    &service,
                                    context,
                                    &mut writer,
                                    RunnerInboundFrameKind::WorkspaceReady,
                                    failure,
                                )
                                .await?;
                                return Ok(());
                            }
                        }
                    }
                    Message::LeaseClaim(claim) => {
                        if !lease_correlation_matches_connection(&claim.correlation, context) {
                            terminalize_protocol_rejection(
                                &service,
                                context,
                                &mut writer,
                                RunnerInboundFrameKind::LeaseClaim,
                                context.epoch,
                                RunnerRegistrationFailure::new(
                                    RunnerInboundFrameKind::LeaseClaim,
                                    AvailableCorrelation::Lease(claim.correlation),
                                    RejectionCode::CorrelationMismatch,
                                ),
                            )
                            .await?;
                            return Ok(());
                        }
                        if !transition_or_reject_not_current(
                            &service,
                            context,
                            &mut writer,
                            RunnerInboundFrameKind::LeaseClaim,
                            context.epoch,
                            RunnerConnectionTransition::Observe,
                        ).await? {
                            return Ok(());
                        }
                        match admit_lease_claim(&operations, claim).await? {
                            Ok((acknowledgement, dispatch)) => {
                                write_message(&mut writer, acknowledgement).await?;
                                write_message(&mut writer, dispatch).await?;
                            }
                            Err(failure) => {
                                terminalize_operation_rejection(
                                    &service,
                                    context,
                                    &mut writer,
                                    RunnerInboundFrameKind::LeaseClaim,
                                    failure,
                                ).await?;
                                return Ok(());
                            }
                        }
                    }
                    Message::Result(result) => {
                        if !lease_correlation_matches_connection(&result.correlation, context) {
                            terminalize_protocol_rejection(
                                &service,
                                context,
                                &mut writer,
                                RunnerInboundFrameKind::Result,
                                context.epoch,
                                RunnerRegistrationFailure::new(
                                    RunnerInboundFrameKind::Result,
                                    AvailableCorrelation::Lease(result.correlation),
                                    RejectionCode::CorrelationMismatch,
                                ),
                            )
                            .await?;
                            return Ok(());
                        }
                        if !transition_or_reject_not_current(
                            &service,
                            context,
                            &mut writer,
                            RunnerInboundFrameKind::Result,
                            context.epoch,
                            RunnerConnectionTransition::Observe,
                        ).await? {
                            return Ok(());
                        }
                        match admit_lease_result(&operations, result).await? {
                            Ok(acknowledgement) => {
                                write_message(&mut writer, acknowledgement).await?;
                            }
                            Err(failure) => {
                                terminalize_operation_rejection(
                                    &service,
                                    context,
                                    &mut writer,
                                    RunnerInboundFrameKind::Result,
                                    failure,
                                ).await?;
                                return Ok(());
                            }
                        }
                    }
                    Message::Shutdown(order) => {
                        if order.reason != ShutdownReason::RunnerShutdown {
                            terminalize_protocol_rejection(
                                &service,
                                context,
                                &mut writer,
                                RunnerInboundFrameKind::Shutdown,
                                order.connection_epoch,
                                RunnerRegistrationFailure::new(
                                    RunnerInboundFrameKind::Shutdown,
                                    AvailableCorrelation::ConnectionEpoch(order.connection_epoch),
                                    RejectionCode::CorrelationMismatch,
                                ),
                            )
                            .await?;
                            return Ok(());
                        }
                        if order.connection_epoch != context.epoch {
                            terminalize_protocol_rejection(
                                &service,
                                context,
                                &mut writer,
                                RunnerInboundFrameKind::Shutdown,
                                order.connection_epoch,
                                RunnerRegistrationFailure::new(
                                    RunnerInboundFrameKind::Shutdown,
                                    AvailableCorrelation::ConnectionEpoch(order.connection_epoch),
                                    RejectionCode::StaleConnection,
                                ),
                            )
                            .await?;
                            return Ok(());
                        }
                        transition_or_reject_not_current(
                            &service,
                            context,
                            &mut writer,
                            RunnerInboundFrameKind::Shutdown,
                            order.connection_epoch,
                            RunnerConnectionTransition::RunnerShutdown,
                        ).await?;
                        return Ok(());
                    }
                    message => {
                        terminalize_protocol_rejection(
                            &service,
                            context,
                            &mut writer,
                            inbound_frame_kind(&message),
                            context.epoch,
                            RunnerRegistrationFailure::new(
                                inbound_frame_kind(&message),
                                available_correlation(&message),
                                RejectionCode::CorrelationMismatch,
                            ),
                        )
                        .await?;
                        return Ok(());
                    }
                }
            }
            _ = heartbeat.tick() => {
                match heartbeat_state.next_tick()? {
                    HeartbeatTick::Challenge(challenge) => {
                        write_message(&mut writer, Message::Heartbeat(challenge)).await?;
                    }
                    HeartbeatTick::Missed(1) => {
                        if !transition_or_reject_not_current(
                            &service,
                            context,
                            &mut writer,
                            RunnerInboundFrameKind::Heartbeat,
                            context.epoch,
                            RunnerConnectionTransition::HeartbeatMissed,
                        ).await? {
                            return Ok(());
                        }
                    }
                    HeartbeatTick::Missed(misses)
                        if misses >= HEARTBEAT_MISSES_BEFORE_LOSS => {
                        terminalize_heartbeat_timeout(&service, context, &mut writer).await?;
                        return Ok(());
                    }
                    HeartbeatTick::Missed(_) => {}
                }
            }
            outbound = attachment.receive() => {
                let Some(message) = outbound else {
                    return Ok(());
                };
                if !transition_is_current(
                    &service,
                    context,
                    RunnerConnectionTransition::Observe,
                ).await? {
                    return Ok(());
                }
                write_message(&mut writer, message).await?;
            }
        }
        }
    }
    .await;
    if let Err(error) = &outcome
        && let Some(transition) = connection_failure_transition(error)
    {
        transition_is_current(&service, context, transition).await?;
    }
    outcome
}

fn claimed_resume_messages(
    lease: &RunnerLease,
) -> Result<[Message; 2], RunnerProtocolRuntimeError> {
    Ok([
        RunnerDispatchWireAdapter::lease_claimed(lease)
            .map_err(RunnerProtocolRuntimeError::DispatchWire)?,
        RunnerDispatchWireAdapter::dispatch(lease)
            .map_err(RunnerProtocolRuntimeError::DispatchWire)?,
    ])
}

async fn admit_workspace_ready<W>(
    workspace_ready: &W,
    ready: WorkspaceReady,
) -> Result<Message, RunnerRegistrationFailure>
where
    W: RunnerWorkspaceReadyOperationService,
{
    let correlation = AvailableCorrelation::Provision(ready.correlation.clone());
    let receipt = workspace_ready_receipt(&ready).ok_or_else(|| {
        RunnerRegistrationFailure::new(
            RunnerInboundFrameKind::WorkspaceReady,
            correlation.clone(),
            RejectionCode::CorrelationMismatch,
        )
    })?;
    let recorded = workspace_ready
        .record_workspace_ready(receipt)
        .await
        .map_err(|failure| failure.into_registration_failure(correlation))?;
    let manifest_digest =
        signalbox_runner_wire::Digest::try_new(recorded.manifest_digest().as_str().to_owned())
            .map_err(|_| {
                RunnerRegistrationFailure::new(
                    RunnerInboundFrameKind::WorkspaceReady,
                    AvailableCorrelation::Provision(ready.correlation.clone()),
                    RejectionCode::CorrelationMismatch,
                )
            })?;
    Ok(Message::WorkspaceRecorded(WorkspaceRecorded {
        correlation: ready.correlation,
        manifest_id: CanonicalUuid::from_uuid(recorded.manifest_id().into_uuid()),
        manifest_digest,
    }))
}

fn workspace_ready_receipt(ready: &WorkspaceReady) -> Option<RunnerWorkspaceReadyReceipt> {
    let manifest = &ready.ready.manifest;
    let repository = manifest.repository.as_ref()?;
    let canonical_clone_url_digest = manifest.canonical_clone_url_digest.as_ref()?;
    let recovery = manifest.recovery.as_ref()?;
    let credential_profile = manifest
        .credential_profile
        .as_ref()
        .map(|profile| CredentialProfileName::try_new(profile.as_str().to_owned()))
        .transpose()
        .ok()?;
    let recovery = match recovery {
        WireWorkspaceRecovery::Commit { revision } => WorkspaceRecovery::Commit {
            revision: WorkspaceRevision::try_new(revision.clone()).ok()?,
        },
        WireWorkspaceRecovery::Branch { name, revision } => WorkspaceRecovery::Branch {
            name: WorkspaceBranchName::try_new(name.clone()).ok()?,
            revision: WorkspaceRevision::try_new(revision.clone()).ok()?,
        },
    };
    Some(RunnerWorkspaceReadyReceipt::new(
        WorkspaceProvisioningAuthorizationId::from_uuid(
            ready.correlation.authorization_id.into_uuid(),
        ),
        SessionId::from_uuid(ready.correlation.session_id.into_uuid()),
        RunnerGeneration::try_from_u64(ready.correlation.placement_revision.get())?,
        RunnerId::from_uuid(ready.correlation.runner_id.into_uuid()),
        WorkspaceManifestId::from_uuid(manifest.manifest_id.into_uuid()),
        RunnerReadyManifestDigest::try_new(ready.ready.manifest_digest.as_str().to_owned()).ok()?,
        WorkspaceRepositoryKey::try_new(repository.as_str().to_owned()).ok()?,
        CanonicalCloneUrlDigest::try_new(canonical_clone_url_digest.as_str().to_owned()).ok()?,
        credential_profile,
        match manifest.sandbox_profile {
            SandboxProfile::Ambient => RunnerSandboxProfile::Ambient,
            SandboxProfile::WorkspaceRestricted => RunnerSandboxProfile::WorkspaceRestricted,
        },
        WorkspaceRelativePath::try_new(manifest.relative_path.clone()).ok()?,
        recovery,
    ))
}

async fn admit_lease_claim<O>(
    operations: &O,
    claim: LeaseClaim,
) -> Result<Result<(Message, Message), RunnerRegistrationFailure>, RunnerProtocolRuntimeError>
where
    O: RunnerLeaseOperationService,
{
    let correlation = AvailableCorrelation::Lease(claim.correlation.clone());
    let request = match RunnerDispatchWireAdapter::claim_request(claim) {
        Ok(request) => request,
        Err(_) => {
            return Ok(Err(RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::LeaseClaim,
                correlation,
                RejectionCode::CorrelationMismatch,
            )));
        }
    };
    let claimed = match operations.claim(request).await {
        Ok(claimed) => claimed,
        Err(failure) => {
            return Ok(Err(failure.into_registration_failure(
                RunnerInboundFrameKind::LeaseClaim,
                correlation,
            )));
        }
    };
    Ok(Ok((
        RunnerDispatchWireAdapter::lease_claimed(&claimed)
            .map_err(RunnerProtocolRuntimeError::DispatchWire)?,
        RunnerDispatchWireAdapter::dispatch(&claimed)
            .map_err(RunnerProtocolRuntimeError::DispatchWire)?,
    )))
}

async fn admit_lease_result<O>(
    operations: &O,
    result: ResultFrame,
) -> Result<Result<Message, RunnerRegistrationFailure>, RunnerProtocolRuntimeError>
where
    O: RunnerLeaseOperationService,
{
    let correlation = AvailableCorrelation::Lease(result.correlation.clone());
    let request = match RunnerDispatchWireAdapter::result_request(result) {
        Ok(request) => request,
        Err(_) => {
            return Ok(Err(RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Result,
                correlation,
                RejectionCode::CorrelationMismatch,
            )));
        }
    };
    let completed = match operations.record_result(request).await {
        Ok(completed) => completed,
        Err(failure) => {
            return Ok(Err(failure.into_registration_failure(
                RunnerInboundFrameKind::Result,
                correlation,
            )));
        }
    };
    Ok(Ok(RunnerDispatchWireAdapter::result_recorded(&completed)
        .map_err(RunnerProtocolRuntimeError::DispatchWire)?))
}

fn lease_correlation_matches_connection(
    correlation: &LeaseCorrelation,
    context: ConnectionContext,
) -> bool {
    correlation.runner_id == context.runner
}

fn workspace_ready_matches_connection(ready: &WorkspaceReady, context: ConnectionContext) -> bool {
    ready.correlation.runner_id == context.runner
        && ready.correlation.registration_revision == context.registration_revision
}

#[derive(Clone, Copy)]
struct ConnectionContext {
    enrollment: CanonicalUuid,
    runner: CanonicalUuid,
    registration_revision: PositiveU64,
    epoch: PositiveU64,
}

fn connection_address(
    context: ConnectionContext,
) -> Result<RunnerConnectionAddress, RunnerProtocolRuntimeError> {
    let epoch = RunnerConnectionEpoch::try_from_u64(context.epoch.get()).ok_or(
        RunnerProtocolRuntimeError::Broker(RunnerConnectionBrokerError::StateUnavailable),
    )?;
    Ok(RunnerConnectionAddress::new(
        RunnerEnrollmentId::from_uuid(context.enrollment.into_uuid()),
        RunnerId::from_uuid(context.runner.into_uuid()),
        epoch,
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectionCurrency {
    Current,
    Stale,
    EnrollmentRevoked,
}

const fn connection_failure_transition(
    error: &RunnerProtocolRuntimeError,
) -> Option<RunnerConnectionTransition> {
    match error {
        RunnerProtocolRuntimeError::Read(_)
        | RunnerProtocolRuntimeError::Write(_)
        | RunnerProtocolRuntimeError::Closed => Some(RunnerConnectionTransition::TransportClosed),
        RunnerProtocolRuntimeError::Decode(_)
        | RunnerProtocolRuntimeError::Encode(_)
        | RunnerProtocolRuntimeError::HeartbeatSequenceExhausted => {
            Some(RunnerConnectionTransition::ProtocolFailure)
        }
        RunnerProtocolRuntimeError::Broker(_) | RunnerProtocolRuntimeError::DispatchWire(_) => {
            Some(RunnerConnectionTransition::TransportClosed)
        }
        RunnerProtocolRuntimeError::Accept(_)
        | RunnerProtocolRuntimeError::Cleanup(_)
        | RunnerProtocolRuntimeError::ConnectionTask(_)
        | RunnerProtocolRuntimeError::ConnectionDrainTimeout { .. }
        | RunnerProtocolRuntimeError::HandshakeTimeout
        | RunnerProtocolRuntimeError::OwnershipUnavailable
        | RunnerProtocolRuntimeError::Lifecycle(_) => None,
    }
}

const fn rejection_terminal_transition(
    cause: RunnerRegistrationFailureCause,
) -> RunnerConnectionTransition {
    match cause {
        RunnerRegistrationFailureCause::PeerInput
        | RunnerRegistrationFailureCause::EnrollmentAuthority
        | RunnerRegistrationFailureCause::Policy => RunnerConnectionTransition::ProtocolFailure,
        RunnerRegistrationFailureCause::Database
        | RunnerRegistrationFailureCause::CommitAmbiguous
        | RunnerRegistrationFailureCause::Corruption => RunnerConnectionTransition::TransportClosed,
    }
}

async fn transition_is_current<S>(
    service: &S,
    context: ConnectionContext,
    transition: RunnerConnectionTransition,
) -> Result<bool, RunnerProtocolRuntimeError>
where
    S: RunnerRegistrationService,
{
    Ok(matches!(
        transition_currency(service, context, transition).await?,
        ConnectionCurrency::Current
    ))
}

async fn transition_currency<S>(
    service: &S,
    context: ConnectionContext,
    transition: RunnerConnectionTransition,
) -> Result<ConnectionCurrency, RunnerProtocolRuntimeError>
where
    S: RunnerRegistrationService,
{
    match service
        .transition_connection(context.enrollment, context.epoch, transition)
        .await
        .map_err(RunnerProtocolRuntimeError::Lifecycle)?
    {
        RunnerConnectionTransitionOutcome::Current(snapshot)
            if snapshot.cause() == RunnerConnectionCause::EnrollmentRevoked =>
        {
            Ok(ConnectionCurrency::EnrollmentRevoked)
        }
        RunnerConnectionTransitionOutcome::Current(_) => Ok(ConnectionCurrency::Current),
        RunnerConnectionTransitionOutcome::Stale { .. } => Ok(ConnectionCurrency::Stale),
    }
}

async fn transition_or_reject_not_current<S>(
    service: &S,
    context: ConnectionContext,
    writer: &mut OwnedWriteHalf,
    offending_kind: RunnerInboundFrameKind,
    evidence_epoch: PositiveU64,
    transition: RunnerConnectionTransition,
) -> Result<bool, RunnerProtocolRuntimeError>
where
    S: RunnerRegistrationService,
{
    match transition_currency(service, context, transition).await? {
        ConnectionCurrency::Current => Ok(true),
        ConnectionCurrency::Stale => {
            write_stale_epoch(writer, offending_kind, evidence_epoch).await?;
            Ok(false)
        }
        ConnectionCurrency::EnrollmentRevoked => {
            write_rejected(
                writer,
                RunnerRegistrationFailure::new(
                    offending_kind,
                    AvailableCorrelation::ConnectionEpoch(evidence_epoch),
                    RejectionCode::EnrollmentRevoked,
                ),
            )
            .await?;
            Ok(false)
        }
    }
}

async fn terminalize_heartbeat_timeout<S>(
    service: &S,
    context: ConnectionContext,
    writer: &mut OwnedWriteHalf,
) -> Result<(), RunnerProtocolRuntimeError>
where
    S: RunnerRegistrationService,
{
    if !transition_or_reject_not_current(
        service,
        context,
        writer,
        RunnerInboundFrameKind::Heartbeat,
        context.epoch,
        RunnerConnectionTransition::HeartbeatTimeout,
    )
    .await?
    {
        return Ok(());
    }
    Ok(())
}

async fn terminalize_protocol_rejection<S>(
    service: &S,
    context: ConnectionContext,
    writer: &mut OwnedWriteHalf,
    offending_kind: RunnerInboundFrameKind,
    evidence_epoch: PositiveU64,
    failure: RunnerRegistrationFailure,
) -> Result<(), RunnerProtocolRuntimeError>
where
    S: RunnerRegistrationService,
{
    if transition_or_reject_not_current(
        service,
        context,
        writer,
        offending_kind,
        evidence_epoch,
        RunnerConnectionTransition::ProtocolFailure,
    )
    .await?
    {
        write_rejected(writer, failure).await?;
    }
    Ok(())
}

async fn terminalize_operation_rejection<S>(
    service: &S,
    context: ConnectionContext,
    writer: &mut OwnedWriteHalf,
    offending_kind: RunnerInboundFrameKind,
    failure: RunnerRegistrationFailure,
) -> Result<(), RunnerProtocolRuntimeError>
where
    S: RunnerRegistrationService,
{
    let transition = rejection_terminal_transition(failure.cause());
    if transition_or_reject_not_current(
        service,
        context,
        writer,
        offending_kind,
        context.epoch,
        transition,
    )
    .await?
    {
        write_rejected(writer, failure).await?;
    }
    Ok(())
}

async fn write_stale_epoch(
    writer: &mut OwnedWriteHalf,
    offending_kind: RunnerInboundFrameKind,
    epoch: PositiveU64,
) -> Result<(), RunnerProtocolRuntimeError> {
    write_rejected(
        writer,
        RunnerRegistrationFailure::new(
            offending_kind,
            AvailableCorrelation::ConnectionEpoch(epoch),
            RejectionCode::StaleConnection,
        ),
    )
    .await
}

#[derive(Clone, Debug, PartialEq)]
enum HeartbeatTick {
    Challenge(Heartbeat),
    Missed(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HeartbeatState {
    last_challenge: u64,
    last_accepted_runner_sequence: u64,
    outstanding_challenge: Option<u64>,
    missed_intervals: u8,
}

impl HeartbeatState {
    const fn new() -> Self {
        Self {
            last_challenge: 0,
            last_accepted_runner_sequence: 0,
            outstanding_challenge: None,
            missed_intervals: 0,
        }
    }

    fn next_tick(&mut self) -> Result<HeartbeatTick, RunnerProtocolRuntimeError> {
        if self.outstanding_challenge.is_some() {
            self.missed_intervals = self.missed_intervals.saturating_add(1);
            return Ok(HeartbeatTick::Missed(self.missed_intervals));
        }
        let sequence = self
            .last_challenge
            .checked_add(1)
            .and_then(|value| PositiveU64::try_new(value).ok())
            .ok_or(RunnerProtocolRuntimeError::HeartbeatSequenceExhausted)?;
        self.last_challenge = sequence.get();
        self.outstanding_challenge = Some(sequence.get());
        Ok(HeartbeatTick::Challenge(Heartbeat {
            sequence,
            last_accepted_peer_sequence: self.last_accepted_runner_sequence,
        }))
    }

    fn accept(&mut self, acknowledgement: &HeartbeatAck) -> Result<(), RunnerRegistrationFailure> {
        if acknowledgement.lease_phase.is_some() || acknowledgement.workspace_phase.is_some() {
            return Err(RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::HeartbeatAck,
                heartbeat_ack_correlation(acknowledgement),
                RejectionCode::Unavailable,
            ));
        }
        if Some(acknowledgement.challenge_sequence.get()) != self.outstanding_challenge
            || acknowledgement.runner_sequence.get() <= self.last_accepted_runner_sequence
        {
            return Err(RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::HeartbeatAck,
                AvailableCorrelation::None,
                RejectionCode::CorrelationMismatch,
            ));
        }
        self.last_accepted_runner_sequence = acknowledgement.runner_sequence.get();
        self.outstanding_challenge = None;
        self.missed_intervals = 0;
        Ok(())
    }
}

async fn read_frame(
    reader: &mut BufReader<OwnedReadHalf>,
) -> Result<Frame, RunnerProtocolRuntimeError> {
    let mut line = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(RunnerProtocolRuntimeError::Read)?;
        if available.is_empty() {
            if line.is_empty() {
                return Err(RunnerProtocolRuntimeError::Closed);
            }
            return Err(RunnerProtocolRuntimeError::Decode(
                FrameError::MissingNewline,
            ));
        }
        let boundary = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        let observed = line.len().saturating_add(boundary);
        if observed > MAX_FRAME_BYTES {
            return Err(RunnerProtocolRuntimeError::Decode(FrameError::TooLarge {
                bytes: observed,
            }));
        }
        line.extend_from_slice(&available[..boundary]);
        reader.consume(boundary);
        if line.ends_with(b"\n") {
            break;
        }
    }
    decode_line(&line).map_err(RunnerProtocolRuntimeError::Decode)
}

async fn write_rejected(
    writer: &mut OwnedWriteHalf,
    failure: RunnerRegistrationFailure,
) -> Result<(), RunnerProtocolRuntimeError> {
    write_message(writer, Message::Rejected(failure.into_rejected())).await
}

async fn write_decode_rejected(
    writer: &mut OwnedWriteHalf,
    error: &FrameError,
) -> Result<(), RunnerProtocolRuntimeError> {
    let code = match error {
        FrameError::UnsupportedVersion(_) => RejectionCode::UnsupportedVersion,
        FrameError::MissingNewline
        | FrameError::TooLarge { .. }
        | FrameError::MalformedJson(_)
        | FrameError::InvalidValue(_) => RejectionCode::MalformedFrame,
    };
    write_rejected(
        writer,
        RunnerRegistrationFailure::new(
            RunnerInboundFrameKind::Registration,
            AvailableCorrelation::None,
            code,
        ),
    )
    .await
}

async fn write_message(
    writer: &mut OwnedWriteHalf,
    message: Message,
) -> Result<(), RunnerProtocolRuntimeError> {
    let frame = Frame::try_new(message).map_err(RunnerProtocolRuntimeError::Encode)?;
    let encoded = encode_line(&frame).map_err(RunnerProtocolRuntimeError::Encode)?;
    writer
        .write_all(&encoded)
        .await
        .map_err(RunnerProtocolRuntimeError::Write)
}

fn available_correlation(message: &Message) -> AvailableCorrelation {
    match message {
        Message::Enroll(value) => AvailableCorrelation::Enrollment(value.request_id),
        Message::Enrolled(value) => AvailableCorrelation::Enrollment(value.request_id),
        Message::Resume(value) => AvailableCorrelation::Enrollment(value.request_id),
        Message::Resumed(value) => AvailableCorrelation::Registration(value.registration_revision),
        Message::ReplacementPending(value) => AvailableCorrelation::Enrollment(value.request_id),
        Message::Advertise(value) => {
            AvailableCorrelation::Registration(value.registration_revision)
        }
        Message::Registered(value) => {
            AvailableCorrelation::Registration(value.registration_revision)
        }
        Message::WorkspaceLeakPage(value) => {
            AvailableCorrelation::LeakPage(value.page.correlation.clone())
        }
        Message::WorkspaceLeakRecorded(value) => {
            AvailableCorrelation::LeakPage(value.correlation.clone())
        }
        Message::WorkspaceProvision(value) => {
            AvailableCorrelation::Provision(value.correlation.clone())
        }
        Message::WorkspaceReady(value) => {
            AvailableCorrelation::Provision(value.correlation.clone())
        }
        Message::WorkspaceRecorded(value) => {
            AvailableCorrelation::Provision(value.correlation.clone())
        }
        Message::WorkspaceRelease(value) => {
            AvailableCorrelation::Release(value.correlation.clone())
        }
        Message::WorkspaceReleased(value) => {
            AvailableCorrelation::Release(value.correlation.clone())
        }
        Message::WorkspaceReleaseRecorded(value) => {
            AvailableCorrelation::Release(value.correlation.clone())
        }
        Message::LeaseOffer(value) => AvailableCorrelation::Lease(value.correlation.clone()),
        Message::LeaseClaim(value) => AvailableCorrelation::Lease(value.correlation.clone()),
        Message::LeaseClaimed(value) => AvailableCorrelation::Lease(value.correlation.clone()),
        Message::Dispatch(value) => AvailableCorrelation::Lease(value.correlation.clone()),
        Message::Result(value) => AvailableCorrelation::Lease(value.correlation.clone()),
        Message::ResultRecorded(value) => AvailableCorrelation::Lease(value.correlation.clone()),
        Message::OperationFailed(value) => {
            AvailableCorrelation::OperationFailure(value.failure.correlation.clone())
        }
        Message::OperationFailureRecorded(value) => {
            AvailableCorrelation::OperationFailure(value.correlation.clone())
        }
        Message::Shutdown(value) => AvailableCorrelation::ConnectionEpoch(value.connection_epoch),
        Message::Rejected(value) => value.available_correlation.clone(),
        Message::Heartbeat(_) => AvailableCorrelation::None,
        Message::HeartbeatAck(value) => heartbeat_ack_correlation(value),
    }
}

fn heartbeat_ack_correlation(acknowledgement: &HeartbeatAck) -> AvailableCorrelation {
    match (
        acknowledgement.lease_phase.as_ref(),
        acknowledgement.workspace_phase.as_ref(),
    ) {
        (Some(lease), None | Some(_)) => AvailableCorrelation::Lease(lease.correlation.clone()),
        (None, Some(HeartbeatWorkspacePhase::Provisioning { correlation }))
        | (None, Some(HeartbeatWorkspacePhase::ReadyUnrecorded { correlation })) => {
            AvailableCorrelation::Provision(correlation.clone())
        }
        (None, Some(HeartbeatWorkspacePhase::ReleaseAccepted { correlation }))
        | (None, Some(HeartbeatWorkspacePhase::ReleaseCompleted { correlation })) => {
            AvailableCorrelation::Release(correlation.clone())
        }
        (
            None,
            Some(HeartbeatWorkspacePhase::FailureUnrecorded {
                correlation: WorkspaceFailureCorrelation::Provision(correlation),
            }),
        ) => AvailableCorrelation::Provision(correlation.clone()),
        (
            None,
            Some(HeartbeatWorkspacePhase::FailureUnrecorded {
                correlation: WorkspaceFailureCorrelation::Release(correlation),
            }),
        ) => AvailableCorrelation::Release(correlation.clone()),
        (None, None) => AvailableCorrelation::None,
    }
}

fn inbound_frame_kind(message: &Message) -> RunnerInboundFrameKind {
    match message {
        Message::Enroll(_) => RunnerInboundFrameKind::Enroll,
        Message::Enrolled(_) => RunnerInboundFrameKind::Enrolled,
        Message::Resume(_) => RunnerInboundFrameKind::Resume,
        Message::Resumed(_) => RunnerInboundFrameKind::Resumed,
        Message::ReplacementPending(_) => RunnerInboundFrameKind::ReplacementPending,
        Message::Advertise(_) => RunnerInboundFrameKind::Advertise,
        Message::Registered(_) => RunnerInboundFrameKind::Registered,
        Message::Heartbeat(_) => RunnerInboundFrameKind::Heartbeat,
        Message::HeartbeatAck(_) => RunnerInboundFrameKind::HeartbeatAck,
        Message::WorkspaceLeakPage(_) => RunnerInboundFrameKind::WorkspaceLeakPage,
        Message::WorkspaceLeakRecorded(_) => RunnerInboundFrameKind::WorkspaceLeakRecorded,
        Message::WorkspaceProvision(_) => RunnerInboundFrameKind::WorkspaceProvision,
        Message::WorkspaceReady(_) => RunnerInboundFrameKind::WorkspaceReady,
        Message::WorkspaceRecorded(_) => RunnerInboundFrameKind::WorkspaceRecorded,
        Message::WorkspaceRelease(_) => RunnerInboundFrameKind::WorkspaceRelease,
        Message::WorkspaceReleased(_) => RunnerInboundFrameKind::WorkspaceReleased,
        Message::WorkspaceReleaseRecorded(_) => RunnerInboundFrameKind::WorkspaceReleaseRecorded,
        Message::LeaseOffer(_) => RunnerInboundFrameKind::LeaseOffer,
        Message::LeaseClaim(_) => RunnerInboundFrameKind::LeaseClaim,
        Message::LeaseClaimed(_) => RunnerInboundFrameKind::LeaseClaimed,
        Message::Dispatch(_) => RunnerInboundFrameKind::Dispatch,
        Message::Result(_) => RunnerInboundFrameKind::Result,
        Message::ResultRecorded(_) => RunnerInboundFrameKind::ResultRecorded,
        Message::OperationFailed(_) => RunnerInboundFrameKind::OperationFailed,
        Message::OperationFailureRecorded(_) => RunnerInboundFrameKind::OperationFailureRecorded,
        Message::Shutdown(_) => RunnerInboundFrameKind::Shutdown,
        Message::Rejected(_) => RunnerInboundFrameKind::Rejected,
    }
}

/// Typed transport, framing, admission, or durable-lifecycle failure.
#[derive(Debug)]
pub enum RunnerProtocolRuntimeError {
    Accept(io::Error),
    Cleanup(LocalSocketError),
    Read(io::Error),
    Write(io::Error),
    Decode(FrameError),
    Encode(FrameError),
    Closed,
    HandshakeTimeout,
    OwnershipUnavailable,
    Broker(RunnerConnectionBrokerError),
    DispatchWire(RunnerDispatchWireError),
    HeartbeatSequenceExhausted,
    ConnectionTask(JoinError),
    ConnectionDrainTimeout {
        remaining: usize,
        initiating: Option<Box<Self>>,
    },
    Lifecycle(RunnerRegistrationFailure),
}

impl fmt::Display for RunnerProtocolRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Accept(_) => formatter.write_str("runner listener accept failed"),
            Self::Cleanup(_) => formatter.write_str("runner listener cleanup failed"),
            Self::Read(_) => formatter.write_str("runner frame read failed"),
            Self::Write(_) => formatter.write_str("runner frame write failed"),
            Self::Decode(_) => formatter.write_str("runner frame decoding failed"),
            Self::Encode(_) => formatter.write_str("runner frame encoding failed"),
            Self::Closed => formatter.write_str("runner connection closed"),
            Self::HandshakeTimeout => formatter.write_str("runner handshake timed out"),
            Self::OwnershipUnavailable => {
                formatter.write_str("runner runtime ownership was unavailable")
            }
            Self::Broker(_) => formatter.write_str("runner connection broker failed"),
            Self::DispatchWire(_) => formatter.write_str("runner lease wire projection failed"),
            Self::HeartbeatSequenceExhausted => {
                formatter.write_str("runner heartbeat sequence exhausted")
            }
            Self::ConnectionTask(_) => formatter.write_str("runner connection task failed"),
            Self::ConnectionDrainTimeout { .. } => {
                formatter.write_str("runner connection task drain timed out")
            }
            Self::Lifecycle(_) => formatter.write_str("runner lifecycle persistence failed"),
        }
    }
}

impl Error for RunnerProtocolRuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Accept(error) | Self::Read(error) | Self::Write(error) => Some(error),
            Self::Cleanup(error) => Some(error),
            Self::Decode(error) | Self::Encode(error) => Some(error),
            Self::Lifecycle(error) => Some(error),
            Self::Broker(error) => Some(error),
            Self::DispatchWire(error) => Some(error),
            Self::ConnectionTask(error) => Some(error),
            Self::ConnectionDrainTimeout {
                initiating: Some(error),
                ..
            } => Some(error.as_ref()),
            Self::Closed
            | Self::HandshakeTimeout
            | Self::OwnershipUnavailable
            | Self::HeartbeatSequenceExhausted
            | Self::ConnectionDrainTimeout {
                initiating: None, ..
            } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use rust_decimal::Decimal;
    use signalbox_domain::{
        CreateSession, DirectModelSelection, DurableCommandId, ModelSelectionRequest,
        NormalizedToolArguments, RunnerAdvertisement, RunnerAuthenticationId,
        RunnerCapabilityClass, RunnerCatalog, RunnerEnrollment, RunnerEnrollmentId,
        RunnerGeneration, RunnerLeaseCorrelation, RunnerLeaseId, RunnerLeaseReconstitutionInput,
        RunnerLeaseRetryPreparation, RunnerLeaseState, RunnerLostBeforePin, RunnerRepositoryEntry,
        RunnerSandboxProfile, RunnerSelector, RunnerToolDeclaration, RunnerToolEffectClass,
        RunnerToolModelDefinition, RunnerToolPermissionOverrides, RunnerWorkingDirectory,
        SessionConfigurationDefaults, SessionCreationCause, SessionCreationProvenance, SessionId,
        SessionRunnerPlacement, SessionRunnerPlacementRequest, SessionRunnerPlacementState,
        ToolAdmissibleLoci, ToolAttemptDispatchCorrelation,
        ToolAttemptDispatchCorrelationReconstitutionInput, ToolAttemptId, ToolDispatchGeneration,
        ToolName, ToolPermissionDefault, ToolRequestId, ToolResultContent, ToolResultText,
        TranscriptAncestry, TurnAttemptId, TurnId, ValidatedRunnerRegistration,
        WorkingDirectorySelection, WorkspaceCapability, WorkspaceRequirement,
    };
    use signalbox_persistence::{
        create_session::CreateSessionRepository,
        disposable_test_container_labels, local_test_connection_options, migrate,
        runner_protocol::{RunnerConnectionCause, RunnerConnectionState},
        session_credentials::{SessionCredentialPin, SessionModelCredential},
    };
    use sqlx::{PgPool, postgres::PgPoolOptions};
    use testcontainers_modules::{
        postgres::Postgres,
        testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
    };

    const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
    const DATABASE_USER: &str = "signalbox";
    const DATABASE_PASSWORD: &str = "signalbox-test";
    const DATABASE_NAME: &str = "signalbox";
    const CONFIGURED_REPOSITORY: &str = "signalbox";
    const ARBITRARY_HEARTBEAT_CHALLENGE_SEQUENCE: u64 = 1;
    const ARBITRARY_HEARTBEAT_RUNNER_SEQUENCE: u64 = 1;
    const ARBITRARY_PROVISION_AUTHORIZATION_ID_SEED: u128 = 5;
    const ARBITRARY_PROVISION_SESSION_ID_SEED: u128 = 6;
    const ARBITRARY_PROVISION_RUNNER_ID_SEED: u128 = 7;
    const ARBITRARY_WORKSPACE_MANIFEST_ID_SEED: u128 = 8;
    const ARBITRARY_PROVISION_PLACEMENT_REVISION: u64 = 1;
    const ARBITRARY_PROVISION_REGISTRATION_REVISION: u64 = 1;
    const ARBITRARY_RUNNER_ENROLLMENT_REQUEST_ID_SEED: u128 = 0x300;
    const ARBITRARY_RUNNER_SESSION_COMMAND_ID_SEED: u128 = 0x301;
    const ARBITRARY_RUNNER_SESSION_MODEL_SELECTION_SEED: u128 = 0x302;
    const ARBITRARY_RUNNER_SESSION_ID_SEED: u128 = 0x303;
    const RUNNER_SESSION_WORKING_DIRECTORY: &str = "/workspace/session";
    const SYNTHETIC_MODEL_FAMILY: &str = "fixture-model-family";
    const SYNTHETIC_CREDENTIAL_REFERENCE: &str = "fixture-credential-reference";

    #[derive(Clone)]
    struct EnrollmentService {
        response: Enrolled,
    }

    impl RunnerRegistrationService for EnrollmentService {
        fn enroll(
            &self,
            _request: Enroll,
        ) -> RunnerRegistrationFuture<'_, RunnerEnrollmentAccepted> {
            Box::pin(std::future::ready(Ok(RunnerEnrollmentAccepted::Active(
                self.response.clone(),
            ))))
        }

        fn resume(&self, request: Resume) -> RunnerRegistrationFuture<'_, RunnerResumeAccepted> {
            Box::pin(std::future::ready(Err(RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Resume,
                AvailableCorrelation::Enrollment(request.request_id),
                RejectionCode::Unavailable,
            ))))
        }

        fn advertise(
            &self,
            _connection_enrollment: CanonicalUuid,
            request: Advertise,
            _epoch: PositiveU64,
        ) -> RunnerRegistrationFuture<'_, Registered> {
            Box::pin(std::future::ready(Err(RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Advertise,
                AvailableCorrelation::Registration(request.registration_revision),
                RejectionCode::Unavailable,
            ))))
        }

        fn transition_connection(
            &self,
            _enrollment: CanonicalUuid,
            epoch: PositiveU64,
            _transition: RunnerConnectionTransition,
        ) -> RunnerRegistrationFuture<'_, RunnerConnectionTransitionOutcome> {
            let epoch = RunnerConnectionEpoch::try_from_u64(epoch.get())
                .expect("the wire epoch is positive");
            Box::pin(std::future::ready(Ok(
                RunnerConnectionTransitionOutcome::Stale {
                    observed: epoch,
                    current: epoch,
                },
            )))
        }
    }

    fn identity(value: u128) -> CanonicalUuid {
        CanonicalUuid::from_uuid(uuid::Uuid::from_u128(value))
    }

    async fn create_runner_placed_session(
        pool: &PgPool,
        store: &RunnerProtocolStore,
        session: SessionId,
        runner: RunnerId,
    ) {
        let credentials = SessionCredentialPin::try_new(vec![SessionModelCredential::new(
            SYNTHETIC_MODEL_FAMILY,
            SYNTHETIC_CREDENTIAL_REFERENCE,
        )])
        .expect("the synthetic credential pin is valid");
        let creation = CreateSession::new(
            DurableCommandId::from_uuid(uuid::Uuid::from_u128(
                ARBITRARY_RUNNER_SESSION_COMMAND_ID_SEED,
            )),
            SessionCreationProvenance::new(
                SessionCreationCause::UserInitiated,
                TranscriptAncestry::None,
            ),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
                DirectModelSelection::from_uuid(uuid::Uuid::from_u128(
                    ARBITRARY_RUNNER_SESSION_MODEL_SELECTION_SEED,
                )),
            )),
        )
        .prepare(session)
        .expect("the runner session creation is preparable");
        CreateSessionRepository::new(pool.clone(), credentials)
            .handle(creation)
            .await
            .expect("the runner session is created");
        let placement = SessionRunnerPlacement::new(
            session,
            SessionRunnerPlacementRequest {
                selector: RunnerSelector::Identity(runner),
                working_directory: WorkingDirectorySelection::Exact(
                    RunnerWorkingDirectory::try_new(RUNNER_SESSION_WORKING_DIRECTORY.to_owned())
                        .expect("the synthetic runner directory is valid"),
                ),
                credential_profile: None,
                workspace: WorkspaceRequirement::None,
                sandbox: RunnerSandboxProfile::Ambient,
                permission_overrides: RunnerToolPermissionOverrides::try_new([])
                    .expect("the empty permission override inventory is valid"),
            },
        );
        store
            .store_placement(&placement, None, None)
            .await
            .expect("the runner placement is stored");
    }

    fn canonical_lease_correlation() -> signalbox_runner_wire::LeaseCorrelation {
        let arbitrary_identity = identity(1);
        let first = PositiveU64::try_new(1).expect("the first fixture generation is positive");
        signalbox_runner_wire::LeaseCorrelation {
            registration_revision: first,
            lease_id: arbitrary_identity,
            lease_generation: first,
            runner_id: arbitrary_identity,
            placement_revision: first,
            working_directory: signalbox_runner_wire::WorkingDirectory::try_new(
                RUNNER_SESSION_WORKING_DIRECTORY.to_owned(),
            )
            .expect("the fixture working directory is valid"),
            sandbox_profile: signalbox_runner_wire::SandboxProfile::Ambient,
            tool_name: signalbox_runner_wire::WireToolName::try_new("git_fetch".to_owned())
                .expect("the fixture tool name is valid"),
            session_id: arbitrary_identity,
            turn_id: arbitrary_identity,
            tool_request_id: arbitrary_identity,
            tool_attempt_id: arbitrary_identity,
            issuing_turn_attempt_id: arbitrary_identity,
            tool_dispatch_generation: first,
        }
    }

    #[test]
    fn s31_inv011_inv043_retained_terminal_resume_discards_the_exact_pair() {
        let correlation = canonical_lease_correlation();
        let inventory = ReconnectInventory {
            lease: Some(signalbox_runner_wire::LeasePhase {
                correlation: correlation.clone(),
                phase: LeasePhaseKind::ExecutionMayHaveStarted,
            }),
            result: Some(signalbox_runner_wire::RetainedResult {
                correlation: correlation.clone(),
                result: signalbox_runner_wire::TerminalResult::Success {
                    text: String::from("completed"),
                },
            }),
            ..ReconnectInventory::default()
        };

        let operation =
            classify_resume_inventory(&inventory).expect("the complete retained pair is supported");
        let directives = retained_result_directives(&inventory, DirectiveAction::DiscardAsRecorded)
            .expect("the exact inventory accepts matching directives");

        assert_eq!(
            operation,
            ResumeOperation::RetainedResult(ResultFrame {
                correlation,
                result: inventory
                    .result
                    .as_ref()
                    .expect("the result exists")
                    .result
                    .clone(),
            })
        );
        assert_eq!(directives.validate_against(&inventory), Ok(()));
        assert_eq!(
            directives.lease.expect("the lease directive exists").action,
            DirectiveAction::DiscardAsRecorded
        );
        assert_eq!(
            directives
                .result
                .expect("the result directive exists")
                .action,
            DirectiveAction::DiscardAsRecorded
        );
    }

    #[test]
    fn s31_inv011_inv043_stale_retained_terminal_resume_fails_both_items() {
        let correlation = canonical_lease_correlation();
        let inventory = ReconnectInventory {
            lease: Some(signalbox_runner_wire::LeasePhase {
                correlation: correlation.clone(),
                phase: LeasePhaseKind::ExecutionMayHaveStarted,
            }),
            result: Some(signalbox_runner_wire::RetainedResult {
                correlation,
                result: signalbox_runner_wire::TerminalResult::Success {
                    text: String::from("completed"),
                },
            }),
            ..ReconnectInventory::default()
        };

        let directives = retained_result_directives(&inventory, DirectiveAction::FailStale)
            .expect("the exact stale inventory accepts matching directives");

        assert_eq!(directives.validate_against(&inventory), Ok(()));
        assert_eq!(
            directives.lease.expect("the lease directive exists").action,
            DirectiveAction::FailStale
        );
        assert_eq!(
            directives
                .result
                .expect("the result directive exists")
                .action,
            DirectiveAction::FailStale
        );
    }

    #[test]
    fn s31_inv011_inv043_retained_terminal_resume_rejects_result_without_lease() {
        let correlation = canonical_lease_correlation();
        let inventory = ReconnectInventory {
            result: Some(signalbox_runner_wire::RetainedResult {
                correlation,
                result: signalbox_runner_wire::TerminalResult::Success {
                    text: String::from("completed"),
                },
            }),
            ..ReconnectInventory::default()
        };

        let rejected = classify_resume_inventory(&inventory)
            .expect_err("terminal evidence requires its execution-possible lease phase");

        assert_eq!(rejected, RejectionCode::CorrelationMismatch);
    }

    #[test]
    fn s32_inv012_ready_workspace_resume_resends_matching_authenticated_evidence() {
        let correlation = repository_workspace_provision_correlation();
        let inventory = ReconnectInventory {
            workspace_operation: Some(WorkspaceOperation::Provision {
                correlation: correlation.clone(),
                phase: ProvisionPhase::ReadyUnrecorded,
            }),
            ..ReconnectInventory::default()
        };

        let operation = classify_resume_inventory(&inventory)
            .expect("one ready-unrecorded workspace is structurally admissible");
        let directives = ready_workspace_directives(&inventory, DirectiveAction::Resend)
            .expect("the exact ready inventory accepts a resend directive");

        assert_eq!(
            operation,
            ResumeOperation::ReadyWorkspace(correlation.clone())
        );
        assert_eq!(directives.validate_against(&inventory), Ok(()));
        assert_eq!(
            directives.workspace_operation,
            Some(Directive {
                correlation: signalbox_runner_wire::OperationCorrelation::Provision(correlation),
                action: DirectiveAction::Resend,
            })
        );
    }

    #[test]
    fn s32_provisioning_resume_remains_unavailable_without_ready_evidence() {
        let inventory = ReconnectInventory {
            workspace_operation: Some(WorkspaceOperation::Provision {
                correlation: repository_workspace_provision_correlation(),
                phase: ProvisionPhase::Provisioning,
            }),
            ..ReconnectInventory::default()
        };

        let rejected = classify_resume_inventory(&inventory)
            .expect_err("an in-progress clone has no resumable ready payload");

        assert_eq!(rejected, RejectionCode::Unavailable);
    }

    #[test]
    fn s32_ready_workspace_resume_rejects_a_second_inventory_slot() {
        let inventory = ReconnectInventory {
            workspace_operation: Some(WorkspaceOperation::Provision {
                correlation: repository_workspace_provision_correlation(),
                phase: ProvisionPhase::ReadyUnrecorded,
            }),
            lease: Some(signalbox_runner_wire::LeasePhase {
                correlation: canonical_lease_correlation(),
                phase: LeasePhaseKind::WaitingDispatch,
            }),
            ..ReconnectInventory::default()
        };

        let rejected = classify_resume_inventory(&inventory)
            .expect_err("workspace recovery remains serial with lease recovery");

        assert_eq!(rejected, RejectionCode::CorrelationMismatch);
    }

    #[test]
    fn s31_inv011_inv043_waiting_dispatch_resume_awaits_the_exact_claimed_lease() {
        let correlation = canonical_lease_correlation();
        let inventory = ReconnectInventory {
            lease: Some(signalbox_runner_wire::LeasePhase {
                correlation: correlation.clone(),
                phase: LeasePhaseKind::WaitingDispatch,
            }),
            ..ReconnectInventory::default()
        };

        let operation =
            classify_resume_inventory(&inventory).expect("a waiting claimed lease is recoverable");
        let directives = claimed_lease_directives(&inventory, DirectiveAction::Await)
            .expect("the exact claimed lease accepts an await directive");

        assert_eq!(operation, ResumeOperation::ClaimedLease(correlation));
        assert_eq!(directives.validate_against(&inventory), Ok(()));
        assert_eq!(
            directives.lease.expect("the lease directive exists").action,
            DirectiveAction::Await
        );
    }

    #[test]
    fn s31_inv011_inv043_dispatch_received_resume_awaits_the_exact_claimed_lease() {
        let correlation = canonical_lease_correlation();
        let inventory = ReconnectInventory {
            lease: Some(signalbox_runner_wire::LeasePhase {
                correlation: correlation.clone(),
                phase: LeasePhaseKind::DispatchReceived,
            }),
            ..ReconnectInventory::default()
        };

        let operation = classify_resume_inventory(&inventory)
            .expect("a journaled dispatch remains recoverable before execution starts");
        let directives = claimed_lease_directives(&inventory, DirectiveAction::Await)
            .expect("the journaled dispatch accepts an await directive");

        assert_eq!(operation, ResumeOperation::ClaimedLease(correlation));
        assert_eq!(directives.validate_against(&inventory), Ok(()));
        assert_eq!(
            directives.lease.expect("the lease directive exists").action,
            DirectiveAction::Await
        );
    }

    #[test]
    fn s31_inv011_inv043_stale_claimed_resume_fails_the_exact_lease() {
        let correlation = canonical_lease_correlation();
        let inventory = ReconnectInventory {
            lease: Some(signalbox_runner_wire::LeasePhase {
                correlation: correlation.clone(),
                phase: LeasePhaseKind::WaitingDispatch,
            }),
            ..ReconnectInventory::default()
        };

        let directives = claimed_lease_directives(&inventory, DirectiveAction::FailStale)
            .expect("the stale claimed lease accepts an exact terminal directive");

        assert_eq!(directives.validate_against(&inventory), Ok(()));
        assert_eq!(
            directives.lease,
            Some(Directive {
                correlation,
                action: DirectiveAction::FailStale,
            })
        );
    }

    #[test]
    fn s31_inv011_inv043_execution_possible_resume_without_result_stays_unavailable() {
        let inventory = ReconnectInventory {
            lease: Some(signalbox_runner_wire::LeasePhase {
                correlation: canonical_lease_correlation(),
                phase: LeasePhaseKind::ExecutionMayHaveStarted,
            }),
            ..ReconnectInventory::default()
        };

        let rejected = classify_resume_inventory(&inventory)
            .expect_err("execution-possible recovery still requires durable loss handling");

        assert_eq!(rejected, RejectionCode::Unavailable);
    }

    fn runner_operation_tool() -> ToolName {
        ToolName::try_new("git_fetch".to_owned()).expect("the fixture tool name is valid")
    }

    fn runner_operation_class() -> RunnerCapabilityClass {
        RunnerCapabilityClass::try_new("linux.workspace".to_owned())
            .expect("the fixture capability class is valid")
    }

    fn runner_operation_arguments_value() -> serde_json::Value {
        serde_json::json!({"remote": "origin"})
    }

    fn runner_operation_arguments() -> NormalizedToolArguments {
        NormalizedToolArguments::try_from_provider_text(
            runner_operation_arguments_value().to_string(),
        )
        .expect("the fixture arguments are canonical")
    }

    fn runner_operation_registration() -> ValidatedRunnerRegistration {
        let declaration = RunnerToolDeclaration::new(
            runner_operation_tool(),
            RunnerToolModelDefinition::try_new(
                "Fetch one configured remote".to_owned(),
                r#"{"type":"object"}"#.to_owned(),
            )
            .expect("the fixture model definition is valid"),
            ToolPermissionDefault::Auto,
            RunnerToolEffectClass::Pure,
            ToolAdmissibleLoci::RunnerOnly {
                selector: RunnerSelector::CapabilityClass(runner_operation_class()),
            },
        );
        let catalog = RunnerCatalog::try_new(
            [runner_operation_class()],
            [declaration],
            [],
            Vec::<WorkspaceCapability>::new(),
            [RunnerSandboxProfile::Ambient],
        )
        .expect("the fixture runner catalog is internally consistent");
        RunnerEnrollment::new(
            RunnerEnrollmentId::from_uuid(identity(1).into_uuid()),
            RunnerId::from_uuid(identity(1).into_uuid()),
            RunnerAuthenticationId::from_uuid(identity(1).into_uuid()),
            [runner_operation_class()],
        )
        .register(
            RunnerAdvertisement::new(
                [runner_operation_class()],
                [runner_operation_tool()],
                [],
                [],
                [RunnerSandboxProfile::Ambient],
                Vec::<RunnerRepositoryEntry>::new(),
            ),
            &catalog,
        )
        .expect("the fixture advertisement is admitted")
    }

    fn runner_operation_correlation() -> RunnerLeaseCorrelation {
        RunnerLeaseCorrelation {
            lease: RunnerLeaseId::from_uuid(identity(1).into_uuid()),
            runner: RunnerId::from_uuid(identity(1).into_uuid()),
            registration_revision: RunnerGeneration::one(),
            placement_revision: RunnerGeneration::one(),
            working_directory: RunnerWorkingDirectory::try_new(
                RUNNER_SESSION_WORKING_DIRECTORY.to_owned(),
            )
            .expect("the fixture working directory is valid"),
            sandbox: RunnerSandboxProfile::Ambient,
            tool: runner_operation_tool(),
            dispatch: ToolAttemptDispatchCorrelation::reconstitute(
                ToolAttemptDispatchCorrelationReconstitutionInput {
                    session: SessionId::from_uuid(identity(1).into_uuid()),
                    turn: TurnId::from_uuid(identity(1).into_uuid()),
                    issuing_attempt: TurnAttemptId::from_uuid(identity(1).into_uuid()),
                    request: ToolRequestId::from_uuid(identity(1).into_uuid()),
                    attempt: ToolAttemptId::from_uuid(identity(1).into_uuid()),
                    generation: ToolDispatchGeneration::first(),
                },
            ),
            generation: RunnerGeneration::one(),
        }
    }

    fn runner_operation_lease(state: RunnerLeaseState) -> RunnerLease {
        let registration = runner_operation_registration();
        let correlation = runner_operation_correlation();
        let arguments = runner_operation_arguments();
        RunnerLease::reconstitute(
            RunnerLeaseReconstitutionInput {
                lease: correlation.lease,
                dispatch: correlation.dispatch,
                runner: correlation.runner,
                registration_revision: correlation.registration_revision,
                placement_revision: correlation.placement_revision,
                working_directory: correlation.working_directory.clone(),
                sandbox: correlation.sandbox,
                tool: correlation.tool.clone(),
                arguments: arguments.clone(),
                effect: RunnerToolEffectClass::Pure,
                credential_authorization: None,
                generation: correlation.generation,
                state,
                recorded_correlation: correlation,
                recorded_session: SessionId::from_uuid(identity(1).into_uuid()),
                recorded_effect: RunnerToolEffectClass::Pure,
                recorded_arguments: arguments,
                recorded_credential_authorization: None,
                recorded_state: state,
                retry_preparation: RunnerLeaseRetryPreparation::Available,
            },
            &registration,
        )
        .expect("the fixture lease is self-consistent")
    }

    #[derive(Clone, Default)]
    struct RecordingLeaseOperationService {
        claims: Arc<std::sync::Mutex<Vec<RunnerLeaseClaimRequest>>>,
        results: Arc<std::sync::Mutex<Vec<RunnerLeaseResultRequest>>>,
    }

    impl RunnerLeaseOperationService for RecordingLeaseOperationService {
        fn claim(
            &self,
            request: RunnerLeaseClaimRequest,
        ) -> RunnerLeaseOperationFuture<'_, RunnerLease> {
            self.claims
                .lock()
                .expect("the fixture claim recorder is available")
                .push(request);
            Box::pin(std::future::ready(Ok(runner_operation_lease(
                RunnerLeaseState::Claimed,
            ))))
        }

        fn record_result(
            &self,
            request: RunnerLeaseResultRequest,
        ) -> RunnerLeaseOperationFuture<'_, RunnerLease> {
            self.results
                .lock()
                .expect("the fixture result recorder is available")
                .push(request);
            Box::pin(std::future::ready(Ok(runner_operation_lease(
                RunnerLeaseState::Completed,
            ))))
        }
    }

    #[derive(Clone, Default)]
    struct RecordingWorkspaceReadyOperationService {
        receipts: Arc<std::sync::Mutex<Vec<RunnerWorkspaceReadyReceipt>>>,
    }

    impl RunnerWorkspaceReadyOperationService for RecordingWorkspaceReadyOperationService {
        fn record_workspace_ready(
            &self,
            receipt: RunnerWorkspaceReadyReceipt,
        ) -> RunnerWorkspaceReadyOperationFuture<'_, RunnerWorkspaceReadyReceipt> {
            self.receipts
                .lock()
                .expect("the fixture workspace-ready recorder is available")
                .push(receipt.clone());
            Box::pin(std::future::ready(Ok(receipt)))
        }
    }

    #[tokio::test]
    async fn s32_inv012_inv044_workspace_ready_commit_precedes_exact_acknowledgement() {
        let workspace_ready = RecordingWorkspaceReadyOperationService::default();
        let ready = repository_workspace_ready();
        let expected_receipt = repository_workspace_ready_receipt();

        let acknowledgement = admit_workspace_ready(&workspace_ready, ready.clone())
            .await
            .expect("the committed workspace-ready receipt projects");
        let recorded = workspace_ready
            .receipts
            .lock()
            .expect("the fixture workspace-ready recorder is available")
            .pop()
            .expect("one receipt reached the durable boundary");

        assert_eq!(recorded, expected_receipt);
        assert_eq!(
            acknowledgement,
            Message::WorkspaceRecorded(WorkspaceRecorded {
                correlation: ready.correlation,
                manifest_id: ready.ready.manifest.manifest_id,
                manifest_digest: ready.ready.manifest_digest,
            })
        );
    }

    #[tokio::test]
    async fn s32_inv044_unavailable_workspace_ready_transaction_emits_no_acknowledgement() {
        let rejection = admit_workspace_ready(
            &UnavailableRunnerWorkspaceReadyOperationService,
            repository_workspace_ready(),
        )
        .await
        .expect_err("an unavailable transaction cannot emit workspace acknowledgement");

        assert_eq!(rejection.code, RejectionCode::Unavailable);
        assert_eq!(rejection.cause, RunnerRegistrationFailureCause::Policy);
    }

    #[test]
    fn s32_inv044_workspace_ready_frames_require_exact_connection_authority() {
        let ready = repository_workspace_ready();
        let matching_context = ConnectionContext {
            enrollment: identity(2),
            runner: identity(ARBITRARY_PROVISION_RUNNER_ID_SEED),
            registration_revision: ready.correlation.registration_revision,
            epoch: PositiveU64::try_new(1).expect("the fixture epoch is positive"),
        };
        let foreign_runner_context = ConnectionContext {
            enrollment: identity(2),
            runner: identity(3),
            registration_revision: ready.correlation.registration_revision,
            epoch: PositiveU64::try_new(1).expect("the fixture epoch is positive"),
        };
        let foreign_revision_context = ConnectionContext {
            enrollment: identity(2),
            runner: identity(ARBITRARY_PROVISION_RUNNER_ID_SEED),
            registration_revision: PositiveU64::try_new(
                ready.correlation.registration_revision.get() + 1,
            )
            .expect("the foreign fixture registration revision is positive"),
            epoch: PositiveU64::try_new(1).expect("the fixture epoch is positive"),
        };

        assert!(workspace_ready_matches_connection(&ready, matching_context));
        assert!(!workspace_ready_matches_connection(
            &ready,
            foreign_runner_context
        ));
        assert!(!workspace_ready_matches_connection(
            &ready,
            foreign_revision_context
        ));
    }

    #[test]
    fn s31_inv011_inv043_claimed_resume_replays_acknowledgement_before_exact_dispatch() {
        let correlation = canonical_lease_correlation();
        let claimed = runner_operation_lease(RunnerLeaseState::Claimed);

        let messages = claimed_resume_messages(&claimed)
            .expect("the canonical claimed lease projects its recovery frames");

        assert_eq!(
            messages,
            [
                Message::LeaseClaimed(signalbox_runner_wire::LeaseClaimed {
                    correlation: correlation.clone(),
                }),
                Message::Dispatch(signalbox_runner_wire::Dispatch {
                    correlation,
                    normalized_arguments: runner_operation_arguments_value(),
                }),
            ]
        );
    }

    #[tokio::test]
    async fn s16_inv043_claim_commit_precedes_exact_acknowledgement_and_dispatch_projection() {
        let operations = RecordingLeaseOperationService::default();
        let wire_correlation = canonical_lease_correlation();

        let (acknowledgement, dispatch) = admit_lease_claim(
            &operations,
            LeaseClaim {
                correlation: wire_correlation.clone(),
            },
        )
        .await
        .expect("the committed claim projects")
        .expect("the fixture transaction admits the claim");
        let recorded = operations
            .claims
            .lock()
            .expect("the fixture claim recorder is available")
            .pop()
            .expect("one claim reached the transaction");

        assert_eq!(
            recorded,
            RunnerLeaseClaimRequest::new(runner_operation_correlation())
        );
        assert_eq!(
            acknowledgement,
            Message::LeaseClaimed(signalbox_runner_wire::LeaseClaimed {
                correlation: wire_correlation.clone(),
            })
        );
        assert_eq!(
            dispatch,
            Message::Dispatch(signalbox_runner_wire::Dispatch {
                correlation: wire_correlation,
                normalized_arguments: runner_operation_arguments_value(),
            })
        );
    }

    #[tokio::test]
    async fn s12_inv043_result_commit_precedes_exact_recorded_acknowledgement() {
        let operations = RecordingLeaseOperationService::default();
        let wire_correlation = canonical_lease_correlation();
        let result_text = String::from("fetched");

        let acknowledgement = admit_lease_result(
            &operations,
            ResultFrame {
                correlation: wire_correlation.clone(),
                result: signalbox_runner_wire::TerminalResult::Success {
                    text: result_text.clone(),
                },
            },
        )
        .await
        .expect("the committed result projects")
        .expect("the fixture transaction admits the result");
        let recorded = operations
            .results
            .lock()
            .expect("the fixture result recorder is available")
            .pop()
            .expect("one result reached the transaction");

        assert_eq!(
            recorded,
            RunnerLeaseResultRequest::new(
                runner_operation_correlation(),
                signalbox_domain::ToolAttemptObservation::Completed {
                    result: ToolResultContent::Text(
                        ToolResultText::try_new(result_text)
                            .expect("the fixture result text is bounded"),
                    ),
                },
            )
        );
        assert_eq!(
            acknowledgement,
            Message::ResultRecorded(signalbox_runner_wire::ResultRecorded {
                correlation: wire_correlation,
            })
        );
    }

    #[tokio::test]
    async fn s16_inv043_unavailable_claim_transaction_emits_no_capability_frames() {
        let rejection = admit_lease_claim(
            &UnavailableRunnerLeaseOperationService,
            LeaseClaim {
                correlation: canonical_lease_correlation(),
            },
        )
        .await
        .expect("the unavailable disposition is representable")
        .expect_err("an unavailable transaction cannot emit acknowledgement or dispatch");

        assert_eq!(rejection.code, RejectionCode::Unavailable);
        assert_eq!(rejection.cause, RunnerRegistrationFailureCause::Policy);
    }

    #[test]
    fn s16_inv043_lease_frames_are_bound_to_the_established_runner_identity() {
        let matching_context = ConnectionContext {
            enrollment: identity(2),
            runner: identity(1),
            registration_revision: PositiveU64::try_new(1)
                .expect("the fixture registration revision is positive"),
            epoch: PositiveU64::try_new(1).expect("the fixture epoch is positive"),
        };
        let foreign_context = ConnectionContext {
            enrollment: identity(2),
            runner: identity(3),
            registration_revision: PositiveU64::try_new(1)
                .expect("the fixture registration revision is positive"),
            epoch: PositiveU64::try_new(1).expect("the fixture epoch is positive"),
        };
        let correlation = canonical_lease_correlation();

        assert!(lease_correlation_matches_connection(
            &correlation,
            matching_context
        ));
        assert!(!lease_correlation_matches_connection(
            &correlation,
            foreign_context
        ));
    }

    #[track_caller]
    fn expect_applied_transition(
        effect: RunnerConnectionTransitionEffect,
    ) -> AppliedRunnerConnectionTransition {
        match effect {
            RunnerConnectionTransitionEffect::Applied(applied) => applied,
            RunnerConnectionTransitionEffect::Unchanged(outcome) => {
                panic!("expected an applied connection transition, observed {outcome:?}")
            }
        }
    }

    fn challenge_tick(state: &mut HeartbeatState) -> Heartbeat {
        let HeartbeatTick::Challenge(challenge) = state
            .next_tick()
            .expect("the next heartbeat tick is representable")
        else {
            panic!("the fixture has no outstanding heartbeat");
        };
        challenge
    }

    fn empty_advertisement() -> signalbox_runner_wire::Advertisement {
        signalbox_runner_wire::Advertisement {
            capability_classes: Vec::new(),
            tools: Vec::new(),
            workspace_capabilities: Vec::new(),
            sandbox_profiles: Vec::new(),
            credential_profiles: Vec::new(),
            repositories: Vec::new(),
        }
    }

    fn configured_advertisement() -> signalbox_runner_wire::Advertisement {
        let profile = signalbox_runner_wire::ProfileName::try_new(
            REGISTRATION_ONLY_CREDENTIAL_PROFILE.to_owned(),
        )
        .expect("the configured credential profile is checked");
        signalbox_runner_wire::Advertisement {
            capability_classes: Vec::new(),
            tools: Vec::new(),
            workspace_capabilities: Vec::new(),
            sandbox_profiles: Vec::new(),
            credential_profiles: vec![profile.clone()],
            repositories: vec![signalbox_runner_wire::RepositoryEntry {
                key: signalbox_runner_wire::RepositoryKey::try_new(
                    CONFIGURED_REPOSITORY.to_owned(),
                )
                .expect("the configured repository key is checked"),
                credential_profile: Some(profile),
            }],
        }
    }

    fn workspace_provision_correlation() -> signalbox_runner_wire::ProvisionCorrelation {
        signalbox_runner_wire::ProvisionCorrelation {
            authorization_id: identity(ARBITRARY_PROVISION_AUTHORIZATION_ID_SEED),
            session_id: identity(ARBITRARY_PROVISION_SESSION_ID_SEED),
            placement_revision: PositiveU64::try_new(ARBITRARY_PROVISION_PLACEMENT_REVISION)
                .expect("the fixture placement revision is positive"),
            runner_id: identity(ARBITRARY_PROVISION_RUNNER_ID_SEED),
            registration_revision: PositiveU64::try_new(ARBITRARY_PROVISION_REGISTRATION_REVISION)
                .expect("the fixture registration revision is positive"),
            repository: None,
            sandbox_profile: signalbox_runner_wire::SandboxProfile::Ambient,
            credential_profile: None,
        }
    }

    fn repository_workspace_provision_correlation() -> signalbox_runner_wire::ProvisionCorrelation {
        signalbox_runner_wire::ProvisionCorrelation {
            authorization_id: identity(ARBITRARY_PROVISION_AUTHORIZATION_ID_SEED),
            session_id: identity(ARBITRARY_PROVISION_SESSION_ID_SEED),
            placement_revision: PositiveU64::try_new(ARBITRARY_PROVISION_PLACEMENT_REVISION)
                .expect("the fixture placement revision is positive"),
            runner_id: identity(ARBITRARY_PROVISION_RUNNER_ID_SEED),
            registration_revision: PositiveU64::try_new(ARBITRARY_PROVISION_REGISTRATION_REVISION)
                .expect("the fixture registration revision is positive"),
            repository: Some(
                signalbox_runner_wire::RepositoryKey::try_new(CONFIGURED_REPOSITORY.to_owned())
                    .expect("the fixture repository key is checked"),
            ),
            sandbox_profile: signalbox_runner_wire::SandboxProfile::WorkspaceRestricted,
            credential_profile: Some(
                signalbox_runner_wire::ProfileName::try_new(
                    REGISTRATION_ONLY_CREDENTIAL_PROFILE.to_owned(),
                )
                .expect("the fixture credential profile is checked"),
            ),
        }
    }

    fn workspace_revision_text() -> String {
        "a".repeat(40)
    }

    fn clone_url_digest_text() -> String {
        "b".repeat(64)
    }

    fn repository_workspace_ready() -> WorkspaceReady {
        let correlation = repository_workspace_provision_correlation();
        let manifest = signalbox_runner_wire::WorkspaceManifest {
            lifecycle: signalbox_runner_wire::ManifestLifecycle::Ready,
            manifest_id: identity(ARBITRARY_WORKSPACE_MANIFEST_ID_SEED),
            session: correlation.session_id,
            placement_revision: correlation.placement_revision,
            runner: correlation.runner_id,
            repository: correlation.repository.clone(),
            canonical_clone_url_digest: Some(
                signalbox_runner_wire::Digest::try_new(clone_url_digest_text())
                    .expect("the fixture clone URL digest is canonical"),
            ),
            credential_profile: correlation.credential_profile.clone(),
            sandbox_profile: correlation.sandbox_profile,
            relative_path: format!(
                "sessions/{}/{}/repo",
                correlation.session_id,
                correlation.placement_revision.get()
            ),
            recovery: Some(WireWorkspaceRecovery::Commit {
                revision: workspace_revision_text(),
            }),
        };
        let manifest_digest = signalbox_runner_wire::workspace_manifest_digest(&manifest)
            .expect("the fixture ready manifest has a canonical digest");
        WorkspaceReady {
            correlation,
            ready: signalbox_runner_wire::ReadyManifest {
                manifest,
                manifest_digest,
            },
        }
    }

    fn repository_workspace_ready_receipt() -> RunnerWorkspaceReadyReceipt {
        let correlation = repository_workspace_provision_correlation();
        let ready = repository_workspace_ready();
        RunnerWorkspaceReadyReceipt::new(
            WorkspaceProvisioningAuthorizationId::from_uuid(
                correlation.authorization_id.into_uuid(),
            ),
            SessionId::from_uuid(correlation.session_id.into_uuid()),
            RunnerGeneration::one(),
            RunnerId::from_uuid(correlation.runner_id.into_uuid()),
            WorkspaceManifestId::from_uuid(
                identity(ARBITRARY_WORKSPACE_MANIFEST_ID_SEED).into_uuid(),
            ),
            RunnerReadyManifestDigest::try_new(ready.ready.manifest_digest.as_str().to_owned())
                .expect("the fixture ready digest is canonical"),
            WorkspaceRepositoryKey::try_new(CONFIGURED_REPOSITORY.to_owned())
                .expect("the fixture repository key is checked"),
            CanonicalCloneUrlDigest::try_new(clone_url_digest_text())
                .expect("the fixture clone URL digest is canonical"),
            Some(
                CredentialProfileName::try_new(REGISTRATION_ONLY_CREDENTIAL_PROFILE.to_owned())
                    .expect("the fixture credential profile is checked"),
            ),
            RunnerSandboxProfile::WorkspaceRestricted,
            WorkspaceRelativePath::try_new(format!(
                "sessions/{}/{}/repo",
                correlation.session_id,
                correlation.placement_revision.get()
            ))
            .expect("the fixture relative path is checked"),
            WorkspaceRecovery::Commit {
                revision: WorkspaceRevision::try_new(workspace_revision_text())
                    .expect("the fixture revision is canonical"),
            },
        )
    }

    fn private_tempdir() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("the socket fixture directory exists");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("the socket fixture is owner-private");
        directory
    }

    fn empty_catalog() -> RunnerCatalog {
        RunnerCatalog::try_new([], [], [], [], [])
            .expect("the registration-only catalog is internally consistent")
    }

    fn configured_catalog() -> RunnerCatalog {
        registration_only_catalog()
            .expect("the configured registration-only catalog is internally consistent")
    }

    async fn postgres_store() -> (ContainerAsync<Postgres>, String, RunnerProtocolStore) {
        let container = Postgres::default()
            .with_user(DATABASE_USER)
            .with_password(DATABASE_PASSWORD)
            .with_db_name(DATABASE_NAME)
            .with_fsync_enabled()
            .with_tag(POSTGRES_IMAGE_TAG)
            .with_labels(disposable_test_container_labels())
            .start()
            .await
            .expect("the hermetic PostgreSQL container starts");
        let host = container
            .get_host()
            .await
            .expect("the PostgreSQL host is available");
        let port = container
            .get_host_port_ipv4(5432)
            .await
            .expect("the PostgreSQL port is available");
        let database_url =
            format!("postgres://{DATABASE_USER}:{DATABASE_PASSWORD}@{host}:{port}/{DATABASE_NAME}");
        let pool = fresh_pool(&database_url).await;
        migrate(&pool)
            .await
            .expect("the runner schema migration succeeds");
        migrate(&pool)
            .await
            .expect("the runner schema migration is idempotent");
        let store = RunnerProtocolStore::new(pool, empty_catalog());
        (container, database_url, store)
    }

    async fn fresh_pool(database_url: &str) -> PgPool {
        let options = local_test_connection_options(database_url)
            .expect("the hermetic database URL is valid");
        PgPoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await
            .expect("the hermetic PostgreSQL pool connects")
    }

    fn enrolled_response(
        request_id: CanonicalUuid,
        advertisement: &signalbox_runner_wire::Advertisement,
    ) -> Enrolled {
        Enrolled {
            request_id,
            enrollment_id: identity(2),
            runner_id: identity(3),
            authentication_id: identity(4),
            registration_revision: PositiveU64::try_new(1)
                .expect("the first registration revision is positive"),
            connection_epoch: PositiveU64::try_new(1)
                .expect("the first connection epoch is positive"),
            advertisement_digest: advertisement_digest(advertisement)
                .expect("the advertisement has a digest"),
        }
    }

    async fn enroll_over(
        stream: UnixStream,
        request_id: CanonicalUuid,
        advertisement: signalbox_runner_wire::Advertisement,
    ) -> Message {
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        write_message(
            &mut writer,
            Message::Enroll(Enroll {
                request_id,
                digest_version: DIGEST_VERSION,
                advertisement,
            }),
        )
        .await
        .expect("the enrollment request is sent");
        read_frame(&mut reader)
            .await
            .expect("the enrollment response is received")
            .message
    }

    async fn enroll_with_service(
        service: PostgresRunnerRegistrationService,
        request_id: CanonicalUuid,
        advertisement: signalbox_runner_wire::Advertisement,
    ) -> Enrolled {
        let (server, client) = UnixStream::pair().expect("a local runner stream pair exists");
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let server = serve_connection(server, service, shutdown);
        let client = enroll_over(client, request_id, advertisement);
        let (served, observed) = tokio::join!(server, client);
        served.expect("the closed runner connection completes");
        let Message::Enrolled(enrolled) = observed else {
            panic!("the durable service returns an enrolled receipt");
        };
        enrolled
    }

    async fn resume_with_service(
        service: PostgresRunnerRegistrationService,
        enrolled: &Enrolled,
        advertisement: signalbox_runner_wire::Advertisement,
    ) -> Resumed {
        let (server, client) = UnixStream::pair().expect("a local runner stream pair exists");
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let server = serve_connection(server, service, shutdown);
        let client = async {
            let (reader, mut writer) = client.into_split();
            let mut reader = BufReader::new(reader);
            write_message(
                &mut writer,
                Message::Resume(Box::new(Resume {
                    request_id: enrolled.request_id,
                    digest_version: DIGEST_VERSION,
                    enrollment_id: enrolled.enrollment_id,
                    runner_id: enrolled.runner_id,
                    authentication_id: enrolled.authentication_id,
                    advertisement,
                    prior_registration_revision: enrolled.registration_revision,
                    inventory: Default::default(),
                })),
            )
            .await
            .expect("the resume request is sent");
            read_frame(&mut reader)
                .await
                .expect("the resume response is received")
                .message
        };
        let (served, observed) = tokio::join!(server, client);
        served.expect("the resumed runner connection completes");
        let Message::Resumed(resumed) = observed else {
            panic!("the durable service returns a resumed receipt");
        };
        *resumed
    }

    #[tokio::test]
    async fn enrollment_returns_the_exact_durable_service_receipt() {
        let request_id = identity(1);
        let advertisement = empty_advertisement();
        let response = enrolled_response(request_id, &advertisement);
        let service = EnrollmentService {
            response: response.clone(),
        };
        let (server, client) = UnixStream::pair().expect("a local runner stream pair exists");
        let (_shutdown_sender, shutdown) = watch::channel(false);

        let server = serve_connection(server, service, shutdown);
        let client = enroll_over(client, request_id, advertisement);
        let (served, observed) = tokio::join!(server, client);

        served.expect("the closed runner connection completes");
        assert_eq!(observed, Message::Enrolled(response));
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn exact_resumed_connection_receives_brokered_workspace_release() {
        let (_container, _database_url, store) = postgres_store().await;
        let service = PostgresRunnerRegistrationService::new(store.clone(), []);
        let enrolled = service
            .enroll(Enroll {
                request_id: identity(ARBITRARY_RUNNER_ENROLLMENT_REQUEST_ID_SEED),
                digest_version: DIGEST_VERSION,
                advertisement: empty_advertisement(),
            })
            .await
            .expect("the runner enrolls before reconnecting");
        let enrollment = RunnerEnrollmentId::from_uuid(enrolled.enrollment_id().into_uuid());
        let runner = RunnerId::from_uuid(enrolled.runner_id().into_uuid());
        let broker = RunnerConnectionBroker::new();
        let (server, client) = UnixStream::pair().expect("a local runner stream pair exists");
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let (resumed_sender, resumed_receiver) = tokio::sync::oneshot::channel();
        let served = serve_connection_with_broker(server, service, broker.clone(), shutdown);
        let runner_client = async {
            let (reader, mut writer) = client.into_split();
            let mut reader = BufReader::new(reader);
            write_message(
                &mut writer,
                Message::Resume(Box::new(Resume {
                    request_id: enrolled.request_id(),
                    digest_version: DIGEST_VERSION,
                    enrollment_id: enrolled.enrollment_id(),
                    runner_id: enrolled.runner_id(),
                    authentication_id: enrolled.authentication_id(),
                    advertisement: empty_advertisement(),
                    prior_registration_revision: enrolled.registration_revision(),
                    inventory: Default::default(),
                })),
            )
            .await
            .expect("the resume request is sent");
            let _receipt = read_frame(&mut reader)
                .await
                .expect("the resume receipt is received");
            resumed_sender
                .send(())
                .expect("the producer awaits the resume receipt");
            read_frame(&mut reader)
                .await
                .expect("the brokered operation is received")
                .message
        };
        let operation_producer = async {
            resumed_receiver
                .await
                .expect("the runner observes its resume receipt");
            let connection = store
                .load_connection(enrollment)
                .await
                .expect("the resumed connection loads")
                .expect("the resumed connection exists");
            let expected = Message::WorkspaceRelease(signalbox_runner_wire::WorkspaceRelease {
                correlation: signalbox_runner_wire::ReleaseCorrelation {
                    session_id: identity(ARBITRARY_PROVISION_SESSION_ID_SEED),
                    placement_revision: PositiveU64::try_new(
                        ARBITRARY_PROVISION_PLACEMENT_REVISION,
                    )
                    .expect("the fixture placement revision is positive"),
                    runner_id: enrolled.runner_id(),
                    manifest_id: identity(ARBITRARY_PROVISION_AUTHORIZATION_ID_SEED),
                },
            });
            broker
                .send(
                    RunnerConnectionAddress::new(enrollment, runner, connection.epoch()),
                    expected.clone(),
                )
                .expect("the exact resumed connection accepts the operation");
            expected
        };
        let (served, observed, expected) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(served, runner_client, operation_producer)
        })
        .await
        .expect("the resumed broker exchange completes within its test deadline");

        served.expect("the runner connection closes cleanly");
        assert_eq!(observed, expected);
    }

    #[tokio::test]
    async fn local_runner_peer_matches_the_daemon_effective_user() {
        let (server, _client) = UnixStream::pair().expect("a local runner stream pair exists");

        verify_runner_peer(&server).expect("the same-process peer passes same-user admission");
    }

    #[tokio::test]
    async fn listener_guard_drop_removes_the_owned_public_socket() {
        let directory = private_tempdir();
        let path = directory.path().join("runner.sock");
        let listener = LocalProcessListener::bind(&path).expect("the runner listener binds");
        let guard = RunnerListenerGuard::new(listener);

        drop(guard);

        assert!(!path.exists());
    }

    #[tokio::test]
    async fn unpolled_runtime_future_removes_the_owned_public_socket() {
        let directory = private_tempdir();
        let path = directory.path().join("runner.sock");
        let listener = LocalProcessListener::bind(&path).expect("the runner listener binds");
        let request_id = identity(1);
        let advertisement = empty_advertisement();
        let response = enrolled_response(request_id, &advertisement);
        let service = EnrollmentService { response };
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let future = RunnerProtocolRuntime::new(listener, service).run(shutdown);

        drop(future);

        assert!(!path.exists());
    }

    #[test]
    fn rejection_causes_select_honest_terminal_transitions() {
        assert_eq!(
            rejection_terminal_transition(RunnerRegistrationFailureCause::PeerInput),
            RunnerConnectionTransition::ProtocolFailure,
        );
        assert_eq!(
            rejection_terminal_transition(RunnerRegistrationFailureCause::Database),
            RunnerConnectionTransition::TransportClosed,
        );
    }

    #[tokio::test]
    async fn stale_heartbeat_timeout_receives_epoch_evidence() {
        let request_id = identity(1);
        let advertisement = empty_advertisement();
        let response = enrolled_response(request_id, &advertisement);
        let context = ConnectionContext {
            enrollment: response.enrollment_id,
            runner: response.runner_id,
            registration_revision: response.registration_revision,
            epoch: response.connection_epoch,
        };
        let service = EnrollmentService { response };
        let (server, client) = UnixStream::pair().expect("a local runner stream pair exists");
        let (_server_reader, mut server_writer) = server.into_split();
        let (client_reader, _client_writer) = client.into_split();
        let mut client_reader = BufReader::new(client_reader);

        let terminalized = terminalize_heartbeat_timeout(&service, context, &mut server_writer);
        let received = read_frame(&mut client_reader);
        let (terminalized, received) = tokio::join!(terminalized, received);
        let expected = Message::Rejected(
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Heartbeat,
                AvailableCorrelation::ConnectionEpoch(context.epoch),
                RejectionCode::StaleConnection,
            )
            .into_rejected(),
        );

        terminalized.expect("the stale timeout writes its rejection");
        assert_eq!(
            received
                .expect("the stale timeout rejection is received")
                .message,
            expected,
        );
    }

    #[tokio::test]
    async fn incomplete_handshake_expires_with_typed_evidence() {
        let request_id = identity(1);
        let advertisement = empty_advertisement();
        let service = EnrollmentService {
            response: enrolled_response(request_id, &advertisement),
        };
        let (server, _client) = UnixStream::pair().expect("a local runner stream pair exists");
        let (_shutdown_sender, shutdown) = watch::channel(false);

        let error = serve_connection_with_handshake_timeout(
            server,
            service,
            shutdown,
            Duration::from_millis(1),
        )
        .await
        .expect_err("an incomplete handshake expires");

        assert!(matches!(
            error,
            RunnerProtocolRuntimeError::HandshakeTimeout
        ));
    }

    #[tokio::test]
    async fn malformed_initial_frame_receives_a_typed_rejection() {
        let request_id = identity(1);
        let advertisement = empty_advertisement();
        let service = EnrollmentService {
            response: enrolled_response(request_id, &advertisement),
        };
        let (server, client) = UnixStream::pair().expect("a local runner stream pair exists");
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let server = serve_connection(server, service, shutdown);
        let client = async {
            let (reader, mut writer) = client.into_split();
            let mut reader = BufReader::new(reader);
            writer
                .write_all(b"{\n")
                .await
                .expect("the malformed frame is sent");
            read_frame(&mut reader)
                .await
                .expect("the typed rejection is received")
                .message
        };

        let (served, observed) = tokio::join!(server, client);
        let expected = Message::Rejected(
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Registration,
                AvailableCorrelation::None,
                RejectionCode::MalformedFrame,
            )
            .into_rejected(),
        );

        served.expect("the malformed connection closes after rejection");
        assert_eq!(observed, expected);
    }

    #[tokio::test]
    async fn unsupported_initial_version_receives_a_typed_rejection() {
        let request_id = identity(1);
        let advertisement = empty_advertisement();
        let response = enrolled_response(request_id, &advertisement);
        let service = EnrollmentService { response };
        let (server, client) = UnixStream::pair().expect("a local runner stream pair exists");
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let server = serve_connection(server, service, shutdown);
        let client = async {
            let (reader, mut writer) = client.into_split();
            let mut reader = BufReader::new(reader);
            let frame = Frame::try_new(Message::Enroll(Enroll {
                request_id,
                digest_version: DIGEST_VERSION,
                advertisement,
            }))
            .expect("the fixture enrollment frame is valid");
            let encoded = String::from_utf8(
                encode_line(&frame).expect("the fixture enrollment frame encodes"),
            )
            .expect("the encoded fixture is UTF-8")
            .replacen("\"version\":1", "\"version\":2", 1);
            writer
                .write_all(encoded.as_bytes())
                .await
                .expect("the unsupported frame is sent");
            read_frame(&mut reader)
                .await
                .expect("the typed rejection is received")
                .message
        };

        let (served, observed) = tokio::join!(server, client);
        let expected = Message::Rejected(
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Registration,
                AvailableCorrelation::None,
                RejectionCode::UnsupportedVersion,
            )
            .into_rejected(),
        );

        served.expect("the unsupported connection closes after rejection");
        assert_eq!(observed, expected);
    }

    #[tokio::test]
    async fn initial_shutdown_rejection_preserves_its_epoch_evidence() {
        let request_id = identity(1);
        let advertisement = empty_advertisement();
        let service = EnrollmentService {
            response: enrolled_response(request_id, &advertisement),
        };
        let epoch = PositiveU64::try_new(7).expect("the fixture epoch is positive");
        let (server, client) = UnixStream::pair().expect("a local runner stream pair exists");
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let server = serve_connection(server, service, shutdown);
        let client = async {
            let (reader, mut writer) = client.into_split();
            let mut reader = BufReader::new(reader);
            write_message(
                &mut writer,
                Message::Shutdown(Shutdown {
                    connection_epoch: epoch,
                    reason: ShutdownReason::RunnerShutdown,
                }),
            )
            .await
            .expect("the initial shutdown frame is sent");
            read_frame(&mut reader)
                .await
                .expect("the typed rejection is received")
                .message
        };

        let (served, observed) = tokio::join!(server, client);
        let expected = Message::Rejected(
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Shutdown,
                AvailableCorrelation::ConnectionEpoch(epoch),
                RejectionCode::CorrelationMismatch,
            )
            .into_rejected(),
        );

        served.expect("the pre-enrollment shutdown closes after rejection");
        assert_eq!(observed, expected);
    }

    #[tokio::test]
    async fn initial_workspace_provision_rejection_preserves_its_complete_correlation() {
        let correlation = workspace_provision_correlation();
        let advertisement = empty_advertisement();
        let service = EnrollmentService {
            response: enrolled_response(correlation.authorization_id, &advertisement),
        };
        let (server, client) = UnixStream::pair().expect("a local runner stream pair exists");
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let server = serve_connection(server, service, shutdown);
        let client = async {
            let (reader, mut writer) = client.into_split();
            let mut reader = BufReader::new(reader);
            write_message(
                &mut writer,
                Message::WorkspaceProvision(signalbox_runner_wire::WorkspaceProvision {
                    correlation: correlation.clone(),
                }),
            )
            .await
            .expect("the initial workspace provision frame is sent");
            read_frame(&mut reader)
                .await
                .expect("the typed rejection is received")
                .message
        };

        let (served, observed) = tokio::join!(server, client);
        let expected = Message::Rejected(
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::WorkspaceProvision,
                AvailableCorrelation::Provision(correlation),
                RejectionCode::CorrelationMismatch,
            )
            .into_rejected(),
        );

        served.expect("the pre-enrollment workspace provision closes after rejection");
        assert_eq!(observed, expected);
    }

    #[tokio::test]
    async fn initial_heartbeat_ack_rejection_preserves_its_complete_phase_correlation() {
        let correlation = workspace_provision_correlation();
        let advertisement = empty_advertisement();
        let service = EnrollmentService {
            response: enrolled_response(correlation.authorization_id, &advertisement),
        };
        let (server, client) = UnixStream::pair().expect("a local runner stream pair exists");
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let server = serve_connection(server, service, shutdown);
        let client = async {
            let (reader, mut writer) = client.into_split();
            let mut reader = BufReader::new(reader);
            write_message(
                &mut writer,
                Message::HeartbeatAck(HeartbeatAck {
                    challenge_sequence: PositiveU64::try_new(
                        ARBITRARY_HEARTBEAT_CHALLENGE_SEQUENCE,
                    )
                    .expect("the fixture challenge sequence is positive"),
                    runner_sequence: PositiveU64::try_new(ARBITRARY_HEARTBEAT_RUNNER_SEQUENCE)
                        .expect("the fixture runner sequence is positive"),
                    lease_phase: None,
                    workspace_phase: Some(HeartbeatWorkspacePhase::Provisioning {
                        correlation: correlation.clone(),
                    }),
                }),
            )
            .await
            .expect("the initial heartbeat acknowledgement is sent");
            read_frame(&mut reader)
                .await
                .expect("the typed rejection is received")
                .message
        };

        let (served, observed) = tokio::join!(server, client);
        let expected = Message::Rejected(
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::HeartbeatAck,
                AvailableCorrelation::Provision(correlation),
                RejectionCode::CorrelationMismatch,
            )
            .into_rejected(),
        );

        served.expect("the pre-enrollment heartbeat acknowledgement closes after rejection");
        assert_eq!(observed, expected);
    }

    #[tokio::test]
    async fn second_peer_enrolls_while_first_peer_is_stalled() {
        let directory = private_tempdir();
        let path = directory.path().join("runner.sock");
        let listener = LocalProcessListener::bind(&path).expect("the guarded listener binds");
        let request_id = identity(1);
        let advertisement = empty_advertisement();
        let response = enrolled_response(request_id, &advertisement);
        let service = EnrollmentService {
            response: response.clone(),
        };
        let (shutdown_sender, shutdown) = watch::channel(false);
        let runtime = tokio::spawn(RunnerProtocolRuntime::new(listener, service).run(shutdown));
        let stalled = UnixStream::connect(&path)
            .await
            .expect("the stalled peer connects");
        let active = UnixStream::connect(&path)
            .await
            .expect("the active peer connects");

        let observed = tokio::time::timeout(
            Duration::from_secs(1),
            enroll_over(active, request_id, advertisement),
        )
        .await
        .expect("the active peer is served without waiting for the stalled peer");
        shutdown_sender
            .send(true)
            .expect("the runtime shutdown receiver remains live");
        runtime
            .await
            .expect("the runtime task joins")
            .expect("the runtime stops cleanly");
        drop(stalled);

        assert_eq!(observed, Message::Enrolled(response));
    }

    #[tokio::test]
    async fn unframed_peer_does_not_block_runtime_shutdown() {
        let directory = private_tempdir();
        let path = directory.path().join("runner.sock");
        let listener = LocalProcessListener::bind(&path).expect("the guarded listener binds");
        let request_id = identity(1);
        let advertisement = empty_advertisement();
        let response = enrolled_response(request_id, &advertisement);
        let service = EnrollmentService { response };
        let (shutdown_sender, shutdown) = watch::channel(false);
        let runtime = tokio::spawn(RunnerProtocolRuntime::new(listener, service).run(shutdown));
        let stalled = UnixStream::connect(&path)
            .await
            .expect("the unframed peer connects");

        shutdown_sender
            .send(true)
            .expect("the runtime shutdown receiver remains live");
        tokio::time::timeout(Duration::from_secs(1), runtime)
            .await
            .expect("runtime shutdown is bounded")
            .expect("the runtime task joins")
            .expect("the runtime stops cleanly");
        drop(stalled);

        assert!(!path.exists());
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn enrollment_against_postgres_returns_the_durable_identity_receipt() {
        let (_container, _database_url, store) = postgres_store().await;
        let request_id = identity(1);
        let advertisement = empty_advertisement();
        let service = PostgresRunnerRegistrationService::new(store.clone(), []);

        let enrolled = enroll_with_service(service, request_id, advertisement).await;
        let enrollment = store
            .load_enrollment(RunnerEnrollmentId::from_uuid(
                enrolled.enrollment_id.into_uuid(),
            ))
            .await
            .expect("the durable enrollment loads")
            .expect("the durable enrollment exists");
        let registration = store
            .load_current_registration(&enrollment)
            .await
            .expect("the durable registration loads")
            .expect("the durable registration exists");
        let observed = (
            enrollment.runner().into_uuid(),
            enrollment.authentication().into_uuid(),
            registration.revision().get(),
        );
        let expected = (
            enrolled.runner_id.into_uuid(),
            enrolled.authentication_id.into_uuid(),
            enrolled.registration_revision.get(),
        );

        assert_eq!(observed, expected);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn lost_active_runner_admits_one_pending_successor_over_the_wire() {
        let (_container, _database_url, store) = postgres_store().await;
        let service = PostgresRunnerRegistrationService::new(store, []);
        let advertisement = empty_advertisement();
        let active = service
            .enroll(Enroll {
                request_id: identity(1),
                digest_version: DIGEST_VERSION,
                advertisement: advertisement.clone(),
            })
            .await
            .expect("the initial runner receives active authority");
        service
            .transition_connection(
                active.enrollment_id(),
                active.connection_epoch(),
                RunnerConnectionTransition::TransportClosed,
            )
            .await
            .expect("the predecessor connection loss commits");
        let pending_request = identity(2);
        let pending = service
            .enroll(Enroll {
                request_id: pending_request,
                digest_version: DIGEST_VERSION,
                advertisement: advertisement.clone(),
            })
            .await
            .expect("the lost predecessor admits one pending successor");
        let (server, client) = UnixStream::pair().expect("a local runner stream pair exists");
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let server = serve_connection(server, service, shutdown);
        let replay = enroll_over(client, pending_request, advertisement.clone());
        let (served, observed) = tokio::join!(server, replay);
        let expected = Message::ReplacementPending(ReplacementPending {
            request_id: pending_request,
            enrollment_id: pending.enrollment_id(),
            runner_id: pending.runner_id(),
            authentication_id: pending.authentication_id(),
            registration_revision: pending.registration_revision(),
            connection_epoch: PositiveU64::try_new(2)
                .expect("the replay connection epoch is positive"),
            advertisement_digest: advertisement_digest(&advertisement)
                .expect("the advertisement has a digest"),
        });

        served.expect("the replayed pending connection completes");
        assert_eq!(observed, expected);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn restart_resumes_the_same_registration_through_a_fresh_pool() {
        let (_container, database_url, store) = postgres_store().await;
        let request_id = identity(1);
        let advertisement = empty_advertisement();
        let enrolled = enroll_with_service(
            PostgresRunnerRegistrationService::new(store, []),
            request_id,
            advertisement.clone(),
        )
        .await;
        let restarted_store =
            RunnerProtocolStore::new(fresh_pool(&database_url).await, empty_catalog());
        let resumed = resume_with_service(
            PostgresRunnerRegistrationService::new(restarted_store, []),
            &enrolled,
            advertisement,
        )
        .await;
        let expected = Resumed {
            registration_revision: enrolled.registration_revision,
            connection_epoch: PositiveU64::try_new(2)
                .expect("the resumed connection epoch is positive"),
            directives: ReconnectDirectives::default(),
        };

        assert_eq!(resumed, expected);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn configured_capability_advertisement_round_trips_exactly() {
        let (_container, database_url, _empty_store) = postgres_store().await;
        let store = RunnerProtocolStore::new(fresh_pool(&database_url).await, configured_catalog());
        let request_id = identity(1);
        let advertisement = configured_advertisement();
        let expected = advertisement
            .clone()
            .try_into_domain()
            .expect("the configured advertisement is domain-valid");
        let enrolled = enroll_with_service(
            PostgresRunnerRegistrationService::new(store.clone(), []),
            request_id,
            advertisement,
        )
        .await;
        let receipt = store
            .resume_registration(
                RunnerEnrollmentRequestId::from_uuid(request_id.into_uuid()),
                IssuedRunnerEnrollmentIdentities::new(
                    RunnerEnrollmentId::from_uuid(enrolled.enrollment_id.into_uuid()),
                    RunnerId::from_uuid(enrolled.runner_id.into_uuid()),
                    RunnerAuthenticationId::from_uuid(enrolled.authentication_id.into_uuid()),
                ),
                RunnerRegistrationRevision::first(),
                expected.clone(),
            )
            .await
            .expect("the exact persisted advertisement resumes");

        assert_eq!(receipt.advertisement(), expected);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn runner_shutdown_is_a_durable_shutdown_state() {
        let (_container, _database_url, store) = postgres_store().await;
        let service = PostgresRunnerRegistrationService::new(store.clone(), []);
        let enrolled = service
            .enroll(Enroll {
                request_id: identity(ARBITRARY_RUNNER_ENROLLMENT_REQUEST_ID_SEED),
                digest_version: DIGEST_VERSION,
                advertisement: empty_advertisement(),
            })
            .await
            .expect("the real registration service enrolls the runner");

        service
            .transition_connection(
                enrolled.enrollment_id(),
                enrolled.connection_epoch(),
                RunnerConnectionTransition::RunnerShutdown,
            )
            .await
            .expect("the epoch-targeted shutdown commits");
        let observed = store
            .load_connection(RunnerEnrollmentId::from_uuid(
                enrolled.enrollment_id().into_uuid(),
            ))
            .await
            .expect("the shutdown state loads")
            .expect("the connection lifecycle exists");

        assert_eq!(observed.state(), RunnerConnectionState::Shutdown);
        assert_eq!(observed.cause(), RunnerConnectionCause::RunnerShutdown);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn stale_shutdown_epoch_is_fatal_protocol_loss() {
        let (_container, _database_url, store) = postgres_store().await;
        let service = PostgresRunnerRegistrationService::new(store.clone(), []);
        let (server, client) = UnixStream::pair().expect("a local runner stream pair exists");
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let server = serve_connection(server, service, shutdown);
        let runner = async {
            let (reader, mut writer) = client.into_split();
            let mut reader = BufReader::new(reader);
            write_message(
                &mut writer,
                Message::Enroll(Enroll {
                    request_id: identity(1),
                    digest_version: DIGEST_VERSION,
                    advertisement: empty_advertisement(),
                }),
            )
            .await
            .expect("the runner enrolls");
            let Message::Enrolled(enrolled) = read_frame(&mut reader)
                .await
                .expect("the enrollment acknowledgement is received")
                .message
            else {
                panic!("the runner receives an enrollment acknowledgement");
            };
            let stale_epoch = PositiveU64::try_new(enrolled.connection_epoch.get() + 1)
                .expect("the stale epoch fixture is positive");
            write_message(
                &mut writer,
                Message::Shutdown(Shutdown {
                    connection_epoch: stale_epoch,
                    reason: ShutdownReason::RunnerShutdown,
                }),
            )
            .await
            .expect("the stale shutdown order is sent");
            let refused = read_frame(&mut reader)
                .await
                .expect("the fatal stale-epoch rejection is received")
                .message;
            (enrolled, stale_epoch, refused)
        };

        let (served, (enrolled, stale_epoch, refused)) = tokio::join!(server, runner);
        let observed = store
            .load_connection(RunnerEnrollmentId::from_uuid(
                enrolled.enrollment_id.into_uuid(),
            ))
            .await
            .expect("the terminal connection state loads")
            .expect("the connection lifecycle exists");
        let expected = Message::Rejected(
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Shutdown,
                AvailableCorrelation::ConnectionEpoch(stale_epoch),
                RejectionCode::StaleConnection,
            )
            .into_rejected(),
        );

        served.expect("the fatal stale shutdown closes the connection task");
        assert_eq!(refused, expected);
        assert_eq!(observed.state(), RunnerConnectionState::Lost);
        assert_eq!(observed.cause(), RunnerConnectionCause::ProtocolFailure);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn hub_shutdown_sends_the_assigned_epoch_after_durable_shutdown() {
        let (_container, _database_url, store) = postgres_store().await;
        let service = PostgresRunnerRegistrationService::new(store.clone(), []);
        let (server, client) = UnixStream::pair().expect("a local runner stream pair exists");
        let (shutdown_sender, shutdown) = watch::channel(false);
        let served = serve_connection(server, service, shutdown);
        let runner = async {
            let (reader, mut writer) = client.into_split();
            let mut reader = BufReader::new(reader);
            write_message(
                &mut writer,
                Message::Enroll(Enroll {
                    request_id: identity(1),
                    digest_version: DIGEST_VERSION,
                    advertisement: empty_advertisement(),
                }),
            )
            .await
            .expect("the enrollment request is sent");
            let Message::Enrolled(enrolled) = read_frame(&mut reader)
                .await
                .expect("the enrollment response is received")
                .message
            else {
                panic!("the real hub enrolls the runner");
            };
            shutdown_sender
                .send(true)
                .expect("the connection shutdown receiver remains live");
            let shutdown = read_frame(&mut reader)
                .await
                .expect("the epoch-targeted shutdown is received")
                .message;
            (enrolled, shutdown)
        };
        let (served, (enrolled, shutdown)) = tokio::join!(served, runner);
        served.expect("the hub connection shuts down cleanly");
        let observed = store
            .load_connection(RunnerEnrollmentId::from_uuid(
                enrolled.enrollment_id.into_uuid(),
            ))
            .await
            .expect("the shutdown state loads")
            .expect("the connection lifecycle exists");

        assert_eq!(
            shutdown,
            Message::Shutdown(Shutdown {
                connection_epoch: enrolled.connection_epoch,
                reason: ShutdownReason::DaemonShutdown,
            })
        );
        assert_eq!(observed.state(), RunnerConnectionState::Shutdown);
        assert_eq!(observed.cause(), RunnerConnectionCause::DaemonShutdown);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn abrupt_transport_death_is_durably_lost_not_healthy() {
        let (_container, _database_url, store) = postgres_store().await;
        let service = PostgresRunnerRegistrationService::new(store.clone(), []);
        let enrolled = service
            .enroll(Enroll {
                request_id: identity(1),
                digest_version: DIGEST_VERSION,
                advertisement: empty_advertisement(),
            })
            .await
            .expect("the real registration service enrolls the runner");

        service
            .transition_connection(
                enrolled.enrollment_id(),
                enrolled.connection_epoch(),
                RunnerConnectionTransition::TransportClosed,
            )
            .await
            .expect("the dead transport commits loss");
        let observed = store
            .load_connection(RunnerEnrollmentId::from_uuid(
                enrolled.enrollment_id().into_uuid(),
            ))
            .await
            .expect("the loss state loads")
            .expect("the connection lifecycle exists");

        assert_eq!(observed.state(), RunnerConnectionState::Lost);
        assert_eq!(observed.cause(), RunnerConnectionCause::TransportClosed);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn duplicate_transport_loss_reports_only_first_transition_as_applied() {
        let (_container, _database_url, store) = postgres_store().await;
        let service = PostgresRunnerRegistrationService::new(store.clone(), []);
        let enrolled = service
            .enroll(Enroll {
                request_id: identity(1),
                digest_version: DIGEST_VERSION,
                advertisement: empty_advertisement(),
            })
            .await
            .expect("the real registration service enrolls the runner");
        let enrollment = RunnerEnrollmentId::from_uuid(enrolled.enrollment_id().into_uuid());
        let epoch = RunnerConnectionEpoch::try_from_u64(enrolled.connection_epoch().get())
            .expect("the enrolled connection epoch is positive");

        let first = store
            .transition_connection_with_effect(
                enrollment,
                epoch,
                RunnerConnectionTransition::TransportClosed,
            )
            .await
            .expect("the first transport loss commits");
        let observed = store
            .load_connection(enrollment)
            .await
            .expect("the loss state loads")
            .expect("the connection lifecycle exists");
        let replayed = store
            .transition_connection_with_effect(
                enrollment,
                epoch,
                RunnerConnectionTransition::TransportClosed,
            )
            .await
            .expect("the repeated loss observes terminal state");
        let applied = expect_applied_transition(first);

        assert_eq!(applied.enrollment(), enrollment);
        assert_eq!(applied.snapshot(), observed);
        assert_eq!(
            replayed,
            RunnerConnectionTransitionEffect::Unchanged(
                RunnerConnectionTransitionOutcome::Current(observed)
            )
        );
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn enrollment_ack_write_failure_cannot_leave_the_runner_healthy() {
        let (_container, database_url, store) = postgres_store().await;
        let service = PostgresRunnerRegistrationService::new(store, []);
        let (server, client) = UnixStream::pair().expect("a local runner stream pair exists");
        let (client_reader, mut client_writer) = client.into_split();
        drop(client_reader);
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let served = serve_connection(server, service, shutdown);
        let runner = async {
            write_message(
                &mut client_writer,
                Message::Enroll(Enroll {
                    request_id: identity(1),
                    digest_version: DIGEST_VERSION,
                    advertisement: empty_advertisement(),
                }),
            )
            .await
            .expect("the enrollment request is sent before the read half closes");
            drop(client_writer);
        };
        let (served, ()) = tokio::join!(served, runner);
        let pool = fresh_pool(&database_url).await;
        let state: String = sqlx::query_scalar(
            "SELECT event.state_kind
               FROM runner_enrollment_request_receipt AS receipt
               JOIN runner_connection_event AS event
                 ON event.enrollment_id = receipt.enrollment_id
              WHERE receipt.request_id = $1
              ORDER BY event.connection_epoch DESC, event.event_ordinal DESC
              LIMIT 1",
        )
        .bind(identity(1).into_uuid())
        .fetch_one(&pool)
        .await
        .expect("the failed acknowledgement lifecycle head loads");

        assert!(matches!(served, Err(RunnerProtocolRuntimeError::Write(_))));
        assert_eq!(state, "lost");
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn startup_marks_a_prior_process_connection_lost_before_admission() {
        let (_container, _database_url, store) = postgres_store().await;
        let service = PostgresRunnerRegistrationService::new(store.clone(), []);
        let enrolled = service
            .enroll(Enroll {
                request_id: identity(1),
                digest_version: DIGEST_VERSION,
                advertisement: empty_advertisement(),
            })
            .await
            .expect("the prior process enrolls the runner");

        let transitions = service
            .mark_orphaned_connections_lost()
            .await
            .expect("startup classifies prior-process connection heads");
        let applied = transitions
            .first()
            .expect("the prior-process connection produces one applied loss");
        let observed = store
            .load_connection(RunnerEnrollmentId::from_uuid(
                enrolled.enrollment_id().into_uuid(),
            ))
            .await
            .expect("the startup loss state loads")
            .expect("the connection lifecycle exists");

        assert_eq!(transitions.len(), 1);
        assert_eq!(
            applied.enrollment().into_uuid(),
            enrolled.enrollment_id().into_uuid()
        );
        assert_eq!(applied.snapshot(), observed);
        assert_eq!(observed.state(), RunnerConnectionState::Lost);
        assert_eq!(observed.cause(), RunnerConnectionCause::TransportClosed);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn s32_inv044_terminal_connection_transition_propagates_loss_to_placed_sessions() {
        let (_container, database_url, store) = postgres_store().await;
        let service = PostgresRunnerRegistrationService::new(store.clone(), []);
        let enrolled = service
            .enroll(Enroll {
                request_id: identity(1),
                digest_version: DIGEST_VERSION,
                advertisement: empty_advertisement(),
            })
            .await
            .expect("the runner enrolls before session placement");
        let pool = fresh_pool(&database_url).await;
        let session = SessionId::from_uuid(uuid::Uuid::from_u128(ARBITRARY_RUNNER_SESSION_ID_SEED));
        let runner = RunnerId::from_uuid(enrolled.runner_id().into_uuid());
        create_runner_placed_session(&pool, &store, session, runner).await;

        service
            .transition_connection(
                enrolled.enrollment_id(),
                enrolled.connection_epoch(),
                RunnerConnectionTransition::TransportClosed,
            )
            .await
            .expect("the terminal transition propagates its durable loss cursor");
        let placement = store
            .load_placement(session)
            .await
            .expect("the projected placement loads")
            .expect("the runner placement remains canonical");
        let pending = store
            .load_pending_connection_losses()
            .await
            .expect("the pending cursor inventory loads");

        assert_eq!(
            placement.placement().state(),
            &SessionRunnerPlacementState::RunnerLostBeforePin(RunnerLostBeforePin::from_stored(
                runner,
            ))
        );
        assert_eq!(pending, []);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn s32_inv044_terminal_connection_replay_resumes_its_pending_loss_cursor() {
        let (_container, database_url, store) = postgres_store().await;
        let service = PostgresRunnerRegistrationService::new(store.clone(), []);
        let enrolled = service
            .enroll(Enroll {
                request_id: identity(ARBITRARY_RUNNER_ENROLLMENT_REQUEST_ID_SEED),
                digest_version: DIGEST_VERSION,
                advertisement: empty_advertisement(),
            })
            .await
            .expect("the runner enrolls before session placement");
        let pool = fresh_pool(&database_url).await;
        let session = SessionId::from_uuid(uuid::Uuid::from_u128(ARBITRARY_RUNNER_SESSION_ID_SEED));
        let runner = RunnerId::from_uuid(enrolled.runner_id().into_uuid());
        create_runner_placed_session(&pool, &store, session, runner).await;
        store
            .transition_connection_with_effect(
                RunnerEnrollmentId::from_uuid(enrolled.enrollment_id().into_uuid()),
                RunnerConnectionEpoch::try_from_u64(enrolled.connection_epoch().get())
                    .expect("the connection epoch is positive"),
                RunnerConnectionTransition::TransportClosed,
            )
            .await
            .expect("the terminal connection state commits before propagation");

        let replayed = service
            .transition_connection(
                enrolled.enrollment_id(),
                enrolled.connection_epoch(),
                RunnerConnectionTransition::TransportClosed,
            )
            .await
            .expect("the terminal replay resumes pending propagation");
        let placement = store
            .load_placement(session)
            .await
            .expect("the projected placement loads")
            .expect("the runner placement remains canonical");
        let pending = store
            .load_pending_connection_losses()
            .await
            .expect("the completed cursor inventory loads");

        assert_eq!(
            replayed,
            RunnerConnectionTransitionOutcome::Current(
                store
                    .load_connection(RunnerEnrollmentId::from_uuid(
                        enrolled.enrollment_id().into_uuid(),
                    ))
                    .await
                    .expect("the connection state loads")
                    .expect("the connection lifecycle exists")
            )
        );
        assert_eq!(
            placement.placement().state(),
            &SessionRunnerPlacementState::RunnerLostBeforePin(RunnerLostBeforePin::from_stored(
                runner,
            ))
        );
        assert_eq!(pending, []);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn s32_inv044_startup_resumes_a_previously_committed_loss_cursor() {
        let (_container, database_url, store) = postgres_store().await;
        let service = PostgresRunnerRegistrationService::new(store.clone(), []);
        let enrolled = service
            .enroll(Enroll {
                request_id: identity(ARBITRARY_RUNNER_ENROLLMENT_REQUEST_ID_SEED),
                digest_version: DIGEST_VERSION,
                advertisement: empty_advertisement(),
            })
            .await
            .expect("the prior daemon enrolls the runner");
        let pool = fresh_pool(&database_url).await;
        let session = SessionId::from_uuid(uuid::Uuid::from_u128(ARBITRARY_RUNNER_SESSION_ID_SEED));
        let runner = RunnerId::from_uuid(enrolled.runner_id().into_uuid());
        create_runner_placed_session(&pool, &store, session, runner).await;
        store
            .transition_connection_with_effect(
                RunnerEnrollmentId::from_uuid(enrolled.enrollment_id().into_uuid()),
                RunnerConnectionEpoch::try_from_u64(enrolled.connection_epoch().get())
                    .expect("the connection epoch is positive"),
                RunnerConnectionTransition::TransportClosed,
            )
            .await
            .expect("the prior daemon commits loss before propagation");
        let pending_before = store
            .load_pending_connection_losses()
            .await
            .expect("the stranded cursor inventory loads");

        let transitions = service
            .mark_orphaned_connections_lost()
            .await
            .expect("startup resumes every pending loss cursor");
        let placement = store
            .load_placement(session)
            .await
            .expect("the resumed placement loads")
            .expect("the runner placement remains canonical");
        let pending_after = store
            .load_pending_connection_losses()
            .await
            .expect("the completed cursor inventory loads");

        assert_eq!(pending_before.len(), 1);
        assert_eq!(transitions, []);
        assert_eq!(
            placement.placement().state(),
            &SessionRunnerPlacementState::RunnerLostBeforePin(RunnerLostBeforePin::from_stored(
                runner,
            ))
        );
        assert_eq!(pending_after, []);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn stale_shutdown_epoch_cannot_mutate_the_fresh_connection() {
        let (_container, _database_url, store) = postgres_store().await;
        let service = PostgresRunnerRegistrationService::new(store.clone(), []);
        let advertisement = empty_advertisement();
        let enrolled = service
            .enroll(Enroll {
                request_id: identity(1),
                digest_version: DIGEST_VERSION,
                advertisement: advertisement.clone(),
            })
            .await
            .expect("the real registration service enrolls the runner");
        let resumed = service
            .resume(Resume {
                request_id: enrolled.request_id(),
                digest_version: DIGEST_VERSION,
                enrollment_id: enrolled.enrollment_id(),
                runner_id: enrolled.runner_id(),
                authentication_id: enrolled.authentication_id(),
                advertisement,
                prior_registration_revision: enrolled.registration_revision(),
                inventory: Default::default(),
            })
            .await
            .expect("the fresh physical connection resumes");

        let refused = service
            .transition_connection(
                enrolled.enrollment_id(),
                enrolled.connection_epoch(),
                RunnerConnectionTransition::RunnerShutdown,
            )
            .await
            .expect("stale epoch detection is a typed outcome");
        let observed = store
            .load_connection(RunnerEnrollmentId::from_uuid(
                enrolled.enrollment_id().into_uuid(),
            ))
            .await
            .expect("the current connection state loads")
            .expect("the connection lifecycle exists");
        let expected_stale = RunnerConnectionTransitionOutcome::Stale {
            observed: RunnerConnectionEpoch::try_from_u64(enrolled.connection_epoch().get())
                .expect("the enrolled epoch is positive"),
            current: RunnerConnectionEpoch::try_from_u64(resumed.connection_epoch().get())
                .expect("the resumed epoch is positive"),
        };

        assert_eq!(refused, expected_stale);
        assert_eq!(observed.epoch().get(), resumed.connection_epoch().get());
        assert_eq!(observed.state(), RunnerConnectionState::Connected);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn stale_epoch_cannot_mutate_registration_through_advertise() {
        let (_container, _database_url, store) = postgres_store().await;
        let service = PostgresRunnerRegistrationService::new(store.clone(), []);
        let advertisement = empty_advertisement();
        let enrolled = service
            .enroll(Enroll {
                request_id: identity(1),
                digest_version: DIGEST_VERSION,
                advertisement: advertisement.clone(),
            })
            .await
            .expect("the real registration service enrolls the runner");
        let resumed = service
            .resume(Resume {
                request_id: enrolled.request_id(),
                digest_version: DIGEST_VERSION,
                enrollment_id: enrolled.enrollment_id(),
                runner_id: enrolled.runner_id(),
                authentication_id: enrolled.authentication_id(),
                advertisement: advertisement.clone(),
                prior_registration_revision: enrolled.registration_revision(),
                inventory: Default::default(),
            })
            .await
            .expect("the fresh physical connection resumes");

        let refused = service
            .advertise(
                enrolled.enrollment_id(),
                Advertise {
                    enrollment_id: enrolled.enrollment_id(),
                    runner_id: enrolled.runner_id(),
                    authentication_id: enrolled.authentication_id(),
                    registration_revision: resumed.registration_revision(),
                    advertisement,
                },
                enrolled.connection_epoch(),
            )
            .await
            .expect_err("the superseded epoch cannot advertise");
        let enrollment = store
            .load_enrollment(RunnerEnrollmentId::from_uuid(
                enrolled.enrollment_id().into_uuid(),
            ))
            .await
            .expect("the enrollment loads")
            .expect("the enrollment exists");
        let registration = store
            .load_current_registration(&enrollment)
            .await
            .expect("the registration head loads")
            .expect("the registration head exists");

        assert_eq!(refused.code, RejectionCode::StaleConnection);
        assert_eq!(
            registration.revision().get(),
            resumed.registration_revision().get()
        );
    }

    /// INV-042 / INV-044: the daemon does not acknowledge a changed
    /// advertisement until its durable registration cursor is complete.
    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn s31_inv042_inv044_advertise_completes_registration_reconciliation_before_ack() {
        let (_container, database_url, _empty_store) = postgres_store().await;
        let pool = fresh_pool(&database_url).await;
        let store = RunnerProtocolStore::new(pool.clone(), configured_catalog());
        let service = PostgresRunnerRegistrationService::new(store.clone(), []);
        let enrolled = service
            .enroll(Enroll {
                request_id: identity(1),
                digest_version: DIGEST_VERSION,
                advertisement: configured_advertisement(),
            })
            .await
            .expect("the configured runner enrolls");

        let registered = service
            .advertise(
                enrolled.enrollment_id(),
                Advertise {
                    enrollment_id: enrolled.enrollment_id(),
                    runner_id: enrolled.runner_id(),
                    authentication_id: enrolled.authentication_id(),
                    registration_revision: enrolled.registration_revision(),
                    advertisement: empty_advertisement(),
                },
                enrolled.connection_epoch(),
            )
            .await
            .expect("the changed advertisement is acknowledged after reconciliation");
        let state: String = sqlx::query_scalar(
            "SELECT state_kind
               FROM runner_registration_reconciliation
              WHERE enrollment_id = $1 AND registration_revision = $2",
        )
        .bind(enrolled.enrollment_id().into_uuid())
        .bind(Decimal::from(registered.registration_revision.get()))
        .fetch_one(&pool)
        .await
        .expect("the exact registration cursor loads");
        let pending = store
            .load_pending_registration_reconciliations()
            .await
            .expect("the pending registration inventory loads");

        assert_eq!(registered.registration_revision.get(), 2);
        assert_eq!(state, "completed");
        assert_eq!(pending, []);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn workspace_ready_uses_the_registration_revision_returned_by_advertise() {
        let (_container, database_url, _empty_store) = postgres_store().await;
        let store = RunnerProtocolStore::new(fresh_pool(&database_url).await, configured_catalog());
        let service = PostgresRunnerRegistrationService::new(store, []);
        let workspace_ready = RecordingWorkspaceReadyOperationService::default();
        let (server, client) = UnixStream::pair().expect("a local runner stream pair exists");
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let server = serve_connection_with_operations_and_broker(
            server,
            service,
            UnavailableRunnerLeaseOperationService,
            workspace_ready,
            RunnerConnectionBroker::new(),
            shutdown,
        );
        let client = async {
            let (reader, mut writer) = client.into_split();
            let mut reader = BufReader::new(reader);
            write_message(
                &mut writer,
                Message::Enroll(Enroll {
                    request_id: identity(1),
                    digest_version: DIGEST_VERSION,
                    advertisement: configured_advertisement(),
                }),
            )
            .await
            .expect("the configured runner enrolls");
            let Message::Enrolled(enrolled) = read_frame(&mut reader)
                .await
                .expect("the enrollment response is received")
                .message
            else {
                panic!("the runner receives its enrollment receipt");
            };
            write_message(
                &mut writer,
                Message::Advertise(Advertise {
                    enrollment_id: enrolled.enrollment_id,
                    runner_id: enrolled.runner_id,
                    authentication_id: enrolled.authentication_id,
                    registration_revision: enrolled.registration_revision,
                    advertisement: empty_advertisement(),
                }),
            )
            .await
            .expect("the changed advertisement is sent");
            let Message::Registered(registered) = read_frame(&mut reader)
                .await
                .expect("the changed advertisement is acknowledged")
                .message
            else {
                panic!("the runner receives its updated registration receipt");
            };
            let mut ready = repository_workspace_ready();
            ready.correlation.runner_id = enrolled.runner_id;
            ready.correlation.registration_revision = registered.registration_revision;
            ready.ready.manifest.runner = enrolled.runner_id;
            ready.ready.manifest_digest =
                signalbox_runner_wire::workspace_manifest_digest(&ready.ready.manifest)
                    .expect("the updated fixture manifest has a canonical digest");
            write_message(&mut writer, Message::WorkspaceReady(ready.clone()))
                .await
                .expect("workspace-ready is sent under the updated registration");
            let acknowledgement = read_frame(&mut reader)
                .await
                .expect("workspace-ready is acknowledged")
                .message;
            (enrolled, registered, ready, acknowledgement)
        };

        let (served, (enrolled, registered, ready, acknowledgement)) = tokio::join!(server, client);

        served.expect("the established connection closes cleanly");
        assert_ne!(
            registered.registration_revision,
            enrolled.registration_revision
        );
        assert_eq!(
            acknowledgement,
            Message::WorkspaceRecorded(WorkspaceRecorded {
                correlation: ready.correlation,
                manifest_id: ready.ready.manifest.manifest_id,
                manifest_digest: ready.ready.manifest_digest,
            })
        );
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn foreign_enrollment_cannot_mutate_registration_through_advertise() {
        let (_container, _database_url, store) = postgres_store().await;
        let service = PostgresRunnerRegistrationService::new(store.clone(), []);
        let advertisement = empty_advertisement();
        let enrolled = service
            .enroll(Enroll {
                request_id: identity(1),
                digest_version: DIGEST_VERSION,
                advertisement: advertisement.clone(),
            })
            .await
            .expect("the real registration service enrolls the runner");

        let refused = service
            .advertise(
                enrolled.enrollment_id(),
                Advertise {
                    enrollment_id: identity(99),
                    runner_id: enrolled.runner_id(),
                    authentication_id: enrolled.authentication_id(),
                    registration_revision: enrolled.registration_revision(),
                    advertisement,
                },
                enrolled.connection_epoch(),
            )
            .await
            .expect_err("the connection cannot advertise for another enrollment");
        let enrollment = store
            .load_enrollment(RunnerEnrollmentId::from_uuid(
                enrolled.enrollment_id().into_uuid(),
            ))
            .await
            .expect("the enrollment loads")
            .expect("the enrollment exists");
        let registration = store
            .load_current_registration(&enrollment)
            .await
            .expect("the registration head loads")
            .expect("the registration head exists");

        assert_eq!(refused.code, RejectionCode::CorrelationMismatch);
        assert_eq!(
            registration.revision().get(),
            enrolled.registration_revision().get()
        );
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn rejected_advertisement_terminalizes_the_established_connection() {
        let (_container, _database_url, store) = postgres_store().await;
        let service = PostgresRunnerRegistrationService::new(store.clone(), []);
        let advertisement = empty_advertisement();
        let request_id = identity(1);
        let (server, client) = UnixStream::pair().expect("a local runner stream pair exists");
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let server = serve_connection(server, service, shutdown);
        let client = async {
            let (reader, mut writer) = client.into_split();
            let mut reader = BufReader::new(reader);
            write_message(
                &mut writer,
                Message::Enroll(Enroll {
                    request_id,
                    digest_version: DIGEST_VERSION,
                    advertisement: advertisement.clone(),
                }),
            )
            .await
            .expect("the enrollment request is sent");
            let Message::Enrolled(enrolled) = read_frame(&mut reader)
                .await
                .expect("the enrollment response is received")
                .message
            else {
                panic!("the durable service returns an enrolled receipt");
            };
            write_message(
                &mut writer,
                Message::Advertise(Advertise {
                    enrollment_id: identity(99),
                    runner_id: enrolled.runner_id,
                    authentication_id: enrolled.authentication_id,
                    registration_revision: enrolled.registration_revision,
                    advertisement,
                }),
            )
            .await
            .expect("the foreign advertisement is sent");
            let rejected = read_frame(&mut reader)
                .await
                .expect("the advertisement rejection is received")
                .message;
            (enrolled, rejected)
        };

        let (served, (enrolled, rejected)) = tokio::join!(server, client);
        let enrollment_id = RunnerEnrollmentId::from_uuid(enrolled.enrollment_id.into_uuid());
        let connection = store
            .load_connection(enrollment_id)
            .await
            .expect("the terminal connection loads")
            .expect("the connection lifecycle exists");
        let expected = Message::Rejected(
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Advertise,
                AvailableCorrelation::Registration(enrolled.registration_revision),
                RejectionCode::CorrelationMismatch,
            )
            .into_rejected(),
        );

        served.expect("the rejected connection closes after terminalization");
        assert_eq!(rejected, expected);
        assert_eq!(connection.state(), RunnerConnectionState::Lost);
        assert_eq!(connection.cause(), RunnerConnectionCause::ProtocolFailure);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn invalid_heartbeat_terminalizes_before_failed_rejection_write() {
        let (_container, _database_url, store) = postgres_store().await;
        let service = PostgresRunnerRegistrationService::new(store.clone(), []);
        let advertisement = empty_advertisement();
        let request_id = identity(1);
        let (server, client) = UnixStream::pair().expect("a local runner stream pair exists");
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let server = serve_connection(server, service, shutdown);
        let client = async {
            let (reader, mut writer) = client.into_split();
            let mut reader = BufReader::new(reader);
            write_message(
                &mut writer,
                Message::Enroll(Enroll {
                    request_id,
                    digest_version: DIGEST_VERSION,
                    advertisement,
                }),
            )
            .await
            .expect("the enrollment request is sent");
            let Message::Enrolled(enrolled) = read_frame(&mut reader)
                .await
                .expect("the enrollment response is received")
                .message
            else {
                panic!("the durable service returns an enrolled receipt");
            };
            rustix::net::shutdown(reader.get_ref().as_ref(), rustix::net::Shutdown::Read)
                .expect("the peer refuses every later inbound frame");
            write_message(
                &mut writer,
                Message::HeartbeatAck(HeartbeatAck {
                    challenge_sequence: PositiveU64::try_new(1)
                        .expect("the unsolicited challenge sequence is positive"),
                    runner_sequence: PositiveU64::try_new(1)
                        .expect("the first runner sequence is positive"),
                    lease_phase: None,
                    workspace_phase: None,
                }),
            )
            .await
            .expect("the invalid heartbeat acknowledgement is sent");
            enrolled
        };

        let (served, enrolled) = tokio::join!(server, client);
        let enrollment_id = RunnerEnrollmentId::from_uuid(enrolled.enrollment_id.into_uuid());
        let connection = store
            .load_connection(enrollment_id)
            .await
            .expect("the terminal connection loads")
            .expect("the connection lifecycle exists");

        let failure = served.expect_err("the peer rejects the outbound failure evidence");
        assert!(matches!(failure, RunnerProtocolRuntimeError::Write(_)));
        assert_eq!(connection.state(), RunnerConnectionState::Lost);
        assert_eq!(connection.cause(), RunnerConnectionCause::ProtocolFailure);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn wrong_direction_shutdown_terminalizes_the_connection() {
        let (_container, _database_url, store) = postgres_store().await;
        let service = PostgresRunnerRegistrationService::new(store.clone(), []);
        let advertisement = empty_advertisement();
        let request_id = identity(1);
        let (server, client) = UnixStream::pair().expect("a local runner stream pair exists");
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let server = serve_connection(server, service, shutdown);
        let client = async {
            let (reader, mut writer) = client.into_split();
            let mut reader = BufReader::new(reader);
            write_message(
                &mut writer,
                Message::Enroll(Enroll {
                    request_id,
                    digest_version: DIGEST_VERSION,
                    advertisement,
                }),
            )
            .await
            .expect("the enrollment request is sent");
            let Message::Enrolled(enrolled) = read_frame(&mut reader)
                .await
                .expect("the enrollment response is received")
                .message
            else {
                panic!("the durable service returns an enrolled receipt");
            };
            write_message(
                &mut writer,
                Message::Shutdown(Shutdown {
                    connection_epoch: enrolled.connection_epoch,
                    reason: ShutdownReason::DaemonShutdown,
                }),
            )
            .await
            .expect("the wrong-direction shutdown is sent");
            let rejected = read_frame(&mut reader)
                .await
                .expect("the shutdown rejection is received")
                .message;
            (enrolled, rejected)
        };

        let (served, (enrolled, rejected)) = tokio::join!(server, client);
        let enrollment_id = RunnerEnrollmentId::from_uuid(enrolled.enrollment_id.into_uuid());
        let connection = store
            .load_connection(enrollment_id)
            .await
            .expect("the terminal connection loads")
            .expect("the connection lifecycle exists");
        let expected = Message::Rejected(
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Shutdown,
                AvailableCorrelation::ConnectionEpoch(enrolled.connection_epoch),
                RejectionCode::CorrelationMismatch,
            )
            .into_rejected(),
        );

        served.expect("the wrong-direction shutdown closes after terminalization");
        assert_eq!(rejected, expected);
        assert_eq!(connection.state(), RunnerConnectionState::Lost);
        assert_eq!(connection.cause(), RunnerConnectionCause::ProtocolFailure);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn out_of_state_enroll_is_a_fatal_protocol_failure() {
        let (_container, _database_url, store) = postgres_store().await;
        let service = PostgresRunnerRegistrationService::new(store.clone(), []);
        let advertisement = empty_advertisement();
        let request_id = identity(1);
        let (server, client) = UnixStream::pair().expect("a local runner stream pair exists");
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let server = serve_connection(server, service, shutdown);
        let client = async {
            let (reader, mut writer) = client.into_split();
            let mut reader = BufReader::new(reader);
            let enrollment = Enroll {
                request_id,
                digest_version: DIGEST_VERSION,
                advertisement,
            };
            write_message(&mut writer, Message::Enroll(enrollment.clone()))
                .await
                .expect("the enrollment request is sent");
            let Message::Enrolled(enrolled) = read_frame(&mut reader)
                .await
                .expect("the enrollment response is received")
                .message
            else {
                panic!("the durable service returns an enrolled receipt");
            };
            write_message(&mut writer, Message::Enroll(enrollment))
                .await
                .expect("the out-of-state enrollment is sent");
            let rejected = read_frame(&mut reader)
                .await
                .expect("the fatal rejection is received")
                .message;
            (enrolled, rejected)
        };

        let (served, (enrolled, rejected)) = tokio::join!(server, client);
        let enrollment_id = RunnerEnrollmentId::from_uuid(enrolled.enrollment_id.into_uuid());
        let connection = store
            .load_connection(enrollment_id)
            .await
            .expect("the terminal connection loads")
            .expect("the connection lifecycle exists");
        let expected = Message::Rejected(
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Enroll,
                AvailableCorrelation::Enrollment(request_id),
                RejectionCode::CorrelationMismatch,
            )
            .into_rejected(),
        );

        served.expect("the out-of-state frame closes after terminalization");
        assert_eq!(rejected, expected);
        assert_eq!(connection.state(), RunnerConnectionState::Lost);
        assert_eq!(connection.cause(), RunnerConnectionCause::ProtocolFailure);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn revocation_terminalizes_a_live_connection_before_enrollment_state_changes() {
        let (_container, _database_url, store) = postgres_store().await;
        let service = PostgresRunnerRegistrationService::new(store.clone(), []);
        let client_store = store.clone();
        let advertisement = empty_advertisement();
        let request_id = identity(1);
        let (server, client) = UnixStream::pair().expect("a local runner stream pair exists");
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let server = serve_connection(server, service, shutdown);
        let client = async {
            let (reader, mut writer) = client.into_split();
            let mut reader = BufReader::new(reader);
            write_message(
                &mut writer,
                Message::Enroll(Enroll {
                    request_id,
                    digest_version: DIGEST_VERSION,
                    advertisement: advertisement.clone(),
                }),
            )
            .await
            .expect("the enrollment request is sent");
            let Message::Enrolled(enrolled) = read_frame(&mut reader)
                .await
                .expect("the enrollment response is received")
                .message
            else {
                panic!("the durable service returns an enrolled receipt");
            };
            let enrollment_id = RunnerEnrollmentId::from_uuid(enrolled.enrollment_id.into_uuid());
            let mut enrollment = client_store
                .load_enrollment(enrollment_id)
                .await
                .expect("the enrollment loads")
                .expect("the enrollment exists");
            let revoked = client_store
                .revoke_enrollment(&mut enrollment)
                .await
                .expect("revocation and connection terminalization commit together");
            write_message(
                &mut writer,
                Message::Advertise(Advertise {
                    enrollment_id: enrolled.enrollment_id,
                    runner_id: enrolled.runner_id,
                    authentication_id: enrolled.authentication_id,
                    registration_revision: enrolled.registration_revision,
                    advertisement,
                }),
            )
            .await
            .expect("the revoked runner sends its next ordinary frame");
            let rejected = read_frame(&mut reader)
                .await
                .expect("the revocation rejection is received")
                .message;
            (enrolled, rejected, revoked)
        };

        let (served, (enrolled, rejected, revoked)) = tokio::join!(server, client);
        let enrollment_id = RunnerEnrollmentId::from_uuid(enrolled.enrollment_id.into_uuid());
        let connection = store
            .load_connection(enrollment_id)
            .await
            .expect("the terminal connection loads")
            .expect("the connection lifecycle exists");
        let startup_candidates = store
            .load_nonterminal_connection_heads()
            .await
            .expect("startup reconciliation ignores the terminal connection");
        let expected = Message::Rejected(
            RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Advertise,
                AvailableCorrelation::ConnectionEpoch(enrolled.connection_epoch),
                RejectionCode::EnrollmentRevoked,
            )
            .into_rejected(),
        );

        served.expect("the runtime closes the revoked physical connection");
        assert!(revoked);
        assert_eq!(rejected, expected);
        assert_eq!(connection.state(), RunnerConnectionState::Lost);
        assert_eq!(connection.cause(), RunnerConnectionCause::EnrollmentRevoked);
        assert!(startup_candidates.is_empty());
    }

    #[tokio::test]
    async fn fatal_connection_failure_drains_a_signalled_peer_task() {
        let (shutdown_sender, mut shutdown) = watch::channel(false);
        let (drained_sender, drained) = tokio::sync::oneshot::channel();
        let mut connections = JoinSet::new();
        connections.spawn(async move {
            shutdown
                .changed()
                .await
                .expect("the connection shutdown sender remains live");
            drained_sender
                .send(())
                .expect("the drain observer remains live");
            Ok(())
        });
        shutdown_sender
            .send(true)
            .expect("the peer connection receives shutdown");
        let primary = RunnerProtocolRuntimeError::Lifecycle(RunnerRegistrationFailure::new(
            RunnerInboundFrameKind::Registration,
            AvailableCorrelation::None,
            RejectionCode::Unavailable,
        ));

        let observed = drain_connection_tasks(&mut connections, Some(primary))
            .await
            .expect_err("the initiating lifecycle failure remains primary");
        drained
            .await
            .expect("the peer task finishes before failure propagation");

        assert!(matches!(observed, RunnerProtocolRuntimeError::Lifecycle(_)));
    }

    #[tokio::test]
    async fn listener_accept_failure_drains_a_signalled_peer_task() {
        let (shutdown_sender, mut shutdown) = watch::channel(false);
        let (drained_sender, drained) = tokio::sync::oneshot::channel();
        let mut connections = JoinSet::new();
        connections.spawn(async move {
            shutdown
                .changed()
                .await
                .expect("the connection shutdown sender remains live");
            drained_sender
                .send(())
                .expect("the drain observer remains live");
            Ok(())
        });

        let observed = accepted_stream_or_drain(
            Err(io::Error::other("listener fixture failure")),
            &shutdown_sender,
            &mut connections,
        )
        .await
        .expect_err("the listener accept failure remains primary");
        drained
            .await
            .expect("the peer task finishes before accept failure propagation");

        assert!(matches!(observed, RunnerProtocolRuntimeError::Accept(_)));
    }

    #[tokio::test]
    async fn peer_task_drain_deadline_aborts_a_stuck_task_with_typed_evidence() {
        let mut connections = JoinSet::new();
        connections.spawn(std::future::pending::<Result<(), RunnerProtocolRuntimeError>>());
        let expected_remaining = connections.len();

        let observed = drain_connection_tasks_with_timeout(&mut connections, None, Duration::ZERO)
            .await
            .expect_err("the expired peer drain is typed");
        let RunnerProtocolRuntimeError::ConnectionDrainTimeout {
            remaining,
            initiating,
        } = observed
        else {
            panic!("the stuck task produces drain-timeout evidence");
        };

        assert_eq!(remaining, expected_remaining);
        assert!(initiating.is_none());
        assert!(connections.is_empty());
    }

    #[test]
    fn heartbeat_ack_requires_exact_latest_challenge() {
        let mut state = HeartbeatState::new();
        let challenge = challenge_tick(&mut state);
        let stale = HeartbeatAck {
            challenge_sequence: PositiveU64::try_new(challenge.sequence.get() + 1)
                .expect("the distinct fixture challenge is positive"),
            runner_sequence: PositiveU64::try_new(1)
                .expect("the first runner sequence is positive"),
            lease_phase: None,
            workspace_phase: None,
        };

        let failure = state
            .accept(&stale)
            .expect_err("a nonmatching challenge must fail closed");

        assert_eq!(failure.code, RejectionCode::CorrelationMismatch);
    }

    #[test]
    fn heartbeat_ack_requires_monotonic_runner_sequence() {
        let mut state = HeartbeatState::new();
        let first = challenge_tick(&mut state);
        state
            .accept(&HeartbeatAck {
                challenge_sequence: first.sequence,
                runner_sequence: PositiveU64::try_new(1)
                    .expect("the first runner sequence is positive"),
                lease_phase: None,
                workspace_phase: None,
            })
            .expect("the first acknowledgement is admitted");
        let second = challenge_tick(&mut state);
        let repeated = HeartbeatAck {
            challenge_sequence: second.sequence,
            runner_sequence: PositiveU64::try_new(1)
                .expect("the repeated runner sequence is positive"),
            lease_phase: None,
            workspace_phase: None,
        };

        let failure = state
            .accept(&repeated)
            .expect_err("a repeated runner sequence must fail closed");

        assert_eq!(failure.code, RejectionCode::CorrelationMismatch);
    }

    #[test]
    fn three_missed_intervals_reach_the_durable_loss_boundary() {
        let mut state = HeartbeatState::new();
        let _first = challenge_tick(&mut state);
        let first_miss = state.next_tick().expect("the first miss is represented");
        let second_miss = state.next_tick().expect("the second miss is represented");
        let third_miss = state.next_tick().expect("the third miss is represented");

        assert_eq!(first_miss, HeartbeatTick::Missed(1));
        assert_eq!(second_miss, HeartbeatTick::Missed(2));
        assert_eq!(third_miss, HeartbeatTick::Missed(3));
    }

    #[test]
    fn dual_phase_heartbeat_ack_preserves_a_complete_lease_correlation() {
        let correlation = canonical_lease_correlation();
        let arbitrary_workspace_identity = identity(2);
        let first = PositiveU64::try_new(1).expect("the first fixture revision is positive");
        let acknowledgement = HeartbeatAck {
            challenge_sequence: PositiveU64::try_new(1)
                .expect("the first challenge sequence is positive"),
            runner_sequence: PositiveU64::try_new(1)
                .expect("the first runner sequence is positive"),
            lease_phase: Some(signalbox_runner_wire::LeasePhase {
                correlation: correlation.clone(),
                phase: signalbox_runner_wire::LeasePhaseKind::WaitingDispatch,
            }),
            workspace_phase: Some(HeartbeatWorkspacePhase::Provisioning {
                correlation: signalbox_runner_wire::ProvisionCorrelation {
                    authorization_id: arbitrary_workspace_identity,
                    session_id: arbitrary_workspace_identity,
                    placement_revision: first,
                    runner_id: arbitrary_workspace_identity,
                    registration_revision: first,
                    repository: None,
                    sandbox_profile: signalbox_runner_wire::SandboxProfile::Ambient,
                    credential_profile: None,
                },
            }),
        };

        assert_eq!(
            heartbeat_ack_correlation(&acknowledgement),
            AvailableCorrelation::Lease(correlation)
        );
    }

    #[test]
    fn heartbeat_operation_phase_is_unavailable_in_registration_only_runtime() {
        let mut state = HeartbeatState::new();
        let challenge = challenge_tick(&mut state);
        let correlation = signalbox_runner_wire::ProvisionCorrelation {
            authorization_id: identity(10),
            session_id: identity(11),
            placement_revision: PositiveU64::try_new(1)
                .expect("the first placement revision is positive"),
            runner_id: identity(12),
            registration_revision: PositiveU64::try_new(1)
                .expect("the first registration revision is positive"),
            repository: None,
            sandbox_profile: signalbox_runner_wire::SandboxProfile::Ambient,
            credential_profile: None,
        };
        let acknowledgement = HeartbeatAck {
            challenge_sequence: challenge.sequence,
            runner_sequence: PositiveU64::try_new(1)
                .expect("the first runner sequence is positive"),
            lease_phase: None,
            workspace_phase: Some(
                signalbox_runner_wire::HeartbeatWorkspacePhase::Provisioning {
                    correlation: correlation.clone(),
                },
            ),
        };

        let failure = state
            .accept(&acknowledgement)
            .expect_err("operation state is unavailable in this runtime");

        assert_eq!(failure.code, RejectionCode::Unavailable);
        assert_eq!(
            failure.available_correlation.as_ref(),
            &AvailableCorrelation::Provision(correlation)
        );
    }
}
