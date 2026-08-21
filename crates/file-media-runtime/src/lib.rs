//! Provider-neutral file and media interpretation contracts.
//!
//! This crate owns checked declarations, detection, validation, bounded reads,
//! and the untrusted processor boundary. It deliberately has no dependency on
//! domain, application, persistence, daemon, parser, or media crates.

mod declaration;
mod detection;
mod limits;
mod registry;
mod value;

pub use declaration::{
    FileMediaProvider, FileMediaProviderDeclaration, FileMediaProviderFuture,
    FileMediaProviderReadRequest, FileMediaProviderValidationRequest, ProbeDeclaration,
    ProbeStrength, ReadAccessPattern, ReadOutputKind, ReadViewBounds, ReadViewDeclaration,
    ReaderDeclaration, ReaderDeclarationInput, RegistryDeclarationError, StreamingTextFallback,
};
pub use detection::{
    CancellationSignal, FileInspection, FileInspectionStatus, FileMediaFailure, FileMediaProcessor,
    FileMediaProcessorFuture, FileReadRequest, FileReadResult, InspectionRequest, NeverCancelled,
    ProcessorBoundaryFailure, ProcessorFailure, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, ReadContinuation, SourceReadError, SourceReadFuture, ValidatedFile,
    ValidationEvidence, VerifiedBlobSource,
};
pub use limits::{
    FileMediaCeilings, MAX_AUDIO_CHANNELS, MAX_AUDIO_CLIP_SECONDS, MAX_AUDIO_SAMPLE_RATE_HZ,
    MAX_DECODED_IMAGE_PIXELS, MAX_IMAGE_AXIS, MAX_OBSERVED_CONTAINER_ENTRIES,
    MAX_PRESENTED_AUDIO_BYTES, MAX_PRESENTED_FILE_BYTES, MAX_PRESENTED_IMAGE_BYTES,
    MAX_PROBE_CUMULATIVE_BYTES, MAX_PROBE_PREFIX_BYTES, MAX_PROBE_RANGES, MAX_PROBE_SUFFIX_BYTES,
    MAX_PROCESSOR_FRAME_BYTES, MAX_READ_RANGES, MAX_READ_SOURCE_BYTES, MAX_STRUCTURED_DEPTH,
    MAX_STRUCTURED_NODES, MAX_TEXT_OR_JSON_BYTES, MAX_VALIDATION_RANGES,
    MAX_VALIDATION_SOURCE_BYTES,
};
pub use registry::{FileMediaRegistry, FileMediaRegistryConstructionError, ProcessorIsolation};
pub use value::{
    AttachmentKind, BoundedMetadata, CanonicalJsonObjectSchema, CanonicalMediaType,
    DeclaredMediaType, DisplayFilename, FileDigest, FileReaderName, FileReaderProviderName,
    FileReaderRevision, FileUse, MediaTypeParseError, ReadContinuationCursor, ReadViewName,
    ReaderIdentity, ReasonCode, RegistryValueError, VisiblePartSelector,
};

/// Stable model-facing inspection tool name.
pub const FILE_INSPECT_NAME: &str = "file_inspect";

/// Stable model-facing typed-read tool name.
pub const FILE_READ_NAME: &str = "file_read";
