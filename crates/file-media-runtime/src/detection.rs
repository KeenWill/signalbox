use std::{error::Error, fmt, future::Future, num::NonZeroU64, pin::Pin};

use crate::{
    BoundedMetadata, CanonicalMediaType, FileDigest, FileUse, ProbeStrength,
    ReadContinuationCursor, ReadViewDeclaration, ReadViewName, ReaderIdentity, ReasonCode,
    VisiblePartSelector,
};

/// Asynchronous exact-range read result from a verified blob source.
pub type SourceReadFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<u8>, SourceReadError>> + Send + 'a>>;

/// Verified, placement-free byte authority exposed to a processor broker.
pub trait VerifiedBlobSource: Send + Sync {
    /// Returns the exact verified digest.
    fn digest(&self) -> FileDigest;

    /// Returns the exact verified positive length.
    fn byte_length(&self) -> NonZeroU64;

    /// Reads one exact in-bounds range without exposing a path or store locator.
    fn read_range(&self, offset: u64, length: NonZeroU64) -> SourceReadFuture<'_>;
}

/// Content-silent verified-source failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceReadError {
    /// The immutable object no longer exists at any replica.
    Missing,
    /// Every readable replica contradicted its digest or length.
    Corrupt,
    /// At least one candidate could not presently be read.
    Unavailable,
    /// The requested exact range exceeded the source.
    RangeOutOfBounds,
    /// Source identity or catalog evidence was internally inconsistent.
    Integrity,
}

impl fmt::Display for SourceReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Missing => "verified blob source is missing",
            Self::Corrupt => "verified blob source is corrupt",
            Self::Unavailable => "verified blob source is unavailable",
            Self::RangeOutOfBounds => "verified blob source range is out of bounds",
            Self::Integrity => "verified blob source evidence is inconsistent",
        })
    }
}

impl Error for SourceReadError {}

/// Cooperative cancellation observed by providers and processor clients.
pub trait CancellationSignal: Send + Sync {
    /// Returns whether authoritative cancellation has been requested.
    fn is_cancelled(&self) -> bool;
}

/// Cancellation signal that never fires, useful for bounded synchronous callers.
#[derive(Clone, Copy, Debug, Default)]
pub struct NeverCancelled;

impl CancellationSignal for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
}

/// Registry request to inspect one visible semantic use.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InspectionRequest {
    /// Exact caller-supplied use metadata.
    pub source: FileUse,
    /// Stable selector when a digest appears through several visible uses.
    pub visible_part: Option<VisiblePartSelector>,
}

/// Closed, content-silent validation evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationEvidence {
    /// A strong format signature was structurally validated.
    StrongSignature,
    /// Structure was validated without a strong signature.
    StructuralValidation,
    /// A declared candidate was independently structurally validated.
    DeclaredCandidateStructurallyValidated,
    /// Complete streaming UTF-8 and control policy validation succeeded.
    StreamingTextValidation,
}

/// Registry-produced evidence that a reader validated exact bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedFile {
    source: FileUse,
    detected_media_type: CanonicalMediaType,
    reader: ReaderIdentity,
    validation: ValidationEvidence,
    metadata: BoundedMetadata,
    views: Vec<ReadViewDeclaration>,
}

impl ValidatedFile {
    pub(crate) fn new(
        source: FileUse,
        detected_media_type: CanonicalMediaType,
        reader: ReaderIdentity,
        validation: ValidationEvidence,
        metadata: BoundedMetadata,
        views: Vec<ReadViewDeclaration>,
    ) -> Self {
        Self {
            source,
            detected_media_type,
            reader,
            validation,
            metadata,
            views,
        }
    }

    /// Borrows the exact semantic use.
    pub const fn source(&self) -> &FileUse {
        &self.source
    }

    /// Borrows the byte-validated canonical type.
    pub const fn detected_media_type(&self) -> &CanonicalMediaType {
        &self.detected_media_type
    }

    /// Borrows the exact reader identity and revision.
    pub const fn reader(&self) -> &ReaderIdentity {
        &self.reader
    }

    /// Returns the evidence class.
    pub const fn validation(&self) -> ValidationEvidence {
        self.validation
    }

    /// Borrows bounded provider metadata.
    pub const fn metadata(&self) -> &BoundedMetadata {
        &self.metadata
    }

    /// Borrows ordered provider-owned views.
    pub fn views(&self) -> &[ReadViewDeclaration] {
        &self.views
    }
}

