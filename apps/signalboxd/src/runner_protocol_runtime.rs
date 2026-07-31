//! Hub-side serial registration runtime for the local runner wire.
//!
//! Connection epochs and durable heartbeat-loss transitions remain behind the
//! typed seam at the bottom of this module until their wire contract is fixed.

use std::{error::Error, fmt, future::Future, io, pin::Pin, time::Duration};

use signalbox_domain::{
    RunnerAuthenticationId, RunnerCapabilityClass, RunnerDomainError, RunnerEnrollmentId, RunnerId,
};
use signalbox_persistence::runner_protocol::{
    IssuedRunnerEnrollmentIdentities, PristineRunnerEnrollmentRequest,
    RunnerEnrollmentRequestFailure, RunnerEnrollmentRequestId, RunnerProtocolStore,
    RunnerProtocolStoreError, RunnerRegistrationRevision,
};
use signalbox_runner_wire::{
    Advertise, AvailableCorrelation, CanonicalUuid, DIGEST_VERSION, Enroll, Enrolled, Frame,
    FrameError, Heartbeat, HeartbeatAck, MAX_FRAME_BYTES, Message, PositiveU64,
    ReconnectDirectives, Registered, Rejected, RejectionCode, Resume, Resumed,
    advertisement_digest, decode_line, encode_line,
};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
    net::{UnixStream, unix::OwnedReadHalf, unix::OwnedWriteHalf},
    sync::watch,
    task::{JoinError, JoinSet},
    time::{MissedTickBehavior, interval},
};

use crate::LocalProcessListener;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

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
    fn advertise(&self, request: Advertise) -> RunnerRegistrationFuture<'_, Registered>;
}

/// PostgreSQL-backed durable registration authority for the local runner wire.
#[derive(Clone, Debug)]
pub struct PostgresRunnerRegistrationService {
    store: RunnerProtocolStore,
    allowed_classes: Vec<RunnerCapabilityClass>,
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
        }
    }

    async fn enroll_durably(&self, request: Enroll) -> Result<Enrolled, RunnerRegistrationFailure> {
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
            .map_err(|error| store_failure(RunnerInboundFrameKind::Enroll, correlation, error))?;
        let receipt = outcome.receipt();
        let identities = receipt.identities();
        Ok(Enrolled {
            request_id: CanonicalUuid::from_uuid(receipt.request().into_uuid()),
            enrollment_id: CanonicalUuid::from_uuid(identities.enrollment().into_uuid()),
            runner_id: CanonicalUuid::from_uuid(identities.runner().into_uuid()),
            authentication_id: CanonicalUuid::from_uuid(identities.authentication().into_uuid()),
            registration_revision: positive_revision(receipt.registration().revision())?,
            advertisement_digest: digest,
        })
    }

    async fn resume_durably(&self, request: Resume) -> Result<Resumed, RunnerRegistrationFailure> {
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
        let receipt = self
            .store
            .resume_registration(
                RunnerEnrollmentRequestId::from_uuid(request.request_id.into_uuid()),
                identities,
                prior,
                advertisement,
            )
            .await
            .map_err(|error| store_failure(RunnerInboundFrameKind::Resume, correlation, error))?;
        Ok(Resumed {
            registration_revision: positive_revision(receipt.registration().revision())?,
            directives: ReconnectDirectives::default(),
        })
    }

    async fn advertise_durably(
        &self,
        request: Advertise,
    ) -> Result<Registered, RunnerRegistrationFailure> {
        let correlation = AvailableCorrelation::Registration(request.registration_revision);
        let enrollment = self
            .store
            .load_enrollment(RunnerEnrollmentId::from_uuid(
                request.enrollment_id.into_uuid(),
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
        Ok(Registered {
            registration_revision: positive_revision(registration.revision())?,
            advertisement_digest: digest,
        })
    }
}

impl RunnerRegistrationService for PostgresRunnerRegistrationService {
    fn enroll(&self, request: Enroll) -> RunnerRegistrationFuture<'_, Enrolled> {
        Box::pin(self.enroll_durably(request))
    }

    fn resume(&self, request: Resume) -> RunnerRegistrationFuture<'_, Resumed> {
        Box::pin(self.resume_durably(request))
    }

    fn advertise(&self, request: Advertise) -> RunnerRegistrationFuture<'_, Registered> {
        Box::pin(self.advertise_durably(request))
    }
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
        }
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
    listener: LocalProcessListener,
    service: S,
}

