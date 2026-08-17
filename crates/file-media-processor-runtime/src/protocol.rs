use std::{num::NonZeroU64, str::FromStr};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use signalbox_file_media_runtime::{
    AttachmentKind, BoundedMetadata, CanonicalMediaType, DeclaredMediaType, DisplayFilename,
    FileDigest, FileMediaProviderReadRequest, FileMediaProviderValidationRequest, FileReaderName,
    FileReaderProviderName, FileReaderRevision, FileUse, ProbeDeclaration, ProcessorProbeOutput,
    ProcessorReadOutput, ProcessorValidationOutput, ReadAccessPattern, ReadViewDeclaration,
    ReadViewName, ReaderIdentity, RegistryValueError, ValidationEvidence,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Invocation {
    Probe {
        reader: WireReaderIdentity,
        source: WireSource,
        envelope: WireReadEnvelope,
    },
    Validate {
        reader: WireReaderIdentity,
        source: WireSource,
        envelope: WireReadEnvelope,
        request: WireValidationRequest,
    },
    Read {
        reader: WireReaderIdentity,
        source: WireSource,
        envelope: WireReadEnvelope,
        request: WireReadRequest,
    },
}

impl Invocation {
    pub(crate) const fn source(&self) -> &WireSource {
        match self {
            Self::Probe { source, .. }
            | Self::Validate { source, .. }
            | Self::Read { source, .. } => source,
        }
    }

    pub(crate) const fn envelope(&self) -> WireReadEnvelope {
        match self {
            Self::Probe { envelope, .. }
            | Self::Validate { envelope, .. }
            | Self::Read { envelope, .. } => *envelope,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "message", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum WorkerFrame {
    ReadRange { offset: u64, length: u64 },
    ProbeResult { output: ProcessorProbeOutput },
    ValidationResult { output: ProcessorValidationOutput },
    ReadResult { output: ProcessorReadOutput },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "message", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum DaemonFrame {
    Invocation { invocation: Box<Invocation> },
    RangeBytes { bytes_base64: String },
    RangeFailure,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireSource {
    digest: [u8; 32],
    byte_length: u64,
}

impl WireSource {
    pub(crate) fn from_source(
        source: &dyn signalbox_file_media_runtime::VerifiedBlobSource,
    ) -> Self {
        Self {
            digest: *source.digest().as_bytes(),
            byte_length: source.byte_length().get(),
        }
    }

    pub(crate) const fn digest(self) -> FileDigest {
        FileDigest::from_bytes(self.digest)
    }

    pub(crate) fn byte_length(self) -> Result<NonZeroU64, ProtocolValueError> {
        NonZeroU64::new(self.byte_length).ok_or(ProtocolValueError)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(tag = "access", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum WireReadEnvelope {
    Probe {
        prefix_bytes: u64,
        suffix_bytes: u64,
        ranges: u32,
        cumulative_bytes: u64,
    },
    Streaming {
        cumulative_bytes: u64,
    },
    RandomAccess {
        ranges: u32,
        cumulative_bytes: u64,
    },
}

impl WireReadEnvelope {
    pub(crate) const fn for_probe(probe: ProbeDeclaration) -> Self {
        Self::Probe {
            prefix_bytes: probe.prefix_bytes(),
            suffix_bytes: probe.suffix_bytes(),
            ranges: probe.range_count(),
            cumulative_bytes: probe.cumulative_bytes(),
        }
    }

    pub(crate) const fn for_view(view: &ReadViewDeclaration) -> Self {
        match view.access() {
            ReadAccessPattern::Streaming => Self::Streaming {
                cumulative_bytes: view.bounds().source_bytes(),
            },
            ReadAccessPattern::RandomAccess { maximum_ranges } => Self::RandomAccess {
                ranges: maximum_ranges,
                cumulative_bytes: view.bounds().source_bytes(),
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireReaderIdentity {
    provider: String,
    reader: String,
    revision: String,
}

impl From<&ReaderIdentity> for WireReaderIdentity {
    fn from(identity: &ReaderIdentity) -> Self {
        Self {
            provider: identity.provider().as_str().to_owned(),
            reader: identity.reader().as_str().to_owned(),
            revision: identity.revision().as_str().to_owned(),
        }
    }
}

impl TryFrom<WireReaderIdentity> for ReaderIdentity {
    type Error = ProtocolValueError;

    fn try_from(value: WireReaderIdentity) -> Result<Self, Self::Error> {
        Ok(Self::new(
            FileReaderProviderName::try_new(value.provider).map_err(map_value_error)?,
            FileReaderName::try_new(value.reader).map_err(map_value_error)?,
            FileReaderRevision::try_new(value.revision).map_err(map_value_error)?,
        ))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireFileUse {
    digest: [u8; 32],
    byte_length: u64,
    attachment_kind: WireAttachmentKind,
    declared_media_type: String,
    display_filename: Option<String>,
}

impl From<&FileUse> for WireFileUse {
    fn from(source: &FileUse) -> Self {
        Self {
            digest: *source.digest().as_bytes(),
            byte_length: source.byte_length().get(),
            attachment_kind: source.attachment_kind().into(),
            declared_media_type: source.declared_media_type().as_str().to_owned(),
            display_filename: source
                .display_filename()
                .map(|name| name.as_str().to_owned()),
        }
    }
}

impl TryFrom<WireFileUse> for FileUse {
    type Error = ProtocolValueError;

    fn try_from(value: WireFileUse) -> Result<Self, Self::Error> {
        Ok(Self::new(
            FileDigest::from_bytes(value.digest),
            NonZeroU64::new(value.byte_length).ok_or(ProtocolValueError)?,
            value.attachment_kind.into(),
            DeclaredMediaType::try_new(value.declared_media_type).map_err(map_value_error)?,
            value
                .display_filename
                .map(DisplayFilename::try_new)
                .transpose()
                .map_err(map_value_error)?,
        ))
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireAttachmentKind {
    Image,
    Audio,
    Document,
    File,
}

impl From<AttachmentKind> for WireAttachmentKind {
    fn from(value: AttachmentKind) -> Self {
        match value {
            AttachmentKind::Image => Self::Image,
            AttachmentKind::Audio => Self::Audio,
            AttachmentKind::Document => Self::Document,
            AttachmentKind::File => Self::File,
        }
    }
}

impl From<WireAttachmentKind> for AttachmentKind {
    fn from(value: WireAttachmentKind) -> Self {
        match value {
            WireAttachmentKind::Image => Self::Image,
            WireAttachmentKind::Audio => Self::Audio,
            WireAttachmentKind::Document => Self::Document,
            WireAttachmentKind::File => Self::File,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireValidationRequest {
    source: WireFileUse,
    media_type: String,
    evidence: ValidationEvidence,
}

impl From<&FileMediaProviderValidationRequest> for WireValidationRequest {
    fn from(request: &FileMediaProviderValidationRequest) -> Self {
        Self {
            source: (&request.source).into(),
            media_type: request.media_type.as_str().to_owned(),
            evidence: request.evidence,
        }
    }
}

impl TryFrom<WireValidationRequest> for FileMediaProviderValidationRequest {
    type Error = ProtocolValueError;

    fn try_from(value: WireValidationRequest) -> Result<Self, Self::Error> {
        Ok(Self {
            source: value.source.try_into()?,
            media_type: CanonicalMediaType::from_str(&value.media_type)
                .map_err(|_| ProtocolValueError)?,
            evidence: value.evidence,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireReadRequest {
    source: WireFileUse,
    detected_media_type: String,
    validation: ValidationEvidence,
    metadata_json: String,
    view: String,
    options: serde_json::Value,
}

impl From<&FileMediaProviderReadRequest> for WireReadRequest {
    fn from(request: &FileMediaProviderReadRequest) -> Self {
        Self {
            source: (&request.source).into(),
            detected_media_type: request.detected_media_type.as_str().to_owned(),
            validation: request.validation,
            metadata_json: request.metadata.as_str().to_owned(),
            view: request.view.as_str().to_owned(),
            options: request.options.clone(),
        }
    }
}

impl TryFrom<WireReadRequest> for FileMediaProviderReadRequest {
    type Error = ProtocolValueError;

    fn try_from(value: WireReadRequest) -> Result<Self, Self::Error> {
        Ok(Self {
            source: value.source.try_into()?,
            detected_media_type: CanonicalMediaType::from_str(&value.detected_media_type)
                .map_err(|_| ProtocolValueError)?,
            validation: value.validation,
            metadata: BoundedMetadata::try_new(&value.metadata_json).map_err(map_value_error)?,
            view: ReadViewName::try_new(value.view).map_err(map_value_error)?,
            options: value.options,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProtocolValueError;

fn map_value_error(_: RegistryValueError) -> ProtocolValueError {
    ProtocolValueError
}

pub(crate) fn encode_bytes(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

pub(crate) fn decode_bytes(encoded: &str) -> Result<Vec<u8>, ProtocolValueError> {
    STANDARD.decode(encoded).map_err(|_| ProtocolValueError)
}