/// Compact inspection status vocabulary exposed to agents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileInspectionStatus {
    /// A reader validated the bytes and declared views.
    Validated,
    /// No reader safely recognized the bytes.
    Unknown,
    /// A recognized format was malformed.
    Malformed,
    /// Incompatible strong claims made type selection unsafe.
    Ambiguous,
    /// Caller declaration disagreed with detected bytes.
    DeclaredMismatch,
    /// A recognized encrypted or locked file is terminal in version one.
    EncryptedOrLocked,
}

/// Complete registry inspection outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileInspection {
    /// Validated bytes and available views.
    Validated(ValidatedFile),
    /// Ordinary unknown bytes, still raw-readable.
    Unknown {
        /// Exact inspected use.
        source: FileUse,
    },
    /// A recognized signature or structure was malformed.
    Malformed {
        /// Exact inspected use.
        source: FileUse,
        /// Recognized type.
        media_type: CanonicalMediaType,
        /// Registered sanitized reason.
        reason_code: ReasonCode,
    },
    /// Incompatible strong candidates were observed.
    Ambiguous {
        /// Exact inspected use.
        source: FileUse,
        /// Canonically sorted distinct claims.
        media_types: Vec<CanonicalMediaType>,
    },
    /// Declared metadata disagreed with byte evidence.
    DeclaredMismatch {
        /// Exact inspected use.
        source: FileUse,
        /// Parsed canonical declaration.
        declared: CanonicalMediaType,
        /// Byte-detected type.
        detected: CanonicalMediaType,
    },
    /// Recognized encrypted content; no password channel exists.
    EncryptedOrLocked {
        /// Exact inspected use.
        source: FileUse,
        /// Recognized type.
        media_type: CanonicalMediaType,
    },
}

impl FileInspection {
    /// Returns the compact agent-visible status.
    pub const fn status(&self) -> FileInspectionStatus {
        match self {
            Self::Validated(_) => FileInspectionStatus::Validated,
            Self::Unknown { .. } => FileInspectionStatus::Unknown,
            Self::Malformed { .. } => FileInspectionStatus::Malformed,
            Self::Ambiguous { .. } => FileInspectionStatus::Ambiguous,
            Self::DeclaredMismatch { .. } => FileInspectionStatus::DeclaredMismatch,
            Self::EncryptedOrLocked { .. } => FileInspectionStatus::EncryptedOrLocked,
        }
    }

    /// Borrows the exact semantic use in every outcome.
    pub const fn source(&self) -> &FileUse {
        match self {
            Self::Validated(validated) => validated.source(),
            Self::Unknown { source }
            | Self::Malformed { source, .. }
            | Self::Ambiguous { source, .. }
            | Self::DeclaredMismatch { source, .. }
            | Self::EncryptedOrLocked { source, .. } => source,
        }
    }
}

/// Agent request for one provider-owned view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileReadRequest {
    /// Inspection inputs; the registry repeats inspection.
    pub inspection: InspectionRequest,
    /// Exact provider-owned view name.
    pub view: ReadViewName,
    /// Closed initial-options or continuation input.
    pub input: FileReadInput,
}

/// Closed input mode for one typed read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileReadInput {
    /// Initial request carrying structured model-supplied options.
    Initial {
        /// Provider-owned view options.
        options: serde_json::Value,
    },
    /// Continuation request carrying a checked prior-page cursor.
    Continuation {
        /// Opaque restart-ephemeral continuation.
        cursor: ReadContinuationCursor,
    },
}

/// Sanitized continuation state for one typed read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadContinuation {
    /// The returned body is complete.
    Complete,
    /// More complete semantic units remain.
    More {
        /// Opaque restart-ephemeral continuation.
        cursor: ReadContinuationCursor,
    },
}

/// Bounded typed-read result currently representable without durable media references.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileReadResult {
    /// Admitted UTF-8 body.
    Text {
        /// Complete bounded body.
        body: String,
        /// Sanitized completeness or continuation evidence.
        continuation: ReadContinuation,
    },
    /// Admitted structured value.
    Structured {
        /// Parsed bounded JSON body.
        body: serde_json::Value,
        /// Sanitized completeness or continuation evidence.
        continuation: ReadContinuation,
    },
}

/// Raw untrusted probe response crossing the processor boundary.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProcessorProbeOutput {
    /// Reader found no evidence.
    NoMatch,
    /// Reader found one candidate.
    Candidate {
        /// Untrusted claimed canonical media type spelling.
        media_type: String,
        /// Claimed evidence strength.
        strength: ProbeStrength,
    },
    /// Reader recognized a malformed format.
    RecognizedMalformed {
        /// Untrusted claimed media type spelling.
        media_type: String,
        /// Untrusted reason spelling, checked against the declaration.
        reason_code: String,
    },
}