impl<S> RunnerProtocolRuntime<S>
where
    S: RunnerRegistrationService,
{
    /// Composes the dedicated guarded listener with durable runner authority.
    pub const fn new(listener: LocalProcessListener, service: S) -> Self {
        Self { listener, service }
    }

    /// Accepts runner connections until shutdown; each connection is serial.
    pub async fn run(
        self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), RunnerProtocolRuntimeError> {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(());
                    }
                }
                completed = connections.join_next(), if !connections.is_empty() => {
                    match completed {
                        Some(Ok(Ok(()))) | None => {}
                        Some(Ok(Err(error))) => {
                            tracing::warn!(
                                error = %error,
                                "runner connection closed after a typed protocol failure"
                            );
                        }
                        Some(Err(error)) => {
                            return Err(RunnerProtocolRuntimeError::ConnectionTask(error));
                        }
                    }
                }
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted.map_err(RunnerProtocolRuntimeError::Accept)?;
                    connections.spawn(serve_connection(
                        stream,
                        self.service.clone(),
                        shutdown.clone(),
                    ));
                }
            }
        }
    }
}

async fn serve_connection<S>(
    stream: UnixStream,
    service: S,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), RunnerProtocolRuntimeError>
where
    S: RunnerRegistrationService,
{
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let first = loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
            frame = read_frame(&mut reader) => break frame?,
        }
    };
    match first.message {
        Message::Enroll(request) => match service.enroll(request).await {
            Ok(response) => {
                write_message(&mut writer, Message::Enrolled(response)).await?;
            }
            Err(failure) => {
                write_rejected(&mut writer, failure).await?;
                return Ok(());
            }
        },
        Message::Resume(request) => match service.resume(*request).await {
            Ok(response) => {
                write_message(&mut writer, Message::Resumed(Box::new(response))).await?;
            }
            Err(failure) => {
                write_rejected(&mut writer, failure).await?;
                return Ok(());
            }
        },
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
    }

    let mut heartbeat = interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    heartbeat.tick().await;
    let mut heartbeat_state = HeartbeatState::new();
    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Err(RunnerProtocolRuntimeError::ConnectionEpochUnavailable(
                        ConnectionEpochUnavailable::DaemonShutdown,
                    ));
                }
            }
            frame = read_frame(&mut reader) => {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(RunnerProtocolRuntimeError::Closed) => return Ok(()),
                    Err(error) => return Err(error),
                };
                match frame.message {
                    Message::Advertise(request) => {
                        match service.advertise(request).await {
                            Ok(response) => {
                                write_message(&mut writer, Message::Registered(response)).await?;
                            }
                            Err(failure) => {
                                write_rejected(&mut writer, failure).await?;
                                return Ok(());
                            }
                        }
                    }
                    Message::HeartbeatAck(acknowledgement) => {
                        if let Err(failure) = heartbeat_state.accept(&acknowledgement) {
                            write_rejected(&mut writer, failure).await?;
                            return Ok(());
                        }
                    }
                    Message::Shutdown(_) => {
                        return Err(RunnerProtocolRuntimeError::ConnectionEpochUnavailable(
                            ConnectionEpochUnavailable::RunnerShutdown,
                        ));
                    }
                    message => {
                        write_rejected(
                            &mut writer,
                            RunnerRegistrationFailure::new(
                                inbound_frame_kind(&message),
                                available_correlation(&message),
                                RejectionCode::Unavailable,
                            ),
                        ).await?;
                        return Ok(());
                    }
                }
            }
            _ = heartbeat.tick() => {
                let challenge = heartbeat_state.next_challenge()?;
                write_message(&mut writer, Message::Heartbeat(challenge)).await?;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HeartbeatState {
    last_challenge: u64,
    last_accepted_runner_sequence: u64,
    outstanding_challenge: Option<u64>,
}

impl HeartbeatState {
    const fn new() -> Self {
        Self {
            last_challenge: 0,
            last_accepted_runner_sequence: 0,
            outstanding_challenge: None,
        }
    }

    fn next_challenge(&mut self) -> Result<Heartbeat, RunnerProtocolRuntimeError> {
        if self.outstanding_challenge.is_some() {
            return Err(RunnerProtocolRuntimeError::ConnectionEpochUnavailable(
                ConnectionEpochUnavailable::HeartbeatLoss,
            ));
        }
        let sequence = self
            .last_challenge
            .checked_add(1)
            .and_then(|value| PositiveU64::try_new(value).ok())
            .ok_or(RunnerProtocolRuntimeError::HeartbeatSequenceExhausted)?;
        self.last_challenge = sequence.get();
        self.outstanding_challenge = Some(sequence.get());
        Ok(Heartbeat {
            sequence,
            last_accepted_peer_sequence: self.last_accepted_runner_sequence,
        })
    }

    fn accept(&mut self, acknowledgement: &HeartbeatAck) -> Result<(), RunnerRegistrationFailure> {
        if acknowledgement.lease_phase.is_some() || acknowledgement.workspace_phase.is_some() {
            return Err(RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::HeartbeatAck,
                AvailableCorrelation::None,
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
        Message::Resume(value) => AvailableCorrelation::Enrollment(value.request_id),
        Message::Advertise(value) => {
            AvailableCorrelation::Registration(value.registration_revision)
        }
        Message::WorkspaceLeakPage(value) => {
            AvailableCorrelation::LeakPage(value.page.correlation.clone())
        }
        Message::WorkspaceReady(value) => {
            AvailableCorrelation::Provision(value.correlation.clone())
        }
        Message::WorkspaceReleased(value) => {
            AvailableCorrelation::Release(value.correlation.clone())
        }
        Message::LeaseClaim(value) => AvailableCorrelation::Lease(value.correlation.clone()),
        Message::Result(value) => AvailableCorrelation::Lease(value.correlation.clone()),
        Message::OperationFailed(value) => {
            AvailableCorrelation::OperationFailure(value.failure.correlation.clone())
        }
        Message::Enrolled(_)
        | Message::Resumed(_)
        | Message::ReplacementPending(_)
        | Message::Registered(_)
        | Message::Heartbeat(_)
        | Message::HeartbeatAck(_)
        | Message::WorkspaceLeakRecorded(_)
        | Message::WorkspaceProvision(_)
        | Message::WorkspaceRecorded(_)
        | Message::WorkspaceRelease(_)
        | Message::WorkspaceReleaseRecorded(_)
        | Message::LeaseOffer(_)
        | Message::LeaseClaimed(_)
        | Message::Dispatch(_)
        | Message::ResultRecorded(_)
        | Message::OperationFailureRecorded(_)
        | Message::Shutdown(_)
        | Message::Rejected(_) => AvailableCorrelation::None,
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

/// Epoch-dependent lifecycle edge intentionally unavailable in this slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionEpochUnavailable {
    /// Hub shutdown cannot construct its required frame without the epoch.
    DaemonShutdown,
    /// A received runner shutdown cannot be correlated without the epoch.
    RunnerShutdown,
    /// Durable suspect/lost fencing cannot be committed without epoch authority.
    HeartbeatLoss,
}

impl fmt::Display for ConnectionEpochUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DaemonShutdown => "daemon shutdown awaits connection-epoch authority",
            Self::RunnerShutdown => "runner shutdown awaits connection-epoch authority",
            Self::HeartbeatLoss => "heartbeat loss awaits connection-epoch authority",
        })
    }
}

