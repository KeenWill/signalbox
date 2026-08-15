//! Closed version-two JSON-lines runner wire.
//!
//! This crate owns transport representations, frame validation, and canonical
//! digest bytes. Domain, persistence, and orchestration representations remain
//! distinct explicit mappings (`docs/spec/runner-protocol.md`).

mod digest;
mod frame;
mod value;

use std::{error::Error, fmt};

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

pub use digest::*;
pub use frame::*;
pub use value::*;

/// Maximum complete encoded frame bytes, including the final newline.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// A complete validated version-two runner frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    /// Closed message and exact payload.
    pub message: Message,
}

impl Frame {
    /// Validates and constructs one frame.
    pub fn try_new(message: Message) -> Result<Self, FrameError> {
        message.validate().map_err(FrameError::InvalidValue)?;
        Ok(Self { message })
    }
}

/// Fail-closed line framing or message validation error.
#[derive(Debug)]
pub enum FrameError {
    /// A decoder received bytes without exactly one final newline boundary.
    MissingNewline,
    /// The complete line exceeded 8 MiB.
    TooLarge {
        /// Observed complete line bytes.
        bytes: usize,
    },
    /// JSON shape, field, token, or UTF-8 decoding failed.
    MalformedJson(serde_json::Error),
    /// The required version was not version two.
    UnsupportedVersion(u64),
    /// A decoded or constructed payload violated a cross-member invariant.
    InvalidValue(ValueError),
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingNewline => formatter.write_str("runner frame requires one final newline"),
            Self::TooLarge { bytes } => write!(
                formatter,
                "runner frame is {bytes} bytes; maximum is {MAX_FRAME_BYTES}"
            ),
            Self::MalformedJson(error) => {
                write!(formatter, "runner frame JSON is malformed: {error}")
            }
            Self::UnsupportedVersion(version) => {
                write!(formatter, "runner frame version {version} is unsupported")
            }
            Self::InvalidValue(error) => {
                write!(formatter, "runner frame payload is invalid: {error}")
            }
        }
    }
}

impl Error for FrameError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MalformedJson(error) => Some(error),
            Self::InvalidValue(error) => Some(error),
            Self::MissingNewline | Self::TooLarge { .. } | Self::UnsupportedVersion(_) => None,
        }
    }
}

impl From<serde_json::Error> for FrameError {
    fn from(value: serde_json::Error) -> Self {
        Self::MalformedJson(value)
    }
}

#[derive(Serialize)]
struct EncodedFrame<'a> {
    version: u64,
    #[serde(flatten)]
    message: &'a Message,
}

/// Encodes a validated frame and final newline, enforcing the 8 MiB send bound.
pub fn encode_line(frame: &Frame) -> Result<Vec<u8>, FrameError> {
    frame.message.validate().map_err(FrameError::InvalidValue)?;
    let mut encoded = serde_json::to_vec(&EncodedFrame {
        version: PROTOCOL_VERSION,
        message: &frame.message,
    })?;
    encoded.push(b'\n');
    if encoded.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge {
            bytes: encoded.len(),
        });
    }
    Ok(encoded)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodedEnvelope<'a> {
    version: u64,
    kind: String,
    #[serde(borrow)]
    payload: &'a RawValue,
}

#[derive(Deserialize)]
struct RawOperationFailed<'a> {
    #[serde(borrow)]
    failure: RawOperationFailure<'a>,
}

#[derive(Deserialize)]
struct RawOperationFailure<'a> {
    #[serde(borrow)]
    detail: &'a RawValue,
}

#[derive(Deserialize)]
struct RawFailureDetail<'a> {
    #[serde(borrow)]
    payload: &'a RawValue,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FrameKind {
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

