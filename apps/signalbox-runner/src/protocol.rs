//! Fail-closed newline runner-wire connection lifecycle.

use std::{
    error::Error,
    fmt, fs, io,
    os::unix::fs::{FileTypeExt as _, MetadataExt as _},
    path::Path,
};

use rustix::process::geteuid;
use serde_json::json;
use signalbox_runner_wire::{
    Advertise, Advertisement, CanonicalUuid, DIGEST_VERSION, DetailName, Digest, Enroll,
    FailureCategory, FailureDetail, Frame, FrameError, Heartbeat, HeartbeatAck, MAX_FRAME_BYTES,
    Message, OperationCorrelation, OperationFailed, OperationFailure, PositiveU64,
    ReconnectInventory, Registered, RejectionCode, Resume, ShutdownReason, ValueError,
    advertisement_digest, decode_line, encode_line,
};
use tokio::{
    io::{
        AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _,
        BufReader,
    },
    net::UnixStream,
};

use crate::{
    EnrollmentAuthority, EnrollmentReceipt, RunnerState, RunnerStateError, RunnerStateRoot,
};

const SOCKET_MODE: u32 = 0o600;
const PERMISSION_MASK: u32 = 0o7777;
const FIRST_RUNNER_SEQUENCE: u64 = 1;
const NO_ACCEPTED_PEER_SEQUENCE: u64 = 0;

/// Connects only to one stable owner-only same-user Unix socket identity.
pub async fn connect_verified(path: &Path) -> Result<UnixStream, SocketConnectError> {
    let first = fs::symlink_metadata(path).map_err(SocketConnectError::InspectSocket)?;
    let effective_user = geteuid().as_raw();
    let identity = SocketIdentity::capture(&first, effective_user)
        .ok_or(SocketConnectError::InvalidSocketIdentity)?;
    let stream = UnixStream::connect(path)
        .await
        .map_err(SocketConnectError::Connect)?;
    let peer = stream
        .peer_cred()
        .map_err(SocketConnectError::InspectPeer)?;
    if peer.uid() != effective_user {
        return Err(SocketConnectError::PeerOwnerMismatch {
            expected: effective_user,
            observed: peer.uid(),
        });
    }
    let second = fs::symlink_metadata(path).map_err(SocketConnectError::ReinspectSocket)?;
    if !identity.matches(&second, effective_user) {
        return Err(SocketConnectError::SocketIdentityChanged);
    }
    Ok(stream)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

impl SocketIdentity {
    fn capture(metadata: &fs::Metadata, effective_user: u32) -> Option<Self> {
        (metadata.file_type().is_socket()
            && metadata.uid() == effective_user
            && metadata.mode() & PERMISSION_MASK == SOCKET_MODE)
            .then_some(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
    }

    fn matches(self, metadata: &fs::Metadata, effective_user: u32) -> bool {
        Self::capture(metadata, effective_user) == Some(self)
    }
}

/// Typed evidence-bearing runner-socket connection failure.
#[derive(Debug)]
pub enum SocketConnectError {
    InspectSocket(io::Error),
    InvalidSocketIdentity,
    Connect(io::Error),
    InspectPeer(io::Error),
    PeerOwnerMismatch { expected: u32, observed: u32 },
    ReinspectSocket(io::Error),
    SocketIdentityChanged,
}

impl fmt::Display for SocketConnectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InspectSocket(_) => formatter.write_str("failed to inspect runner socket"),
            Self::InvalidSocketIdentity => {
                formatter.write_str("runner socket is not a stable owner-only socket")
            }
            Self::Connect(_) => formatter.write_str("failed to connect to runner socket"),
            Self::InspectPeer(_) => formatter.write_str("failed to inspect runner socket peer"),
            Self::PeerOwnerMismatch { expected, observed } => write!(
                formatter,
                "runner socket peer user {observed} differs from effective user {expected}"
            ),
            Self::ReinspectSocket(_) => {
                formatter.write_str("failed to re-inspect connected runner socket")
            }
            Self::SocketIdentityChanged => {
                formatter.write_str("runner socket identity changed while connecting")
            }
        }
    }
}

impl Error for SocketConnectError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InspectSocket(error)
            | Self::Connect(error)
            | Self::InspectPeer(error)
            | Self::ReinspectSocket(error) => Some(error),
            Self::InvalidSocketIdentity
            | Self::PeerOwnerMismatch { .. }
            | Self::SocketIdentityChanged => None,
        }
    }
}

/// Closed runner-wire message identity used in typed diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageKind {
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
}

