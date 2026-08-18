use std::{error::Error, fmt, future::Future, pin::Pin};

use crate::{
    CancellationSignal, CanonicalJsonObjectSchema, CanonicalMediaType, FileReaderName,
    FileReaderProviderName, FileReaderRevision, FileUse, ProcessorFailure, ProcessorProbeOutput,
    ProcessorReadOutput, ProcessorValidationOutput, ReadViewName, ReaderIdentity, ReasonCode,
    VerifiedBlobSource,
};

/// Strength of one byte-derived probe candidate.
#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStrength {
    /// Caller declaration nominates a provider but is not evidence.
    DeclaredCandidate,
    /// Bounded structure suggests a candidate requiring full validation.
    StructuralCandidate,
    /// A format-owned signature identifies a candidate.
    Strong,
}

/// Finite source-read envelope for one probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProbeDeclaration {
    prefix_bytes: u64,
    suffix_bytes: u64,
    range_count: u32,
    cumulative_bytes: u64,
}

impl ProbeDeclaration {
    /// Declares one finite probe envelope. Registry construction checks ceilings.
    pub const fn new(
        prefix_bytes: u64,
        suffix_bytes: u64,
        range_count: u32,
        cumulative_bytes: u64,
    ) -> Self {
        Self {
            prefix_bytes,
            suffix_bytes,
            range_count,
            cumulative_bytes,
        }
    }

    /// Returns the prefix budget.
    pub const fn prefix_bytes(self) -> u64 {
        self.prefix_bytes
    }

    /// Returns the suffix budget.
    pub const fn suffix_bytes(self) -> u64 {
        self.suffix_bytes
    }

    /// Returns the arbitrary-range count.
    pub const fn range_count(self) -> u32 {
        self.range_count
    }

    /// Returns the cumulative byte budget.
    pub const fn cumulative_bytes(self) -> u64 {
        self.cumulative_bytes
    }
}

/// Whether one reader is eligible for the complete-stream UTF-8 fallback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamingTextFallback {
    /// The reader never claims untyped bytes as text.
    Disabled,
    /// The reader may claim only after complete streaming validation.
    Enabled,
}

/// Declared source access posture for one view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadAccessPattern {
    /// Monotonic streaming access.
    Streaming,
    /// Bounded exact-range access.
    RandomAccess {
        /// Maximum ranges requested for one read.
        maximum_ranges: u32,
    },
}

/// Closed common output vocabulary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadOutputKind {
    /// Bounded UTF-8.
    Text,
    /// Bounded canonical JSON.
    Structured,
    /// Immutable image bytes registered before durable result commit.
    Image,
    /// Immutable audio bytes registered before durable result commit.
    Audio,
    /// Immutable general-file bytes admitted by a reviewed model adapter.
    File,
}

/// Output-specific finite bounds for one read view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadViewBounds {
    /// Text body and cumulative source work.
    Text {
        /// Maximum source bytes requested.
        source_bytes: u64,
        /// Maximum UTF-8 bytes returned.
        output_bytes: usize,
    },
    /// Structured body and tree limits.
    Structured {
        /// Maximum source bytes requested.
        source_bytes: u64,
        /// Maximum compact JSON bytes returned.
        output_bytes: usize,
        /// Maximum JSON nesting.
        depth: u32,
        /// Maximum JSON nodes.
        nodes: u64,
        /// Maximum cumulative string bytes.
        string_bytes: usize,
    },
    /// Image output envelope.
    Image {
        /// Maximum source bytes requested.
        source_bytes: u64,
        /// Maximum width.
        width: u32,
        /// Maximum height.
        height: u32,
        /// Maximum decoded pixels.
        pixels: u64,
        /// Maximum presented bytes.
        output_bytes: u64,
    },
    /// Audio output envelope.
    Audio {
        /// Maximum source bytes requested.
        source_bytes: u64,
        /// Maximum channels.
        channels: u16,
        /// Maximum sample rate.
        sample_rate_hz: u32,
        /// Maximum duration.
        duration_seconds: u32,
        /// Maximum presented bytes.
        output_bytes: u64,
    },
    /// General-file output envelope.
    File {
        /// Maximum source bytes requested.
        source_bytes: u64,
        /// Maximum presented bytes.
        output_bytes: u64,
    },
}

