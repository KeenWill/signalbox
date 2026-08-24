use std::{borrow::Borrow, num::NonZeroU64, str::FromStr};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use signalbox_file_media_runtime::{
    AttachmentKind, BoundedMetadata, CanonicalMediaType, DeclaredMediaType, DisplayFilename,
    FileDigest, FileMediaProviderDeclaration, FileMediaProviderReadRequest,
    FileMediaProviderValidationRequest, FileReaderName, FileReaderProviderName, FileReaderRevision,
    FileUse, ProbeDeclaration, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, ReadAccessPattern, ReadContinuationCursor, ReadOutputKind,
    ReadViewBounds, ReadViewDeclaration, ReadViewName, ReaderIdentity, RegistryValueError,
    StreamingTextFallback, ValidationEvidence,
};

pub(crate) fn declaration_fingerprint(declarations: &[FileMediaProviderDeclaration]) -> [u8; 32] {
    let mut declarations = declarations.iter().collect::<Vec<_>>();
    declarations.sort_by(|left, right| left.provider().cmp(right.provider()));
    declaration_fingerprint_ordered(declarations.len(), declarations)
}

pub(crate) fn declaration_fingerprint_ordered<I, D>(
    declaration_count: usize,
    declarations: I,
) -> [u8; 32]
where
    I: IntoIterator<Item = D>,
    D: Borrow<FileMediaProviderDeclaration>,
{
    let mut fingerprint = Sha256::new();
    fingerprint_field(&mut fingerprint, b"signalbox-file-media-catalog-v1");
    fingerprint_len(&mut fingerprint, declaration_count);
    for declaration in declarations {
        let declaration = declaration.borrow();
        fingerprint_field(&mut fingerprint, declaration.provider().as_str().as_bytes());
        let mut readers = declaration.readers().iter().collect::<Vec<_>>();
        readers.sort_by(|left, right| left.identity().cmp(right.identity()));
        fingerprint_len(&mut fingerprint, readers.len());
        for reader in readers {
            fingerprint_field(
                &mut fingerprint,
                reader.identity().provider().as_str().as_bytes(),
            );
            fingerprint_field(
                &mut fingerprint,
                reader.identity().reader().as_str().as_bytes(),
            );
            fingerprint_field(
                &mut fingerprint,
                reader.identity().revision().as_str().as_bytes(),
            );
            fingerprint_len(&mut fingerprint, reader.media_types().len());
            for media_type in reader.media_types() {
                fingerprint_field(&mut fingerprint, media_type.as_str().as_bytes());
            }
            let probe = reader.probe();
            fingerprint_u64(&mut fingerprint, probe.prefix_bytes());
            fingerprint_u64(&mut fingerprint, probe.suffix_bytes());
            fingerprint_u64(&mut fingerprint, u64::from(probe.range_count()));
            fingerprint_u64(&mut fingerprint, probe.cumulative_bytes());
            fingerprint_len(&mut fingerprint, reader.views().len());
            for view in reader.views() {
                fingerprint_field(&mut fingerprint, view.name().as_str().as_bytes());
                fingerprint_field(&mut fingerprint, view.description().as_bytes());
                fingerprint_field(
                    &mut fingerprint,
                    view.arguments_schema().as_str().as_bytes(),
                );
                match view.access() {
                    ReadAccessPattern::Streaming { maximum_ranges } => {
                        fingerprint_field(&mut fingerprint, b"streaming");
                        fingerprint_u64(&mut fingerprint, u64::from(maximum_ranges));
                    }
                    ReadAccessPattern::RandomAccess { maximum_ranges } => {
                        fingerprint_field(&mut fingerprint, b"random_access");
                        fingerprint_u64(&mut fingerprint, u64::from(maximum_ranges));
                    }
                }
                fingerprint_field(
                    &mut fingerprint,
                    match view.output_kind() {
                        ReadOutputKind::Text => b"text",
                        ReadOutputKind::Structured => b"structured",
                        ReadOutputKind::Image => b"image",
                        ReadOutputKind::Audio => b"audio",
                        ReadOutputKind::File => b"file",
                    },
                );
                fingerprint_view_bounds(&mut fingerprint, view.bounds());
            }
            fingerprint_len(&mut fingerprint, reader.reason_codes().len());
            for reason in reader.reason_codes() {
                fingerprint_field(&mut fingerprint, reason.as_str().as_bytes());
            }
            fingerprint_field(
                &mut fingerprint,
                match reader.streaming_text_fallback() {
                    StreamingTextFallback::Disabled => b"disabled",
                    StreamingTextFallback::Enabled => b"enabled",
                },
            );
        }
    }
    fingerprint.finalize().into()
}

fn fingerprint_view_bounds(fingerprint: &mut Sha256, bounds: ReadViewBounds) {
    fingerprint_u64(fingerprint, bounds.source_bytes());
    match bounds {
        ReadViewBounds::Text { output_bytes, .. } => {
            fingerprint_usize(fingerprint, output_bytes);
        }
        ReadViewBounds::Structured {
            output_bytes,
            depth,
            nodes,
            string_bytes,
            ..
        } => {
            fingerprint_usize(fingerprint, output_bytes);
            fingerprint_u64(fingerprint, u64::from(depth));
            fingerprint_u64(fingerprint, nodes);
            fingerprint_usize(fingerprint, string_bytes);
        }
        ReadViewBounds::Image {
            width,
            height,
            pixels,
            output_bytes,
            ..
        } => {
            fingerprint_u64(fingerprint, u64::from(width));
            fingerprint_u64(fingerprint, u64::from(height));
            fingerprint_u64(fingerprint, pixels);
            fingerprint_u64(fingerprint, output_bytes);
        }
        ReadViewBounds::Audio {
            channels,
            sample_rate_hz,
            duration_seconds,
            output_bytes,
            ..
        } => {
            fingerprint_u64(fingerprint, u64::from(channels));
            fingerprint_u64(fingerprint, u64::from(sample_rate_hz));
            fingerprint_u64(fingerprint, u64::from(duration_seconds));
            fingerprint_u64(fingerprint, output_bytes);
        }
        ReadViewBounds::File { output_bytes, .. } => {
            fingerprint_u64(fingerprint, output_bytes);
        }
    }
}