impl MessageKind {
    fn of(message: &Message) -> Self {
        match message {
            Message::Enroll(_) => Self::Enroll,
            Message::Enrolled(_) => Self::Enrolled,
            Message::Resume(_) => Self::Resume,
            Message::Resumed(_) => Self::Resumed,
            Message::ReplacementPending(_) => Self::ReplacementPending,
            Message::Advertise(_) => Self::Advertise,
            Message::Registered(_) => Self::Registered,
            Message::Heartbeat(_) => Self::Heartbeat,
            Message::HeartbeatAck(_) => Self::HeartbeatAck,
            Message::WorkspaceLeakPage(_) => Self::WorkspaceLeakPage,
            Message::WorkspaceLeakRecorded(_) => Self::WorkspaceLeakRecorded,
            Message::WorkspaceProvision(_) => Self::WorkspaceProvision,
            Message::WorkspaceReady(_) => Self::WorkspaceReady,
            Message::WorkspaceRecorded(_) => Self::WorkspaceRecorded,
            Message::WorkspaceRelease(_) => Self::WorkspaceRelease,
            Message::WorkspaceReleased(_) => Self::WorkspaceReleased,
            Message::WorkspaceReleaseRecorded(_) => Self::WorkspaceReleaseRecorded,
            Message::LeaseOffer(_) => Self::LeaseOffer,
            Message::LeaseClaim(_) => Self::LeaseClaim,
            Message::LeaseClaimed(_) => Self::LeaseClaimed,
            Message::Dispatch(_) => Self::Dispatch,
            Message::Result(_) => Self::Result,
            Message::ResultRecorded(_) => Self::ResultRecorded,
            Message::OperationFailed(_) => Self::OperationFailed,
            Message::OperationFailureRecorded(_) => Self::OperationFailureRecorded,
            Message::Shutdown(_) => Self::Shutdown,
            Message::Rejected(_) => Self::Rejected,
        }
    }
}

impl fmt::Display for MessageKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
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
        })
    }
}

/// Exact establishment path completed for the live connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnrollmentOutcome {
    Enrolled,
    ReplacementPending,
    Resumed,
}

/// Honest terminal outcome of one established connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionEnd {
    DaemonShutdown {
        connection_epoch: PositiveU64,
    },
    UnsupportedOperationRefused {
        operation: MessageKind,
        category: FailureCategory,
    },
}

/// Closed local recovery gap; no wire recovery facts are fabricated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryGap {
    UnbornHeadNotRepresentable,
}

/// Typed proof that recovery is deliberately unavailable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoveryUnavailable {
    gap: RecoveryGap,
}

impl RecoveryUnavailable {
    /// Returns the exact representational gap preventing recovery.
    pub const fn gap(self) -> RecoveryGap {
        self.gap
    }
}

impl fmt::Display for RecoveryUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("runner recovery is unavailable because unborn HEAD is unrepresentable")
    }
}

impl Error for RecoveryUnavailable {}

/// Evidence that the runner cannot construct an epoch-correlated shutdown yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShutdownUnavailable {
    runner_id: CanonicalUuid,
    registration_revision: PositiveU64,
}

impl ShutdownUnavailable {
    pub const fn runner_id(self) -> CanonicalUuid {
        self.runner_id
    }

    pub const fn registration_revision(self) -> PositiveU64 {
        self.registration_revision
    }
}

impl fmt::Display for ShutdownUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "runner {} at registration {} has no daemon-issued connection epoch",
            self.runner_id,
            self.registration_revision.get()
        )
    }
}

impl Error for ShutdownUnavailable {}

/// Closed peer or local lifecycle violation with exact evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProtocolViolation {
    UnexpectedFrame {
        expected: MessageKind,
        received: MessageKind,
    },
    RequestMismatch {
        expected: CanonicalUuid,
        observed: CanonicalUuid,
    },
    AdvertisementDigestMismatch,
    RegistrationRevisionRegressed {
        prior: PositiveU64,
        observed: PositiveU64,
    },
    RegistrationRevisionDidNotAdvance {
        prior: PositiveU64,
        observed: PositiveU64,
    },
    ResumeDirectives,
    HeartbeatSequenceDidNotAdvance {
        prior: PositiveU64,
        observed: PositiveU64,
    },
    HeartbeatReplayMismatch {
        sequence: PositiveU64,
    },
    HeartbeatPeerSequenceMismatch {
        expected: u64,
        observed: u64,
    },
    RunnerSequenceExhausted,
    FailureAcknowledgementMismatch,
    InvalidShutdownReason,
    PendingRegistrationMutation,
}

impl fmt::Display for ProtocolViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedFrame { expected, received } => {
                write!(formatter, "expected {expected} but received {received}")
            }
            Self::RequestMismatch { expected, observed } => write!(
                formatter,
                "enrollment request correlation {observed} differs from {expected}"
            ),
            Self::AdvertisementDigestMismatch => {
                formatter.write_str("accepted advertisement digest differs from the sent value")
            }
            Self::RegistrationRevisionRegressed { prior, observed } => write!(
                formatter,
                "registration revision {} regressed from {}",
                observed.get(),
                prior.get()
            ),
            Self::RegistrationRevisionDidNotAdvance { prior, observed } => write!(
                formatter,
                "registration revision {} did not advance from {}",
                observed.get(),
                prior.get()
            ),
            Self::ResumeDirectives => {
                formatter.write_str("resume directives do not match the sent inventory")
            }
            Self::HeartbeatSequenceDidNotAdvance { prior, observed } => write!(
                formatter,
                "heartbeat challenge {} did not advance from {}",
                observed.get(),
                prior.get()
            ),
            Self::HeartbeatReplayMismatch { sequence } => write!(
                formatter,
                "heartbeat challenge {} replay changed payload",
                sequence.get()
            ),
            Self::HeartbeatPeerSequenceMismatch { expected, observed } => write!(
                formatter,
                "heartbeat accepted-peer sequence {observed} differs from {expected}"
            ),
            Self::RunnerSequenceExhausted => {
                formatter.write_str("runner heartbeat sequence exhausted")
            }
            Self::FailureAcknowledgementMismatch => {
                formatter.write_str("operation-failure acknowledgement correlation differs")
            }
            Self::InvalidShutdownReason => {
                formatter.write_str("daemon sent a shutdown frame with runner reason")
            }
            Self::PendingRegistrationMutation => {
                formatter.write_str("pending replacement cannot mutate registration")
            }
        }
    }
}

