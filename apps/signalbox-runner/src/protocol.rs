//! Fail-closed newline runner-wire connection lifecycle.

use std::{
    error::Error,
    fmt, fs,
    future::Future,
    io,
    os::unix::fs::{FileTypeExt as _, MetadataExt as _},
    path::Path,
};

use rustix::process::geteuid;
use signalbox_runner_wire::{
    Advertise, Advertisement, AvailableCorrelation, CanonicalUuid, DIGEST_VERSION, DetailName,
    Digest, DirectiveAction, Dispatch, EffectClass, Enroll, FailureCategory, FailureDetail, Frame,
    FrameError, Heartbeat, HeartbeatAck, HeartbeatWorkspacePhase, LeaseClaim, LeaseClaimed,
    LeaseCorrelation, LeaseOffer, LeasePhase, LeasePhaseKind, MAX_FRAME_BYTES, Message,
    OperationCorrelation, OperationFailed, OperationFailure, PositiveU64, ReconnectDirectives,
    ReconnectInventory, Registered, Rejected, RejectionCode, ResultFrame, Resume, SandboxProfile,
    Shutdown, ShutdownReason, TerminalResult, ValueError, WorkspaceFailureCorrelation,
    WorkspaceOperation, WorkspaceReleased, advertisement_digest, decode_line, encode_line,
};
use tokio::{
    io::{
        AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _,
        BufReader,
    },
    net::UnixStream,
};

use crate::{
    AcceptedWorkspaceRelease, EnrollmentAuthority, EnrollmentReceipt, RunnerState,
    RunnerStateError, RunnerStateRoot,
};
use signalbox_tools_exec::{ExecArguments, SANDBOXED_EXEC_NAME};

const SOCKET_MODE: u32 = 0o600;
const PERMISSION_MASK: u32 = 0o7777;
const FIRST_RUNNER_SEQUENCE: u64 = 1;
const NO_ACCEPTED_PEER_SEQUENCE: u64 = 0;
const TOOL_UNAVAILABLE_DETAIL_CODE: &str = "tool-unavailable";
const TOOL_UNAVAILABLE_DETAIL_MESSAGE: &str = "offered tool is absent from the registered catalog";
const LEASE_REFUSED_DETAIL_CODE: &str = "lease-admission-refused";
const LEASE_REFUSED_DETAIL_MESSAGE: &str = "offered execution facts are not locally admissible";
const WORKSPACE_CLEANUP_DETAIL_CODE: &str = "workspace-cleanup-failed";
const WORKSPACE_CLEANUP_DETAIL_MESSAGE: &str = "the accepted workspace cleanup failed";

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

impl SocketConnectError {
    /// Reports whether a hub restart can make a later connection attempt valid.
    pub fn is_reconnectable(&self) -> bool {
        match self {
            Self::InspectSocket(error) => error.kind() == io::ErrorKind::NotFound,
            Self::Connect(error) => matches!(
                error.kind(),
                io::ErrorKind::NotFound
                    | io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::TimedOut
            ),
            Self::ReinspectSocket(error) => error.kind() == io::ErrorKind::NotFound,
            Self::InvalidSocketIdentity | Self::SocketIdentityChanged => true,
            Self::InspectPeer(_) | Self::PeerOwnerMismatch { .. } => false,
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
    RunnerShutdown {
        connection_epoch: PositiveU64,
    },
    /// A fatal stale-connection rejection was written before closing the stream.
    StaleConnectionRejected {
        connection_epoch: PositiveU64,
    },
}

/// One serial serving-loop boundary observed without cancelling an outbound frame.
#[derive(Debug, Eq, PartialEq)]
pub enum ServeOutcome {
    /// The daemon supplied a clean terminal order.
    ConnectionEnded(ConnectionEnd),
    /// Local shutdown is ready at a boundary with no in-flight write.
    ShutdownReady,
    /// Canonical claim and dispatch reached the local executor boundary.
    DispatchReady(Box<RunnerDispatchReady>),
    /// One exact release was durably accepted before local cleanup handoff.
    WorkspaceReleaseReady(Box<RunnerWorkspaceReleaseReady>),
}

impl ServeOutcome {
    /// Consumes only the dispatch-ready arm for executor composition.
    pub fn into_dispatch_ready(self) -> Result<RunnerDispatchReady, Self> {
        match self {
            Self::DispatchReady(dispatch) => Ok(*dispatch),
            other => Err(other),
        }
    }

    /// Consumes only the release-ready arm for cleanup composition.
    pub fn into_workspace_release_ready(self) -> Result<RunnerWorkspaceReleaseReady, Self> {
        match self {
            Self::WorkspaceReleaseReady(release) => Ok(*release),
            other => Err(other),
        }
    }
}

/// One canonical claimed dispatch that crossed the serial protocol gate.
#[derive(Debug, Eq, PartialEq)]
pub struct RunnerDispatchReady {
    correlation: LeaseCorrelation,
    normalized_arguments: serde_json::Value,
    connection_epoch: PositiveU64,
}

impl RunnerDispatchReady {
    /// Borrows the complete immutable lease and physical-attempt correlation.
    pub const fn correlation(&self) -> &LeaseCorrelation {
        &self.correlation
    }

    /// Borrows the daemon's canonical normalized argument object.
    pub const fn normalized_arguments(&self) -> &serde_json::Value {
        &self.normalized_arguments
    }
}

/// One exact workspace release whose accepted journal precedes cleanup.
#[derive(Debug, Eq, PartialEq)]
pub struct RunnerWorkspaceReleaseReady {
    accepted: AcceptedWorkspaceRelease,
    connection_epoch: PositiveU64,
}

impl RunnerWorkspaceReleaseReady {
    /// Borrows the exact accepted release correlation.
    pub const fn correlation(&self) -> &signalbox_runner_wire::ReleaseCorrelation {
        self.accepted.correlation()
    }

    /// Borrows the journal-backed authority passed to the cleanup adapter.
    pub const fn accepted(&self) -> &AcceptedWorkspaceRelease {
        &self.accepted
    }
}

/// Closed local recovery gap; no wire recovery facts are fabricated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryGap {
    /// No live workspace provisioning operation is composed with the connection.
    WorkspaceProvisioningUnavailable,
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
        formatter
            .write_str("runner recovery is unavailable because workspace provisioning is absent")
    }
}

impl Error for RecoveryUnavailable {}

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
    RegistrationRevisionExhausted {
        prior: PositiveU64,
    },
    InitialRegistrationRevision {
        observed: PositiveU64,
    },
    ResumeAdvertisementChangedWithoutRevision {
        revision: PositiveU64,
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
    WorkspaceAcknowledgementMismatch,
    ResultAcknowledgementMismatch,
    LeaseAcknowledgementMismatch,
    DispatchMismatch,
    ExecutionHandoffMismatch,
    WorkspaceReleaseHandoffMismatch,
    WorkspaceReleaseHandoffUncomposed,
    InvalidShutdownReason,
    PendingRegistrationMutation,
    ConnectionCorrelationMismatch,
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
            Self::RegistrationRevisionExhausted { prior } => write!(
                formatter,
                "registration revision {} has no successor",
                prior.get()
            ),
            Self::InitialRegistrationRevision { observed } => write!(
                formatter,
                "initial registration revision {} is not one",
                observed.get()
            ),
            Self::ResumeAdvertisementChangedWithoutRevision { revision } => write!(
                formatter,
                "resume changed advertisement without advancing registration revision {}",
                revision.get()
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
            Self::WorkspaceAcknowledgementMismatch => {
                formatter.write_str("workspace acknowledgement correlation differs")
            }
            Self::ResultAcknowledgementMismatch => {
                formatter.write_str("result acknowledgement correlation differs")
            }
            Self::LeaseAcknowledgementMismatch => {
                formatter.write_str("claimed-lease acknowledgement correlation differs")
            }
            Self::DispatchMismatch => {
                formatter.write_str("dispatch correlation differs from the claimed capability")
            }
            Self::ExecutionHandoffMismatch => {
                formatter.write_str("dispatch handoff belongs to another connection")
            }
            Self::WorkspaceReleaseHandoffMismatch => {
                formatter.write_str("workspace release handoff belongs to another connection")
            }
            Self::WorkspaceReleaseHandoffUncomposed => {
                formatter.write_str("workspace release handoff has no cleanup composition")
            }
            Self::InvalidShutdownReason => {
                formatter.write_str("daemon sent a shutdown frame with runner reason")
            }
            Self::PendingRegistrationMutation => {
                formatter.write_str("pending replacement cannot mutate registration")
            }
            Self::ConnectionCorrelationMismatch => {
                formatter.write_str("runner operation names a stale or foreign connection")
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
        available_correlation: Box<AvailableCorrelation>,
    },
    Violation(ProtocolViolation),
    InvalidLocalFrame(ValueError),
    RecoveryUnavailable(RecoveryUnavailable),
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
                ..
            } => write!(formatter, "daemon rejected {offending_kind} with {code:?}"),
            Self::Violation(error) => write!(formatter, "runner protocol violation: {error}"),
            Self::InvalidLocalFrame(_) => formatter.write_str("runner local frame is invalid"),
            Self::RecoveryUnavailable(error) => error.fmt(formatter),
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
            Self::RecoveryUnavailable(error) => Some(error),
            Self::PeerClosed | Self::PeerRejected { .. } => None,
        }
    }
}

impl From<RunnerStateError> for RunnerConnectionError {
    fn from(value: RunnerStateError) -> Self {
        Self::State(value)
    }
}

impl RunnerConnectionError {
    /// Reports whether transport loss can be repaired by exact receipt resume.
    pub const fn is_reconnectable(&self) -> bool {
        matches!(
            self,
            Self::PeerClosed
                | Self::Read(_)
                | Self::Write(_)
                | Self::PeerRejected {
                    code: RejectionCode::Unavailable | RejectionCode::ShuttingDown,
                    ..
                }
        )
    }
}