impl ReadViewBounds {
    /// Returns the common output kind implied by this envelope.
    pub const fn output_kind(self) -> ReadOutputKind {
        match self {
            Self::Text { .. } => ReadOutputKind::Text,
            Self::Structured { .. } => ReadOutputKind::Structured,
            Self::Image { .. } => ReadOutputKind::Image,
            Self::Audio { .. } => ReadOutputKind::Audio,
            Self::File { .. } => ReadOutputKind::File,
        }
    }

    /// Returns the maximum cumulative source bytes.
    pub const fn source_bytes(self) -> u64 {
        match self {
            Self::Text { source_bytes, .. }
            | Self::Structured { source_bytes, .. }
            | Self::Image { source_bytes, .. }
            | Self::Audio { source_bytes, .. }
            | Self::File { source_bytes, .. } => source_bytes,
        }
    }
}

/// One provider-owned read view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadViewDeclaration {
    name: ReadViewName,
    description: String,
    arguments_schema: CanonicalJsonObjectSchema,
    access: ReadAccessPattern,
    bounds: ReadViewBounds,
}

impl ReadViewDeclaration {
    /// Constructs one declaration. Registry construction checks every bound.
    pub fn try_new(
        name: ReadViewName,
        description: String,
        arguments_schema: CanonicalJsonObjectSchema,
        access: ReadAccessPattern,
        bounds: ReadViewBounds,
    ) -> Result<Self, RegistryDeclarationError> {
        if description.is_empty()
            || description.len() > 512
            || description.contains('\0')
            || description.chars().any(char::is_control)
        {
            return Err(RegistryDeclarationError::Description);
        }
        Ok(Self {
            name,
            description,
            arguments_schema,
            access,
            bounds,
        })
    }

    /// Borrows the view name.
    pub const fn name(&self) -> &ReadViewName {
        &self.name
    }

    /// Borrows the model-facing bounded description.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Borrows the object schema.
    pub const fn arguments_schema(&self) -> &CanonicalJsonObjectSchema {
        &self.arguments_schema
    }

    /// Returns the declared access posture.
    pub const fn access(&self) -> ReadAccessPattern {
        self.access
    }

    /// Returns output-specific bounds.
    pub const fn bounds(&self) -> ReadViewBounds {
        self.bounds
    }

    /// Returns the common output kind.
    pub const fn output_kind(&self) -> ReadOutputKind {
        self.bounds.output_kind()
    }
}

/// Static declaration for one reader implementation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReaderDeclaration {
    identity: ReaderIdentity,
    media_types: Vec<CanonicalMediaType>,
    probe: ProbeDeclaration,
    views: Vec<ReadViewDeclaration>,
    reason_codes: Vec<ReasonCode>,
    streaming_text_fallback: StreamingTextFallback,
}

/// Labeled candidate fields for one reader declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReaderDeclarationInput {
    /// Provider that owns the reader.
    pub provider: FileReaderProviderName,
    /// Reader name within that provider.
    pub reader: FileReaderName,
    /// Immutable implementation revision.
    pub revision: FileReaderRevision,
    /// Exact canonical media types owned by this reader.
    pub media_types: Vec<CanonicalMediaType>,
    /// Finite probe envelope.
    pub probe: ProbeDeclaration,
    /// Nonempty provider-owned view inventory.
    pub views: Vec<ReadViewDeclaration>,
    /// Nonempty sanitized reason-code inventory.
    pub reason_codes: Vec<ReasonCode>,
    /// Complete-stream text fallback posture.
    pub streaming_text_fallback: StreamingTextFallback,
}

impl ReaderDeclaration {
    /// Constructs one nonempty reader declaration.
    pub fn try_new(input: ReaderDeclarationInput) -> Result<Self, RegistryDeclarationError> {
        if input.media_types.is_empty() || input.views.is_empty() || input.reason_codes.is_empty() {
            return Err(RegistryDeclarationError::EmptyInventory);
        }
        Ok(Self {
            identity: ReaderIdentity::new(input.provider, input.reader, input.revision),
            media_types: input.media_types,
            probe: input.probe,
            views: input.views,
            reason_codes: input.reason_codes,
            streaming_text_fallback: input.streaming_text_fallback,
        })
    }

    /// Borrows the immutable reader identity.
    pub const fn identity(&self) -> &ReaderIdentity {
        &self.identity
    }

