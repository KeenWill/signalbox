//! Frame wire representations and validation.

use crate::error::{
    ErrorCode, RejectionDetail, validate_blob_read_detail, validate_blob_upload_detail,
    validate_conversation_import_detail, validate_rejection_detail,
};
use crate::request::ClientRequest;
use crate::response::ServerMessage;
use crate::scalars::{
    FrameDecodeError, FrameEncodeError, FrameValidationError, MAX_FRAME_BYTES, ProtocolVersion,
    RequestId, SystemPromptMember, checked_line_content, probe_header,
};
use serde::{Deserialize, Deserializer, Serialize};

#[derive(signalbox_derive::Accessors)]
/// One validated client frame.

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientFrame {
    version: ProtocolVersion,
    request_id: RequestId,
    /// Borrows the closed request.
    #[get]
    request: ClientRequest,
}

impl ClientFrame {
    /// Constructs a single-version frame with a correlated request identity.
    pub fn try_new(
        request_id: RequestId,
        request: ClientRequest,
    ) -> Result<Self, FrameValidationError> {
        Self::try_new_for_version(ProtocolVersion::One, request_id, request)
    }

    /// Constructs a frame in one admitted protocol version.
    pub fn try_new_for_version(
        version: ProtocolVersion,
        request_id: RequestId,
        request: ClientRequest,
    ) -> Result<Self, FrameValidationError> {
        let frame = Self {
            version,
            request_id,
            request,
        };
        frame.validate()?;
        Ok(frame)
    }

    /// Returns the admitted protocol version.
    pub const fn version(&self) -> ProtocolVersion {
        self.version
    }

    /// Returns the correlation identity.
    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }

    /// Transfers the admitted version, correlation identity, and closed
    /// request out of the frame.
    pub fn into_parts(self) -> (ProtocolVersion, RequestId, ClientRequest) {
        (self.version, self.request_id, self.request)
    }

    fn validate(&self) -> Result<(), FrameValidationError> {
        if !self.request_id.is_correlated() {
            return Err(FrameValidationError::UncorrelatedClientRequest);
        }
        if let ClientRequest::CreateSession { system_prompt, .. }
        | ClientRequest::ReplaceSessionDefaults { system_prompt, .. } = &self.request
        {
            validate_system_prompt_member(system_prompt)?;
        }
        self.request.validate()
    }
}

/// Requires the presence-checked system-prompt member.
fn validate_system_prompt_member(member: &SystemPromptMember) -> Result<(), FrameValidationError> {
    if member.is_absent() {
        return Err(FrameValidationError::SystemPromptShape);
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawClientFrame {
    version: ProtocolVersion,
    request_id: RequestId,
    request: ClientRequest,
}

impl<'de> Deserialize<'de> for ClientFrame {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let raw = RawClientFrame::deserialize(deserializer)?;
        let frame = Self {
            version: raw.version,
            request_id: raw.request_id,
            request: raw.request,
        };
        frame.validate().map_err(serde::de::Error::custom)?;
        Ok(frame)
    }
}

#[derive(signalbox_derive::Accessors)]
/// One validated server frame.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerFrame {
    pub(crate) version: ProtocolVersion,
    pub(crate) request_id: RequestId,
    /// Borrows the closed server message.
    #[get]
    pub(crate) message: ServerMessage,
}

impl ServerFrame {
    /// Constructs a single-version response frame.
    pub fn try_new(
        request_id: RequestId,
        message: ServerMessage,
    ) -> Result<Self, FrameValidationError> {
        Self::try_new_for_version(ProtocolVersion::One, request_id, message)
    }

    /// Constructs one response in an admitted protocol version.
    pub fn try_new_for_version(
        version: ProtocolVersion,
        request_id: RequestId,
        message: ServerMessage,
    ) -> Result<Self, FrameValidationError> {
        let frame = Self {
            version,
            request_id,
            message,
        };
        frame.validate()?;
        Ok(frame)
    }

    /// Returns the admitted protocol version.
    pub const fn version(&self) -> ProtocolVersion {
        self.version
    }