/// Raw untrusted validation response crossing the processor boundary.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProcessorValidationOutput {
    /// Validation succeeded.
    Validated {
        /// Untrusted claimed type, checked against the selected candidate.
        media_type: String,
        /// Untrusted evidence claim, checked against the selected path.
        evidence: ValidationEvidence,
        /// Untrusted bounded JSON object spelling.
        metadata_json: String,
    },
    /// Selected candidate was malformed.
    Malformed {
        /// Untrusted claimed type.
        media_type: String,
        /// Untrusted reason spelling.
        reason_code: String,
    },
    /// Selected candidate is encrypted or locked.
    EncryptedOrLocked {
        /// Untrusted claimed type.
        media_type: String,
    },
    /// Candidate did not survive structural validation.
    NoMatch,
}

/// Raw untrusted read response crossing the processor boundary.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProcessorReadOutput {
    /// Text body and continuation facts.
    Text {
        /// Untrusted text body.
        body: String,
        /// Whether complete semantic units remain.
        truncated: bool,
        /// Untrusted opaque cursor.
        cursor: Option<String>,
    },
    /// Compact JSON spelling and continuation facts.
    Structured {
        /// Untrusted JSON text.
        body_json: String,
        /// Whether complete semantic units remain.
        truncated: bool,
        /// Untrusted opaque cursor.
        cursor: Option<String>,
    },
    /// Provider rejected model-supplied options.
    InvalidViewArguments,
    /// Provider declined a declared view.
    UnsupportedView,
    /// Source exceeds a declared intrinsic whole-decode size limit.
    ///
    /// Version one declares only cumulative source-work budgets, so the
    /// registry rejects this processor outcome until such a limit exists.
    SourceTooLarge {
        /// Untrusted claimed intrinsic maximum.
        maximum_bytes: u64,
    },
    /// Decode expansion crossed a registered named limit.
    ExpansionLimitExceeded {
        /// Untrusted reason spelling, checked against the reader declaration.
        limit_kind: String,
    },
    /// One complete semantic unit could not fit.
    OutputUnitTooLarge,
}

/// Content-silent process execution failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessorFailure {
    /// Isolation or worker startup was unavailable.
    Unavailable,
    /// Worker exited unsuccessfully or returned incomplete output.
    Failed,
    /// Worker exceeded wall time.
    TimedOut,
    /// Authoritative cancellation terminated work.
    Cancelled,
    /// Framing or output validation failed.
    Protocol,
}

impl fmt::Display for ProcessorFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "file processor is unavailable",
            Self::Failed => "file processor failed",
            Self::TimedOut => "file processor timed out",
            Self::Cancelled => "file processing was cancelled",
            Self::Protocol => "file processor returned invalid output",
        })
    }
}

impl Error for ProcessorFailure {}

/// Authenticated failure returned by the daemon-side processor broker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessorBoundaryFailure {
    /// Process execution failed without a verified-source classification.
    Processor(ProcessorFailure),
    /// The verified source failed while serving processor reads.
    Source(SourceReadError),
}

impl fmt::Display for ProcessorBoundaryFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Processor(failure) => failure.fmt(formatter),
            Self::Source(failure) => failure.fmt(formatter),
        }
    }
}

impl Error for ProcessorBoundaryFailure {}

impl From<ProcessorFailure> for ProcessorBoundaryFailure {
    fn from(value: ProcessorFailure) -> Self {
        Self::Processor(value)
    }
}

impl From<SourceReadError> for ProcessorBoundaryFailure {
    fn from(value: SourceReadError) -> Self {
        Self::Source(value)
    }
}

/// Boxed future returned by a daemon-side isolated processor client.
pub type FileMediaProcessorFuture<'a, Output> =
    Pin<Box<dyn Future<Output = Result<Output, ProcessorBoundaryFailure>> + Send + 'a>>;

/// Daemon-side process boundary used by detection and reads.
pub trait FileMediaProcessor: Send + Sync {
    /// Runs one reader probe under its registered source envelope.
    fn probe<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorProbeOutput>;

    /// Runs full validation for the sole registry-selected candidate.
    fn validate<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: crate::FileMediaProviderValidationRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorValidationOutput>;

    /// Runs one declared view after validation.
    fn read<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: crate::FileMediaProviderReadRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorReadOutput>;
}