fn fingerprint_field(fingerprint: &mut Sha256, value: &[u8]) {
    fingerprint_len(fingerprint, value.len());
    fingerprint.update(value);
}

fn fingerprint_len(fingerprint: &mut Sha256, value: usize) {
    fingerprint_usize(fingerprint, value);
}

fn fingerprint_usize(fingerprint: &mut Sha256, value: usize) {
    fingerprint_u64(fingerprint, u64::try_from(value).unwrap_or(u64::MAX));
}

fn fingerprint_u64(fingerprint: &mut Sha256, value: u64) {
    fingerprint.update(value.to_be_bytes());
}

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
        ranges: u32,
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
            ReadAccessPattern::Streaming { maximum_ranges } => Self::Streaming {
                ranges: maximum_ranges,
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
    Document,
    File,
}

impl From<AttachmentKind> for WireAttachmentKind {
    fn from(value: AttachmentKind) -> Self {
        match value {
            AttachmentKind::Image => Self::Image,
            AttachmentKind::Document => Self::Document,
            AttachmentKind::File => Self::File,
        }
    }
}

impl From<WireAttachmentKind> for AttachmentKind {
    fn from(value: WireAttachmentKind) -> Self {
        match value {
            WireAttachmentKind::Image => Self::Image,
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
    maximum_source_bytes: u64,
    maximum_ranges: u32,
}

impl From<&FileMediaProviderValidationRequest> for WireValidationRequest {
    fn from(request: &FileMediaProviderValidationRequest) -> Self {
        Self {
            source: (&request.source).into(),
            media_type: request.media_type.as_str().to_owned(),
            evidence: request.evidence,
            maximum_source_bytes: request.maximum_source_bytes,
            maximum_ranges: request.maximum_ranges,
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
            maximum_source_bytes: value.maximum_source_bytes,
            maximum_ranges: value.maximum_ranges,
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
    options: Option<serde_json::Value>,
    continuation: Option<String>,
    maximum_container_entries: u64,
}

impl From<&FileMediaProviderReadRequest> for WireReadRequest {
    fn from(request: &FileMediaProviderReadRequest) -> Self {
        let (options, continuation) = match &request.input {
            signalbox_file_media_runtime::FileReadInput::Initial { options } => {
                (Some(options.clone()), None)
            }
            signalbox_file_media_runtime::FileReadInput::Continuation { cursor } => {
                (None, Some(cursor.as_str().to_owned()))
            }
        };
        Self {
            source: (&request.source).into(),
            detected_media_type: request.detected_media_type.as_str().to_owned(),
            validation: request.validation,
            metadata_json: request.metadata.as_str().to_owned(),
            view: request.view.as_str().to_owned(),
            options,
            continuation,
            maximum_container_entries: request.maximum_container_entries,
        }
    }
}

impl TryFrom<WireReadRequest> for FileMediaProviderReadRequest {
    type Error = ProtocolValueError;

    fn try_from(value: WireReadRequest) -> Result<Self, Self::Error> {
        let input = match (value.options, value.continuation) {
            (Some(options), None) => {
                signalbox_file_media_runtime::FileReadInput::Initial { options }
            }
            (None, Some(cursor)) => signalbox_file_media_runtime::FileReadInput::Continuation {
                cursor: ReadContinuationCursor::try_new(cursor).map_err(map_value_error)?,
            },
            (Some(_), Some(_)) | (None, None) => return Err(ProtocolValueError),
        };
        Ok(Self {
            source: value.source.try_into()?,
            detected_media_type: CanonicalMediaType::from_str(&value.detected_media_type)
                .map_err(|_| ProtocolValueError)?,
            validation: value.validation,
            metadata: BoundedMetadata::try_new(&value.metadata_json).map_err(map_value_error)?,
            view: ReadViewName::try_new(value.view).map_err(map_value_error)?,
            input,
            maximum_container_entries: value.maximum_container_entries,
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

#[cfg(test)]
mod tests {
    use signalbox_file_media_runtime::{
        MAX_PROCESSOR_FRAME_BYTES, MAX_TEXT_OR_JSON_BYTES, ProcessorReadOutput,
    };

    use super::WorkerFrame;

    #[test]
    fn maximum_escape_heavy_structured_output_fits_one_frame() {
        let body_json = format!("\"{}\"", "\\\\".repeat((MAX_TEXT_OR_JSON_BYTES - 2) / 2));
        assert_eq!(body_json.len(), MAX_TEXT_OR_JSON_BYTES);
        let frame = WorkerFrame::ReadResult {
            output: ProcessorReadOutput::Structured {
                body_json,
                truncated: false,
                cursor: None,
            },
        };
        let encoded = serde_json::to_vec(&frame).expect("worker frame serializes");
        assert!(encoded.len() <= MAX_PROCESSOR_FRAME_BYTES);
    }
}