impl Error for ProtocolViolation {}

/// Typed connection lifecycle, framing, peer, or durable-state failure.
#[derive(Debug)]
pub enum RunnerConnectionError {
    State(RunnerStateError),
    Encode(FrameError),
    Decode(FrameError),
    Read(io::Error),
    Write(io::Error),
    PeerClosed,
    PeerRejected {
        code: RejectionCode,
        offending_kind: String,
    },
    Violation(ProtocolViolation),
    InvalidLocalFrame(ValueError),
}

impl fmt::Display for RunnerConnectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::State(_) => formatter.write_str("runner durable state update failed"),
            Self::Encode(_) => formatter.write_str("runner frame encoding failed"),
            Self::Decode(_) => formatter.write_str("daemon runner frame failed validation"),
            Self::Read(_) => formatter.write_str("failed to read runner wire"),
            Self::Write(_) => formatter.write_str("failed to write runner wire"),
            Self::PeerClosed => {
                formatter.write_str("daemon closed the runner wire without shutdown")
            }
            Self::PeerRejected {
                code,
                offending_kind,
            } => write!(formatter, "daemon rejected {offending_kind} with {code:?}"),
            Self::Violation(error) => write!(formatter, "runner protocol violation: {error}"),
            Self::InvalidLocalFrame(_) => formatter.write_str("runner local frame is invalid"),
        }
    }
}

impl Error for RunnerConnectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::State(error) => Some(error),
            Self::Encode(error) | Self::Decode(error) => Some(error),
            Self::Read(error) | Self::Write(error) => Some(error),
            Self::Violation(error) => Some(error),
            Self::InvalidLocalFrame(error) => Some(error),
            Self::PeerClosed | Self::PeerRejected { .. } => None,
        }
    }
}

impl From<RunnerStateError> for RunnerConnectionError {
    fn from(value: RunnerStateError) -> Self {
        Self::State(value)
    }
}

/// Established serial runner connection and exact active receipt.
pub struct RunnerConnection<S> {
    io: BufReader<S>,
    receipt: EnrollmentReceipt,
    advertisement: Advertisement,
    outcome: EnrollmentOutcome,
    heartbeat: Option<HeartbeatExchange>,
}

#[derive(Clone)]
struct HeartbeatExchange {
    challenge: Heartbeat,
    acknowledgement: HeartbeatAck,
}