    /// Returns the request correlation identity.
    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }

    fn validate(&self) -> Result<(), FrameValidationError> {
        if let ServerMessage::TranscriptTurn { state, .. } = &self.message {
            state.validate()?;
        }
        if let ServerMessage::SessionDefaultsReplaced { system_prompt, .. } = &self.message {
            validate_system_prompt_member(system_prompt)?;
        }
        self.message.validate()?;
        match &self.message {
            ServerMessage::Error { code, detail, .. } => {
                if !self.request_id.is_correlated()
                    && !matches!(
                        code,
                        ErrorCode::MalformedFrame | ErrorCode::UnsupportedVersion
                    )
                {
                    return Err(FrameValidationError::UncorrelatedApplicationError);
                }
                if let Some(RejectionDetail::ImportedFrontierPositionOutOfRange {
                    requested_position,
                    last_position,
                    ..
                }) = detail.value()
                {
                    // An imported conversation's positions are the contiguous
                    // sequence `1..=last_position`, so a nonpositive bound or a
                    // requested ordinal inside that range contradicts the
                    // rejection the detail states.
                    if last_position.value() == 0
                        || requested_position.value() <= last_position.value()
                    {
                        return Err(FrameValidationError::ImportedFrontierRangeShape);
                    }
                }
                if let Some(detail) = detail.value() {
                    if detail.is_bulk_ingest() {
                        if *code != ErrorCode::InvalidRequest {
                            return Err(FrameValidationError::ErrorDetailShape);
                        }
                    } else if detail.is_conversation_import() {
                        if *code != ErrorCode::InvalidRequest {
                            return Err(FrameValidationError::ErrorDetailShape);
                        }
                        validate_conversation_import_detail(detail)?;
                    } else if detail.is_blob_upload() {
                        if *code != ErrorCode::InvalidRequest {
                            return Err(FrameValidationError::ErrorDetailShape);
                        }
                        validate_blob_upload_detail(detail)?;
                    } else if detail.is_blob_read() {
                        if *code != ErrorCode::InvalidRequest {
                            return Err(FrameValidationError::ErrorDetailShape);
                        }
                        validate_blob_read_detail(detail)?;
                    } else if *code != ErrorCode::Rejected {
                        return Err(FrameValidationError::ErrorDetailShape);
                    } else {
                        validate_rejection_detail(detail)?;
                    }
                } else if *code == ErrorCode::Rejected {
                    return Err(FrameValidationError::ErrorDetailShape);
                }
            }
            _ if !self.request_id.is_correlated() => {
                return Err(FrameValidationError::UncorrelatedSuccess);
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServerFrame {
    version: ProtocolVersion,
    request_id: RequestId,
    message: ServerMessage,
}

impl<'de> Deserialize<'de> for ServerFrame {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        let raw = RawServerFrame::deserialize(deserializer)?;
        let frame = Self {
            version: raw.version,
            request_id: raw.request_id,
            message: raw.message,
        };
        frame.validate().map_err(serde::de::Error::custom)?;
        Ok(frame)
    }
}

/// Decodes and validates one complete client line including its final newline.
pub fn decode_client_line(line: &[u8]) -> Result<ClientFrame, FrameDecodeError> {
    let content = checked_line_content(line, false)?;
    let header = probe_header(content, "request", false)?;
    let frame: ClientFrame = serde_json::from_slice(content)
        .map_err(|_| FrameDecodeError::malformed(header.request_id))?;
    frame
        .validate()
        .map_err(|_| FrameDecodeError::malformed(header.request_id))?;
    Ok(frame)
}

/// Decodes and validates one complete server line including its final newline.
pub fn decode_server_line(line: &[u8]) -> Result<ServerFrame, FrameDecodeError> {
    let content = checked_line_content(line, true)?;
    let header = probe_header(content, "message", true)?;
    let frame: ServerFrame = serde_json::from_slice(content)
        .map_err(|_| FrameDecodeError::malformed(header.request_id))?;
    frame
        .validate()
        .map_err(|_| FrameDecodeError::malformed(header.request_id))?;
    Ok(frame)
}

/// Encodes one validated client frame with its final newline.
pub fn encode_client_line(frame: &ClientFrame) -> Result<Vec<u8>, FrameEncodeError> {
    frame.validate()?;
    encode_line(frame)
}

/// Encodes one validated server frame with its final newline.
pub fn encode_server_line(frame: &ServerFrame) -> Result<Vec<u8>, FrameEncodeError> {
    frame.validate()?;
    encode_line(frame)
}

fn encode_line<T: Serialize>(frame: &T) -> Result<Vec<u8>, FrameEncodeError> {
    let mut encoded = serde_json::to_vec(frame)?;
    encoded.push(b'\n');
    if encoded.len() > MAX_FRAME_BYTES {
        return Err(FrameEncodeError::OversizedFrame);
    }
    Ok(encoded)
}