/// Decodes exactly one newline-terminated frame, enforcing the 8 MiB receive bound first.
pub fn decode_line(line: &[u8]) -> Result<Frame, FrameError> {
    if line.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge { bytes: line.len() });
    }
    if !line.ends_with(b"\n") || line[..line.len().saturating_sub(1)].contains(&b'\n') {
        return Err(FrameError::MissingNewline);
    }
    let content = &line[..line.len() - 1];
    let envelope: DecodedEnvelope<'_> = serde_json::from_slice(content)?;
    if envelope.version != PROTOCOL_VERSION {
        return Err(FrameError::UnsupportedVersion(envelope.version));
    }
    let kind = serde_json::from_value::<FrameKind>(serde_json::Value::String(envelope.kind))?;
    let message = decode_payload(kind, envelope.payload)?;
    Frame::try_new(message)
}

fn decode_payload(kind: FrameKind, payload: &RawValue) -> Result<Message, FrameError> {
    macro_rules! parse {
        ($variant:ident, $payload:ty) => {
            Message::$variant(serde_json::from_str::<$payload>(payload.get())?)
        };
    }
    Ok(match kind {
        FrameKind::Enroll => parse!(Enroll, Enroll),
        FrameKind::Enrolled => parse!(Enrolled, Enrolled),
        FrameKind::Resume => parse!(Resume, Box<Resume>),
        FrameKind::Resumed => parse!(Resumed, Box<Resumed>),
        FrameKind::ReplacementPending => parse!(ReplacementPending, ReplacementPending),
        FrameKind::Advertise => parse!(Advertise, Advertise),
        FrameKind::Registered => parse!(Registered, Registered),
        FrameKind::Heartbeat => parse!(Heartbeat, Heartbeat),
        FrameKind::HeartbeatAck => parse!(HeartbeatAck, HeartbeatAck),
        FrameKind::WorkspaceLeakPage => parse!(WorkspaceLeakPage, WorkspaceLeakPage),
        FrameKind::WorkspaceLeakRecorded => parse!(WorkspaceLeakRecorded, WorkspaceLeakRecorded),
        FrameKind::WorkspaceProvision => parse!(WorkspaceProvision, WorkspaceProvision),
        FrameKind::WorkspaceReady => parse!(WorkspaceReady, WorkspaceReady),
        FrameKind::WorkspaceRecorded => parse!(WorkspaceRecorded, WorkspaceRecorded),
        FrameKind::WorkspaceRelease => parse!(WorkspaceRelease, WorkspaceRelease),
        FrameKind::WorkspaceReleased => parse!(WorkspaceReleased, WorkspaceReleased),
        FrameKind::WorkspaceReleaseRecorded => {
            parse!(WorkspaceReleaseRecorded, WorkspaceReleaseRecorded)
        }
        FrameKind::LeaseOffer => parse!(LeaseOffer, LeaseOffer),
        FrameKind::LeaseClaim => parse!(LeaseClaim, LeaseClaim),
        FrameKind::LeaseClaimed => parse!(LeaseClaimed, LeaseClaimed),
        FrameKind::Dispatch => parse!(Dispatch, Dispatch),
        FrameKind::Result => parse!(Result, ResultFrame),
        FrameKind::ResultRecorded => parse!(ResultRecorded, ResultRecorded),
        FrameKind::OperationFailed => {
            let raw = serde_json::from_str::<RawOperationFailed<'_>>(payload.get())?;
            if raw.failure.detail.get().len() > MAX_FAILURE_DETAIL_BYTES {
                return Err(FrameError::InvalidValue(ValueError::FailureDetail));
            }
            let detail = serde_json::from_str::<RawFailureDetail<'_>>(raw.failure.detail.get())?;
            if detail.payload.get().len() > MAX_FAILURE_PAYLOAD_BYTES {
                return Err(FrameError::InvalidValue(ValueError::FailureDetail));
            }
            parse!(OperationFailed, OperationFailed)
        }
        FrameKind::OperationFailureRecorded => {
            parse!(OperationFailureRecorded, OperationFailureRecorded)
        }
        FrameKind::Shutdown => parse!(Shutdown, Shutdown),
        FrameKind::Rejected => parse!(Rejected, Rejected),
    })
}

#[cfg(test)]
mod tests;