impl<S> RunnerConnection<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Enrolls pristine state or resumes one exact fsynced receipt.
    pub async fn establish(
        stream: S,
        state: &mut RunnerStateRoot,
        advertisement: &Advertisement,
    ) -> Result<Self, RunnerConnectionError> {
        let digest = advertisement_digest(advertisement)
            .map_err(RunnerConnectionError::InvalidLocalFrame)?;
        let mut io = BufReader::new(stream);
        let (receipt, outcome) = match state.state().clone() {
            RunnerState::Pristine { request_id } => {
                send_message(
                    &mut io,
                    Message::Enroll(Enroll {
                        request_id,
                        digest_version: DIGEST_VERSION,
                        advertisement: advertisement.clone(),
                    }),
                )
                .await?;
                let message = receive_message(&mut io).await?;
                let (receipt, outcome) = accept_enrollment(message, request_id, digest)?;
                state.record_receipt(receipt.clone())?;
                (receipt, outcome)
            }
            RunnerState::Enrolled { receipt } => {
                let inventory = ReconnectInventory::default();
                send_message(
                    &mut io,
                    Message::Resume(Box::new(Resume {
                        request_id: receipt.request_id(),
                        digest_version: DIGEST_VERSION,
                        enrollment_id: receipt.enrollment_id(),
                        runner_id: receipt.runner_id(),
                        authentication_id: receipt.authentication_id(),
                        advertisement: advertisement.clone(),
                        prior_registration_revision: receipt.registration_revision(),
                        inventory: inventory.clone(),
                    })),
                )
                .await?;
                let message = receive_message(&mut io).await?;
                let resumed = match message {
                    Message::Resumed(resumed) => resumed,
                    Message::Rejected(rejected) => return Err(rejected_error(rejected)),
                    other => {
                        return Err(unexpected(MessageKind::Resumed, &other));
                    }
                };
                resumed
                    .directives
                    .validate_against(&inventory)
                    .map_err(|_| {
                        RunnerConnectionError::Violation(ProtocolViolation::ResumeDirectives)
                    })?;
                if resumed.registration_revision < receipt.registration_revision() {
                    return Err(RunnerConnectionError::Violation(
                        ProtocolViolation::RegistrationRevisionRegressed {
                            prior: receipt.registration_revision(),
                            observed: resumed.registration_revision,
                        },
                    ));
                }
                let receipt = state.record_registration(resumed.registration_revision, digest)?;
                (receipt, EnrollmentOutcome::Resumed)
            }
        };
        Ok(Self {
            io,
            receipt,
            advertisement: advertisement.clone(),
            outcome,
            heartbeat: None,
        })
    }

    /// Returns the establishment path used by this live connection.
    pub const fn outcome(&self) -> EnrollmentOutcome {
        self.outcome
    }

    /// Borrows the exact current fsynced receipt.
    pub const fn receipt(&self) -> &EnrollmentReceipt {
        &self.receipt
    }

    /// Borrows the exact advertisement registered for this connection.
    pub const fn advertisement(&self) -> &Advertisement {
        &self.advertisement
    }

    /// Reports the recovery design gap without constructing wire recovery facts.
    pub const fn recovery_unavailable(&self) -> RecoveryUnavailable {
        RecoveryUnavailable {
            gap: RecoveryGap::UnbornHeadNotRepresentable,
        }
    }

    /// Returns typed missing evidence instead of fabricating a shutdown epoch.
    pub const fn runner_shutdown_unavailable(&self) -> ShutdownUnavailable {
        ShutdownUnavailable {
            runner_id: self.receipt.runner_id(),
            registration_revision: self.receipt.registration_revision(),
        }
    }

    /// Replaces all six inventories and persists only an exact `registered` reply.
    pub async fn register_advertisement(
        &mut self,
        state: &mut RunnerStateRoot,
        advertisement: Advertisement,
    ) -> Result<(), RunnerConnectionError> {
        if self.receipt.authority() == EnrollmentAuthority::ReplacementPending {
            return Err(RunnerConnectionError::Violation(
                ProtocolViolation::PendingRegistrationMutation,
            ));
        }
        let expected_digest = advertisement_digest(&advertisement)
            .map_err(RunnerConnectionError::InvalidLocalFrame)?;
        let prior = self.receipt.registration_revision();
        send_message(
            &mut self.io,
            Message::Advertise(Advertise {
                enrollment_id: self.receipt.enrollment_id(),
                runner_id: self.receipt.runner_id(),
                authentication_id: self.receipt.authentication_id(),
                registration_revision: prior,
                advertisement: advertisement.clone(),
            }),
        )
        .await?;
        let registered = match receive_message(&mut self.io).await? {
            Message::Registered(registered) => registered,
            Message::Rejected(rejected) => return Err(rejected_error(rejected)),
            other => return Err(unexpected(MessageKind::Registered, &other)),
        };
        validate_registered(&registered, prior, &expected_digest)?;
        self.receipt = state.record_registration(
            registered.registration_revision,
            registered.advertisement_digest,
        )?;
        self.advertisement = advertisement;
        Ok(())
    }

    /// Serves heartbeats and fails closed every other post-registration frame.
    pub async fn serve(
        &mut self,
        state: &mut RunnerStateRoot,
    ) -> Result<ConnectionEnd, RunnerConnectionError> {
        loop {
            if let Some(end) = self.serve_one(state).await? {
                return Ok(end);
            }
        }
    }

    /// Handles one complete daemon frame; exposed for hermetic protocol harnesses.
    pub async fn serve_one(
        &mut self,
        _state: &mut RunnerStateRoot,
    ) -> Result<Option<ConnectionEnd>, RunnerConnectionError> {
        let message = receive_message(&mut self.io).await?;
        match message {
            Message::Heartbeat(challenge) => {
                let acknowledgement = self.heartbeat_acknowledgement(challenge)?;
                send_message(&mut self.io, Message::HeartbeatAck(acknowledgement)).await?;
                Ok(None)
            }
            Message::Shutdown(shutdown) if shutdown.reason == ShutdownReason::DaemonShutdown => {
                Ok(Some(ConnectionEnd::DaemonShutdown {
                    connection_epoch: shutdown.connection_epoch,
                }))
            }
            Message::Shutdown(_) => Err(RunnerConnectionError::Violation(
                ProtocolViolation::InvalidShutdownReason,
            )),
            Message::WorkspaceProvision(provision) => {
                let correlation = OperationCorrelation::Provision(provision.correlation);
                self.refuse_unsupported(
                    MessageKind::WorkspaceProvision,
                    correlation,
                    FailureCategory::SandboxUnavailable,
                )
                .await
                .map(Some)
            }
            Message::WorkspaceRelease(release) => {
                let correlation = OperationCorrelation::Release(release.correlation);
                self.refuse_unsupported(
                    MessageKind::WorkspaceRelease,
                    correlation,
                    FailureCategory::WorkspaceCleanupFailed,
                )
                .await
                .map(Some)
            }
            Message::LeaseOffer(offer) => {
                let correlation = OperationCorrelation::LeaseOffer(offer.correlation);
                self.refuse_unsupported(
                    MessageKind::LeaseOffer,
                    correlation,
                    FailureCategory::LeaseAdmissionRefused,
                )
                .await
                .map(Some)
            }
            Message::Rejected(rejected) => Err(rejected_error(rejected)),
            other => Err(RunnerConnectionError::Violation(
                ProtocolViolation::UnexpectedFrame {
                    expected: MessageKind::Heartbeat,
                    received: MessageKind::of(&other),
                },
            )),
        }
    }

    fn heartbeat_acknowledgement(
        &mut self,
        challenge: Heartbeat,
    ) -> Result<HeartbeatAck, RunnerConnectionError> {
        if let Some(previous) = &self.heartbeat {
            if challenge.sequence == previous.challenge.sequence {
                if challenge != previous.challenge {
                    return Err(RunnerConnectionError::Violation(
                        ProtocolViolation::HeartbeatReplayMismatch {
                            sequence: challenge.sequence,
                        },
                    ));
                }
                return Ok(previous.acknowledgement.clone());
            }
            if challenge.sequence < previous.challenge.sequence {
                return Err(RunnerConnectionError::Violation(
                    ProtocolViolation::HeartbeatSequenceDidNotAdvance {
                        prior: previous.challenge.sequence,
                        observed: challenge.sequence,
                    },
                ));
            }
        }
        let expected_peer_sequence = self
            .heartbeat
            .as_ref()
            .map_or(NO_ACCEPTED_PEER_SEQUENCE, |exchange| {
                exchange.acknowledgement.runner_sequence.get()
            });
        if challenge.last_accepted_peer_sequence != expected_peer_sequence {
            return Err(RunnerConnectionError::Violation(
                ProtocolViolation::HeartbeatPeerSequenceMismatch {
                    expected: expected_peer_sequence,
                    observed: challenge.last_accepted_peer_sequence,
                },
            ));
        }
        let runner_sequence = match &self.heartbeat {
            None => FIRST_RUNNER_SEQUENCE,
            Some(exchange) => exchange
                .acknowledgement
                .runner_sequence
                .get()
                .checked_add(1)
                .ok_or(RunnerConnectionError::Violation(
                    ProtocolViolation::RunnerSequenceExhausted,
                ))?,
        };
        let acknowledgement = HeartbeatAck {
            challenge_sequence: challenge.sequence,
            runner_sequence: PositiveU64::try_new(runner_sequence)
                .map_err(RunnerConnectionError::InvalidLocalFrame)?,
            lease_phase: None,
            workspace_phase: None,
        };
        self.heartbeat = Some(HeartbeatExchange {
            challenge,
            acknowledgement: acknowledgement.clone(),
        });
        Ok(acknowledgement)
    }

    async fn refuse_unsupported(
        &mut self,
        operation: MessageKind,
        correlation: OperationCorrelation,
        category: FailureCategory,
    ) -> Result<ConnectionEnd, RunnerConnectionError> {
        let detail = FailureDetail::try_new(
            DetailName::try_new("runner.runtime-unavailable".to_owned())
                .map_err(RunnerConnectionError::InvalidLocalFrame)?,
            format!("{operation} has no compiled runtime provider"),
            json!({}),
        )
        .map_err(RunnerConnectionError::InvalidLocalFrame)?;
        send_message(
            &mut self.io,
            Message::OperationFailed(OperationFailed {
                failure: OperationFailure {
                    correlation: correlation.clone(),
                    category,
                    detail,
                },
            }),
        )
        .await?;
        match receive_message(&mut self.io).await? {
            Message::OperationFailureRecorded(recorded) if recorded.correlation == correlation => {
                Ok(ConnectionEnd::UnsupportedOperationRefused {
                    operation,
                    category,
                })
            }
            Message::OperationFailureRecorded(_) => Err(RunnerConnectionError::Violation(
                ProtocolViolation::FailureAcknowledgementMismatch,
            )),
            Message::Rejected(rejected) => Err(rejected_error(rejected)),
            other => Err(unexpected(MessageKind::OperationFailureRecorded, &other)),
        }
    }
}