impl Error for ConnectionEpochUnavailable {}

/// Typed transport, framing, admission, or deferred-epoch failure.
#[derive(Debug)]
pub enum RunnerProtocolRuntimeError {
    Accept(io::Error),
    Read(io::Error),
    Write(io::Error),
    Decode(FrameError),
    Encode(FrameError),
    Closed,
    HeartbeatSequenceExhausted,
    ConnectionTask(JoinError),
    ConnectionEpochUnavailable(ConnectionEpochUnavailable),
}

impl fmt::Display for RunnerProtocolRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Accept(_) => formatter.write_str("runner listener accept failed"),
            Self::Read(_) => formatter.write_str("runner frame read failed"),
            Self::Write(_) => formatter.write_str("runner frame write failed"),
            Self::Decode(_) => formatter.write_str("runner frame decoding failed"),
            Self::Encode(_) => formatter.write_str("runner frame encoding failed"),
            Self::Closed => formatter.write_str("runner connection closed"),
            Self::HeartbeatSequenceExhausted => {
                formatter.write_str("runner heartbeat sequence exhausted")
            }
            Self::ConnectionTask(_) => formatter.write_str("runner connection task failed"),
            Self::ConnectionEpochUnavailable(error) => error.fmt(formatter),
        }
    }
}

impl Error for RunnerProtocolRuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Accept(error) | Self::Read(error) | Self::Write(error) => Some(error),
            Self::Decode(error) | Self::Encode(error) => Some(error),
            Self::ConnectionEpochUnavailable(error) => Some(error),
            Self::ConnectionTask(error) => Some(error),
            Self::Closed | Self::HeartbeatSequenceExhausted => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalbox_domain::{CredentialProfileName, CredentialProfilePolicy, RunnerCatalog};
    use signalbox_persistence::{local_test_connection_options, migrate};
    use sqlx::{PgPool, postgres::PgPoolOptions};
    use testcontainers_modules::{
        postgres::Postgres,
        testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
    };

    const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
    const DATABASE_USER: &str = "signalbox";
    const DATABASE_PASSWORD: &str = "signalbox-test";
    const DATABASE_NAME: &str = "signalbox";
    const CONFIGURED_PROFILE: &str = "github-runner";
    const CONFIGURED_REPOSITORY: &str = "signalbox";

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

        fn advertise(&self, request: Advertise) -> RunnerRegistrationFuture<'_, Registered> {
            Box::pin(std::future::ready(Err(RunnerRegistrationFailure::new(
                RunnerInboundFrameKind::Advertise,
                AvailableCorrelation::Registration(request.registration_revision),
                RejectionCode::Unavailable,
            ))))
        }
    }

    fn identity(value: u128) -> CanonicalUuid {
        CanonicalUuid::from_uuid(uuid::Uuid::from_u128(value))
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
        let profile = signalbox_runner_wire::ProfileName::try_new(CONFIGURED_PROFILE.to_owned())
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

    fn empty_catalog() -> RunnerCatalog {
        RunnerCatalog::try_new([], [], [], [], [])
            .expect("the registration-only catalog is internally consistent")
    }

    fn configured_catalog() -> RunnerCatalog {
        let profile = CredentialProfileName::try_new(CONFIGURED_PROFILE.to_owned())
            .expect("the configured credential profile is checked");
        let policy = CredentialProfilePolicy::try_new(profile, [])
            .expect("the configured credential policy is internally consistent");
        RunnerCatalog::try_new([], [], [policy], [], [])
            .expect("the configured registration-only catalog is internally consistent")
    }

    async fn postgres_store() -> (ContainerAsync<Postgres>, String, RunnerProtocolStore) {
        let container = Postgres::default()
            .with_user(DATABASE_USER)
            .with_password(DATABASE_PASSWORD)
            .with_db_name(DATABASE_NAME)
            .with_fsync_enabled()
            .with_tag(POSTGRES_IMAGE_TAG)
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
    async fn second_peer_enrolls_while_first_peer_is_stalled() {
        let directory = tempfile::tempdir().expect("the socket fixture directory exists");
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
        let directory = tempfile::tempdir().expect("the socket fixture directory exists");
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
    }

    #[tokio::test]
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
            directives: ReconnectDirectives::default(),
        };

        assert_eq!(resumed, expected);
    }

    #[tokio::test]
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

    #[test]
    fn heartbeat_ack_requires_exact_latest_challenge() {
        let mut state = HeartbeatState::new();
        let challenge = state.next_challenge().expect("the first challenge exists");
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
        let first = state.next_challenge().expect("the first challenge exists");
        state
            .accept(&HeartbeatAck {
                challenge_sequence: first.sequence,
                runner_sequence: PositiveU64::try_new(1)
                    .expect("the first runner sequence is positive"),
                lease_phase: None,
                workspace_phase: None,
            })
            .expect("the first acknowledgement is admitted");
        let second = state.next_challenge().expect("the second challenge exists");
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
    fn second_unanswered_challenge_stops_at_the_connection_epoch_seam() {
        let mut state = HeartbeatState::new();
        let _first = state.next_challenge().expect("the first challenge exists");

        let error = state
            .next_challenge()
            .expect_err("a missed acknowledgement needs durable loss authority");

        assert_eq!(
            error.to_string(),
            ConnectionEpochUnavailable::HeartbeatLoss.to_string()
        );
    }

    #[test]
    fn heartbeat_operation_phase_is_unavailable_in_registration_only_runtime() {
        let mut state = HeartbeatState::new();
        let challenge = state.next_challenge().expect("the first challenge exists");
        let acknowledgement = HeartbeatAck {
            challenge_sequence: challenge.sequence,
            runner_sequence: PositiveU64::try_new(1)
                .expect("the first runner sequence is positive"),
            lease_phase: None,
            workspace_phase: Some(
                signalbox_runner_wire::HeartbeatWorkspacePhase::Provisioning {
                    correlation: signalbox_runner_wire::ProvisionCorrelation {
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
                    },
                },
            ),
        };

        let failure = state
            .accept(&acknowledgement)
            .expect_err("operation state is unavailable in this runtime");

        assert_eq!(failure.code, RejectionCode::Unavailable);
    }
}
