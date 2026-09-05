//! Hub-side serial registration and lifecycle runtime for the local runner wire.

use std::{error::Error, fmt, future::Future, io, pin::Pin, sync::Arc, time::Duration};

use rustix::process::geteuid;
use signalbox_domain::{
    CredentialProfileName, CredentialProfilePolicy, RunnerAuthenticationId, RunnerCapabilityClass,
    RunnerCatalog, RunnerDomainError, RunnerEnrollmentId, RunnerId,
};
use signalbox_persistence::runner_protocol::{
    AppliedRunnerConnectionTransition, IssuedRunnerEnrollmentIdentities,
    PristineRunnerEnrollmentRequest, RunnerConnectionCause, RunnerConnectionEpoch,
    RunnerConnectionState, RunnerConnectionTransition, RunnerConnectionTransitionEffect,
    RunnerConnectionTransitionOutcome, RunnerEnrollmentDisposition, RunnerEnrollmentRequestFailure,
    RunnerEnrollmentRequestId, RunnerProtocolStore, RunnerProtocolStoreError,
    RunnerRegistrationRevision,
};
use signalbox_runner_wire::{
    Advertise, AvailableCorrelation, CanonicalUuid, DIGEST_VERSION, Enroll, Enrolled, Frame,
    FrameError, Heartbeat, HeartbeatAck, HeartbeatWorkspacePhase, MAX_FRAME_BYTES, Message,
    PositiveU64, ReconnectDirectives, Registered, Rejected, RejectionCode, Resume, Resumed,
    Shutdown, ShutdownReason, WorkspaceFailureCorrelation, advertisement_digest, decode_line,
    encode_line,
};
use sqlx::PgPool;
use tokio::{
    io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
    net::{UnixStream, unix::OwnedReadHalf, unix::OwnedWriteHalf},
    sync::{Mutex, watch},
    task::{JoinError, JoinSet},
    time::{MissedTickBehavior, interval, timeout},
};

use crate::LocalProcessListener;
use crate::local_socket::LocalSocketError;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const HEARTBEAT_MISSES_BEFORE_LOSS: u8 = 3;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECTION_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const MAXIMUM_CONCURRENT_CONNECTIONS: usize = 64;
const REGISTRATION_ONLY_CREDENTIAL_PROFILE: &str = "github-runner";

/// Boxed future returned by the injected durable registration service.
pub type RunnerRegistrationFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, RunnerRegistrationFailure>> + Send + 'a>>;