/// Established serial runner connection and exact active receipt.
pub struct RunnerConnection<S> {
    io: BufReader<S>,
    receipt: EnrollmentReceipt,
    advertisement: Advertisement,
    outcome: EnrollmentOutcome,
    connection_epoch: PositiveU64,
    heartbeat: Option<HeartbeatExchange>,
    claimed_capability: Option<LeaseCorrelation>,
    pending_offer: Option<LeaseOffer>,
    deferred_release: Option<signalbox_runner_wire::WorkspaceRelease>,
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
        let (receipt, outcome, connection_epoch) = match state.state().clone() {
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
                let (receipt, outcome, connection_epoch) =
                    accept_enrollment(message, request_id, digest)?;
                state.record_receipt(receipt.clone())?;
                (receipt, outcome, connection_epoch)
            }
            RunnerState::Enrolled { receipt } => {
                let inventory = state.reconnect_inventory().clone();
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
                if resumed.registration_revision == receipt.registration_revision()
                    && digest != *receipt.advertisement_digest()
                {
                    return Err(RunnerConnectionError::Violation(
                        ProtocolViolation::ResumeAdvertisementChangedWithoutRevision {
                            revision: resumed.registration_revision,
                        },
                    ));
                }
                let receipt = state.record_registration(resumed.registration_revision, digest)?;
                let replay = apply_resume_directives(state, &inventory, &resumed.directives)?;
                if let Some(replay) = replay {
                    let message = match replay {
                        ResumeReplay::OperationFailure(failure) => {
                            Message::OperationFailed(OperationFailed { failure })
                        }
                        ResumeReplay::WorkspaceReady(ready) => Message::WorkspaceReady(ready),
                    };
                    send_message(&mut io, message).await?;
                }
                (
                    receipt,
                    EnrollmentOutcome::Resumed,
                    resumed.connection_epoch,
                )
            }
        };
        Ok(Self {
            io,
            receipt,
            advertisement: advertisement.clone(),
            outcome,
            connection_epoch,
            heartbeat: None,
            claimed_capability: None,
            pending_offer: None,
            deferred_release: None,
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

    /// Returns the hub-issued epoch of this physical connection.
    pub const fn connection_epoch(&self) -> PositiveU64 {
        self.connection_epoch
    }

    /// Reports the recovery design gap without constructing wire recovery facts.
    pub const fn recovery_unavailable(&self) -> RecoveryUnavailable {
        RecoveryUnavailable {
            gap: RecoveryGap::WorkspaceProvisioningUnavailable,
        }
    }

    /// Sends one shutdown order naming this exact physical connection epoch.
    pub async fn shutdown(&mut self) -> Result<ConnectionEnd, RunnerConnectionError> {
        send_message(
            &mut self.io,
            Message::Shutdown(Shutdown {
                connection_epoch: self.connection_epoch,
                reason: ShutdownReason::RunnerShutdown,
            }),
        )
        .await?;
        Ok(ConnectionEnd::RunnerShutdown {
            connection_epoch: self.connection_epoch,
        })
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
    ) -> Result<ServeOutcome, RunnerConnectionError> {
        loop {
            if let Some(outcome) = self.serve_one(state).await? {
                return Ok(outcome);
            }
        }
    }

    /// Serves until the connection ends or local shutdown reaches a clean frame boundary.
    pub async fn serve_until_shutdown<F>(
        &mut self,
        state: &mut RunnerStateRoot,
        shutdown: F,
    ) -> Result<ServeOutcome, RunnerConnectionError>
    where
        F: Future<Output = ()>,
    {
        tokio::pin!(shutdown);
        loop {
            let message = tokio::select! {
                message = receive_message(&mut self.io) => message?,
                () = &mut shutdown => return Ok(ServeOutcome::ShutdownReady),
            };
            if let Some(outcome) = self.serve_message(state, message).await? {
                return Ok(outcome);
            }
        }
    }

    /// Handles one complete daemon frame; exposed for hermetic protocol harnesses.
    pub async fn serve_one(
        &mut self,
        state: &mut RunnerStateRoot,
    ) -> Result<Option<ServeOutcome>, RunnerConnectionError> {
        if let Some(release) = self.deferred_release.take() {
            return self
                .serve_message(state, Message::WorkspaceRelease(release))
                .await;
        }
        let message = receive_message(&mut self.io).await?;
        self.serve_message(state, message).await
    }

    async fn serve_message(
        &mut self,
        state: &mut RunnerStateRoot,
        message: Message,
    ) -> Result<Option<ServeOutcome>, RunnerConnectionError> {
        match message {
            Message::Heartbeat(challenge) => {
                let acknowledgement = self.heartbeat_acknowledgement(challenge, state)?;
                send_message(&mut self.io, Message::HeartbeatAck(acknowledgement)).await?;
                Ok(None)
            }
            Message::Shutdown(shutdown)
                if shutdown.reason == ShutdownReason::DaemonShutdown
                    && shutdown.connection_epoch == self.connection_epoch =>
            {
                Ok(Some(ServeOutcome::ConnectionEnded(
                    ConnectionEnd::DaemonShutdown {
                        connection_epoch: shutdown.connection_epoch,
                    },
                )))
            }
            Message::Shutdown(shutdown) if shutdown.reason == ShutdownReason::DaemonShutdown => {
                send_message(
                    &mut self.io,
                    Message::Rejected(Rejected {
                        offending_kind: MessageKind::Shutdown.to_string(),
                        available_correlation: AvailableCorrelation::ConnectionEpoch(
                            shutdown.connection_epoch,
                        ),
                        code: RejectionCode::StaleConnection,
                    }),
                )
                .await?;
                Ok(Some(ServeOutcome::ConnectionEnded(
                    ConnectionEnd::StaleConnectionRejected {
                        connection_epoch: shutdown.connection_epoch,
                    },
                )))
            }
            Message::Shutdown(_) => Err(RunnerConnectionError::Violation(
                ProtocolViolation::InvalidShutdownReason,
            )),
            Message::WorkspaceProvision(provision) => {
                self.validate_connection_correlation(
                    provision.correlation.runner_id,
                    provision.correlation.registration_revision,
                )?;
                Err(RunnerConnectionError::RecoveryUnavailable(
                    self.recovery_unavailable(),
                ))
            }
            Message::WorkspaceRelease(release) => {
                if release.correlation.runner_id != self.receipt.runner_id() {
                    return Err(RunnerConnectionError::Violation(
                        ProtocolViolation::ConnectionCorrelationMismatch,
                    ));
                }
                let accepted = state.accept_workspace_release(release.correlation)?;
                Ok(Some(ServeOutcome::WorkspaceReleaseReady(Box::new(
                    RunnerWorkspaceReleaseReady {
                        accepted,
                        connection_epoch: self.connection_epoch,
                    },
                ))))
            }
            Message::LeaseOffer(offer) => {
                self.validate_connection_correlation(
                    offer.correlation.runner_id,
                    offer.correlation.registration_revision,
                )?;
                let advertised = self
                    .advertisement
                    .tools
                    .binary_search(&offer.correlation.tool_name)
                    .is_ok();
                if advertised
                    && self.pending_offer.is_none()
                    && state.reconnect_inventory() == &ReconnectInventory::default()
                    && live_offer_is_admissible(&offer)
                {
                    let correlation = offer.correlation.clone();
                    self.pending_offer = Some(offer);
                    send_message(
                        &mut self.io,
                        Message::LeaseClaim(LeaseClaim { correlation }),
                    )
                    .await?;
                    return Ok(None);
                }
                let failure = if advertised {
                    refused_offer_failure(offer.correlation)?
                } else {
                    empty_catalog_offer_failure(offer.correlation)?
                };
                state.record_lease_offer_failure(failure.clone())?;
                send_message(
                    &mut self.io,
                    Message::OperationFailed(OperationFailed { failure }),
                )
                .await?;
                Ok(None)
            }
            Message::LeaseClaimed(LeaseClaimed { correlation }) => {
                let pending_matches = self
                    .pending_offer
                    .as_ref()
                    .is_some_and(|offer| offer.correlation == correlation);
                let retained_matches =
                    state
                        .reconnect_inventory()
                        .lease
                        .as_ref()
                        .is_some_and(|lease| {
                            lease.correlation == correlation
                                && matches!(
                                    lease.phase,
                                    LeasePhaseKind::WaitingDispatch
                                        | LeasePhaseKind::DispatchReceived
                                )
                        });
                if (!pending_matches && !retained_matches)
                    || state.reconnect_inventory().result.is_some()
                    || self
                        .claimed_capability
                        .as_ref()
                        .is_some_and(|current| current != &correlation)
                {
                    return Err(RunnerConnectionError::Violation(
                        ProtocolViolation::LeaseAcknowledgementMismatch,
                    ));
                }
                if pending_matches {
                    state.record_lease_phase(LeasePhase {
                        correlation: correlation.clone(),
                        phase: LeasePhaseKind::WaitingDispatch,
                    })?;
                }
                self.claimed_capability = Some(correlation);
                Ok(None)
            }
            Message::Dispatch(Dispatch {
                correlation,
                normalized_arguments,
            }) => {
                if self.claimed_capability.as_ref() != Some(&correlation) {
                    return Err(RunnerConnectionError::Violation(
                        ProtocolViolation::DispatchMismatch,
                    ));
                }
                if self.pending_offer.as_ref().is_some_and(|offer| {
                    offer.correlation != correlation
                        || offer.normalized_arguments != normalized_arguments
                }) {
                    return Err(RunnerConnectionError::Violation(
                        ProtocolViolation::DispatchMismatch,
                    ));
                }
                state
                    .record_lease_phase(LeasePhase {
                        correlation: correlation.clone(),
                        phase: LeasePhaseKind::DispatchReceived,
                    })
                    .map_err(|error| match error {
                        RunnerStateError::InvalidTransition
                        | RunnerStateError::OperationCorrelationMismatch => {
                            RunnerConnectionError::Violation(ProtocolViolation::DispatchMismatch)
                        }
                        other => RunnerConnectionError::State(other),
                    })?;
                self.claimed_capability = None;
                self.pending_offer = None;
                Ok(Some(ServeOutcome::DispatchReady(Box::new(
                    RunnerDispatchReady {
                        correlation,
                        normalized_arguments,
                        connection_epoch: self.connection_epoch,
                    },
                ))))
            }
            Message::ResultRecorded(recorded) => {
                self.validate_connection_correlation(
                    recorded.correlation.runner_id,
                    recorded.correlation.registration_revision,
                )?;
                state
                    .acknowledge_terminal_result(&recorded.correlation)
                    .map_err(|error| match error {
                        RunnerStateError::InvalidTransition
                        | RunnerStateError::OperationCorrelationMismatch => {
                            RunnerConnectionError::Violation(
                                ProtocolViolation::ResultAcknowledgementMismatch,
                            )
                        }
                        other => RunnerConnectionError::State(other),
                    })?;
                Ok(None)
            }
            Message::WorkspaceRecorded(recorded) => {
                if recorded.correlation.runner_id != self.receipt.runner_id() {
                    return Err(RunnerConnectionError::Violation(
                        ProtocolViolation::ConnectionCorrelationMismatch,
                    ));
                }
                state
                    .acknowledge_workspace_ready(&recorded)
                    .map_err(|error| match error {
                        RunnerStateError::InvalidTransition
                        | RunnerStateError::OperationCorrelationMismatch => {
                            RunnerConnectionError::Violation(
                                ProtocolViolation::WorkspaceAcknowledgementMismatch,
                            )
                        }
                        other => RunnerConnectionError::State(other),
                    })?;
                Ok(None)
            }
            Message::WorkspaceReleaseRecorded(recorded) => {
                if recorded.correlation.runner_id != self.receipt.runner_id() {
                    return Err(RunnerConnectionError::Violation(
                        ProtocolViolation::ConnectionCorrelationMismatch,
                    ));
                }
                state
                    .acknowledge_workspace_release(&recorded.correlation)
                    .map_err(|error| match error {
                        RunnerStateError::InvalidTransition
                        | RunnerStateError::OperationCorrelationMismatch => {
                            RunnerConnectionError::Violation(
                                ProtocolViolation::WorkspaceAcknowledgementMismatch,
                            )
                        }
                        other => RunnerConnectionError::State(other),
                    })?;
                Ok(None)
            }
            Message::OperationFailureRecorded(recorded) => {
                match &recorded.correlation {
                    OperationCorrelation::LeaseOffer(correlation) => {
                        self.validate_connection_correlation(
                            correlation.runner_id,
                            correlation.registration_revision,
                        )?;
                        state.acknowledge_lease_offer_failure(&recorded.correlation)
                    }
                    OperationCorrelation::Release(correlation)
                        if correlation.runner_id == self.receipt.runner_id() =>
                    {
                        state.acknowledge_workspace_release_failure(correlation)
                    }
                    OperationCorrelation::Release(_) | OperationCorrelation::Provision(_) => {
                        return Err(RunnerConnectionError::Violation(
                            ProtocolViolation::FailureAcknowledgementMismatch,
                        ));
                    }
                }
                .map_err(|error| match error {
                    RunnerStateError::InvalidTransition
                    | RunnerStateError::OperationCorrelationMismatch => {
                        RunnerConnectionError::Violation(
                            ProtocolViolation::FailureAcknowledgementMismatch,
                        )
                    }
                    other => RunnerConnectionError::State(other),
                })?;
                Ok(None)
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
        state: &RunnerStateRoot,
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
        let lease_phase = state.reconnect_inventory().lease.clone();
        if let Some(lease) = &lease_phase {
            self.validate_connection_correlation(
                lease.correlation.runner_id,
                lease.correlation.registration_revision,
            )?;
        }
        let workspace_phase = heartbeat_workspace_phase(state.reconnect_inventory());
        let acknowledgement = HeartbeatAck {
            challenge_sequence: challenge.sequence,
            runner_sequence: PositiveU64::try_new(runner_sequence)
                .map_err(RunnerConnectionError::InvalidLocalFrame)?,
            lease_phase,
            workspace_phase,
        };
        self.heartbeat = Some(HeartbeatExchange {
            challenge,
            acknowledgement: acknowledgement.clone(),
        });
        Ok(acknowledgement)
    }

    fn validate_connection_correlation(
        &self,
        runner: CanonicalUuid,
        revision: PositiveU64,
    ) -> Result<(), RunnerConnectionError> {
        if runner == self.receipt.runner_id() && revision == self.receipt.registration_revision() {
            Ok(())
        } else {
            Err(RunnerConnectionError::Violation(
                ProtocolViolation::ConnectionCorrelationMismatch,
            ))
        }
    }

    /// Runs one sealed dispatch future while continuing to serve heartbeats.
    ///
    /// The durable execution-possible phase commits immediately before the
    /// future can first be polled. Terminal evidence then commits before its
    /// wire projection; a transport failure therefore retains exact replay.
    pub async fn execute_while_serving<F>(
        &mut self,
        state: &mut RunnerStateRoot,
        dispatch: RunnerDispatchReady,
        execution: F,
    ) -> Result<(), RunnerConnectionError>
    where
        F: Future<Output = TerminalResult>,
    {
        if dispatch.connection_epoch != self.connection_epoch {
            return Err(RunnerConnectionError::Violation(
                ProtocolViolation::ExecutionHandoffMismatch,
            ));
        }
        state
            .record_lease_phase(LeasePhase {
                correlation: dispatch.correlation.clone(),
                phase: LeasePhaseKind::ExecutionMayHaveStarted,
            })
            .map_err(RunnerConnectionError::State)?;
        tokio::pin!(execution);
        loop {
            tokio::select! {
                result = &mut execution => {
                    result
                        .validate()
                        .map_err(RunnerConnectionError::InvalidLocalFrame)?;
                    state.record_terminal_result(signalbox_runner_wire::RetainedResult {
                        correlation: dispatch.correlation.clone(),
                        result: result.clone(),
                    })?;
                    send_message(
                        &mut self.io,
                        Message::Result(ResultFrame {
                            correlation: dispatch.correlation,
                            result,
                        }),
                    )
                    .await?;
                    return Ok(());
                }
                message = receive_message(&mut self.io) => {
                    let message = message?;
                    if let Message::WorkspaceRelease(release) = message {
                        if release.correlation.runner_id != self.receipt.runner_id()
                            || self.deferred_release.is_some()
                        {
                            return Err(RunnerConnectionError::Violation(
                                ProtocolViolation::ConnectionCorrelationMismatch,
                            ));
                        }
                        self.deferred_release = Some(release);
                        continue;
                    }
                    if self.serve_message(state, message).await?.is_some() {
                        return Err(RunnerConnectionError::Violation(
                            ProtocolViolation::UnexpectedFrame {
                                expected: MessageKind::Heartbeat,
                                received: MessageKind::Shutdown,
                            },
                        ));
                    }
                }
            }
        }
    }

    /// Runs accepted cleanup while continuing to serve heartbeats.
    ///
    /// Success advances the journal before projecting `workspace_released`.
    /// Failure retains the accepted release beside one bounded failure frame.
    pub async fn release_while_serving<F, E>(
        &mut self,
        state: &mut RunnerStateRoot,
        release: RunnerWorkspaceReleaseReady,
        cleanup: F,
    ) -> Result<(), RunnerConnectionError>
    where
        F: Future<Output = Result<(), E>>,
    {
        if release.connection_epoch != self.connection_epoch {
            return Err(RunnerConnectionError::Violation(
                ProtocolViolation::WorkspaceReleaseHandoffMismatch,
            ));
        }
        let correlation = release.correlation().clone();
        tokio::pin!(cleanup);
        loop {
            tokio::select! {
                result = &mut cleanup => {
                    match result {
                        Ok(()) => {
                            state.record_workspace_release_phase(
                                correlation.clone(),
                                signalbox_runner_wire::ReleasePhase::ReleaseCompleted,
                            )?;
                            send_message(
                                &mut self.io,
                                Message::WorkspaceReleased(WorkspaceReleased {
                                    correlation,
                                }),
                            )
                            .await?;
                        }
                        Err(_) => {
                            let failure = workspace_cleanup_failure(correlation)?;
                            state.record_workspace_release_failure(failure.clone())?;
                            send_message(
                                &mut self.io,
                                Message::OperationFailed(OperationFailed { failure }),
                            )
                            .await?;
                        }
                    }
                    return Ok(());
                }
                message = receive_message(&mut self.io) => {
                    let message = message?;
                    let received = MessageKind::of(&message);
                    if !matches!(&message, Message::Heartbeat(_)) {
                        return Err(RunnerConnectionError::Violation(
                            ProtocolViolation::UnexpectedFrame {
                                expected: MessageKind::Heartbeat,
                                received,
                            },
                        ));
                    }
                    if self.serve_message(state, message).await?.is_some() {
                        return Err(RunnerConnectionError::Violation(
                            ProtocolViolation::UnexpectedFrame {
                                expected: MessageKind::Heartbeat,
                                received,
                            },
                        ));
                    }
                }
            }
        }
    }
}

fn workspace_cleanup_failure(
    correlation: signalbox_runner_wire::ReleaseCorrelation,
) -> Result<OperationFailure, RunnerConnectionError> {
    Ok(OperationFailure {
        correlation: OperationCorrelation::Release(correlation),
        category: FailureCategory::WorkspaceCleanupFailed,
        detail: FailureDetail::try_new(
            DetailName::try_new(WORKSPACE_CLEANUP_DETAIL_CODE.to_owned())
                .map_err(RunnerConnectionError::InvalidLocalFrame)?,
            WORKSPACE_CLEANUP_DETAIL_MESSAGE.to_owned(),
            serde_json::json!({}),
        )
        .map_err(RunnerConnectionError::InvalidLocalFrame)?,
    })
}

fn heartbeat_workspace_phase(inventory: &ReconnectInventory) -> Option<HeartbeatWorkspacePhase> {
    if let Some(OperationFailure {
        correlation: OperationCorrelation::Release(correlation),
        ..
    }) = inventory.operation_failure.as_ref()
    {
        return Some(HeartbeatWorkspacePhase::FailureUnrecorded {
            correlation: WorkspaceFailureCorrelation::Release(correlation.clone()),
        });
    }
    match inventory.workspace_operation.as_ref() {
        Some(WorkspaceOperation::Release { correlation, phase }) => Some(match phase {
            signalbox_runner_wire::ReleasePhase::ReleaseAccepted => {
                HeartbeatWorkspacePhase::ReleaseAccepted {
                    correlation: correlation.clone(),
                }
            }
            signalbox_runner_wire::ReleasePhase::ReleaseCompleted => {
                HeartbeatWorkspacePhase::ReleaseCompleted {
                    correlation: correlation.clone(),
                }
            }
        }),
        Some(WorkspaceOperation::Provision { .. }) | None => None,
    }
}

fn accept_enrollment(
    message: Message,
    request_id: CanonicalUuid,
    expected_digest: Digest,
) -> Result<(EnrollmentReceipt, EnrollmentOutcome, PositiveU64), RunnerConnectionError> {
    match message {
        Message::Enrolled(enrolled) => {
            if enrolled.request_id != request_id {
                return Err(request_mismatch(request_id, enrolled.request_id));
            }
            if enrolled.advertisement_digest != expected_digest {
                return Err(digest_mismatch());
            }
            validate_initial_revision(enrolled.registration_revision)?;
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
                enrolled.connection_epoch,
            ))
        }
        Message::ReplacementPending(pending) => {
            if pending.request_id != request_id {
                return Err(request_mismatch(request_id, pending.request_id));
            }
            if pending.advertisement_digest != expected_digest {
                return Err(digest_mismatch());
            }
            validate_initial_revision(pending.registration_revision)?;
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
                pending.connection_epoch,
            ))
        }
        Message::Rejected(rejected) => Err(rejected_error(rejected)),
        other => Err(unexpected(MessageKind::Enrolled, &other)),
    }
}

