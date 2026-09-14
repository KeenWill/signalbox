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
    Advertise, Advertisement, AvailableCorrelation, CanonicalUuid, DIGEST_VERSION, Digest,
    DirectiveAction, EffectClass, Enroll, Frame, FrameError, Heartbeat, HeartbeatAck, LeaseClaim,
    LeaseCorrelation, LeaseOffer, LeasePhase, LeasePhaseKind, MAX_FRAME_BYTES, Message,
    PositiveU64, Registered, Rejected, RejectionCode, ResultFrame, Resume, RetainedResult,
    SandboxProfile, Shutdown, ShutdownReason, TerminalResult, ValueError, advertisement_digest,
    decode_line, encode_line,
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

mod leaks;
mod workspaces;

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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServeOutcome {
    /// The daemon supplied a clean terminal order.
    ConnectionEnded(ConnectionEnd),
    /// Local shutdown is ready at a boundary with no in-flight write.
    ShutdownReady,
}

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
    InvalidShutdownReason,
    PendingRegistrationMutation,
    ConnectionCorrelationMismatch,
    LeaseMismatch,
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
            Self::LeaseMismatch => {
                formatter.write_str("lease frame does not match retained execution authority")
            }
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
    Workspace(crate::WorkspaceProvisionError),
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
            Self::Workspace(error) => error.fmt(formatter),
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
            Self::Workspace(error) => Some(error),
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
    receive_buffer: Vec<u8>,
    pending_offer: Option<LeaseOffer>,
    resumed_lease: Option<LeaseCorrelation>,
    execution: Option<RunnerExecution>,
    last_recorded: Option<LeaseCorrelation>,
    configuration: Option<crate::RunnerConfiguration>,
    workspace: Option<workspaces::WorkspaceExecution>,
    last_workspace_recorded: Option<signalbox_runner_wire::WorkspaceRecorded>,
    last_provision_failure: Option<signalbox_runner_wire::OperationCorrelation>,
    last_release_recorded: Option<(signalbox_runner_wire::ReleaseCorrelation, bool)>,
    startup_report: leaks::StartupReport,
    leak_sent: bool,
    last_leak_recorded: Option<signalbox_runner_wire::WorkspaceLeakRecorded>,
    deferred_dispatch: Option<signalbox_runner_wire::Dispatch>,
    deferred_release: Option<signalbox_runner_wire::WorkspaceRelease>,
    offer_claimed: bool,
    receipt: EnrollmentReceipt,
    advertisement: Advertisement,
    outcome: EnrollmentOutcome,
    connection_epoch: PositiveU64,
    heartbeat: Option<HeartbeatExchange>,
}

enum RunnerEvent {
    Message(Message),
    Result(RetainedResult),
    Workspace(workspaces::WorkspaceCompletion),
    LeakScan(Vec<signalbox_runner_wire::LeakFact>),
}

struct RunnerExecution {
    correlation: LeaseCorrelation,
    task: tokio::task::JoinHandle<signalbox_domain::ToolAttemptEnd>,
}