    /// Borrows exact owned media types.
    pub fn media_types(&self) -> &[CanonicalMediaType] {
        &self.media_types
    }

    /// Returns the probe envelope.
    pub const fn probe(&self) -> ProbeDeclaration {
        self.probe
    }

    /// Borrows provider-owned views.
    pub fn views(&self) -> &[ReadViewDeclaration] {
        &self.views
    }

    /// Borrows registered sanitized reason codes.
    pub fn reason_codes(&self) -> &[ReasonCode] {
        &self.reason_codes
    }

    /// Returns text fallback posture.
    pub const fn streaming_text_fallback(&self) -> StreamingTextFallback {
        self.streaming_text_fallback
    }
}

/// Static declaration contributed by one compiled provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileMediaProviderDeclaration {
    provider: FileReaderProviderName,
    readers: Vec<ReaderDeclaration>,
}

impl FileMediaProviderDeclaration {
    /// Constructs one provider whose readers all carry its exact identity.
    pub fn try_new(
        provider: FileReaderProviderName,
        readers: Vec<ReaderDeclaration>,
    ) -> Result<Self, RegistryDeclarationError> {
        if readers.is_empty() {
            return Err(RegistryDeclarationError::EmptyInventory);
        }
        if readers
            .iter()
            .any(|reader| reader.identity().provider() != &provider)
        {
            return Err(RegistryDeclarationError::ForeignReader);
        }
        Ok(Self { provider, readers })
    }

    /// Borrows the provider identity.
    pub const fn provider(&self) -> &FileReaderProviderName {
        &self.provider
    }

    /// Borrows declared readers.
    pub fn readers(&self) -> &[ReaderDeclaration] {
        &self.readers
    }

    pub(crate) fn sort_readers(&mut self) {
        self.readers
            .sort_by(|left, right| left.identity().cmp(right.identity()));
    }
}

/// Provider request to validate one candidate selected by the registry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileMediaProviderValidationRequest {
    /// Exact semantic use.
    pub source: FileUse,
    /// Candidate media type owned by this reader.
    pub media_type: CanonicalMediaType,
    /// Evidence path requested by the registry.
    pub evidence: crate::ValidationEvidence,
}

/// Provider request to interpret one validated file through one view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileMediaProviderReadRequest {
    /// Exact semantic use whose bytes the registry validated.
    pub source: FileUse,
    /// Registry-selected canonical media type.
    pub detected_media_type: CanonicalMediaType,
    /// Registry-admitted validation evidence.
    pub validation: crate::ValidationEvidence,
    /// Registry-sanitized provider metadata.
    pub metadata: crate::BoundedMetadata,
    /// Exact provider-owned view.
    pub view: ReadViewName,
    /// Model-supplied options retained as structured data.
    pub options: serde_json::Value,
}

/// Boxed adapter future used by isolated worker-side provider implementations.
pub type FileMediaProviderFuture<'a, Output> =
    Pin<Box<dyn Future<Output = Result<Output, ProcessorFailure>> + Send + 'a>>;

/// Worker-side format adapter contract.
///
/// The daemon never calls this trait directly. Slice-two processor isolation
/// hosts implementations in a fresh worker and exposes only sanitized outputs
/// to the registry.
pub trait FileMediaProvider: Send + Sync {
    /// Returns this adapter's static declaration.
    fn declaration(&self) -> FileMediaProviderDeclaration;

    /// Runs a bounded byte probe.
    fn probe<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProviderFuture<'a, ProcessorProbeOutput>;

    /// Validates the registry-selected candidate before interpretation.
    fn inspect<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: FileMediaProviderValidationRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProviderFuture<'a, ProcessorValidationOutput>;

    /// Produces one bounded view from prior validation evidence.
    fn read<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: FileMediaProviderReadRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProviderFuture<'a, ProcessorReadOutput>;
}

/// Closed declaration-construction failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryDeclarationError {
    /// A required inventory was empty.
    EmptyInventory,
    /// A reader named another provider.
    ForeignReader,
    /// A view description was empty, excessive, or control-bearing.
    Description,
}

impl fmt::Display for RegistryDeclarationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EmptyInventory => "provider declaration has an empty required inventory",
            Self::ForeignReader => "reader identity names another provider",
            Self::Description => "view description is invalid",
        })
    }
}

impl Error for RegistryDeclarationError {}