fn accept_enrollment(
    message: Message,
    request_id: CanonicalUuid,
    expected_digest: Digest,
) -> Result<(EnrollmentReceipt, EnrollmentOutcome), RunnerConnectionError> {
    match message {
        Message::Enrolled(enrolled) => {
            if enrolled.request_id != request_id {
                return Err(request_mismatch(request_id, enrolled.request_id));
            }
            if enrolled.advertisement_digest != expected_digest {
                return Err(digest_mismatch());
            }
            Ok((
                EnrollmentReceipt::new(
                    request_id,
                    enrolled.enrollment_id,
                    enrolled.runner_id,
                    enrolled.authentication_id,
                    enrolled.registration_revision,
                    enrolled.advertisement_digest,
                    EnrollmentAuthority::Active,
                ),
                EnrollmentOutcome::Enrolled,
            ))
        }
        Message::ReplacementPending(pending) => {
            if pending.request_id != request_id {
                return Err(request_mismatch(request_id, pending.request_id));
            }
            if pending.advertisement_digest != expected_digest {
                return Err(digest_mismatch());
            }
            Ok((
                EnrollmentReceipt::new(
                    request_id,
                    pending.enrollment_id,
                    pending.runner_id,
                    pending.authentication_id,
                    pending.registration_revision,
                    pending.advertisement_digest,
                    EnrollmentAuthority::ReplacementPending,
                ),
                EnrollmentOutcome::ReplacementPending,
            ))
        }
        Message::Rejected(rejected) => Err(rejected_error(rejected)),
        other => Err(unexpected(MessageKind::Enrolled, &other)),
    }
}