impl Drop for RunnerExecution {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn execution_finished(
    execution: &mut Option<RunnerExecution>,
) -> Result<RetainedResult, RunnerConnectionError> {
    let Some(execution) = execution else {
        return std::future::pending().await;
    };
    let end = (&mut execution.task).await.map_err(|_| lease_mismatch())?;
    let result = TerminalResult::from_attempt_end(&end).map_err(|_| lease_mismatch())?;
    Ok(RetainedResult {
        correlation: execution.correlation.clone(),
        result,
    })
}

fn lease_mismatch() -> RunnerConnectionError {
    RunnerConnectionError::Violation(ProtocolViolation::LeaseMismatch)
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
        let mut resumed_lease = None;
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
                let inventory = state.reconnect_inventory();
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
                if let Some(directive) = &resumed.directives.lease {
                    match directive.action {
                        DirectiveAction::Await
                            if inventory.result.is_none()
                                && inventory.lease.as_ref().is_some_and(|lease| {
                                    matches!(
                                        lease.phase,
                                        LeasePhaseKind::WaitingDispatch
                                            | LeasePhaseKind::DispatchReceived
                                    )
                                }) =>
                        {
                            resumed_lease = Some(directive.correlation.clone());
                        }
                        DirectiveAction::DiscardAsRecorded
                            if resumed.directives.result.as_ref().is_some_and(|result| {
                                result.action == DirectiveAction::DiscardAsRecorded
                            }) =>
                        {
                            state.acknowledge_terminal_result(&directive.correlation)?;
                        }
                        DirectiveAction::FailStale
                            if resumed.directives.result.as_ref().is_none_or(|result| {
                                result.action == DirectiveAction::FailStale
                            }) =>
                        {
                            state.discard_reconciled_lease(&directive.correlation)?;
                        }
                        _ => {
                            return Err(RunnerConnectionError::Violation(
                                ProtocolViolation::ResumeDirectives,
                            ));
                        }
                    }
                }
                workspaces::apply_workspace_directives(state, &resumed.directives)?;
                if let Some(directive) = &resumed.directives.leak_page {
                    match directive.action {
                        DirectiveAction::Resend => {}
                        DirectiveAction::DiscardAsRecorded => {
                            state.acknowledge_leak_page(&directive.correlation)?
                        }
                        _ => {
                            return Err(RunnerConnectionError::Violation(
                                ProtocolViolation::ResumeDirectives,
                            ));
                        }
                    }
                }
                let receipt = state.record_registration(resumed.registration_revision, digest)?;
                if let Some(correlation) = &resumed_lease {
                    send_message(
                        &mut io,
                        Message::LeaseClaim(LeaseClaim {
                            correlation: correlation.clone(),
                        }),
                    )
                    .await?;
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
            receive_buffer: Vec::new(),
            pending_offer: None,
            resumed_lease,
            execution: None,
            last_recorded: None,
            configuration: None,
            workspace: None,
            last_workspace_recorded: None,
            last_provision_failure: None,
            last_release_recorded: None,
            startup_report: leaks::StartupReport::default(),
            leak_sent: false,
            last_leak_recorded: None,
            deferred_dispatch: None,
            deferred_release: None,
            offer_claimed: false,
            receipt,
            advertisement: advertisement.clone(),
            outcome,
            connection_epoch,
            heartbeat: None,
        })
    }

    /// Supplies the local repository and sandbox configuration for workspace operations.
    pub fn with_configuration(mut self, configuration: crate::RunnerConfiguration) -> Self {
        self.configuration = Some(configuration);
        self.startup_report = leaks::StartupReport::Pending;
        self
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

    /// Serves liveness and the serial pure-tool lease machine.
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

    /// Drains admitted execution and its acknowledgement before local shutdown.
    pub async fn serve_until_shutdown<F>(
        &mut self,
        state: &mut RunnerStateRoot,
        shutdown: F,
    ) -> Result<ServeOutcome, RunnerConnectionError>
    where
        F: Future<Output = ()>,
    {
        tokio::pin!(shutdown);
        let mut shutdown_requested = false;
        self.ensure_workspace(state)?;
        loop {
            self.advance_local(state).await?;
            if shutdown_requested && !self.has_unsettled_lease(state) {
                return Ok(ServeOutcome::ShutdownReady);
            }
            let event = tokio::select! {
                event = self.receive_event() => event?,
                () = &mut shutdown, if !shutdown_requested => {
                    shutdown_requested = true;
                    continue;
                },
            };
            if let Some(end) = self.serve_event(state, event).await? {
                return Ok(ServeOutcome::ConnectionEnded(end));
            }
        }
    }

    fn has_unsettled_lease(&self, state: &RunnerStateRoot) -> bool {
        let inventory = state.reconnect_inventory();
        self.pending_offer.is_some()
            || self.execution.is_some()
            || inventory.lease.is_some()
            || inventory.result.is_some()
            || inventory.workspace_operation.is_some()
            || inventory.leak_page.is_some()
            || !self.startup_report.complete()
    }

    /// Handles one complete daemon frame or completed child result.
    pub async fn serve_one(
        &mut self,
        state: &mut RunnerStateRoot,
    ) -> Result<Option<ConnectionEnd>, RunnerConnectionError> {
        self.advance_local(state).await?;
        let event = self.receive_event().await?;
        self.serve_event(state, event).await
    }

    async fn receive_event(&mut self) -> Result<RunnerEvent, RunnerConnectionError> {
        tokio::select! {
            message = receive_message_buffered(&mut self.io, &mut self.receive_buffer) => message.map(RunnerEvent::Message),
            result = execution_finished(&mut self.execution) => result.map(RunnerEvent::Result),
            ready = workspaces::workspace_finished(&mut self.workspace) => ready.map(RunnerEvent::Workspace),
            facts = leaks::scan_finished(&mut self.startup_report) => facts.map(RunnerEvent::LeakScan),
        }
    }

    async fn serve_event(
        &mut self,
        state: &mut RunnerStateRoot,
        event: RunnerEvent,
    ) -> Result<Option<ConnectionEnd>, RunnerConnectionError> {
        match event {
            RunnerEvent::LeakScan(facts) => {
                let pages =
                    crate::workspace::leaks::pages(self.receipt.registration_revision(), &facts)
                        .map_err(|error| RunnerConnectionError::Workspace(error.into()))?;
                self.startup_report = leaks::StartupReport::Reporting(pages);
                Ok(None)
            }
            RunnerEvent::Message(message) => self.serve_message(state, message).await,
            RunnerEvent::Workspace(completion) => {
                self.finish_workspace(state, completion).await?;
                Ok(None)
            }
            RunnerEvent::Result(result) => {
                state.record_terminal_result(result)?;
                self.execution = None;
                self.send_retained_result(state).await?;
                Ok(None)
            }
        }
    }

    async fn serve_message(
        &mut self,
        state: &mut RunnerStateRoot,
        message: Message,
    ) -> Result<Option<ConnectionEnd>, RunnerConnectionError> {
        match message {
            Message::Enrolled(promoted) => {
                if promoted.connection_epoch != self.connection_epoch {
                    return Err(RunnerConnectionError::Violation(
                        ProtocolViolation::ConnectionCorrelationMismatch,
                    ));
                }
                let receipt = EnrollmentReceipt::new(
                    promoted.request_id,
                    promoted.enrollment_id,
                    promoted.runner_id,
                    promoted.authentication_id,
                    promoted.registration_revision,
                    promoted.advertisement_digest,
                    EnrollmentAuthority::Active,
                );
                state.record_promotion(receipt.clone())?;
                self.receipt = receipt;
                Ok(None)
            }
            Message::Heartbeat(challenge) => {
                let acknowledgement = self.heartbeat_acknowledgement(challenge, state)?;
                send_message(&mut self.io, Message::HeartbeatAck(acknowledgement)).await?;
                self.send_retained_result(state).await?;
                self.send_retained_workspace(state).await?;
                self.send_leak_page(state).await?;
                Ok(None)
            }
            Message::Shutdown(shutdown)
                if shutdown.reason == ShutdownReason::DaemonShutdown
                    && shutdown.connection_epoch == self.connection_epoch =>
            {
                Ok(Some(ConnectionEnd::DaemonShutdown {
                    connection_epoch: shutdown.connection_epoch,
                }))
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
                Ok(Some(ConnectionEnd::StaleConnectionRejected {
                    connection_epoch: shutdown.connection_epoch,
                }))
            }
            Message::Shutdown(_) => Err(RunnerConnectionError::Violation(
                ProtocolViolation::InvalidShutdownReason,
            )),
            Message::WorkspaceProvision(provision) => {
                self.validate_connection_correlation(
                    provision.correlation.runner_id,
                    provision.correlation.registration_revision,
                )?;
                if self.pending_offer.is_some() || self.execution.is_some() {
                    return Err(lease_mismatch());
                }
                self.check_workspace(provision.clone())?;
                state.record_provision(provision)?;
                self.ensure_workspace(state)?;
                self.send_retained_workspace(state).await?;
                Ok(None)
            }
            Message::WorkspaceLeakRecorded(recorded) => {
                self.record_leak_page(state, recorded)?;
                Ok(None)
            }
            Message::WorkspaceRecorded(recorded) => {
                self.record_workspace(state, recorded)?;
                Ok(None)
            }
            Message::WorkspaceRelease(release) => {
                if release.correlation.runner_id != self.receipt.runner_id() {
                    return Err(RunnerConnectionError::Violation(
                        ProtocolViolation::ConnectionCorrelationMismatch,
                    ));
                }
                if self.pending_offer.is_some()
                    || state.reconnect_inventory().lease.is_some()
                    || matches!(
                        self.startup_report,
                        leaks::StartupReport::Scanning(_) | leaks::StartupReport::Reporting(_)
                    )
                {
                    if self
                        .deferred_release
                        .as_ref()
                        .is_some_and(|prior| prior != &release)
                    {
                        return Err(RunnerStateError::InvalidTransition.into());
                    }
                    self.deferred_release = Some(release);
                    return Ok(None);
                }
                state.record_release(release.correlation)?;
                self.ensure_workspace(state)?;
                self.send_retained_workspace(state).await?;
                Ok(None)
            }
            Message::WorkspaceReleaseRecorded(recorded) => {
                self.acknowledge_release(state, recorded.correlation, false)?;
                Ok(None)
            }
            Message::OperationFailureRecorded(recorded) => {
                if matches!(
                    recorded.correlation,
                    signalbox_runner_wire::OperationCorrelation::Provision(_)
                ) {
                    if self.last_provision_failure.as_ref() == Some(&recorded.correlation)
                        && state.retained_provision_failure().is_none()
                    {
                        return Ok(None);
                    }
                    state.acknowledge_provision_failure(&recorded.correlation)?;
                    self.workspace = None;
                    self.last_provision_failure = Some(recorded.correlation);
                    return Ok(None);
                }
                let signalbox_runner_wire::OperationCorrelation::Release(correlation) =
                    recorded.correlation
                else {
                    return Err(RunnerStateError::InvalidTransition.into());
                };
                self.acknowledge_release(state, correlation, true)?;
                Ok(None)
            }
            Message::LeaseOffer(offer) => {
                self.validate_connection_correlation(
                    offer.correlation.runner_id,
                    offer.correlation.registration_revision,
                )?;
                if self.pending_offer.is_some()
                    || state.reconnect_inventory().lease.is_some()
                    || offer.correlation.tool_name.as_str() != signalbox_tools_basic::ECHO_NAME
                    || self
                        .advertisement
                        .tools
                        .binary_search(&offer.correlation.tool_name)
                        .is_err()
                    || offer.effect_class != EffectClass::Pure
                    || offer.correlation.sandbox_profile != SandboxProfile::Ambient
                    || !self
                        .advertisement
                        .sandbox_profiles
                        .contains(&SandboxProfile::Ambient)
                    || offer.credential_profile.as_ref().is_some_and(|profile| {
                        !self.advertisement.credential_profiles.contains(profile)
                    })
                {
                    return Err(lease_mismatch());
                }
                let arguments = signalbox_domain::NormalizedToolArguments::try_from_provider_text(
                    offer.normalized_arguments.to_string(),
                )
                .map_err(|_| lease_mismatch())?;
                signalbox_tools_basic::EchoExecutor::evaluate(&arguments)
                    .map_err(|_| lease_mismatch())?;
                let correlation = offer.correlation.clone();
                self.pending_offer = Some(offer);
                self.offer_claimed = self.startup_report.complete()
                    && state.reconnect_inventory().workspace_operation.is_none();
                if self.offer_claimed {
                    send_message(
                        &mut self.io,
                        Message::LeaseClaim(LeaseClaim { correlation }),
                    )
                    .await?;
                }
                Ok(None)
            }
            Message::LeaseClaimed(claimed) => {
                if self.resumed_lease.as_ref() == Some(&claimed.correlation) {
                    return Ok(None);
                }
                if !self
                    .pending_offer
                    .as_ref()
                    .is_some_and(|offer| offer.correlation == claimed.correlation)
                {
                    return Err(lease_mismatch());
                }
                state.record_lease_phase(LeasePhase {
                    correlation: claimed.correlation,
                    phase: LeasePhaseKind::WaitingDispatch,
                })?;
                Ok(None)
            }
            Message::Dispatch(dispatch) => {
                if !self.startup_report.complete() {
                    if self
                        .deferred_dispatch
                        .as_ref()
                        .is_some_and(|prior| prior != &dispatch)
                    {
                        return Err(lease_mismatch());
                    }
                    self.deferred_dispatch = Some(dispatch);
                    return Ok(None);
                }
                let inventory = state.reconnect_inventory();
                let resumed = self.resumed_lease.as_ref() == Some(&dispatch.correlation);
                if self.execution.is_some()
                    || !(resumed
                        || self.pending_offer.as_ref().is_some_and(|offer| {
                            offer.correlation == dispatch.correlation
                                && offer.normalized_arguments == dispatch.normalized_arguments
                        }))
                    || !inventory.lease.as_ref().is_some_and(|lease| {
                        lease.correlation == dispatch.correlation
                            && (lease.phase == LeasePhaseKind::WaitingDispatch
                                || (resumed && lease.phase == LeasePhaseKind::DispatchReceived))
                    })
                    || inventory.result.is_some()
                {
                    return Err(lease_mismatch());
                }
                state.record_lease_phase(LeasePhase {
                    correlation: dispatch.correlation.clone(),
                    phase: LeasePhaseKind::DispatchReceived,
                })?;
                state.record_lease_phase(LeasePhase {
                    correlation: dispatch.correlation.clone(),
                    phase: LeasePhaseKind::ExecutionMayHaveStarted,
                })?;
                self.execution = Some(RunnerExecution {
                    correlation: dispatch.correlation.clone(),
                    task: tokio::spawn(crate::executor::execute(dispatch)),
                });
                self.resumed_lease = None;
                Ok(None)
            }
            Message::ResultRecorded(recorded) => {
                if self.last_recorded.as_ref() == Some(&recorded.correlation) {
                    return Ok(None);
                }
                state.acknowledge_terminal_result(&recorded.correlation)?;
                self.last_recorded = Some(recorded.correlation);
                self.pending_offer = None;
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

    async fn send_retained_result(
        &mut self,
        state: &RunnerStateRoot,
    ) -> Result<(), RunnerConnectionError> {
        if let Some(result) = state.reconnect_inventory().result {
            send_message(
                &mut self.io,
                Message::Result(ResultFrame {
                    correlation: result.correlation,
                    result: result.result,
                }),
            )
            .await?;
        }
        Ok(())
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
        let acknowledgement = HeartbeatAck {
            challenge_sequence: challenge.sequence,
            runner_sequence: PositiveU64::try_new(runner_sequence)
                .map_err(RunnerConnectionError::InvalidLocalFrame)?,
            lease_phase: state.reconnect_inventory().lease,
            workspace_phase: workspaces::heartbeat_phase(state),
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
    receive_message_buffered(io, &mut Vec::new()).await
}

async fn receive_message_buffered<S>(
    io: &mut BufReader<S>,
    line: &mut Vec<u8>,
) -> Result<Message, RunnerConnectionError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let remaining = (MAX_FRAME_BYTES + 1).saturating_sub(line.len());
    let mut bounded = io.take(remaining as u64);
    let bytes = bounded
        .read_until(b'\n', line)
        .await
        .map_err(RunnerConnectionError::Read)?;
    if bytes == 0 {
        return Err(RunnerConnectionError::PeerClosed);
    }
    let decoded = decode_line(line)
        .map(|frame| frame.message)
        .map_err(RunnerConnectionError::Decode);
    line.clear();
    decoded
}

#[cfg(test)]
mod lease_tests;

#[cfg(test)]
mod tests {
    use signalbox_runner_wire::ReconnectInventory;
    use tempfile::TempDir;
    use tokio::io::DuplexStream;
    use uuid::Uuid;

    use signalbox_runner_wire::{
        Enrolled, ReconnectDirectives, Resumed, Shutdown, WorkspaceProvision,
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
            default_working_directory: None,
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
    async fn workspace_provision_requires_local_configuration_before_acceptance() {
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
                    recovery: None,
                }),
            )
            .await;
        };
        let (error, ()) = tokio::join!(runner, hub);

        assert!(matches!(
            error,
            RunnerConnectionError::Workspace(crate::WorkspaceProvisionError::RepositoryUnavailable)
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
            Some(ConnectionEnd::DaemonShutdown {
                connection_epoch: positive(CONNECTION_EPOCH),
            })
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
            ConnectionEnd::StaleConnectionRejected {
                connection_epoch: stale_epoch,
            }
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

#[cfg(test)]
mod workspace_tests;
