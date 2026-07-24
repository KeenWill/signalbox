use std::{error::Error, fmt, io};

use signalbox_process_protocol::{
    ErrorCode, ErrorDetail, FrameDecodeError, FrameEncodeError, RejectionDetail,
};

#[derive(Debug)]
pub(crate) enum ClientError {
    Io(io::Error),
    Encode(FrameEncodeError),
    Decode(FrameDecodeError),
    Protocol(&'static str),
    Remote {
        code: ErrorCode,
        message: String,
        detail: ErrorDetail,
    },
    AmbiguousMutation,
    Input(&'static str),
    TurnRecoveryRequired,
    TurnFailed,
    TurnRefused,
    TurnCancelled,
    TurnReconciliationRequired,
}

impl ClientError {
    pub(crate) const fn remote(code: ErrorCode, message: String, detail: ErrorDetail) -> Self {
        Self::Remote {
            code,
            message,
            detail,
        }
    }

    pub(crate) fn mutation(self) -> Self {
        match self {
            Self::Remote { .. } => self,
            Self::Io(_)
            | Self::Encode(_)
            | Self::Decode(_)
            | Self::Protocol(_)
            | Self::AmbiguousMutation
            | Self::Input(_)
            | Self::TurnRecoveryRequired
            | Self::TurnFailed
            | Self::TurnRefused
            | Self::TurnCancelled
            | Self::TurnReconciliationRequired => Self::AmbiguousMutation,
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => formatter.write_str("local process communication failed"),
            Self::Encode(_) => formatter.write_str("the client could not encode its request"),
            Self::Decode(_) => {
                formatter.write_str("the server violated the version-one process protocol")
            }
            Self::Protocol(message) => write!(
                formatter,
                "the server violated the version-one process protocol: {message}"
            ),
            Self::Remote {
                code,
                message,
                detail,
            } => {
                write!(formatter, "{}: {message}", error_code_name(*code))?;
                if let Some(detail) = detail.value() {
                    write!(formatter, " ({})", RejectionDisplay(detail))?;
                }
                Ok(())
            }
            Self::AmbiguousMutation => formatter.write_str(
                "the mutation outcome may be ambiguous; retry the exact printed command",
            ),
            Self::Input(message) => formatter.write_str(message),
            Self::TurnRecoveryRequired => formatter.write_str(
                "the submitted turn requires model-call recovery that version one cannot perform",
            ),
            Self::TurnFailed => formatter.write_str("the submitted turn failed"),
            Self::TurnRefused => formatter.write_str("the submitted turn was refused"),
            Self::TurnCancelled => formatter.write_str("the submitted turn was cancelled"),
            Self::TurnReconciliationRequired => {
                formatter.write_str("the submitted turn requires external reconciliation")
            }
        }
    }
}

impl Error for ClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Encode(error) => Some(error),
            Self::Decode(error) => Some(error),
            Self::Protocol(_)
            | Self::Remote { .. }
            | Self::AmbiguousMutation
            | Self::Input(_)
            | Self::TurnRecoveryRequired
            | Self::TurnFailed
            | Self::TurnRefused
            | Self::TurnCancelled
            | Self::TurnReconciliationRequired => None,
        }
    }
}

impl From<io::Error> for ClientError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<FrameEncodeError> for ClientError {
    fn from(error: FrameEncodeError) -> Self {
        Self::Encode(error)
    }
}

impl From<FrameDecodeError> for ClientError {
    fn from(error: FrameDecodeError) -> Self {
        Self::Decode(error)
    }
}

const fn error_code_name(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::MalformedFrame => "malformed_frame",
        ErrorCode::UnsupportedVersion => "unsupported_version",
        ErrorCode::InvalidRequest => "invalid_request",
        ErrorCode::NotFound => "not_found",
        ErrorCode::ConflictingReuse => "conflicting_reuse",
        ErrorCode::Rejected => "rejected",
        ErrorCode::ResyncRequired => "resync_required",
        ErrorCode::Unavailable => "unavailable",
        ErrorCode::Internal => "internal",
    }
}

struct RejectionDisplay(RejectionDetail);

impl fmt::Display for RejectionDisplay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            RejectionDetail::SessionNotFound { session_id } => {
                write!(formatter, "session_not_found session={session_id}")
            }
            RejectionDetail::ActiveTurnPresent {
                session_id,
                active_turn_id,
            } => write!(
                formatter,
                "active_turn_present session={session_id} active_turn={active_turn_id}"
            ),
            RejectionDetail::DefaultsVersionMismatch {
                session_id,
                expected,
                current,
            } => write!(
                formatter,
                "defaults_version_mismatch session={session_id} expected={} current={}",
                expected.value(),
                current.value()
            ),
            RejectionDetail::UnknownModelAlias {
                session_id,
                alias_id,
            } => write!(
                formatter,
                "unknown_model_alias session={session_id} alias={alias_id}"
            ),
            RejectionDetail::AcceptancePositionExhausted { session_id, last } => write!(
                formatter,
                "acceptance_position_exhausted session={session_id} last={}",
                last.value()
            ),
        }
    }
}