fn validate_initial_revision(observed: PositiveU64) -> Result<(), RunnerConnectionError> {
    if observed.get() == 1 {
        Ok(())
    } else {
        Err(RunnerConnectionError::Violation(
            ProtocolViolation::InitialRegistrationRevision { observed },
        ))
    }
}

fn validate_registered(
    registered: &Registered,
    prior: PositiveU64,
    expected_digest: &Digest,
) -> Result<(), RunnerConnectionError> {
    let expected = prior.get().checked_add(1).ok_or({
        RunnerConnectionError::Violation(ProtocolViolation::RegistrationRevisionExhausted { prior })
    })?;
    if registered.registration_revision.get() != expected {
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

fn rejected_error(rejected: Rejected) -> RunnerConnectionError {
    RunnerConnectionError::PeerRejected {
        code: rejected.code,
        offending_kind: rejected.offending_kind,
        available_correlation: Box::new(rejected.available_correlation),
    }
}

fn empty_catalog_offer_failure(
    correlation: LeaseCorrelation,
) -> Result<OperationFailure, RunnerConnectionError> {
    let code = DetailName::try_new(String::from(TOOL_UNAVAILABLE_DETAIL_CODE))
        .map_err(RunnerConnectionError::InvalidLocalFrame)?;
    let detail = FailureDetail::try_new(
        code,
        String::from(TOOL_UNAVAILABLE_DETAIL_MESSAGE),
        serde_json::Value::Object(serde_json::Map::new()),
    )
    .map_err(RunnerConnectionError::InvalidLocalFrame)?;
    Ok(OperationFailure {
        correlation: OperationCorrelation::LeaseOffer(correlation),
        category: FailureCategory::LeaseAdmissionRefused,
        detail,
    })
}

fn refused_offer_failure(
    correlation: LeaseCorrelation,
) -> Result<OperationFailure, RunnerConnectionError> {
    let code = DetailName::try_new(String::from(LEASE_REFUSED_DETAIL_CODE))
        .map_err(RunnerConnectionError::InvalidLocalFrame)?;
    let detail = FailureDetail::try_new(
        code,
        String::from(LEASE_REFUSED_DETAIL_MESSAGE),
        serde_json::Value::Object(serde_json::Map::new()),
    )
    .map_err(RunnerConnectionError::InvalidLocalFrame)?;
    Ok(OperationFailure {
        correlation: OperationCorrelation::LeaseOffer(correlation),
        category: FailureCategory::LeaseAdmissionRefused,
        detail,
    })
}

fn live_offer_is_admissible(offer: &LeaseOffer) -> bool {
    let working_directory = Path::new(offer.correlation.working_directory.as_str());
    offer.correlation.tool_name.as_str() == SANDBOXED_EXEC_NAME
        && offer.correlation.sandbox_profile == SandboxProfile::WorkspaceRestricted
        && offer.effect_class == EffectClass::SideEffecting
        && offer.credential_profile.is_none()
        && offer.grant_revision.is_none()
        && serde_json::from_value::<ExecArguments>(offer.normalized_arguments.clone()).is_ok()
        && working_directory
            .canonicalize()
            .is_ok_and(|canonical| canonical == working_directory && canonical.is_dir())
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

enum ResumeReplay {
    OperationFailure(OperationFailure),
    WorkspaceReady(signalbox_runner_wire::WorkspaceReady),
}

fn apply_resume_directives(
    state: &mut RunnerStateRoot,
    inventory: &ReconnectInventory,
    directives: &ReconnectDirectives,
) -> Result<Option<ResumeReplay>, RunnerConnectionError> {
    if inventory == &ReconnectInventory::default() {
        return Ok(None);
    }
    if let (Some(lease), Some(directive)) = (inventory.lease.as_ref(), directives.lease.as_ref())
        && inventory.result.is_none()
        && inventory.workspace_operation.is_none()
        && inventory.operation_failure.is_none()
        && inventory.leak_page.is_none()
        && directives.result.is_none()
        && directives.workspace_operation.is_none()
        && directives.operation_failure.is_none()
        && directives.leak_page.is_none()
    {
        return match (lease.phase, directive.action) {
            (
                LeasePhaseKind::WaitingDispatch | LeasePhaseKind::DispatchReceived,
                DirectiveAction::Await,
            ) => Ok(None),
            (
                LeasePhaseKind::WaitingDispatch | LeasePhaseKind::DispatchReceived,
                DirectiveAction::FailStale,
            ) => {
                state
                    .fail_stale_unstarted_lease(&lease.correlation)
                    .map_err(|error| match error {
                        RunnerStateError::InvalidTransition
                        | RunnerStateError::OperationCorrelationMismatch => {
                            RunnerConnectionError::Violation(ProtocolViolation::ResumeDirectives)
                        }
                        other => RunnerConnectionError::State(other),
                    })?;
                Ok(None)
            }
            (
                LeasePhaseKind::WaitingDispatch
                | LeasePhaseKind::DispatchReceived
                | LeasePhaseKind::ExecutionMayHaveStarted,
                DirectiveAction::Resend
                | DirectiveAction::DiscardAsRecorded
                | DirectiveAction::Await
                | DirectiveAction::FailStale,
            ) => Err(RunnerConnectionError::Violation(
                ProtocolViolation::ResumeDirectives,
            )),
        };
    }
    if let (Some(failure), Some(directive)) = (
        inventory.operation_failure.as_ref(),
        directives.operation_failure.as_ref(),
    ) && inventory.lease.is_none()
        && inventory.result.is_none()
        && inventory.workspace_operation.is_none()
        && inventory.leak_page.is_none()
        && directives.lease.is_none()
        && directives.result.is_none()
        && directives.workspace_operation.is_none()
        && directives.leak_page.is_none()
    {
        return match directive.action {
            DirectiveAction::Resend => Ok(Some(ResumeReplay::OperationFailure(failure.clone()))),
            DirectiveAction::DiscardAsRecorded | DirectiveAction::FailStale => {
                state
                    .acknowledge_lease_offer_failure(&failure.correlation)
                    .map_err(|error| match error {
                        RunnerStateError::InvalidTransition
                        | RunnerStateError::OperationCorrelationMismatch => {
                            RunnerConnectionError::Violation(ProtocolViolation::ResumeDirectives)
                        }
                        other => RunnerConnectionError::State(other),
                    })?;
                Ok(None)
            }
            DirectiveAction::Await => Err(RunnerConnectionError::Violation(
                ProtocolViolation::ResumeDirectives,
            )),
        };
    }
    if let (
        Some(WorkspaceOperation::Provision {
            correlation,
            phase: signalbox_runner_wire::ProvisionPhase::ReadyUnrecorded,
        }),
        Some(directive),
    ) = (
        inventory.workspace_operation.as_ref(),
        directives.workspace_operation.as_ref(),
    ) && inventory.lease.is_none()
        && inventory.result.is_none()
        && inventory.operation_failure.is_none()
        && inventory.leak_page.is_none()
        && directives.lease.is_none()
        && directives.result.is_none()
        && directives.operation_failure.is_none()
        && directives.leak_page.is_none()
    {
        return match directive.action {
            DirectiveAction::Resend => {
                let ready =
                    state
                        .retained_workspace_ready()
                        .ok_or(RunnerConnectionError::Violation(
                            ProtocolViolation::ResumeDirectives,
                        ))?;
                if ready.correlation != *correlation {
                    return Err(RunnerConnectionError::Violation(
                        ProtocolViolation::ResumeDirectives,
                    ));
                }
                Ok(Some(ResumeReplay::WorkspaceReady(ready.clone())))
            }
            DirectiveAction::FailStale => {
                state
                    .fail_stale_workspace_ready(correlation)
                    .map_err(|error| match error {
                        RunnerStateError::InvalidTransition
                        | RunnerStateError::OperationCorrelationMismatch => {
                            RunnerConnectionError::Violation(ProtocolViolation::ResumeDirectives)
                        }
                        other => RunnerConnectionError::State(other),
                    })?;
                Ok(None)
            }
            DirectiveAction::Await | DirectiveAction::DiscardAsRecorded => Err(
                RunnerConnectionError::Violation(ProtocolViolation::ResumeDirectives),
            ),
        };
    }
    let (Some(lease), Some(result), Some(lease_directive), Some(result_directive)) = (
        inventory.lease.as_ref(),
        inventory.result.as_ref(),
        directives.lease.as_ref(),
        directives.result.as_ref(),
    ) else {
        return Err(RunnerConnectionError::Violation(
            ProtocolViolation::ResumeDirectives,
        ));
    };
    let action_supported = matches!(
        (lease_directive.action, result_directive.action),
        (
            DirectiveAction::DiscardAsRecorded,
            DirectiveAction::DiscardAsRecorded
        ) | (DirectiveAction::FailStale, DirectiveAction::FailStale)
    );
    if !action_supported || lease.correlation != result.correlation {
        return Err(RunnerConnectionError::Violation(
            ProtocolViolation::ResumeDirectives,
        ));
    }
    state
        .acknowledge_terminal_result(&result.correlation)
        .map_err(|error| match error {
            RunnerStateError::InvalidTransition
            | RunnerStateError::OperationCorrelationMismatch => {
                RunnerConnectionError::Violation(ProtocolViolation::ResumeDirectives)
            }
            other => RunnerConnectionError::State(other),
        })?;
    Ok(None)
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
        DetailName, EffectClass, Enrolled, FailureCategory, FailureDetail, LeaseCorrelation,
        LeaseOffer, LeasePhase, LeasePhaseKind, ManifestLifecycle, OperationFailureRecorded,
        ProvisionCorrelation, ReadyManifest, ReconnectDirectives, Recovery, ReleaseCorrelation,
        ReleasePhase, RepositoryKey, ResultBounds, ResultRecorded, Resumed, RetainedResult,
        SandboxProfile, Shutdown, TerminalResult, WireToolName, WorkingDirectory,
        WorkspaceManifest, WorkspaceOperation, WorkspaceProvision, WorkspaceReady,
        WorkspaceRecorded, WorkspaceRelease, WorkspaceReleaseRecorded, workspace_manifest_digest,
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
    /// Arbitrary lease identity used by retained-result fixtures.
    const ARBITRARY_LEASE_UUID: u128 = 0x600;
    /// Another lease identity used to prove acknowledgement fencing.
    const ARBITRARY_OTHER_LEASE_UUID: u128 = 0x601;
    const ARBITRARY_TURN_UUID: u128 = 0x700;
    const ARBITRARY_TOOL_REQUEST_UUID: u128 = 0x800;
    const ARBITRARY_TOOL_ATTEMPT_UUID: u128 = 0x900;
    const ARBITRARY_ISSUING_TURN_ATTEMPT_UUID: u128 = 0xa00;
    const ARBITRARY_MANIFEST_UUID: u128 = 0xb00;
    const ARBITRARY_OTHER_MANIFEST_UUID: u128 = 0xb01;
    const INITIAL_REGISTRATION_REVISION: u64 = 1;
    const NEXT_REGISTRATION_REVISION: u64 = 2;
    const FIRST_CHALLENGE_SEQUENCE: u64 = 7;
    const CONNECTION_EPOCH: u64 = 11;
    const EXPECTED_WORKSPACE_CLEANUP_DETAIL_CODE: &str = "workspace-cleanup-failed";
    const EXPECTED_WORKSPACE_CLEANUP_DETAIL_MESSAGE: &str = "the accepted workspace cleanup failed";

    #[test]
    fn non_dispatch_outcome_cannot_form_an_executor_handoff() {
        assert_eq!(
            ServeOutcome::ShutdownReady.into_dispatch_ready(),
            Err(ServeOutcome::ShutdownReady)
        );
    }

    #[test]
    fn non_release_outcome_cannot_form_a_cleanup_handoff() {
        assert_eq!(
            ServeOutcome::ShutdownReady.into_workspace_release_ready(),
            Err(ServeOutcome::ShutdownReady)
        );
    }

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
        issued_receipt_for(request_id, &empty_advertisement())
    }

    fn issued_receipt_for(
        request_id: CanonicalUuid,
        advertisement: &Advertisement,
    ) -> EnrollmentReceipt {
        EnrollmentReceipt::new(
            request_id,
            identity(ARBITRARY_ENROLLMENT_UUID),
            identity(ARBITRARY_RUNNER_UUID),
            identity(ARBITRARY_AUTHENTICATION_UUID),
            positive(INITIAL_REGISTRATION_REVISION),
            advertisement_digest(advertisement)
                .expect("the explicit fixture advertisement has a digest"),
            EnrollmentAuthority::Active,
        )
    }

    fn retained_lease_correlation() -> LeaseCorrelation {
        LeaseCorrelation {
            registration_revision: positive(INITIAL_REGISTRATION_REVISION),
            lease_id: identity(ARBITRARY_LEASE_UUID),
            lease_generation: positive(1),
            runner_id: identity(ARBITRARY_RUNNER_UUID),
            placement_revision: positive(1),
            working_directory: WorkingDirectory::try_new("sessions/example".to_owned())
                .expect("the fixture working directory is valid"),
            sandbox_profile: SandboxProfile::WorkspaceRestricted,
            tool_name: WireToolName::try_new("sandboxed_exec".to_owned())
                .expect("the generic exec-family fixture name is valid"),
            session_id: identity(ARBITRARY_SESSION_UUID),
            turn_id: identity(ARBITRARY_TURN_UUID),
            tool_request_id: identity(ARBITRARY_TOOL_REQUEST_UUID),
            tool_attempt_id: identity(ARBITRARY_TOOL_ATTEMPT_UUID),
            issuing_turn_attempt_id: identity(ARBITRARY_ISSUING_TURN_ATTEMPT_UUID),
            tool_dispatch_generation: positive(1),
        }
    }

    fn release_correlation() -> ReleaseCorrelation {
        ReleaseCorrelation {
            session_id: identity(ARBITRARY_SESSION_UUID),
            placement_revision: positive(1),
            runner_id: identity(ARBITRARY_RUNNER_UUID),
            manifest_id: identity(ARBITRARY_MANIFEST_UUID),
        }
    }

    fn provision_correlation() -> ProvisionCorrelation {
        ProvisionCorrelation {
            authorization_id: identity(ARBITRARY_AUTHORIZATION_UUID),
            session_id: identity(ARBITRARY_SESSION_UUID),
            placement_revision: positive(1),
            runner_id: identity(ARBITRARY_RUNNER_UUID),
            registration_revision: positive(INITIAL_REGISTRATION_REVISION),
            repository: Some(
                RepositoryKey::try_new("fixture-repository".to_owned())
                    .expect("the fixture repository key is valid"),
            ),
            sandbox_profile: SandboxProfile::WorkspaceRestricted,
            credential_profile: None,
        }
    }

    fn workspace_ready() -> WorkspaceReady {
        let correlation = provision_correlation();
        let manifest = WorkspaceManifest {
            lifecycle: ManifestLifecycle::Ready,
            manifest_id: identity(ARBITRARY_MANIFEST_UUID),
            session: correlation.session_id,
            placement_revision: correlation.placement_revision,
            runner: correlation.runner_id,
            repository: correlation.repository.clone(),
            canonical_clone_url_digest: Some(
                Digest::try_new("b".repeat(64)).expect("the fixture clone digest is canonical"),
            ),
            credential_profile: None,
            sandbox_profile: correlation.sandbox_profile,
            relative_path: format!(
                "sessions/{}/{}/repo",
                correlation.session_id,
                correlation.placement_revision.get()
            ),
            recovery: Some(Recovery::Commit {
                revision: "a".repeat(40),
            }),
        };
        let manifest_digest = workspace_manifest_digest(&manifest)
            .expect("the fixture manifest has a canonical digest");
        WorkspaceReady {
            correlation,
            ready: ReadyManifest {
                manifest,
                manifest_digest,
            },
        }
    }

    fn workspace_recorded() -> WorkspaceRecorded {
        let ready = workspace_ready();
        WorkspaceRecorded {
            correlation: ready.correlation,
            manifest_id: ready.ready.manifest.manifest_id,
            manifest_digest: ready.ready.manifest_digest,
        }
    }

    fn state_with_workspace_ready(parent: &TempDir) -> RunnerStateRoot {
        let mut state = enrolled_state(parent);
        state
            .record_workspace_ready(workspace_ready())
            .expect("the complete ready payload is durable");
        state
    }

    fn release_failure() -> OperationFailure {
        OperationFailure {
            correlation: OperationCorrelation::Release(release_correlation()),
            category: FailureCategory::WorkspaceCleanupFailed,
            detail: FailureDetail::try_new(
                DetailName::try_new("fixture-cleanup".to_owned())
                    .expect("the fixture detail code is valid"),
                String::from("the synthetic cleanup failed"),
                serde_json::json!({}),
            )
            .expect("the fixture failure detail is bounded"),
        }
    }

    fn expected_workspace_cleanup_failure(correlation: ReleaseCorrelation) -> OperationFailure {
        OperationFailure {
            correlation: OperationCorrelation::Release(correlation),
            category: FailureCategory::WorkspaceCleanupFailed,
            detail: FailureDetail::try_new(
                DetailName::try_new(EXPECTED_WORKSPACE_CLEANUP_DETAIL_CODE.to_owned())
                    .expect("the expected cleanup detail code is valid"),
                EXPECTED_WORKSPACE_CLEANUP_DETAIL_MESSAGE.to_owned(),
                serde_json::json!({}),
            )
            .expect("the expected cleanup failure detail is bounded"),
        }
    }

    fn state_with_terminal_result(parent: &TempDir) -> RunnerStateRoot {
        let mut state = enrolled_state(parent);
        record_terminal_result_fixture(&mut state);
        state
    }

    fn enrolled_state(parent: &TempDir) -> RunnerStateRoot {
        enrolled_state_for(parent, &empty_advertisement())
    }

    fn enrolled_state_for(parent: &TempDir, advertisement: &Advertisement) -> RunnerStateRoot {
        let mut state = state_root(parent);
        let receipt = issued_receipt_for(state.state().request_id(), advertisement);
        state
            .record_receipt(receipt)
            .expect("the issued receipt is journaled");
        state
    }

    fn advertisement_with_exec_tool() -> Advertisement {
        Advertisement {
            tools: vec![
                WireToolName::try_new("sandboxed_exec".to_owned())
                    .expect("the generic exec-family fixture name is valid"),
            ],
            ..empty_advertisement()
        }
    }

    fn record_terminal_result_fixture(state: &mut RunnerStateRoot) {
        let correlation = retained_lease_correlation();
        state
            .record_lease_phase(LeasePhase {
                correlation: correlation.clone(),
                phase: LeasePhaseKind::WaitingDispatch,
            })
            .expect("the waiting-dispatch phase is durable");
        state
            .record_lease_phase(LeasePhase {
                correlation: correlation.clone(),
                phase: LeasePhaseKind::DispatchReceived,
            })
            .expect("the dispatch-received phase is durable");
        state
            .record_lease_phase(LeasePhase {
                correlation: correlation.clone(),
                phase: LeasePhaseKind::ExecutionMayHaveStarted,
            })
            .expect("the execution-possible phase is durable");
        state
            .record_terminal_result(RetainedResult {
                correlation,
                result: TerminalResult::Ambiguous,
            })
            .expect("the terminal result is durable");
    }

    fn retained_result_directives(action: DirectiveAction) -> ReconnectDirectives {
        let correlation = retained_lease_correlation();
        ReconnectDirectives {
            lease: Some(signalbox_runner_wire::Directive {
                correlation: correlation.clone(),
                action,
            }),
            result: Some(signalbox_runner_wire::Directive {
                correlation,
                action,
            }),
            ..ReconnectDirectives::default()
        }
    }

    fn retained_lease_directives(action: DirectiveAction) -> ReconnectDirectives {
        ReconnectDirectives {
            lease: Some(signalbox_runner_wire::Directive {
                correlation: retained_lease_correlation(),
                action,
            }),
            ..ReconnectDirectives::default()
        }
    }

    fn retained_workspace_directives(action: DirectiveAction) -> ReconnectDirectives {
        ReconnectDirectives {
            workspace_operation: Some(signalbox_runner_wire::Directive {
                correlation: OperationCorrelation::Provision(provision_correlation()),
                action,
            }),
            ..ReconnectDirectives::default()
        }
    }

    fn expected_resume_message(
        receipt: &EnrollmentReceipt,
        advertisement: &Advertisement,
        inventory: ReconnectInventory,
    ) -> Message {
        Message::Resume(Box::new(Resume {
            request_id: receipt.request_id(),
            digest_version: DIGEST_VERSION,
            enrollment_id: receipt.enrollment_id(),
            runner_id: receipt.runner_id(),
            authentication_id: receipt.authentication_id(),
            advertisement: advertisement.clone(),
            prior_registration_revision: receipt.registration_revision(),
            inventory,
        }))
    }

    fn retained_lease_offer_failure() -> OperationFailure {
        OperationFailure {
            correlation: OperationCorrelation::LeaseOffer(retained_lease_correlation()),
            category: FailureCategory::LeaseAdmissionRefused,
            detail: FailureDetail::try_new(
                DetailName::try_new(TOOL_UNAVAILABLE_DETAIL_CODE.to_owned())
                    .expect("the fixture detail code is valid"),
                String::from(TOOL_UNAVAILABLE_DETAIL_MESSAGE),
                serde_json::json!({}),
            )
            .expect("the fixture failure detail is bounded"),
        }
    }

    fn unavailable_lease_offer() -> LeaseOffer {
        LeaseOffer {
            correlation: retained_lease_correlation(),
            effect_class: EffectClass::SideEffecting,
            credential_profile: None,
            grant_revision: None,
            normalized_arguments: serde_json::json!({ "program": "true" }),
            result_bounds: ResultBounds::version_one(),
        }
    }

    fn retained_dispatch() -> Dispatch {
        Dispatch {
            correlation: retained_lease_correlation(),
            normalized_arguments: serde_json::json!({ "program": "true" }),
        }
    }

    fn retained_failure_directives(
        failure: &OperationFailure,
        action: DirectiveAction,
    ) -> ReconnectDirectives {
        ReconnectDirectives {
            operation_failure: Some(signalbox_runner_wire::Directive {
                correlation: failure.correlation.clone(),
                action,
            }),
            ..ReconnectDirectives::default()
        }
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
    async fn enrollment_publishes_the_exact_durable_receipt() {
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
                    connection_epoch: positive(CONNECTION_EPOCH),
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
    async fn restart_resume_commits_the_canonical_registration_head() {
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
                    connection_epoch: positive(CONNECTION_EPOCH),
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
    async fn heartbeat_challenge_receives_the_exact_acknowledgement() {
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
                    connection_epoch: positive(CONNECTION_EPOCH),
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
        let ((_connection, outcome), acknowledgement) = tokio::join!(runner, hub);

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
    }

    /// INV-011 / INV-024 / INV-042: the registration-only empty catalog
    /// refuses an unknown offer only after its exact failure is durable.
    #[tokio::test]
    async fn s31_inv011_inv024_inv042_empty_catalog_offer_is_durably_refused() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        let advertisement = empty_advertisement();
        let offer = unavailable_lease_offer();
        let expected_failure = retained_lease_offer_failure();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before the lease offer");
            connection
                .serve_one(&mut state)
                .await
                .expect("the unavailable offer is refused without closing the connection")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: ReconnectDirectives::default(),
                })),
            )
            .await;
            send_hub_message(&mut hub_io, Message::LeaseOffer(offer)).await;
            receive_hub_message(&mut hub_io).await
        };
        let (outcome, observed) = tokio::join!(runner, hub);

        assert_eq!(outcome, None);
        assert_eq!(
            observed,
            Message::OperationFailed(OperationFailed {
                failure: expected_failure.clone(),
            })
        );
        assert_eq!(
            state.reconnect_inventory(),
            &ReconnectInventory {
                operation_failure: Some(expected_failure),
                ..ReconnectInventory::default()
            }
        );
    }

    #[tokio::test]
    async fn advertised_offer_emits_claim_without_premature_journaling() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let advertisement = advertisement_with_exec_tool();
        let mut state = enrolled_state_for(&parent, &advertisement);
        let mut offer = unavailable_lease_offer();
        offer.correlation.working_directory = WorkingDirectory::try_new(
            parent
                .path()
                .canonicalize()
                .expect("the fixture directory canonicalizes")
                .display()
                .to_string(),
        )
        .expect("the canonical fixture directory is bounded");
        let expected = Message::LeaseClaim(LeaseClaim {
            correlation: offer.correlation.clone(),
        });
        let correlation = offer.correlation.clone();
        let dispatch = Dispatch {
            correlation: correlation.clone(),
            normalized_arguments: offer.normalized_arguments.clone(),
        };
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before the advertised lease offer");
            let offered = connection
                .serve_one(&mut state)
                .await
                .expect("the admitted offer emits its exact claim");
            let inventory_before_claim = state.reconnect_inventory().clone();
            let claimed = connection
                .serve_one(&mut state)
                .await
                .expect("the canonical acknowledgement is accepted");
            let inventory_after_claim = state.reconnect_inventory().clone();
            let ready = connection
                .serve_one(&mut state)
                .await
                .expect("the admitted dispatch reaches its executor handoff");
            (
                offered,
                inventory_before_claim,
                claimed,
                inventory_after_claim,
                ready,
            )
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: ReconnectDirectives::default(),
                })),
            )
            .await;
            send_hub_message(&mut hub_io, Message::LeaseOffer(offer)).await;
            let claim = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::LeaseClaimed(LeaseClaimed {
                    correlation: correlation.clone(),
                }),
            )
            .await;
            send_hub_message(&mut hub_io, Message::Dispatch(dispatch)).await;
            claim
        };
        let ((offered, before_claim, claimed, after_claim, ready), observed) =
            tokio::join!(runner, hub);

        assert_eq!(offered, None);
        assert_eq!(before_claim, ReconnectInventory::default());
        assert_eq!(claimed, None);
        assert_eq!(
            after_claim.lease,
            Some(LeasePhase {
                correlation: correlation.clone(),
                phase: LeasePhaseKind::WaitingDispatch,
            })
        );
        assert!(matches!(ready, Some(ServeOutcome::DispatchReady(_))));
        assert_eq!(observed, expected);
        assert_eq!(
            state.reconnect_inventory().lease,
            Some(LeasePhase {
                correlation,
                phase: LeasePhaseKind::DispatchReceived,
            })
        );
    }

    /// INV-011 / INV-024: an exact await directive preserves the durable
    /// waiting-dispatch phase while resume establishes the successor epoch.
    #[tokio::test]
    async fn s31_inv011_inv024_resume_awaits_waiting_dispatch_lease() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        state
            .record_lease_phase(LeasePhase {
                correlation: retained_lease_correlation(),
                phase: LeasePhaseKind::WaitingDispatch,
            })
            .expect("the waiting-dispatch phase is durable");
        let retained = state.reconnect_inventory().clone();
        let receipt = state
            .state()
            .receipt()
            .expect("the retained fixture is enrolled")
            .clone();
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = RunnerConnection::establish(runner_io, &mut state, &advertisement);
        let hub = async {
            let observed = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_lease_directives(DirectiveAction::Await),
                })),
            )
            .await;
            observed
        };
        let (connection, observed) = tokio::join!(runner, hub);
        let expected = expected_resume_message(&receipt, &advertisement, retained.clone());

        assert_eq!(
            connection
                .expect("the exact await directive establishes the connection")
                .outcome(),
            EnrollmentOutcome::Resumed
        );
        assert_eq!(observed, expected);
        assert_eq!(state.reconnect_inventory(), &retained);
    }

    /// INV-011 / INV-024: dispatch-received remains execution-impossible and
    /// is retained exactly while the daemon prepares canonical replay.
    #[tokio::test]
    async fn s31_inv011_inv024_resume_awaits_dispatch_received_lease() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        state
            .record_lease_phase(LeasePhase {
                correlation: retained_lease_correlation(),
                phase: LeasePhaseKind::WaitingDispatch,
            })
            .expect("the waiting-dispatch phase is durable");
        state
            .record_lease_phase(LeasePhase {
                correlation: retained_lease_correlation(),
                phase: LeasePhaseKind::DispatchReceived,
            })
            .expect("the dispatch-received phase is durable");
        let retained = state.reconnect_inventory().clone();
        let receipt = state
            .state()
            .receipt()
            .expect("the retained fixture is enrolled")
            .clone();
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = RunnerConnection::establish(runner_io, &mut state, &advertisement);
        let hub = async {
            let observed = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_lease_directives(DirectiveAction::Await),
                })),
            )
            .await;
            observed
        };
        let (connection, observed) = tokio::join!(runner, hub);
        let expected = expected_resume_message(&receipt, &advertisement, retained.clone());

        assert_eq!(
            connection
                .expect("the exact await directive establishes the connection")
                .outcome(),
            EnrollmentOutcome::Resumed
        );
        assert_eq!(observed, expected);
        assert_eq!(state.reconnect_inventory(), &retained);
    }

    /// INV-011 / INV-024: fail-stale consumes only the exact retained lease
    /// whose journal proves execution never became possible.
    #[tokio::test]
    async fn s31_inv011_inv024_resume_clears_stale_unstarted_lease() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        state
            .record_lease_phase(LeasePhase {
                correlation: retained_lease_correlation(),
                phase: LeasePhaseKind::WaitingDispatch,
            })
            .expect("the waiting-dispatch phase is durable");
        let retained = state.reconnect_inventory().clone();
        let receipt = state
            .state()
            .receipt()
            .expect("the retained fixture is enrolled")
            .clone();
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = RunnerConnection::establish(runner_io, &mut state, &advertisement);
        let hub = async {
            let observed = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_lease_directives(DirectiveAction::FailStale),
                })),
            )
            .await;
            observed
        };
        let (connection, observed) = tokio::join!(runner, hub);
        let expected = expected_resume_message(&receipt, &advertisement, retained);

        assert_eq!(
            connection
                .expect("the exact stale directive establishes the connection")
                .outcome(),
            EnrollmentOutcome::Resumed
        );
        assert_eq!(observed, expected);
        assert_eq!(state.reconnect_inventory(), &ReconnectInventory::default());
    }

    /// INV-011 / INV-024: no recorded-result directive can be repurposed to
    /// discard a lease-only journal slot.
    #[tokio::test]
    async fn s31_inv011_inv024_resume_rejects_recorded_action_for_unstarted_lease() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        state
            .record_lease_phase(LeasePhase {
                correlation: retained_lease_correlation(),
                phase: LeasePhaseKind::WaitingDispatch,
            })
            .expect("the waiting-dispatch phase is durable");
        let retained = state.reconnect_inventory().clone();
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = RunnerConnection::establish(runner_io, &mut state, &advertisement);
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_lease_directives(DirectiveAction::DiscardAsRecorded),
                })),
            )
            .await;
        };
        let (rejected, ()) = tokio::join!(runner, hub);
        let rejected = rejected
            .err()
            .expect("the result-only directive meaning fails closed");

        assert!(matches!(
            rejected,
            RunnerConnectionError::Violation(ProtocolViolation::ResumeDirectives)
        ));
        assert_eq!(state.reconnect_inventory(), &retained);
    }

    /// INV-011 / INV-024 / INV-043: a lease that may have executed cannot be
    /// cleared through the execution-impossible resume path.
    #[tokio::test]
    async fn s31_inv011_inv024_inv043_resume_rejects_unpaired_execution_possible_lease() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        state
            .record_lease_phase(LeasePhase {
                correlation: retained_lease_correlation(),
                phase: LeasePhaseKind::WaitingDispatch,
            })
            .expect("the waiting-dispatch phase is durable");
        state
            .record_lease_phase(LeasePhase {
                correlation: retained_lease_correlation(),
                phase: LeasePhaseKind::DispatchReceived,
            })
            .expect("the dispatch-received phase is durable");
        state
            .record_lease_phase(LeasePhase {
                correlation: retained_lease_correlation(),
                phase: LeasePhaseKind::ExecutionMayHaveStarted,
            })
            .expect("the execution-possible phase is durable");
        let retained = state.reconnect_inventory().clone();
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = RunnerConnection::establish(runner_io, &mut state, &advertisement);
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_lease_directives(DirectiveAction::FailStale),
                })),
            )
            .await;
        };
        let (rejected, ()) = tokio::join!(runner, hub);
        let rejected = rejected
            .err()
            .expect("an unpaired execution-possible lease fails closed");

        assert!(matches!(
            rejected,
            RunnerConnectionError::Violation(ProtocolViolation::ResumeDirectives)
        ));
        assert_eq!(state.reconnect_inventory(), &retained);
    }

    /// INV-011 / INV-024 / INV-043: canonical claim acknowledgement and
    /// dispatch replay advance waiting-dispatch to one sealed executor handoff.
    #[tokio::test]
    async fn s31_inv011_inv024_inv043_waiting_dispatch_replay_reaches_executor_boundary() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let advertisement = advertisement_with_exec_tool();
        let mut state = enrolled_state_for(&parent, &advertisement);
        let correlation = retained_lease_correlation();
        state
            .record_lease_phase(LeasePhase {
                correlation: correlation.clone(),
                phase: LeasePhaseKind::WaitingDispatch,
            })
            .expect("the waiting-dispatch phase is durable");
        let dispatch = retained_dispatch();
        let expected = ServeOutcome::DispatchReady(Box::new(RunnerDispatchReady {
            correlation: correlation.clone(),
            normalized_arguments: dispatch.normalized_arguments.clone(),
            connection_epoch: positive(CONNECTION_EPOCH),
        }));
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume accepts the exact retained lease");
            connection
                .serve_until_shutdown(&mut state, std::future::pending())
                .await
                .expect("canonical replay reaches the executor boundary")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_lease_directives(DirectiveAction::Await),
                })),
            )
            .await;
            send_hub_message(
                &mut hub_io,
                Message::LeaseClaimed(LeaseClaimed {
                    correlation: correlation.clone(),
                }),
            )
            .await;
            send_hub_message(&mut hub_io, Message::Dispatch(dispatch)).await;
        };
        let (outcome, ()) = tokio::join!(runner, hub);

        assert_eq!(outcome, expected);
        assert_eq!(
            state.reconnect_inventory().lease,
            Some(LeasePhase {
                correlation,
                phase: LeasePhaseKind::DispatchReceived,
            })
        );
    }

    /// INV-011 / INV-024 / INV-043: replay of an already fsynced dispatch is
    /// idempotent and yields the same single executor handoff.
    #[tokio::test]
    async fn s31_inv011_inv024_inv043_dispatch_received_replay_reaches_executor_boundary() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let advertisement = advertisement_with_exec_tool();
        let mut state = enrolled_state_for(&parent, &advertisement);
        let correlation = retained_lease_correlation();
        state
            .record_lease_phase(LeasePhase {
                correlation: correlation.clone(),
                phase: LeasePhaseKind::WaitingDispatch,
            })
            .expect("the waiting-dispatch phase is durable");
        state
            .record_lease_phase(LeasePhase {
                correlation: correlation.clone(),
                phase: LeasePhaseKind::DispatchReceived,
            })
            .expect("the dispatch-received phase is durable");
        let retained = state.reconnect_inventory().clone();
        let dispatch = retained_dispatch();
        let expected = Some(ServeOutcome::DispatchReady(Box::new(RunnerDispatchReady {
            correlation: correlation.clone(),
            normalized_arguments: dispatch.normalized_arguments.clone(),
            connection_epoch: positive(CONNECTION_EPOCH),
        })));
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume accepts the exact retained lease");
            let claimed = connection
                .serve_one(&mut state)
                .await
                .expect("the canonical claim acknowledgement is accepted");
            let ready = connection
                .serve_one(&mut state)
                .await
                .expect("the canonical dispatch replay is accepted");
            (claimed, ready)
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_lease_directives(DirectiveAction::Await),
                })),
            )
            .await;
            send_hub_message(
                &mut hub_io,
                Message::LeaseClaimed(LeaseClaimed { correlation }),
            )
            .await;
            send_hub_message(&mut hub_io, Message::Dispatch(dispatch)).await;
        };
        let ((claimed, ready), ()) = tokio::join!(runner, hub);

        assert_eq!(claimed, None);
        assert_eq!(ready, expected);
        assert_eq!(state.reconnect_inventory(), &retained);
    }

    /// INV-011 / INV-024 / INV-043: dispatch alone cannot substitute for the
    /// canonical claimed capability on the successor connection.
    #[tokio::test]
    async fn s31_inv011_inv024_inv043_dispatch_without_claim_replay_fails_closed() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let advertisement = advertisement_with_exec_tool();
        let mut state = enrolled_state_for(&parent, &advertisement);
        state
            .record_lease_phase(LeasePhase {
                correlation: retained_lease_correlation(),
                phase: LeasePhaseKind::WaitingDispatch,
            })
            .expect("the waiting-dispatch phase is durable");
        let retained = state.reconnect_inventory().clone();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume accepts the exact retained lease");
            connection
                .serve_one(&mut state)
                .await
                .expect_err("dispatch without its claimed capability fails closed")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_lease_directives(DirectiveAction::Await),
                })),
            )
            .await;
            send_hub_message(&mut hub_io, Message::Dispatch(retained_dispatch())).await;
        };
        let (error, ()) = tokio::join!(runner, hub);

        assert!(matches!(
            error,
            RunnerConnectionError::Violation(ProtocolViolation::DispatchMismatch)
        ));
        assert_eq!(state.reconnect_inventory(), &retained);
    }

    /// INV-011 / INV-024 / INV-043: consuming the claimed capability for one
    /// dispatch handoff prevents an equal replay from minting a second handoff.
    #[tokio::test]
    async fn s31_inv011_inv024_inv043_equal_dispatch_replay_has_one_handoff() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let advertisement = advertisement_with_exec_tool();
        let mut state = enrolled_state_for(&parent, &advertisement);
        let correlation = retained_lease_correlation();
        state
            .record_lease_phase(LeasePhase {
                correlation: correlation.clone(),
                phase: LeasePhaseKind::WaitingDispatch,
            })
            .expect("the waiting-dispatch phase is durable");
        let dispatch = retained_dispatch();
        let repeated = dispatch.clone();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume accepts the exact retained lease");
            let claimed = connection
                .serve_one(&mut state)
                .await
                .expect("the canonical claim acknowledgement is accepted");
            let ready = connection
                .serve_one(&mut state)
                .await
                .expect("the first canonical dispatch is accepted");
            let repeated = connection
                .serve_one(&mut state)
                .await
                .expect_err("an equal dispatch cannot mint another handoff");
            (claimed, ready, repeated)
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_lease_directives(DirectiveAction::Await),
                })),
            )
            .await;
            send_hub_message(
                &mut hub_io,
                Message::LeaseClaimed(LeaseClaimed { correlation }),
            )
            .await;
            send_hub_message(&mut hub_io, Message::Dispatch(dispatch)).await;
            send_hub_message(&mut hub_io, Message::Dispatch(repeated)).await;
        };
        let ((claimed, ready, repeated), ()) = tokio::join!(runner, hub);

        assert_eq!(claimed, None);
        assert!(matches!(ready, Some(ServeOutcome::DispatchReady(_))));
        assert!(matches!(
            repeated,
            RunnerConnectionError::Violation(ProtocolViolation::DispatchMismatch)
        ));
    }

    /// INV-011 / INV-024 / INV-043: a claim acknowledgement for another lease
    /// cannot acquire the occupied journal slot's execution capability.
    #[tokio::test]
    async fn s31_inv011_inv024_inv043_cross_wired_claim_replay_fails_closed() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let advertisement = advertisement_with_exec_tool();
        let mut state = enrolled_state_for(&parent, &advertisement);
        state
            .record_lease_phase(LeasePhase {
                correlation: retained_lease_correlation(),
                phase: LeasePhaseKind::WaitingDispatch,
            })
            .expect("the waiting-dispatch phase is durable");
        let retained = state.reconnect_inventory().clone();
        let mut foreign = retained_lease_correlation();
        foreign.lease_id = CanonicalUuid::from_uuid(Uuid::from_u128(ARBITRARY_OTHER_LEASE_UUID));
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume accepts the exact retained lease");
            connection
                .serve_one(&mut state)
                .await
                .expect_err("another lease acknowledgement fails closed")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_lease_directives(DirectiveAction::Await),
                })),
            )
            .await;
            send_hub_message(
                &mut hub_io,
                Message::LeaseClaimed(LeaseClaimed {
                    correlation: foreign,
                }),
            )
            .await;
        };
        let (error, ()) = tokio::join!(runner, hub);

        assert!(matches!(
            error,
            RunnerConnectionError::Violation(ProtocolViolation::LeaseAcknowledgementMismatch)
        ));
        assert_eq!(state.reconnect_inventory(), &retained);
    }

    /// INV-011 / INV-024 / INV-043: execution crosses its durable boundary
    /// before polling, heartbeats remain live, and terminal evidence commits
    /// before the result frame is projected.
    #[tokio::test]
    async fn s31_inv011_inv024_inv043_execution_serves_heartbeat_and_journals_result() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let advertisement = advertisement_with_exec_tool();
        let mut state = enrolled_state_for(&parent, &advertisement);
        let correlation = retained_lease_correlation();
        state
            .record_lease_phase(LeasePhase {
                correlation: correlation.clone(),
                phase: LeasePhaseKind::WaitingDispatch,
            })
            .expect("the waiting-dispatch phase is durable");
        let dispatch = retained_dispatch();
        let terminal = TerminalResult::Success {
            text: String::from("fixture-result"),
        };
        let expected_terminal = terminal.clone();
        let (release_execution, execution_released) = tokio::sync::oneshot::channel();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume accepts the exact retained lease");
            let claimed = connection
                .serve_one(&mut state)
                .await
                .expect("the canonical claim acknowledgement is accepted");
            let ready = connection
                .serve_one(&mut state)
                .await
                .expect("the canonical dispatch is accepted")
                .expect("dispatch yields one executor handoff");
            let ready = ready
                .into_dispatch_ready()
                .map_err(|_| "dispatch did not yield its executor handoff")?;
            let execution = async {
                execution_released
                    .await
                    .expect("the heartbeat observation releases execution");
                terminal
            };
            connection
                .execute_while_serving(&mut state, ready, execution)
                .await
                .expect("terminal evidence is durably projected");
            Ok::<_, &'static str>(claimed)
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_lease_directives(DirectiveAction::Await),
                })),
            )
            .await;
            send_hub_message(
                &mut hub_io,
                Message::LeaseClaimed(LeaseClaimed {
                    correlation: correlation.clone(),
                }),
            )
            .await;
            send_hub_message(&mut hub_io, Message::Dispatch(dispatch)).await;
            send_hub_message(
                &mut hub_io,
                Message::Heartbeat(Heartbeat {
                    sequence: positive(FIRST_CHALLENGE_SEQUENCE),
                    last_accepted_peer_sequence: NO_ACCEPTED_PEER_SEQUENCE,
                }),
            )
            .await;
            let heartbeat = receive_hub_message(&mut hub_io).await;
            release_execution
                .send(())
                .expect("the execution future is still waiting");
            let result = receive_hub_message(&mut hub_io).await;
            (heartbeat, result)
        };
        let (claimed, (heartbeat, result)) = tokio::join!(runner, hub);

        assert_eq!(claimed.expect("the execution harness completes"), None);
        assert_eq!(
            heartbeat,
            Message::HeartbeatAck(HeartbeatAck {
                challenge_sequence: positive(FIRST_CHALLENGE_SEQUENCE),
                runner_sequence: positive(FIRST_RUNNER_SEQUENCE),
                lease_phase: Some(LeasePhase {
                    correlation: correlation.clone(),
                    phase: LeasePhaseKind::ExecutionMayHaveStarted,
                }),
                workspace_phase: None,
            })
        );
        assert_eq!(
            result,
            Message::Result(ResultFrame {
                correlation: correlation.clone(),
                result: expected_terminal.clone(),
            })
        );
        assert_eq!(
            state.reconnect_inventory().result,
            Some(RetainedResult {
                correlation,
                result: expected_terminal,
            })
        );
    }

    /// INV-011 / INV-024 / INV-043: a workspace release received during
    /// execution is deferred until terminal execution evidence is durable.
    #[tokio::test]
    async fn s31_inv011_inv024_inv043_execution_defers_workspace_release() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let advertisement = advertisement_with_exec_tool();
        let mut state = enrolled_state_for(&parent, &advertisement);
        let lease_correlation = retained_lease_correlation();
        let release_correlation = release_correlation();
        state
            .record_lease_phase(LeasePhase {
                correlation: lease_correlation.clone(),
                phase: LeasePhaseKind::WaitingDispatch,
            })
            .expect("the waiting-dispatch phase is durable");
        let dispatch = retained_dispatch();
        let terminal = TerminalResult::Success {
            text: String::from("fixture-result"),
        };
        let expected_terminal = terminal.clone();
        let (release_execution, execution_released) = tokio::sync::oneshot::channel();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume accepts the exact retained lease");
            connection
                .serve_one(&mut state)
                .await
                .expect("the canonical claim acknowledgement is accepted");
            let ready = connection
                .serve_one(&mut state)
                .await
                .expect("the canonical dispatch is accepted")
                .expect("dispatch yields one executor handoff")
                .into_dispatch_ready()
                .expect("the dispatch forms its executor handoff");
            let execution = async {
                execution_released
                    .await
                    .expect("the release observation preserves execution");
                terminal
            };
            connection
                .execute_while_serving(&mut state, ready, execution)
                .await
                .expect("terminal evidence is projected before release acceptance");
            connection
                .serve_one(&mut state)
                .await
                .expect("the deferred release reaches a serving boundary")
                .expect("the deferred release produces a cleanup handoff")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_lease_directives(DirectiveAction::Await),
                })),
            )
            .await;
            send_hub_message(
                &mut hub_io,
                Message::LeaseClaimed(LeaseClaimed {
                    correlation: lease_correlation.clone(),
                }),
            )
            .await;
            send_hub_message(&mut hub_io, Message::Dispatch(dispatch)).await;
            send_hub_message(
                &mut hub_io,
                Message::WorkspaceRelease(WorkspaceRelease {
                    correlation: release_correlation.clone(),
                }),
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
            let heartbeat = receive_hub_message(&mut hub_io).await;
            let observed_state = state_root(&parent);
            assert_eq!(
                observed_state.reconnect_inventory().workspace_operation,
                None
            );
            release_execution
                .send(())
                .expect("the execution future remains live");
            let result = receive_hub_message(&mut hub_io).await;
            (heartbeat, result)
        };
        let (release, (heartbeat, result)) = tokio::join!(runner, hub);

        assert!(matches!(release, ServeOutcome::WorkspaceReleaseReady(_)));
        assert_eq!(
            heartbeat,
            Message::HeartbeatAck(HeartbeatAck {
                challenge_sequence: positive(FIRST_CHALLENGE_SEQUENCE),
                runner_sequence: positive(FIRST_RUNNER_SEQUENCE),
                lease_phase: Some(LeasePhase {
                    correlation: lease_correlation.clone(),
                    phase: LeasePhaseKind::ExecutionMayHaveStarted,
                }),
                workspace_phase: None,
            })
        );
        assert_eq!(
            result,
            Message::Result(ResultFrame {
                correlation: lease_correlation.clone(),
                result: expected_terminal.clone(),
            })
        );
        assert_eq!(
            state.reconnect_inventory().result,
            Some(RetainedResult {
                correlation: lease_correlation,
                result: expected_terminal,
            })
        );
        assert_eq!(
            state.reconnect_inventory().workspace_operation,
            Some(WorkspaceOperation::Release {
                correlation: release_correlation,
                phase: ReleasePhase::ReleaseAccepted,
            })
        );
    }

    /// INV-011 / INV-024 / INV-043: resume sends the exact retained terminal
    /// pair and atomically clears it only after matching recorded directives.
    #[tokio::test]
    async fn s31_inv011_inv024_inv043_resume_discards_recorded_terminal_pair() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = state_with_terminal_result(&parent);
        let retained = state.reconnect_inventory().clone();
        let receipt = state
            .state()
            .receipt()
            .expect("the retained fixture is enrolled")
            .clone();
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = RunnerConnection::establish(runner_io, &mut state, &advertisement);
        let hub = async {
            let observed = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_result_directives(DirectiveAction::DiscardAsRecorded),
                })),
            )
            .await;
            observed
        };
        let (connection, observed) = tokio::join!(runner, hub);
        let expected = Message::Resume(Box::new(Resume {
            request_id: receipt.request_id(),
            digest_version: DIGEST_VERSION,
            enrollment_id: receipt.enrollment_id(),
            runner_id: receipt.runner_id(),
            authentication_id: receipt.authentication_id(),
            advertisement: advertisement.clone(),
            prior_registration_revision: receipt.registration_revision(),
            inventory: retained,
        }));

        assert_eq!(
            connection
                .expect("the recorded recovery directives establish the connection")
                .outcome(),
            EnrollmentOutcome::Resumed
        );
        assert_eq!(observed, expected);
        assert_eq!(state.reconnect_inventory(), &ReconnectInventory::default());
    }

    /// INV-011 / INV-024 / INV-043: stale terminal directives consume only the
    /// exact pair the runner presented and allow its receipt resume to finish.
    #[tokio::test]
    async fn s31_inv011_inv024_inv043_resume_clears_exact_stale_terminal_pair() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = state_with_terminal_result(&parent);
        let retained = state.reconnect_inventory().clone();
        let receipt = state
            .state()
            .receipt()
            .expect("the retained fixture is enrolled")
            .clone();
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = RunnerConnection::establish(runner_io, &mut state, &advertisement);
        let hub = async {
            let observed = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_result_directives(DirectiveAction::FailStale),
                })),
            )
            .await;
            observed
        };
        let (connection, observed) = tokio::join!(runner, hub);
        let expected = Message::Resume(Box::new(Resume {
            request_id: receipt.request_id(),
            digest_version: DIGEST_VERSION,
            enrollment_id: receipt.enrollment_id(),
            runner_id: receipt.runner_id(),
            authentication_id: receipt.authentication_id(),
            advertisement: advertisement.clone(),
            prior_registration_revision: receipt.registration_revision(),
            inventory: retained,
        }));

        assert_eq!(
            connection
                .expect("the stale recovery directives establish the connection")
                .outcome(),
            EnrollmentOutcome::Resumed
        );
        assert_eq!(observed, expected);
        assert_eq!(state.reconnect_inventory(), &ReconnectInventory::default());
    }

    /// INV-011 / INV-024 / INV-043: an unsupported recovery action cannot
    /// discard either member of the runner's exact retained terminal pair.
    #[tokio::test]
    async fn s31_inv011_inv024_inv043_resume_rejects_unsupported_terminal_action() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = state_with_terminal_result(&parent);
        let retained = state.reconnect_inventory().clone();
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = RunnerConnection::establish(runner_io, &mut state, &advertisement);
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_result_directives(DirectiveAction::Await),
                })),
            )
            .await;
        };
        let (rejected, ()) = tokio::join!(runner, hub);
        let rejected = rejected.err().expect("the unsupported action fails closed");

        assert!(matches!(
            rejected,
            RunnerConnectionError::Violation(ProtocolViolation::ResumeDirectives)
        ));
        assert_eq!(state.reconnect_inventory(), &retained);
    }

    #[tokio::test]
    async fn s32_resume_resends_the_exact_retained_workspace_ready_payload() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = state_with_workspace_ready(&parent);
        let retained = state.reconnect_inventory().clone();
        let expected_ready = workspace_ready();
        let receipt = state
            .state()
            .receipt()
            .expect("the retained fixture is enrolled")
            .clone();
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = RunnerConnection::establish(runner_io, &mut state, &advertisement);
        let hub = async {
            let observed_resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_workspace_directives(DirectiveAction::Resend),
                })),
            )
            .await;
            let observed_ready = receive_hub_message(&mut hub_io).await;
            (observed_resume, observed_ready)
        };
        let (connection, (observed_resume, observed_ready)) = tokio::join!(runner, hub);

        assert_eq!(
            connection
                .expect("the resend directive establishes the connection")
                .outcome(),
            EnrollmentOutcome::Resumed
        );
        assert_eq!(
            observed_resume,
            expected_resume_message(&receipt, &advertisement, retained.clone())
        );
        assert_eq!(observed_ready, Message::WorkspaceReady(expected_ready));
        assert_eq!(state.reconnect_inventory(), &retained);
    }

    #[tokio::test]
    async fn s32_workspace_recorded_acknowledgement_retires_resumed_ready_payload() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = state_with_workspace_ready(&parent);
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("the resend directive establishes the connection");
            connection
                .serve_one(&mut state)
                .await
                .expect("the exact ready acknowledgement is consumed")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_workspace_directives(DirectiveAction::Resend),
                })),
            )
            .await;
            let ready = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::WorkspaceRecorded(workspace_recorded()),
            )
            .await;
            ready
        };
        let (outcome, ready) = tokio::join!(runner, hub);

        assert_eq!(outcome, None);
        assert_eq!(ready, Message::WorkspaceReady(workspace_ready()));
        assert_eq!(state.reconnect_inventory(), &ReconnectInventory::default());
        assert_eq!(state.retained_workspace_ready(), None);
    }

    #[tokio::test]
    async fn s32_resume_fail_stale_retires_the_exact_ready_payload() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = state_with_workspace_ready(&parent);
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = RunnerConnection::establish(runner_io, &mut state, &advertisement);
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_workspace_directives(DirectiveAction::FailStale),
                })),
            )
            .await;
        };
        let (connection, ()) = tokio::join!(runner, hub);

        assert_eq!(
            connection
                .expect("the stale directive establishes the connection")
                .outcome(),
            EnrollmentOutcome::Resumed
        );
        assert_eq!(state.reconnect_inventory(), &ReconnectInventory::default());
        assert_eq!(state.retained_workspace_ready(), None);
    }

    #[tokio::test]
    async fn s32_resume_rejects_await_for_ready_workspace_and_preserves_it() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = state_with_workspace_ready(&parent);
        let retained = state.reconnect_inventory().clone();
        let ready = workspace_ready();
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = RunnerConnection::establish(runner_io, &mut state, &advertisement);
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_workspace_directives(DirectiveAction::Await),
                })),
            )
            .await;
        };
        let (rejected, ()) = tokio::join!(runner, hub);
        let rejected = rejected
            .err()
            .expect("the unsupported ready directive fails closed");

        assert!(matches!(
            rejected,
            RunnerConnectionError::Violation(ProtocolViolation::ResumeDirectives)
        ));
        assert_eq!(state.reconnect_inventory(), &retained);
        assert_eq!(state.retained_workspace_ready(), Some(&ready));
    }

    /// INV-011 / INV-024: reconnect resends the exact retained lease-offer
    /// failure and keeps it durable until acknowledgement.
    #[tokio::test]
    async fn s31_inv011_inv024_resume_resends_retained_lease_offer_failure() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        let failure = retained_lease_offer_failure();
        state
            .record_lease_offer_failure(failure.clone())
            .expect("the lease-offer failure is durable");
        let retained = state.reconnect_inventory().clone();
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = RunnerConnection::establish(runner_io, &mut state, &advertisement);
        let hub = async {
            let observed_resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_failure_directives(&failure, DirectiveAction::Resend),
                })),
            )
            .await;
            let observed_failure = receive_hub_message(&mut hub_io).await;
            (observed_resume, observed_failure)
        };
        let (connection, (observed_resume, observed_failure)) = tokio::join!(runner, hub);
        let receipt = state
            .state()
            .receipt()
            .expect("the retained fixture remains enrolled");
        let expected_resume = Message::Resume(Box::new(Resume {
            request_id: receipt.request_id(),
            digest_version: DIGEST_VERSION,
            enrollment_id: receipt.enrollment_id(),
            runner_id: receipt.runner_id(),
            authentication_id: receipt.authentication_id(),
            advertisement: advertisement.clone(),
            prior_registration_revision: receipt.registration_revision(),
            inventory: retained.clone(),
        }));

        assert_eq!(
            connection
                .expect("the resend directive establishes the connection")
                .outcome(),
            EnrollmentOutcome::Resumed
        );
        assert_eq!(observed_resume, expected_resume);
        assert_eq!(
            observed_failure,
            Message::OperationFailed(OperationFailed {
                failure: failure.clone(),
            })
        );
        assert_eq!(state.reconnect_inventory(), &retained);
    }

    /// INV-011 / INV-024: a daemon-recorded reconnect directive retires the
    /// exact retained lease-offer failure before serving begins.
    #[tokio::test]
    async fn s31_inv011_inv024_resume_discards_recorded_lease_offer_failure() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        let failure = retained_lease_offer_failure();
        state
            .record_lease_offer_failure(failure.clone())
            .expect("the lease-offer failure is durable");
        let retained = state.reconnect_inventory().clone();
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = RunnerConnection::establish(runner_io, &mut state, &advertisement);
        let hub = async {
            let observed = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_failure_directives(
                        &failure,
                        DirectiveAction::DiscardAsRecorded,
                    ),
                })),
            )
            .await;
            observed
        };
        let (connection, observed) = tokio::join!(runner, hub);
        let receipt = state
            .state()
            .receipt()
            .expect("the retained fixture remains enrolled");
        let expected_resume = Message::Resume(Box::new(Resume {
            request_id: receipt.request_id(),
            digest_version: DIGEST_VERSION,
            enrollment_id: receipt.enrollment_id(),
            runner_id: receipt.runner_id(),
            authentication_id: receipt.authentication_id(),
            advertisement: advertisement.clone(),
            prior_registration_revision: receipt.registration_revision(),
            inventory: retained,
        }));

        assert_eq!(
            connection
                .expect("the recorded directive establishes the connection")
                .outcome(),
            EnrollmentOutcome::Resumed
        );
        assert_eq!(observed, expected_resume);
        assert_eq!(state.reconnect_inventory(), &ReconnectInventory::default());
    }

    /// INV-011 / INV-024: an unsupported failure directive preserves the
    /// retained refusal evidence and fails the reconnect closed.
    #[tokio::test]
    async fn s31_inv011_inv024_resume_rejects_awaiting_lease_offer_failure() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        let failure = retained_lease_offer_failure();
        state
            .record_lease_offer_failure(failure.clone())
            .expect("the lease-offer failure is durable");
        let retained = state.reconnect_inventory().clone();
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = RunnerConnection::establish(runner_io, &mut state, &advertisement);
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: retained_failure_directives(&failure, DirectiveAction::Await),
                })),
            )
            .await;
        };
        let (rejected, ()) = tokio::join!(runner, hub);
        let rejected = rejected
            .err()
            .expect("the unsupported failure directive fails closed");

        assert!(matches!(
            rejected,
            RunnerConnectionError::Violation(ProtocolViolation::ResumeDirectives)
        ));
        assert_eq!(state.reconnect_inventory(), &retained);
    }

    /// INV-011 / INV-024: heartbeat progress repeats the exact fsynced lease
    /// phase instead of inventing process-local execution state.
    #[tokio::test]
    async fn inv011_inv024_heartbeat_reports_the_current_durable_lease_phase() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before heartbeat");
            record_terminal_result_fixture(&mut state);
            connection
                .serve_one(&mut state)
                .await
                .expect("the heartbeat is served")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
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
        let (outcome, acknowledgement) = tokio::join!(runner, hub);

        assert_eq!(outcome, None);
        assert_eq!(
            acknowledgement,
            Message::HeartbeatAck(HeartbeatAck {
                challenge_sequence: positive(FIRST_CHALLENGE_SEQUENCE),
                runner_sequence: positive(FIRST_RUNNER_SEQUENCE),
                lease_phase: Some(LeasePhase {
                    correlation: retained_lease_correlation(),
                    phase: LeasePhaseKind::ExecutionMayHaveStarted,
                }),
                workspace_phase: None,
            })
        );
    }

    /// INV-011 / INV-024: heartbeat progress repeats the exact fsynced release
    /// phase instead of inferring cleanup progress from process memory.
    #[tokio::test]
    async fn inv011_inv024_heartbeat_reports_the_current_workspace_release_phase() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before heartbeat");
            state
                .record_workspace_release_phase(
                    release_correlation(),
                    ReleasePhase::ReleaseAccepted,
                )
                .expect("the accepted release is durable");
            connection
                .serve_one(&mut state)
                .await
                .expect("the heartbeat is served")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
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
        let (outcome, acknowledgement) = tokio::join!(runner, hub);

        assert_eq!(outcome, None);
        assert_eq!(
            acknowledgement,
            Message::HeartbeatAck(HeartbeatAck {
                challenge_sequence: positive(FIRST_CHALLENGE_SEQUENCE),
                runner_sequence: positive(FIRST_RUNNER_SEQUENCE),
                lease_phase: None,
                workspace_phase: Some(HeartbeatWorkspacePhase::ReleaseAccepted {
                    correlation: release_correlation(),
                }),
            })
        );
    }

    /// INV-011 / INV-024: a retained cleanup failure supersedes the accepted
    /// phase in heartbeat progress with its exact unrecorded-failure boundary.
    #[tokio::test]
    async fn inv011_inv024_heartbeat_reports_the_workspace_release_failure() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before heartbeat");
            state
                .record_workspace_release_phase(
                    release_correlation(),
                    ReleasePhase::ReleaseAccepted,
                )
                .expect("the accepted release is durable");
            state
                .record_workspace_release_failure(release_failure())
                .expect("the cleanup failure is durable");
            connection
                .serve_one(&mut state)
                .await
                .expect("the heartbeat is served")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
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
        let (outcome, acknowledgement) = tokio::join!(runner, hub);

        assert_eq!(outcome, None);
        assert_eq!(
            acknowledgement,
            Message::HeartbeatAck(HeartbeatAck {
                challenge_sequence: positive(FIRST_CHALLENGE_SEQUENCE),
                runner_sequence: positive(FIRST_RUNNER_SEQUENCE),
                lease_phase: None,
                workspace_phase: Some(HeartbeatWorkspacePhase::FailureUnrecorded {
                    correlation: WorkspaceFailureCorrelation::Release(release_correlation()),
                }),
            })
        );
    }

    #[tokio::test]
    async fn heartbeat_rejects_a_lease_phase_from_another_registration() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before heartbeat");
            record_terminal_result_fixture(&mut state);
            connection
                .register_advertisement(&mut state, advertisement.clone())
                .await
                .expect("the successor registration is accepted");
            connection
                .serve_one(&mut state)
                .await
                .expect_err("a stale-registration lease phase fails closed")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: ReconnectDirectives::default(),
                })),
            )
            .await;
            let _advertise = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Registered(Registered {
                    registration_revision: positive(NEXT_REGISTRATION_REVISION),
                    advertisement_digest: advertisement_digest(&advertisement)
                        .expect("the explicit empty advertisement has a digest"),
                }),
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
        };
        let (error, ()) = tokio::join!(runner, hub);

        assert!(matches!(
            error,
            RunnerConnectionError::Violation(ProtocolViolation::ConnectionCorrelationMismatch)
        ));
        assert_eq!(
            state.reconnect_inventory().result,
            Some(RetainedResult {
                correlation: retained_lease_correlation(),
                result: TerminalResult::Ambiguous,
            })
        );
    }

    /// INV-011 / INV-024: an exact durable result acknowledgement frees the
    /// retained terminal envelope and lease through the live serving loop.
    #[tokio::test]
    async fn inv011_inv024_exact_result_acknowledgement_clears_retained_slots() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        let advertisement = empty_advertisement();
        let correlation = retained_lease_correlation();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before the acknowledgement");
            record_terminal_result_fixture(&mut state);
            connection
                .serve_one(&mut state)
                .await
                .expect("the exact result acknowledgement is consumed")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: ReconnectDirectives::default(),
                })),
            )
            .await;
            send_hub_message(
                &mut hub_io,
                Message::ResultRecorded(ResultRecorded { correlation }),
            )
            .await;
        };
        let (outcome, ()) = tokio::join!(runner, hub);

        assert_eq!(outcome, None);
        assert_eq!(state.reconnect_inventory(), &ReconnectInventory::default());
    }

    /// INV-011 / INV-024: accepted cleanup remains responsive to heartbeat and
    /// records completion before projecting the exact release frame.
    #[tokio::test]
    async fn inv011_inv024_workspace_cleanup_serves_heartbeat_before_release_projection() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        let advertisement = empty_advertisement();
        let correlation = release_correlation();
        let (complete_cleanup, cleanup_completed) = tokio::sync::oneshot::channel();
        let (projection_observed, allow_runner_finish) = tokio::sync::oneshot::channel();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before the release");
            let release = connection
                .serve_one(&mut state)
                .await
                .expect("the release reaches a serving boundary")
                .expect("the release produces a cleanup handoff")
                .into_workspace_release_ready()
                .expect("the accepted release forms the cleanup handoff");
            let cleanup = async {
                cleanup_completed
                    .await
                    .expect("the cleanup fixture is released");
                Ok::<(), ()>(())
            };
            connection
                .release_while_serving(&mut state, release, cleanup)
                .await
                .expect("cleanup completes while the connection stays live");
            allow_runner_finish
                .await
                .expect("the durable projection is observed before runner completion");
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: ReconnectDirectives::default(),
                })),
            )
            .await;
            send_hub_message(
                &mut hub_io,
                Message::WorkspaceRelease(WorkspaceRelease {
                    correlation: correlation.clone(),
                }),
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
            let acknowledgement = receive_hub_message(&mut hub_io).await;
            complete_cleanup
                .send(())
                .expect("the cleanup fixture receives completion");
            let released = receive_hub_message(&mut hub_io).await;
            let projected_state = state_root(&parent);
            assert_eq!(
                projected_state.reconnect_inventory().workspace_operation,
                Some(WorkspaceOperation::Release {
                    correlation: correlation.clone(),
                    phase: ReleasePhase::ReleaseCompleted,
                })
            );
            projection_observed
                .send(())
                .expect("the runner waits for durable projection observation");
            (acknowledgement, released)
        };
        let ((), (acknowledgement, released)) = tokio::join!(runner, hub);

        assert_eq!(
            acknowledgement,
            Message::HeartbeatAck(HeartbeatAck {
                challenge_sequence: positive(FIRST_CHALLENGE_SEQUENCE),
                runner_sequence: positive(FIRST_RUNNER_SEQUENCE),
                lease_phase: None,
                workspace_phase: Some(HeartbeatWorkspacePhase::ReleaseAccepted {
                    correlation: correlation.clone(),
                }),
            })
        );
        assert_eq!(
            released,
            Message::WorkspaceReleased(WorkspaceReleased {
                correlation: correlation.clone(),
            })
        );
        assert_eq!(
            state.reconnect_inventory().workspace_operation,
            Some(WorkspaceOperation::Release {
                correlation,
                phase: ReleasePhase::ReleaseCompleted,
            })
        );
    }

    /// INV-011 / INV-024: cleanup serving rejects state-mutating frames before
    /// they can create unrelated durable operation state.
    #[tokio::test]
    async fn inv011_inv024_workspace_cleanup_rejects_lease_offer_without_journaling() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        let advertisement = empty_advertisement();
        let correlation = release_correlation();
        let offer = unavailable_lease_offer();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before the release");
            let release = connection
                .serve_one(&mut state)
                .await
                .expect("the release reaches a serving boundary")
                .expect("the release produces a cleanup handoff")
                .into_workspace_release_ready()
                .expect("the accepted release forms the cleanup handoff");
            connection
                .release_while_serving(
                    &mut state,
                    release,
                    std::future::pending::<Result<(), ()>>(),
                )
                .await
                .expect_err("a lease offer is not served during cleanup")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: ReconnectDirectives::default(),
                })),
            )
            .await;
            send_hub_message(
                &mut hub_io,
                Message::WorkspaceRelease(WorkspaceRelease {
                    correlation: correlation.clone(),
                }),
            )
            .await;
            send_hub_message(&mut hub_io, Message::LeaseOffer(offer)).await;
        };
        let (error, ()) = tokio::join!(runner, hub);

        assert!(matches!(
            error,
            RunnerConnectionError::Violation(ProtocolViolation::UnexpectedFrame {
                expected: MessageKind::Heartbeat,
                received: MessageKind::LeaseOffer,
            })
        ));
        assert_eq!(state.reconnect_inventory().operation_failure, None);
        assert_eq!(state.reconnect_inventory().lease, None);
        assert_eq!(
            state.reconnect_inventory().workspace_operation,
            Some(WorkspaceOperation::Release {
                correlation,
                phase: ReleasePhase::ReleaseAccepted,
            })
        );
    }

    /// INV-011 / INV-024: cleanup failure is retained beside the accepted
    /// release before the bounded two-layer failure frame is projected.
    #[tokio::test]
    async fn inv011_inv024_workspace_cleanup_failure_is_journaled_before_projection() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        let advertisement = empty_advertisement();
        let correlation = release_correlation();
        let (projection_observed, allow_runner_finish) = tokio::sync::oneshot::channel();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before the release");
            let release = connection
                .serve_one(&mut state)
                .await
                .expect("the release reaches a serving boundary")
                .expect("the release produces a cleanup handoff")
                .into_workspace_release_ready()
                .expect("the accepted release forms the cleanup handoff");
            connection
                .release_while_serving(&mut state, release, async { Err::<(), ()>(()) })
                .await
                .expect("the cleanup failure is durably projected");
            allow_runner_finish
                .await
                .expect("the durable failure is observed before runner completion");
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: ReconnectDirectives::default(),
                })),
            )
            .await;
            send_hub_message(
                &mut hub_io,
                Message::WorkspaceRelease(WorkspaceRelease {
                    correlation: correlation.clone(),
                }),
            )
            .await;
            let failure = receive_hub_message(&mut hub_io).await;
            let expected = expected_workspace_cleanup_failure(correlation.clone());
            let projected_state = state_root(&parent);
            assert_eq!(
                projected_state.reconnect_inventory().workspace_operation,
                Some(WorkspaceOperation::Release {
                    correlation: correlation.clone(),
                    phase: ReleasePhase::ReleaseAccepted,
                })
            );
            assert_eq!(
                projected_state.reconnect_inventory().operation_failure,
                Some(expected)
            );
            projection_observed
                .send(())
                .expect("the runner waits for durable failure observation");
            failure
        };
        let ((), failure) = tokio::join!(runner, hub);
        let expected = expected_workspace_cleanup_failure(correlation.clone());

        assert_eq!(
            failure,
            Message::OperationFailed(OperationFailed {
                failure: expected.clone(),
            })
        );
        assert_eq!(
            state.reconnect_inventory().workspace_operation,
            Some(WorkspaceOperation::Release {
                correlation,
                phase: ReleasePhase::ReleaseAccepted,
            })
        );
        assert_eq!(
            state.reconnect_inventory().operation_failure,
            Some(expected)
        );
    }

    /// INV-011 / INV-024: the live exact acknowledgement clears one completed
    /// workspace release through the serving loop.
    #[tokio::test]
    async fn inv011_inv024_exact_workspace_release_acknowledgement_clears_retained_slot() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        let advertisement = empty_advertisement();
        let correlation = release_correlation();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before the acknowledgement");
            state
                .record_workspace_release_phase(
                    release_correlation(),
                    ReleasePhase::ReleaseAccepted,
                )
                .expect("the accepted release is durable");
            state
                .record_workspace_release_phase(
                    release_correlation(),
                    ReleasePhase::ReleaseCompleted,
                )
                .expect("the completed release is durable");
            connection
                .serve_one(&mut state)
                .await
                .expect("the exact release acknowledgement is consumed")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: ReconnectDirectives::default(),
                })),
            )
            .await;
            send_hub_message(
                &mut hub_io,
                Message::WorkspaceReleaseRecorded(WorkspaceReleaseRecorded { correlation }),
            )
            .await;
        };
        let (outcome, ()) = tokio::join!(runner, hub);

        assert_eq!(outcome, None);
        assert_eq!(state.reconnect_inventory(), &ReconnectInventory::default());
    }

    #[tokio::test]
    async fn another_workspace_release_acknowledgement_preserves_retained_slot() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        let advertisement = empty_advertisement();
        let mut foreign = release_correlation();
        foreign.manifest_id = identity(ARBITRARY_OTHER_MANIFEST_UUID);
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before the acknowledgement");
            state
                .record_workspace_release_phase(
                    release_correlation(),
                    ReleasePhase::ReleaseAccepted,
                )
                .expect("the accepted release is durable");
            state
                .record_workspace_release_phase(
                    release_correlation(),
                    ReleasePhase::ReleaseCompleted,
                )
                .expect("the completed release is durable");
            connection
                .serve_one(&mut state)
                .await
                .expect_err("another manifest cannot acknowledge the retained release")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: ReconnectDirectives::default(),
                })),
            )
            .await;
            send_hub_message(
                &mut hub_io,
                Message::WorkspaceReleaseRecorded(WorkspaceReleaseRecorded {
                    correlation: foreign,
                }),
            )
            .await;
        };
        let (error, ()) = tokio::join!(runner, hub);

        assert!(matches!(
            error,
            RunnerConnectionError::Violation(ProtocolViolation::WorkspaceAcknowledgementMismatch)
        ));
        assert_eq!(
            state.reconnect_inventory().workspace_operation,
            Some(WorkspaceOperation::Release {
                correlation: release_correlation(),
                phase: ReleasePhase::ReleaseCompleted,
            })
        );
    }

    /// INV-011 / INV-024: the exact cleanup-failure acknowledgement atomically
    /// clears both the failure and its accepted release through the serving loop.
    #[tokio::test]
    async fn inv011_inv024_exact_workspace_failure_acknowledgement_clears_both_slots() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        let advertisement = empty_advertisement();
        let correlation = OperationCorrelation::Release(release_correlation());
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before the acknowledgement");
            state
                .record_workspace_release_phase(
                    release_correlation(),
                    ReleasePhase::ReleaseAccepted,
                )
                .expect("the accepted release is durable");
            state
                .record_workspace_release_failure(release_failure())
                .expect("the cleanup failure is durable");
            connection
                .serve_one(&mut state)
                .await
                .expect("the exact failure acknowledgement is consumed")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: ReconnectDirectives::default(),
                })),
            )
            .await;
            send_hub_message(
                &mut hub_io,
                Message::OperationFailureRecorded(OperationFailureRecorded { correlation }),
            )
            .await;
        };
        let (outcome, ()) = tokio::join!(runner, hub);

        assert_eq!(outcome, None);
        assert_eq!(state.reconnect_inventory(), &ReconnectInventory::default());
    }

    #[tokio::test]
    async fn another_lease_result_acknowledgement_preserves_retained_slots() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        let advertisement = empty_advertisement();
        let mut foreign = retained_lease_correlation();
        foreign.lease_id = identity(ARBITRARY_OTHER_LEASE_UUID);
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before the acknowledgement");
            record_terminal_result_fixture(&mut state);
            connection
                .serve_one(&mut state)
                .await
                .expect_err("another lease cannot acknowledge retained evidence")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: ReconnectDirectives::default(),
                })),
            )
            .await;
            send_hub_message(
                &mut hub_io,
                Message::ResultRecorded(ResultRecorded {
                    correlation: foreign,
                }),
            )
            .await;
        };
        let (error, ()) = tokio::join!(runner, hub);

        assert!(matches!(
            error,
            RunnerConnectionError::Violation(ProtocolViolation::ResultAcknowledgementMismatch)
        ));
        assert_eq!(
            state.reconnect_inventory().result,
            Some(RetainedResult {
                correlation: retained_lease_correlation(),
                result: TerminalResult::Ambiguous,
            })
        );
    }

    /// INV-011 / INV-024: the live exact acknowledgement clears a retained
    /// lease-offer failure through the serving loop.
    #[tokio::test]
    async fn s31_inv011_inv024_exact_failure_acknowledgement_clears_retained_slot() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        let advertisement = empty_advertisement();
        let failure = retained_lease_offer_failure();
        let correlation = failure.correlation.clone();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before the acknowledgement");
            state
                .record_lease_offer_failure(failure)
                .expect("the lease-offer failure is durable");
            connection
                .serve_one(&mut state)
                .await
                .expect("the exact failure acknowledgement is consumed")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: ReconnectDirectives::default(),
                })),
            )
            .await;
            send_hub_message(
                &mut hub_io,
                Message::OperationFailureRecorded(OperationFailureRecorded { correlation }),
            )
            .await;
        };
        let (outcome, ()) = tokio::join!(runner, hub);

        assert_eq!(outcome, None);
        assert_eq!(state.reconnect_inventory(), &ReconnectInventory::default());
    }

    #[tokio::test]
    async fn another_failure_acknowledgement_preserves_retained_slot() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = enrolled_state(&parent);
        let advertisement = empty_advertisement();
        let failure = retained_lease_offer_failure();
        let retained = failure.clone();
        let mut foreign = retained_lease_correlation();
        foreign.lease_id = identity(ARBITRARY_OTHER_LEASE_UUID);
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before the acknowledgement");
            state
                .record_lease_offer_failure(failure)
                .expect("the lease-offer failure is durable");
            connection
                .serve_one(&mut state)
                .await
                .expect_err("another lease cannot acknowledge retained failure evidence")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: ReconnectDirectives::default(),
                })),
            )
            .await;
            send_hub_message(
                &mut hub_io,
                Message::OperationFailureRecorded(OperationFailureRecorded {
                    correlation: OperationCorrelation::LeaseOffer(foreign),
                }),
            )
            .await;
        };
        let (error, ()) = tokio::join!(runner, hub);

        assert!(matches!(
            error,
            RunnerConnectionError::Violation(ProtocolViolation::FailureAcknowledgementMismatch)
        ));
        assert_eq!(
            state.reconnect_inventory().operation_failure,
            Some(retained)
        );
    }

    #[tokio::test]
    async fn local_shutdown_waits_for_an_in_flight_heartbeat_frame() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = state_root(&parent);
        let receipt = issued_receipt(state.state().request_id());
        state
            .record_receipt(receipt)
            .expect("the issued receipt is journaled");
        let advertisement = empty_advertisement();
        let frame_backpressure_bytes = 1;
        let (runner_io, hub_io) = tokio::io::duplex(frame_backpressure_bytes);
        let mut hub_io = BufReader::new(hub_io);
        let (mut shutdown_sender, mut shutdown_receiver) = tokio::io::duplex(1);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before heartbeat");
            let outcome = connection
                .serve_until_shutdown(&mut state, async {
                    shutdown_receiver
                        .read_exact(&mut [0_u8])
                        .await
                        .expect("the local shutdown signal is delivered");
                })
                .await
                .expect("the serving loop reaches a clean shutdown boundary");
            let end = connection
                .shutdown()
                .await
                .expect("the epoch-targeted shutdown frame is sent");
            (outcome, end)
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
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
            let buffered_prefix = hub_io
                .fill_buf()
                .await
                .expect("the blocked acknowledgement exposes its first byte")
                .len();
            shutdown_sender
                .write_all(&[1_u8])
                .await
                .expect("the runner still waits at the in-flight write");
            let acknowledgement = receive_hub_message(&mut hub_io).await;
            let shutdown = receive_hub_message(&mut hub_io).await;
            (buffered_prefix, acknowledgement, shutdown)
        };

        let ((outcome, end), (buffered_prefix, acknowledgement, shutdown)) =
            tokio::join!(runner, hub);
        let epoch = positive(CONNECTION_EPOCH);

        assert_eq!(buffered_prefix, frame_backpressure_bytes);
        assert_eq!(outcome, ServeOutcome::ShutdownReady);
        assert_eq!(
            end,
            ConnectionEnd::RunnerShutdown {
                connection_epoch: epoch
            }
        );
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
            shutdown,
            Message::Shutdown(Shutdown {
                connection_epoch: epoch,
                reason: ShutdownReason::RunnerShutdown,
            })
        );
    }

    #[tokio::test]
    async fn recovery_seam_names_unavailable_workspace_provisioning() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = state_root(&parent);
        let receipt = issued_receipt(state.state().request_id());
        state
            .record_receipt(receipt)
            .expect("the issued receipt is journaled");
        let advertisement = empty_advertisement();
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = RunnerConnection::establish(runner_io, &mut state, &advertisement);
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: positive(CONNECTION_EPOCH),
                    directives: ReconnectDirectives::default(),
                })),
            )
            .await;
        };
        let (connection, ()) = tokio::join!(runner, hub);
        let connection = connection.expect("the production connection is established");

        assert_eq!(
            connection.recovery_unavailable().gap(),
            RecoveryGap::WorkspaceProvisioningUnavailable
        );
    }

    #[test]
    fn transient_connection_failures_are_reconnectable() {
        let unavailable = RunnerConnectionError::PeerRejected {
            code: RejectionCode::Unavailable,
            offending_kind: String::from("enroll"),
            available_correlation: Box::new(AvailableCorrelation::None),
        };
        let shutting_down = RunnerConnectionError::PeerRejected {
            code: RejectionCode::ShuttingDown,
            offending_kind: String::from("resume"),
            available_correlation: Box::new(AvailableCorrelation::None),
        };

        assert!(SocketConnectError::InvalidSocketIdentity.is_reconnectable());
        assert!(SocketConnectError::SocketIdentityChanged.is_reconnectable());
        assert!(unavailable.is_reconnectable());
        assert!(shutting_down.is_reconnectable());
    }

    #[test]
    fn policy_rejection_is_not_reconnectable() {
        let rejected = RunnerConnectionError::PeerRejected {
            code: RejectionCode::PolicyRejected,
            offending_kind: String::from("enroll"),
            available_correlation: Box::new(AvailableCorrelation::None),
        };

        assert!(!rejected.is_reconnectable());
    }

    #[tokio::test]
    async fn registered_reply_commits_the_exact_successor_receipt() {
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
                    connection_epoch: positive(CONNECTION_EPOCH),
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
    async fn workspace_provision_stops_at_the_typed_recovery_seam() {
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
                .expect("resume completes before the unavailable operation");
            connection
                .serve_one(&mut state)
                .await
                .expect_err("the recovery-dependent operation is unavailable")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: receipt.registration_revision(),
                    connection_epoch: positive(CONNECTION_EPOCH),
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
        };
        let (error, ()) = tokio::join!(runner, hub);

        assert!(matches!(
            error,
            RunnerConnectionError::RecoveryUnavailable(RecoveryUnavailable {
                gap: RecoveryGap::WorkspaceProvisioningUnavailable,
            })
        ));
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
                    connection_epoch: positive(CONNECTION_EPOCH),
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
            Some(ServeOutcome::ConnectionEnded(
                ConnectionEnd::DaemonShutdown {
                    connection_epoch: positive(CONNECTION_EPOCH),
                }
            ))
        );
    }

    #[tokio::test]
    async fn fatal_stale_daemon_shutdown_rejection_closes_before_following_heartbeat() {
        let parent = TempDir::new().expect("a temporary parent is available");
        let mut state = state_root(&parent);
        let receipt = issued_receipt(state.state().request_id());
        state
            .record_receipt(receipt)
            .expect("the issued receipt is journaled");
        let advertisement = empty_advertisement();
        let stale_epoch = positive(CONNECTION_EPOCH + 1);
        let current_epoch = positive(CONNECTION_EPOCH);
        let (runner_io, hub_io) = tokio::io::duplex(TEST_WIRE_BYTES);
        let mut hub_io = BufReader::new(hub_io);

        let runner = async {
            let mut connection = RunnerConnection::establish(runner_io, &mut state, &advertisement)
                .await
                .expect("resume completes before shutdown");
            connection
                .serve(&mut state)
                .await
                .expect("the fatal stale shutdown rejection terminates serving")
        };
        let hub = async {
            let _resume = receive_hub_message(&mut hub_io).await;
            send_hub_message(
                &mut hub_io,
                Message::Resumed(Box::new(Resumed {
                    registration_revision: positive(INITIAL_REGISTRATION_REVISION),
                    connection_epoch: current_epoch,
                    directives: ReconnectDirectives::default(),
                })),
            )
            .await;
            send_hub_message(
                &mut hub_io,
                Message::Shutdown(Shutdown {
                    connection_epoch: stale_epoch,
                    reason: ShutdownReason::DaemonShutdown,
                }),
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
            let rejected = receive_hub_message(&mut hub_io).await;
            let after_rejection = receive_message(&mut hub_io).await;
            (rejected, after_rejection)
        };
        let (end, (rejected, after_rejection)) = tokio::join!(runner, hub);

        assert_eq!(
            end,
            ServeOutcome::ConnectionEnded(ConnectionEnd::StaleConnectionRejected {
                connection_epoch: stale_epoch,
            })
        );
        assert_eq!(
            rejected,
            Message::Rejected(Rejected {
                offending_kind: MessageKind::Shutdown.to_string(),
                available_correlation: AvailableCorrelation::ConnectionEpoch(stale_epoch),
                code: RejectionCode::StaleConnection,
            })
        );
        assert!(matches!(
            after_rejection,
            Err(RunnerConnectionError::PeerClosed)
        ));
    }
}
