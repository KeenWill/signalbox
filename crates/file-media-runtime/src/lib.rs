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
    FileMediaProvider, FileMediaProviderDeclaration, FileMediaProviderFailure,
    FileMediaProviderFuture, FileMediaProviderReadRequest, FileMediaProviderValidationRequest,
    ProbeDeclaration, ProbeStrength, ReadAccessPattern, ReadOutputKind, ReadViewBounds,
    ReadViewDeclaration, ReaderDeclaration, ReaderDeclarationInput, RegistryDeclarationError,
    StreamingTextFallback, ValidationDeclaration,
};
pub use detection::{
    CancellationSignal, FileInspection, FileInspectionStatus, FileMediaFailure, FileMediaProcessor,
    FileMediaProcessorFuture, FileReadInput, FileReadRequest, FileReadResult, InspectionRequest,
    NeverCancelled, ProcessorBoundaryFailure, ProcessorFailure, ProcessorProbeOutput,
    ProcessorReadOutput, ProcessorValidationOutput, ReadContinuation, SourceReadError,
    SourceReadFuture, ValidatedFile, ValidationEvidence, VerifiedBlobSource,
};
pub use limits::{
    FileMediaCeilings, FileMediaProcessCeilings, FileMediaProcessLimitOverrides,
    MAX_AGGREGATE_MEDIA_BYTES_PER_CALL, MAX_AUDIO_CHANNELS, MAX_AUDIO_CLIP_SECONDS,
    MAX_AUDIO_SAMPLE_RATE_HZ, MAX_DECODED_IMAGE_PIXELS, MAX_IMAGE_AXIS,
    MAX_MEDIA_REFERENCES_PER_CALL, MAX_OBSERVED_CONTAINER_ENTRIES, MAX_PRESENTED_AUDIO_BYTES,
    MAX_PRESENTED_FILE_BYTES, MAX_PRESENTED_IMAGE_BYTES, MAX_PROBE_CUMULATIVE_BYTES,
    MAX_PROBE_PREFIX_BYTES, MAX_PROBE_RANGES, MAX_PROBE_SUFFIX_BYTES, MAX_PROCESSOR_FRAME_BYTES,
    MAX_READ_OPTIONS_BYTES, MAX_READ_RANGES, MAX_READ_SOURCE_BYTES, MAX_STRUCTURED_DEPTH,
    MAX_STRUCTURED_NODES, MAX_TEXT_BODY_BYTES, MAX_TEXT_OR_JSON_BYTES, MAX_VALIDATION_RANGES,
    MAX_VALIDATION_SOURCE_BYTES, MAX_WORKER_CPU_SECONDS, MAX_WORKER_DESCENDANTS,
    MAX_WORKER_FILE_DESCRIPTORS, MAX_WORKER_MEMORY_BYTES, MAX_WORKER_STDERR_BYTES,
    MAX_WORKER_TASKS, MAX_WORKER_WALL_SECONDS, MIN_WORKER_FILE_DESCRIPTORS,
};
pub use registry::{
    FileMediaRegistry, FileMediaRegistryConstructionError, MAX_READERS_PER_PROVIDER,
    MAX_REGISTRY_READERS, ProcessorIsolation, provider_declaration_inventory_fits,
    read_options_fit,
};
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