fn validate_registered(
    registered: &Registered,
    prior: PositiveU64,
    expected_digest: &Digest,
) -> Result<(), RunnerConnectionError> {
    if registered.registration_revision <= prior {
        return Err(RunnerConnectionError::Violation(
            ProtocolViolation::RegistrationRevisionDidNotAdvance {
                prior,
                observed: registered.registration_revision,
            },
        ));
    }
    if &registered.advertisement_digest != expected_digest {
        return Err(digest_mismatch());
    }
    Ok(())
}

fn request_mismatch(expected: CanonicalUuid, observed: CanonicalUuid) -> RunnerConnectionError {
    RunnerConnectionError::Violation(ProtocolViolation::RequestMismatch { expected, observed })
}

fn digest_mismatch() -> RunnerConnectionError {
    RunnerConnectionError::Violation(ProtocolViolation::AdvertisementDigestMismatch)
}

fn unexpected(expected: MessageKind, observed: &Message) -> RunnerConnectionError {
    RunnerConnectionError::Violation(ProtocolViolation::UnexpectedFrame {
        expected,
        received: MessageKind::of(observed),
    })
}

fn rejected_error(rejected: signalbox_runner_wire::Rejected) -> RunnerConnectionError {
    RunnerConnectionError::PeerRejected {
        code: rejected.code,
        offending_kind: rejected.offending_kind,
    }
}

async fn send_message<S>(
    io: &mut BufReader<S>,
    message: Message,
) -> Result<(), RunnerConnectionError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let frame = Frame::try_new(message).map_err(RunnerConnectionError::Encode)?;
    let encoded = encode_line(&frame).map_err(RunnerConnectionError::Encode)?;
    io.get_mut()
        .write_all(&encoded)
        .await
        .map_err(RunnerConnectionError::Write)?;
    io.get_mut()
        .flush()
        .await
        .map_err(RunnerConnectionError::Write)
}