/// Durable-before-ack boundary consumed by the runner socket runtime.
pub trait RunnerRegistrationService: Clone + Send + Sync + 'static {
    /// Atomically creates or exactly replays pristine enrollment authority.
    fn enroll(&self, request: Enroll) -> RunnerRegistrationFuture<'_, Enrolled>;

    /// Validates one reconnect and returns canonical current registration facts.
    fn resume(&self, request: Resume) -> RunnerRegistrationFuture<'_, Resumed>;

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

    /// Classifies prior-process nonterminal connections as lost before admission.
    pub async fn mark_orphaned_connections_lost(
        &self,
    ) -> Result<Vec<AppliedRunnerConnectionTransition>, RunnerProtocolStoreError> {
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
        self.propagate_pending_connection_losses(None).await?;
        Ok(transitions)
    }

    async fn propagate_pending_connection_losses(
        &self,
        only_enrollment: Option<RunnerEnrollmentId>,
    ) -> Result<(), RunnerProtocolStoreError> {
        for loss in self
            .store
            .load_pending_connection_losses()
            .await?
            .into_iter()
            .filter(|loss| only_enrollment.is_none_or(|enrollment| loss.enrollment() == enrollment))
        {
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

    async fn enroll_durably(&self, request: Enroll) -> Result<Enrolled, RunnerRegistrationFailure> {
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
        Ok(Enrolled {
            request_id: CanonicalUuid::from_uuid(receipt.request().into_uuid()),
            enrollment_id: CanonicalUuid::from_uuid(identities.enrollment().into_uuid()),
            runner_id: CanonicalUuid::from_uuid(identities.runner().into_uuid()),
            authentication_id: CanonicalUuid::from_uuid(identities.authentication().into_uuid()),
            registration_revision: positive_revision(receipt.registration().revision())?,
            connection_epoch: positive_epoch(connection.epoch())?,
            advertisement_digest: digest,
        })
    }

    async fn resume_durably(&self, request: Resume) -> Result<Resumed, RunnerRegistrationFailure> {
        let _admission = self.registration_admission.lock().await;
        let correlation = AvailableCorrelation::Enrollment(request.request_id);
        if request.digest_version != DIGEST_VERSION {
            return Err(RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Resume,
                correlation,
                RejectionCode::UnsupportedDigestVersion,
            ));
        }
        if request.inventory != Default::default() {
            return Err(RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Resume,
                correlation,
                RejectionCode::Unavailable,
            ));
        }
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
            .map_err(|error| store_failure(RunnerInboundFrameKind::Resume, correlation, error))?;
        tracing::info!(
            enrollment_id = %receipt.enrollment().enrollment().into_uuid(),
            runner_id = %receipt.enrollment().runner().into_uuid(),
            connection_epoch = connection.epoch().get(),
            connection_cause = ?connection.cause(),
            "runner connection established"
        );
        Ok(Resumed {
            registration_revision: positive_revision(receipt.registration().revision())?,
            connection_epoch: positive_epoch(connection.epoch())?,
            directives: ReconnectDirectives::default(),
        })
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
            let enrollment = RunnerEnrollmentId::from_uuid(enrollment.into_uuid());
            self.propagate_pending_connection_losses(Some(enrollment))
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
    fn enroll(&self, request: Enroll) -> RunnerRegistrationFuture<'_, Enrolled> {
        Box::pin(self.enroll_durably(request))
    }

    fn resume(&self, request: Resume) -> RunnerRegistrationFuture<'_, Resumed> {
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
    let (code, cause) = match &error {
        RunnerProtocolStoreError::EnrollmentRequest(
            RunnerEnrollmentRequestFailure::ActiveEnrollmentExists { .. }
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
    if cause.operator_actionable() {
        tracing::error!(
            error = %error,
            frame_kind = offending_kind.as_str(),
            ?cause,
            "durable runner registration failed"
        );
    }
    RunnerRegistrationFailure::from_durable_cause(offending_kind, correlation, code, cause)
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
pub struct RunnerProtocolRuntime<S> {
    listener: Option<LocalProcessListener>,
    service: Option<S>,
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

impl<S> Drop for RunnerProtocolRuntime<S> {
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

impl<S> RunnerProtocolRuntime<S>
where
    S: RunnerRegistrationService,
{
    /// Composes the dedicated guarded listener with durable runner authority.
    pub const fn new(listener: LocalProcessListener, service: S) -> Self {
        Self {
            listener: Some(listener),
            service: Some(service),
        }
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
        let listener = RunnerListenerGuard::new(listener);
        let outcome = run_connections(listener.listener()?, service, shutdown).await;
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

async fn run_connections<S>(
    listener: &LocalProcessListener,
    service: S,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), RunnerProtocolRuntimeError>
where
    S: RunnerRegistrationService,
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
                connections.spawn(serve_connection(
                    stream,
                    service.clone(),
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

async fn serve_connection<S>(
    stream: UnixStream,
    service: S,
    shutdown: watch::Receiver<bool>,
) -> Result<(), RunnerProtocolRuntimeError>
where
    S: RunnerRegistrationService,
{
    serve_connection_with_handshake_timeout(stream, service, shutdown, HANDSHAKE_TIMEOUT).await
}

async fn serve_connection_with_handshake_timeout<S>(
    stream: UnixStream,
    service: S,
    mut shutdown: watch::Receiver<bool>,
    handshake_timeout: Duration,
) -> Result<(), RunnerProtocolRuntimeError>
where
    S: RunnerRegistrationService,
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
                    enrollment: response.enrollment_id,
                    epoch: response.connection_epoch,
                };
                if let Err(error) = write_message(&mut writer, Message::Enrolled(response)).await {
                    transition_is_current(
                        &service,
                        context,
                        RunnerConnectionTransition::TransportClosed,
                    )
                    .await?;
                    return Err(error);
                }
                context
            }
            Err(failure) => {
                write_rejected(&mut writer, failure).await?;
                return Ok(());
            }
        },
        Message::Resume(request) => {
            let enrollment = request.enrollment_id;
            match service.resume(*request).await {
                Ok(response) => {
                    let context = ConnectionContext {
                        enrollment,
                        epoch: response.connection_epoch,
                    };
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
                    context
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

#[derive(Clone, Copy)]
struct ConnectionContext {
    enrollment: CanonicalUuid,
    epoch: PositiveU64,
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
    use signalbox_domain::{
        CreateSession, DirectModelSelection, DurableCommandId, ModelSelectionRequest,
        RunnerLostBeforePin, RunnerSandboxProfile, RunnerSelector, RunnerToolPermissionOverrides,
        RunnerWorkingDirectory, SessionConfigurationDefaults, SessionCreationCause,
        SessionCreationProvenance, SessionId, SessionRunnerPlacement,
        SessionRunnerPlacementRequest, SessionRunnerPlacementState, TranscriptAncestry,
        WorkingDirectorySelection, WorkspaceRequirement,
    };
    use signalbox_persistence::{
        create_session::CreateSessionRepository,
        disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
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
        fn enroll(&self, _request: Enroll) -> RunnerRegistrationFuture<'_, Enrolled> {
            Box::pin(std::future::ready(Ok(self.response.clone())))
        }

        fn resume(&self, request: Resume) -> RunnerRegistrationFuture<'_, Resumed> {
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
                SessionCreationCause::Interactive,
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
            .with_cmd(disposable_postgres_server_args())
            .with_mount(
                disposable_postgres_state_tmpfs_from_example()
                    .expect("checked-in example provides the test database bound"),
            )
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
                enrolled.enrollment_id,
                enrolled.connection_epoch,
                RunnerConnectionTransition::RunnerShutdown,
            )
            .await
            .expect("the epoch-targeted shutdown commits");
        let observed = store
            .load_connection(RunnerEnrollmentId::from_uuid(
                enrolled.enrollment_id.into_uuid(),
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
                enrolled.enrollment_id,
                enrolled.connection_epoch,
                RunnerConnectionTransition::TransportClosed,
            )
            .await
            .expect("the dead transport commits loss");
        let observed = store
            .load_connection(RunnerEnrollmentId::from_uuid(
                enrolled.enrollment_id.into_uuid(),
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
        let enrollment = RunnerEnrollmentId::from_uuid(enrolled.enrollment_id.into_uuid());
        let epoch = RunnerConnectionEpoch::try_from_u64(enrolled.connection_epoch.get())
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
                enrolled.enrollment_id.into_uuid(),
            ))
            .await
            .expect("the startup loss state loads")
            .expect("the connection lifecycle exists");

        assert_eq!(transitions.len(), 1);
        assert_eq!(
            applied.enrollment().into_uuid(),
            enrolled.enrollment_id.into_uuid()
        );
        assert_eq!(applied.snapshot(), observed);
        assert_eq!(observed.state(), RunnerConnectionState::Lost);
        assert_eq!(observed.cause(), RunnerConnectionCause::TransportClosed);
    }

    #[tokio::test]
    #[ignore = "requires ephemeral PostgreSQL"]
    async fn s32_terminal_connection_transition_propagates_loss_to_placed_sessions() {
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
        let runner = RunnerId::from_uuid(enrolled.runner_id.into_uuid());
        create_runner_placed_session(&pool, &store, session, runner).await;

        service
            .transition_connection(
                enrolled.enrollment_id,
                enrolled.connection_epoch,
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
    async fn s32_terminal_connection_replay_resumes_its_pending_loss_cursor() {
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
        let runner = RunnerId::from_uuid(enrolled.runner_id.into_uuid());
        create_runner_placed_session(&pool, &store, session, runner).await;
        store
            .transition_connection_with_effect(
                RunnerEnrollmentId::from_uuid(enrolled.enrollment_id.into_uuid()),
                RunnerConnectionEpoch::try_from_u64(enrolled.connection_epoch.get())
                    .expect("the connection epoch is positive"),
                RunnerConnectionTransition::TransportClosed,
            )
            .await
            .expect("the terminal connection state commits before propagation");

        let replayed = service
            .transition_connection(
                enrolled.enrollment_id,
                enrolled.connection_epoch,
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
                        enrolled.enrollment_id.into_uuid(),
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
    async fn s32_startup_resumes_a_previously_committed_loss_cursor() {
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
        let runner = RunnerId::from_uuid(enrolled.runner_id.into_uuid());
        create_runner_placed_session(&pool, &store, session, runner).await;
        store
            .transition_connection_with_effect(
                RunnerEnrollmentId::from_uuid(enrolled.enrollment_id.into_uuid()),
                RunnerConnectionEpoch::try_from_u64(enrolled.connection_epoch.get())
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
                request_id: enrolled.request_id,
                digest_version: DIGEST_VERSION,
                enrollment_id: enrolled.enrollment_id,
                runner_id: enrolled.runner_id,
                authentication_id: enrolled.authentication_id,
                advertisement,
                prior_registration_revision: enrolled.registration_revision,
                inventory: Default::default(),
            })
            .await
            .expect("the fresh physical connection resumes");

        let refused = service
            .transition_connection(
                enrolled.enrollment_id,
                enrolled.connection_epoch,
                RunnerConnectionTransition::RunnerShutdown,
            )
            .await
            .expect("stale epoch detection is a typed outcome");
        let observed = store
            .load_connection(RunnerEnrollmentId::from_uuid(
                enrolled.enrollment_id.into_uuid(),
            ))
            .await
            .expect("the current connection state loads")
            .expect("the connection lifecycle exists");
        let expected_stale = RunnerConnectionTransitionOutcome::Stale {
            observed: RunnerConnectionEpoch::try_from_u64(enrolled.connection_epoch.get())
                .expect("the enrolled epoch is positive"),
            current: RunnerConnectionEpoch::try_from_u64(resumed.connection_epoch.get())
                .expect("the resumed epoch is positive"),
        };

        assert_eq!(refused, expected_stale);
        assert_eq!(observed.epoch().get(), resumed.connection_epoch.get());
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
                request_id: enrolled.request_id,
                digest_version: DIGEST_VERSION,
                enrollment_id: enrolled.enrollment_id,
                runner_id: enrolled.runner_id,
                authentication_id: enrolled.authentication_id,
                advertisement: advertisement.clone(),
                prior_registration_revision: enrolled.registration_revision,
                inventory: Default::default(),
            })
            .await
            .expect("the fresh physical connection resumes");

        let refused = service
            .advertise(
                enrolled.enrollment_id,
                Advertise {
                    enrollment_id: enrolled.enrollment_id,
                    runner_id: enrolled.runner_id,
                    authentication_id: enrolled.authentication_id,
                    registration_revision: resumed.registration_revision,
                    advertisement,
                },
                enrolled.connection_epoch,
            )
            .await
            .expect_err("the superseded epoch cannot advertise");
        let enrollment = store
            .load_enrollment(RunnerEnrollmentId::from_uuid(
                enrolled.enrollment_id.into_uuid(),
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
            resumed.registration_revision.get()
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
                enrolled.enrollment_id,
                Advertise {
                    enrollment_id: identity(99),
                    runner_id: enrolled.runner_id,
                    authentication_id: enrolled.authentication_id,
                    registration_revision: enrolled.registration_revision,
                    advertisement,
                },
                enrolled.connection_epoch,
            )
            .await
            .expect_err("the connection cannot advertise for another enrollment");
        let enrollment = store
            .load_enrollment(RunnerEnrollmentId::from_uuid(
                enrolled.enrollment_id.into_uuid(),
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
            enrolled.registration_revision.get()
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