/// Closed application-facing file/media failure algebra.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileMediaFailure {
    /// Digest is outside the rendered-frontier allow-set.
    BlobNotVisible,
    /// Blob catalog identity is absent.
    BlobMissing,
    /// Every replica contradicted exact bytes.
    BlobCorrupt,
    /// Blob access is temporarily unavailable.
    BlobUnavailable,
    /// No registered reader safely recognized the bytes.
    UnknownType,
    /// Incompatible strong candidates made selection unsafe.
    AmbiguousType,
    /// Caller declaration disagreed with byte evidence.
    DeclaredTypeMismatch {
        /// Canonical caller declaration.
        declared: CanonicalMediaType,
        /// Canonical byte-detected type.
        detected: CanonicalMediaType,
    },
    /// Recognized bytes were malformed.
    Malformed {
        /// Recognized canonical type.
        media_type: CanonicalMediaType,
        /// Registered sanitized reason.
        reason_code: ReasonCode,
    },
    /// Recognized encrypted content is terminal in version one.
    EncryptedOrLocked {
        /// Recognized canonical type.
        media_type: CanonicalMediaType,
    },
    /// Selected view does not exist.
    UnsupportedView,
    /// View options failed provider validation.
    InvalidViewArguments,
    /// Source exceeds a reader's bounded whole-decode envelope.
    SourceTooLarge {
        /// Exact declared maximum.
        maximum_bytes: u64,
    },
    /// Decode expansion exceeded a named hard limit.
    ExpansionLimitExceeded {
        /// Registered content-silent limit name.
        limit_kind: ReasonCode,
    },
    /// One semantic output unit could not fit without truncation.
    OutputUnitTooLarge,
    /// Processor isolation or worker startup is unavailable.
    ProcessorUnavailable,
    /// Processor failed without an authenticated typed failure.
    ProcessorFailed,
    /// Processor exceeded wall time.
    ProcessorTimedOut,
    /// Authoritative cancellation stopped work.
    Cancelled,
}

impl fmt::Display for FileMediaFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BlobNotVisible => "blob is not visible to this request",
            Self::BlobMissing => "blob is missing",
            Self::BlobCorrupt => "blob is corrupt",
            Self::BlobUnavailable => "blob is unavailable",
            Self::UnknownType => "file type is unknown",
            Self::AmbiguousType => "file type is ambiguous",
            Self::DeclaredTypeMismatch { .. } => "declared and detected file types disagree",
            Self::Malformed { .. } => "recognized file is malformed",
            Self::EncryptedOrLocked { .. } => "file is encrypted or locked",
            Self::UnsupportedView => "file read view is unsupported",
            Self::InvalidViewArguments => "file read view arguments are invalid",
            Self::SourceTooLarge { .. } => "file source exceeds the declared view bound",
            Self::ExpansionLimitExceeded { .. } => "file expansion limit was exceeded",
            Self::OutputUnitTooLarge => "one output unit exceeds the result bound",
            Self::ProcessorUnavailable => "file processor is unavailable",
            Self::ProcessorFailed => "file processor failed",
            Self::ProcessorTimedOut => "file processor timed out",
            Self::Cancelled => "file processing was cancelled",
        })
    }
}

impl Error for FileMediaFailure {}

impl From<ProcessorFailure> for FileMediaFailure {
    fn from(value: ProcessorFailure) -> Self {
        match value {
            ProcessorFailure::Unavailable => Self::ProcessorUnavailable,
            ProcessorFailure::Failed | ProcessorFailure::Protocol => Self::ProcessorFailed,
            ProcessorFailure::TimedOut => Self::ProcessorTimedOut,
            ProcessorFailure::Cancelled => Self::Cancelled,
        }
    }
}

impl From<ProcessorBoundaryFailure> for FileMediaFailure {
    fn from(value: ProcessorBoundaryFailure) -> Self {
        match value {
            ProcessorBoundaryFailure::Processor(failure) => failure.into(),
            ProcessorBoundaryFailure::Source(failure) => failure.into(),
        }
    }
}

impl From<SourceReadError> for FileMediaFailure {
    fn from(value: SourceReadError) -> Self {
        match value {
            SourceReadError::Missing => Self::BlobMissing,
            SourceReadError::Corrupt => Self::BlobCorrupt,
            SourceReadError::Unavailable => Self::BlobUnavailable,
            SourceReadError::RangeOutOfBounds | SourceReadError::Integrity => Self::ProcessorFailed,
        }
    }
}