async fn receive_message<S>(io: &mut BufReader<S>) -> Result<Message, RunnerConnectionError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut line = Vec::new();
    let mut bounded = io.take((MAX_FRAME_BYTES + 1) as u64);
    let bytes = bounded
        .read_until(b'\n', &mut line)
        .await
        .map_err(RunnerConnectionError::Read)?;
    if bytes == 0 {
        return Err(RunnerConnectionError::PeerClosed);
    }
    decode_line(&line)
        .map(|frame| frame.message)
        .map_err(RunnerConnectionError::Decode)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use tokio::io::DuplexStream;
    use uuid::Uuid;

    use signalbox_runner_wire::{
        Enrolled, OperationFailureRecorded, ReconnectDirectives, Resumed, Shutdown,
        WorkspaceProvision,
    };

    use super::*;

    const TEST_WIRE_BYTES: usize = 64 * 1024;
    /// Arbitrary daemon-issued enrollment identity used by protocol fixtures.
    const ARBITRARY_ENROLLMENT_UUID: u128 = 0x100;
    /// Arbitrary daemon-issued runner identity used by protocol fixtures.
    const ARBITRARY_RUNNER_UUID: u128 = 0x200;
    /// Arbitrary daemon-issued authentication-reference identity used by protocol fixtures.
    const ARBITRARY_AUTHENTICATION_UUID: u128 = 0x300;
    /// Arbitrary workspace authorization identity used by the unsupported-operation test.
    const ARBITRARY_AUTHORIZATION_UUID: u128 = 0x400;
    /// Arbitrary session identity used by the unsupported-operation test.
    const ARBITRARY_SESSION_UUID: u128 = 0x500;
    const INITIAL_REGISTRATION_REVISION: u64 = 1;
    const NEXT_REGISTRATION_REVISION: u64 = 2;
    const FIRST_CHALLENGE_SEQUENCE: u64 = 7;
    const CONNECTION_EPOCH: u64 = 11;

    fn empty_advertisement() -> Advertisement {
        Advertisement {
            capability_classes: Vec::new(),
            tools: Vec::new(),
            workspace_capabilities: Vec::new(),
            sandbox_profiles: Vec::new(),
            credential_profiles: Vec::new(),
            repositories: Vec::new(),
        }
    }

    fn positive(value: u64) -> PositiveU64 {
        PositiveU64::try_new(value).expect("the fixture value is positive")
    }

    fn identity(value: u128) -> CanonicalUuid {
        CanonicalUuid::from_uuid(Uuid::from_u128(value))
    }

    fn issued_receipt(request_id: CanonicalUuid) -> EnrollmentReceipt {
        EnrollmentReceipt::new(
            request_id,
            identity(ARBITRARY_ENROLLMENT_UUID),
            identity(ARBITRARY_RUNNER_UUID),
            identity(ARBITRARY_AUTHENTICATION_UUID),
            positive(INITIAL_REGISTRATION_REVISION),
            advertisement_digest(&empty_advertisement())
                .expect("the explicit empty advertisement has a digest"),
            EnrollmentAuthority::Active,
        )
    }

    fn state_root(parent: &TempDir) -> RunnerStateRoot {
        RunnerStateRoot::open(&parent.path().join("runner-state"))
            .expect("the private state root opens")
    }

    async fn receive_hub_message(io: &mut BufReader<DuplexStream>) -> Message {
        receive_message(io)
            .await
            .expect("the runner sends one valid frame")
    }

    async fn send_hub_message(io: &mut BufReader<DuplexStream>, message: Message) {
        send_message(io, message)
            .await
            .expect("the hub sends one valid frame");
    }

    #[tokio::test]
    async fn enrollment_round_trips_exact_explicit_advertisement_and_journals_receipt() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = state_root(&parent);
        let advertisement = empty_advertisement();
        let request_id = state.state().request_id();
        let advertisement_digest =
            advertisement_digest(&advertisement).expect("the explicit advertisement has a digest");
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = RunnerConnection::establish(runner_io, &mut state, &advertisement);
        let hub = async {
            let observed = receive_hub_message(&mut hub_io).await;
            assert_eq!(
                observed,
                Message::Enroll(Enroll {
                    request_id,
                    digest_version: DIGEST_VERSION,
                    advertisement: advertisement.clone(),
                })
            );
            send_hub_message(
                &mut hub_io,
                Message::Enrolled(Enrolled {
                    request_id,
                    enrollment_id: identity(ARBITRARY_ENROLLMENT_UUID),
                    runner_id: identity(ARBITRARY_RUNNER_UUID),
                    authentication_id: identity(ARBITRARY_AUTHENTICATION_UUID),
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    advertisement_digest: advertisement_digest.clone(),
                }),
            )
            .await;
        };
        let (connection, ()) = tokio::join!(runner, hub);
        let connection = connection.expect("the enrollment completes");

        assert_eq!(connection.outcome(), EnrollmentOutcome::Enrolled);
        assert_eq!(connection.advertisement(), &advertisement);
        assert_eq!(state.state().receipt(), Some(connection.receipt()));
        assert_eq!(
            connection.receipt().advertisement_digest(),
            &advertisement_digest
        );
    }

    #[tokio::test]
    async fn restart_resumes_exact_journaled_identities_and_updates_revision() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut first = state_root(&parent);
        let receipt = issued_receipt(first.state().request_id());
        first
            .record_receipt(receipt.clone())
            .expect("the issued receipt is journaled before restart");
        drop(first);
        let mut restarted = state_root(&parent);
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = RunnerConnection::establish(runner_io, &mut restarted, &advertisement);
        let hub = async {
            let observed = receive_hub_message(&mut hub_io).await;
            assert_eq!(
                observed,
                Message::Resume(Box::new(Resume {
                    request_id: receipt.request_id(),
                    digest_version: DIGEST_VERSION,
                    enrollment_id: receipt.enrollment_id(),
                    runner_id: receipt.runner_id(),
                    authentication_id: receipt.authentication_id(),
                    advertisement: advertisement.clone(),
                    prior_registration_revision: receipt.registration_revision(),
                    inventory: ReconnectInventory::default(),
                }))
            );
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(NEXT_REGISTRATION_REVISION),
                    directives: ReconnectDirectives::default(),
                })),
            )
            .await;
        };
        let (connection, ()) = tokio::join!(runner, hub);
        let connection = connection.expect("the restart resume completes");

        assert_eq!(connection.outcome(), EnrollmentOutcome::Resumed);
        assert_eq!(
            connection.receipt().registration_revision(),
            positive(NEXT_REGISTRATION_REVISION)
        );
        assert_eq!(
            restarted
                .state()
                .receipt()
                .expect("the resumed receipt stays durable")
                .registration_revision(),
            connection.receipt().registration_revision()
        );
    }

    #[tokio::test]
    async fn heartbeat_ack_repeats_challenge_and_advances_runner_sequence() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = state_root(&parent);
        let receipt = issued_receipt(state.state().request_id());
        state
            .record_receipt(receipt)
            .expect("the issued receipt is journaled");
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before heartbeat");
            let outcome = connection
                .serve_one(&mut state)
                .await
                .expect("the heartbeat is served");
            (connection, outcome)
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    directives: ReconnectDirectives::default(),
                })),
            )
            .await;
            send_hub_message(
                &mut hub_io,
                Message::Heartbeat(Heartbeat {
                    sequence: positive(FIRST_CHALLENGE_SEQUENCE),
                    last_accepted_peer_sequence: NO_ACCEPTED_PEER_SEQUENCE,
                }),
            )
            .await;
            receive_hub_message(&mut hub_io).await
        };
        let ((connection, outcome), acknowledgement) = tokio::join!(runner, hub);

        assert_eq!(outcome, None);
        assert_eq!(
            acknowledgement,
            Message::HeartbeatAck(HeartbeatAck {
                challenge_sequence: positive(FIRST_CHALLENGE_SEQUENCE),
                runner_sequence: positive(FIRST_RUNNER_SEQUENCE),
                lease_phase: None,
                workspace_phase: None,
            })
        );
        assert_eq!(
            connection.recovery_unavailable().gap(),
            RecoveryGap::UnbornHeadNotRepresentable
        );
        assert_eq!(
            connection.runner_shutdown_unavailable().runner_id(),
            connection.receipt().runner_id()
        );
    }

    #[tokio::test]
    async fn advertise_accepts_exact_registered_digest_and_journals_new_revision() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = state_root(&parent);
        let receipt = issued_receipt(state.state().request_id());
        state
            .record_receipt(receipt.clone())
            .expect("the issued receipt is journaled");
        let advertisement = empty_advertisement();
        let expected_digest = advertisement_digest(&advertisement)
            .expect("the replacement advertisement has a digest");
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before re-registration");
            connection
                .register_advertisement(&mut state, advertisement.clone())
                .await
                .expect("the exact registered acknowledgement is accepted");
            connection
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: receipt.registration_revision(),
                    directives: ReconnectDirectives::default(),
                })),
            )
            .await;
            let observed = receive_hub_message(&mut hub_io).await;
            assert_eq!(
                observed,
                Message::Advertise(Advertise {
                    enrollment_id: receipt.enrollment_id(),
                    runner_id: receipt.runner_id(),
                    authentication_id: receipt.authentication_id(),
                    registration_revision: receipt.registration_revision(),
                    advertisement: advertisement.clone(),
                })
            );
            send_hub_message(
                &mut hub_io,
                Message::Registered(Registered {
                    registration_revision: positive(NEXT_REGISTRATION_REVISION),
                    advertisement_digest: expected_digest.clone(),
                }),
            )
            .await;
        };
        let (connection, ()) = tokio::join!(runner, hub);

        assert_eq!(
            connection.receipt().registration_revision(),
            positive(NEXT_REGISTRATION_REVISION)
        );
        assert_eq!(
            connection.receipt().advertisement_digest(),
            &expected_digest
        );
        assert_eq!(state.state().receipt(), Some(connection.receipt()));
    }

    #[tokio::test]
    async fn unsupported_workspace_provision_is_exactly_refused_and_acknowledged() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = state_root(&parent);
        let receipt = issued_receipt(state.state().request_id());
        state
            .record_receipt(receipt.clone())
            .expect("the issued receipt is journaled");
        let advertisement = empty_advertisement();
        let correlation = signalbox_runner_wire::ProvisionCorrelation {
            authorization_id: identity(ARBITRARY_AUTHORIZATION_UUID),
            session_id: identity(ARBITRARY_SESSION_UUID),
            placement_revision: positive(INITIAL_REGISTRATION_REVISION),
            runner_id: receipt.runner_id(),
            registration_revision: receipt.registration_revision(),
            repository: None,
            sandbox_profile: signalbox_runner_wire::SandboxProfile::Ambient,
            credential_profile: None,
        };
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before refusal");
            connection
                .serve_one(&mut state)
                .await
                .expect("the unsupported operation is refused")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: receipt.registration_revision(),
                    directives: ReconnectDirectives::default(),
                })),
            )
            .await;
            send_hub_message(
                &mut hub_io,
                Message::WorkspaceProvision(WorkspaceProvision {
                    correlation: correlation.clone(),
                }),
            )
            .await;
            let failure = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::OperationFailureRecorded(OperationFailureRecorded {
                    correlation: OperationCorrelation::Provision(correlation.clone()),
                }),
            )
            .await;
            failure
        };
        let (outcome, failure) = tokio::join!(runner, hub);

        assert_eq!(
            outcome,
            Some(ConnectionEnd::UnsupportedOperationRefused {
                operation: MessageKind::WorkspaceProvision,
                category: FailureCategory::SandboxUnavailable,
            })
        );
        let expected_detail = FailureDetail::try_new(
            DetailName::try_new("runner.runtime-unavailable".to_owned())
                .expect("the fixture detail name is valid"),
            "workspace_provision has no compiled runtime provider".to_owned(),
            json!({}),
        )
        .expect("the fixture failure detail is bounded");
        assert_eq!(
            failure,
            Message::OperationFailed(OperationFailed {
                failure: OperationFailure {
                    correlation: OperationCorrelation::Provision(correlation),
                    category: FailureCategory::SandboxUnavailable,
                    detail: expected_detail,
                },
            })
        );
    }

    #[tokio::test]
    async fn daemon_shutdown_returns_exact_epoch() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = state_root(&parent);
        let receipt = issued_receipt(state.state().request_id());
        state
            .record_receipt(receipt)
            .expect("the issued receipt is journaled");
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before shutdown");
            connection
                .serve_one(&mut state)
                .await
                .expect("daemon shutdown is accepted")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    directives: ReconnectDirectives::default(),
                })),
            )
            .await;
            send_hub_message(
                &mut hub_io,
                Message::Shutdown(Shutdown {
                    connection_epoch: positive(CONNECTION_EPOCH),
                    reason: ShutdownReason::DaemonShutdown,
                }),
            )
            .await;
        };
        let (outcome, ()) = tokio::join!(runner, hub);

        assert_eq!(
            outcome,
            Some(ConnectionEnd::DaemonShutdown {
                connection_epoch: positive(CONNECTION_EPOCH),
            })
        );
    }
}
