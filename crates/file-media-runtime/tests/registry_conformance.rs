#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "conformance fixtures use explicit construction and outcome expectations"
)]

use std::{future::Future, num::NonZeroU64, pin::pin, str::FromStr, task::Context};

use signalbox_file_media_runtime::{
    AttachmentKind, CancellationSignal, CanonicalJsonObjectSchema, CanonicalMediaType,
    DeclaredMediaType, FileDigest, FileInspection, FileMediaCeilings, FileMediaFailure,
    FileMediaProcessor, FileMediaProcessorFuture, FileMediaProviderDeclaration,
    FileMediaProviderReadRequest, FileMediaProviderValidationRequest, FileMediaRegistry,
    FileReadRequest, FileReaderName, FileReaderProviderName, FileReaderRevision, FileUse,
    InspectionRequest, NeverCancelled, ProbeDeclaration, ProbeStrength, ProcessorFailure,
    ProcessorIsolation, ProcessorProbeOutput, ProcessorReadOutput, ProcessorValidationOutput,
    ReadAccessPattern, ReadViewBounds, ReadViewDeclaration, ReadViewName, ReaderDeclaration,
    ReaderDeclarationInput, ReaderIdentity, ReasonCode, SourceReadError, SourceReadFuture,
    StreamingTextFallback, ValidationEvidence, VerifiedBlobSource,
};

const SYNTHETIC_MEDIA_TYPE: &str = "application/x-signalbox-synthetic";
const OTHER_SYNTHETIC_MEDIA_TYPE: &str = "application/x-signalbox-other";
const SYNTHETIC_SIGNATURE: &[u8] = b"SYN1";
const SYNTHETIC_BODY: &[u8] = b"SYN1 generated fixture bytes";
const TEXT_VIEW_NAME: &str = "body_text";
const STRUCTURED_VIEW_NAME: &str = "body_structure";
const MALFORMED_REASON: &str = "invalid_structure";
const EMPTY_OPTIONS_SCHEMA: &str = r#"{"additionalProperties":false,"type":"object"}"#;

struct MemorySource {
    digest: FileDigest,
    bytes: Vec<u8>,
}

impl MemorySource {
    fn synthetic() -> Self {
        Self {
            digest: FileDigest::from_bytes([0x5a; 32]),
            bytes: SYNTHETIC_BODY.to_vec(),
        }
    }
}

impl VerifiedBlobSource for MemorySource {
    fn digest(&self) -> FileDigest {
        self.digest
    }

    fn byte_length(&self) -> NonZeroU64 {
        NonZeroU64::new(self.bytes.len() as u64).expect("the synthetic fixture is nonempty")
    }

    fn read_range(&self, offset: u64, length: NonZeroU64) -> SourceReadFuture<'_> {
        Box::pin(async move {
            let start = usize::try_from(offset).map_err(|_| SourceReadError::RangeOutOfBounds)?;
            let length =
                usize::try_from(length.get()).map_err(|_| SourceReadError::RangeOutOfBounds)?;
            let end = start
                .checked_add(length)
                .ok_or(SourceReadError::RangeOutOfBounds)?;
            self.bytes
                .get(start..end)
                .map(<[u8]>::to_vec)
                .ok_or(SourceReadError::RangeOutOfBounds)
        })
    }
}

#[derive(Clone, Copy)]
enum ValidationBehavior {
    Valid,
    OversizedMetadata,
    MalformedMetadata,
}

#[derive(Clone, Copy)]
enum ReadBehavior {
    Text,
    OversizedText,
    MalformedStructured,
    DuplicateStructuredMember,
    ContradictoryContinuation,
}

struct SyntheticProcessor {
    validation: ValidationBehavior,
    read: ReadBehavior,
}

impl SyntheticProcessor {
    const fn valid_text() -> Self {
        Self {
            validation: ValidationBehavior::Valid,
            read: ReadBehavior::Text,
        }
    }
}

impl FileMediaProcessor for SyntheticProcessor {
    fn probe<'a>(
        &'a self,
        _reader: &'a ReaderIdentity,
        source: &'a dyn VerifiedBlobSource,
        _cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorProbeOutput> {
        Box::pin(async move {
            let prefix_length = NonZeroU64::new(SYNTHETIC_SIGNATURE.len() as u64)
                .ok_or(ProcessorFailure::Failed)?;
            let prefix = source
                .read_range(0, prefix_length)
                .await
                .map_err(|_| ProcessorFailure::Failed)?;
            if prefix == SYNTHETIC_SIGNATURE {
                Ok(ProcessorProbeOutput::Candidate {
                    media_type: String::from(SYNTHETIC_MEDIA_TYPE),
                    strength: ProbeStrength::Strong,
                })
            } else {
                Ok(ProcessorProbeOutput::NoMatch)
            }
        })
    }

    fn validate<'a>(
        &'a self,
        _reader: &'a ReaderIdentity,
        request: FileMediaProviderValidationRequest,
        _source: &'a dyn VerifiedBlobSource,
        _cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorValidationOutput> {
        Box::pin(async move {
            let metadata_json = match self.validation {
                ValidationBehavior::Valid => String::from(r#"{"synthetic":true}"#),
                ValidationBehavior::OversizedMetadata => {
                    format!(r#"{{"filler":"{}"}}"#, "x".repeat(16_385))
                }
                ValidationBehavior::MalformedMetadata => {
                    String::from(r#"</tool><script>alert("injection")</script>"#)
                }
            };
            Ok(ProcessorValidationOutput::Validated {
                media_type: request.media_type.to_string(),
                evidence: request.evidence,
                metadata_json,
            })
        })
    }

    fn read<'a>(
        &'a self,
        _reader: &'a ReaderIdentity,
        _request: FileMediaProviderReadRequest,
        _source: &'a dyn VerifiedBlobSource,
        _cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorReadOutput> {
        Box::pin(async move {
            Ok(match self.read {
                ReadBehavior::Text => ProcessorReadOutput::Text {
                    body: String::from("synthetic admitted text"),
                    truncated: false,
                    cursor: None,
                },
                ReadBehavior::OversizedText => ProcessorReadOutput::Text {
                    body: "x".repeat(65),
                    truncated: false,
                    cursor: None,
                },
                ReadBehavior::MalformedStructured => ProcessorReadOutput::Structured {
                    body_json: String::from(r#"{"value":"</tool><script>","unterminated":true"#),
                    truncated: false,
                    cursor: None,
                },
                ReadBehavior::DuplicateStructuredMember => ProcessorReadOutput::Structured {
                    body_json: String::from(r#"{"kind":"safe","kind":"attacker"}"#),
                    truncated: false,
                    cursor: None,
                },
                ReadBehavior::ContradictoryContinuation => ProcessorReadOutput::Text {
                    body: String::from("synthetic admitted text"),
                    truncated: true,
                    cursor: None,
                },
            })
        })
    }
}

fn block_on_ready<Output>(future: impl Future<Output = Output>) -> Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(std::task::Waker::noop());
    match future.as_mut().poll(&mut context) {
        std::task::Poll::Ready(output) => output,
        std::task::Poll::Pending => panic!("memory-backed conformance future unexpectedly parked"),
    }
}

fn media_type(value: &str) -> CanonicalMediaType {
    CanonicalMediaType::from_str(value).expect("fixture media type is canonical")
}

fn text_view() -> ReadViewDeclaration {
    ReadViewDeclaration::try_new(
        ReadViewName::try_new(TEXT_VIEW_NAME).expect("fixture view name is valid"),
        String::from("Reads the bounded synthetic body."),
        CanonicalJsonObjectSchema::try_new(EMPTY_OPTIONS_SCHEMA)
            .expect("fixture schema is object-rooted"),
        ReadAccessPattern::Streaming,
        ReadViewBounds::Text {
            source_bytes: 1_024,
            output_bytes: 64,
        },
    )
    .expect("fixture view declaration is valid")
}

fn structured_view() -> ReadViewDeclaration {
    ReadViewDeclaration::try_new(
        ReadViewName::try_new(STRUCTURED_VIEW_NAME).expect("fixture view name is valid"),
        String::from("Reads bounded synthetic structure."),
        CanonicalJsonObjectSchema::try_new(EMPTY_OPTIONS_SCHEMA)
            .expect("fixture schema is object-rooted"),
        ReadAccessPattern::Streaming,
        ReadViewBounds::Structured {
            source_bytes: 1_024,
            output_bytes: 256,
            depth: 8,
            nodes: 32,
            string_bytes: 128,
        },
    )
    .expect("fixture view declaration is valid")
}

fn registry_with_view(view: ReadViewDeclaration) -> FileMediaRegistry {
    let provider =
        FileReaderProviderName::try_new("synthetic").expect("fixture provider name is valid");
    let reader = ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: provider.clone(),
        reader: FileReaderName::try_new("fixture").expect("fixture reader name is valid"),
        revision: FileReaderRevision::try_new("1").expect("fixture revision is valid"),
        media_types: vec![media_type(SYNTHETIC_MEDIA_TYPE)],
        probe: ProbeDeclaration::new(4, 0, 0, 4),
        views: vec![view],
        reason_codes: vec![ReasonCode::try_new(MALFORMED_REASON).expect("fixture reason is valid")],
        streaming_text_fallback: StreamingTextFallback::Disabled,
    })
    .expect("fixture reader declaration is nonempty");
    let declaration = FileMediaProviderDeclaration::try_new(provider, vec![reader])
        .expect("fixture provider owns its reader");
    FileMediaRegistry::try_new(
        vec![declaration],
        FileMediaCeilings::version_one(),
        ProcessorIsolation::Available,
    )
    .expect("fixture registry is conflict-free")
}

fn inspection_request(source: &MemorySource, declared: &str) -> InspectionRequest {
    InspectionRequest {
        source: FileUse::new(
            source.digest(),
            source.byte_length(),
            AttachmentKind::File,
            DeclaredMediaType::try_new(declared).expect("fixture declaration is bounded"),
            None,
        ),
        visible_part: None,
    }
}

fn inspect(
    registry: &FileMediaRegistry,
    processor: &SyntheticProcessor,
    source: &MemorySource,
    declared: &str,
) -> Result<FileInspection, FileMediaFailure> {
    block_on_ready(registry.inspect(
        processor,
        inspection_request(source, declared),
        source,
        &NeverCancelled,
    ))
}

/// INV-067: byte signatures, not caller metadata, select one reader.
#[test]
fn inv067_synthetic_signature_produces_validated_detection() {
    let source = MemorySource::synthetic();
    let registry = registry_with_view(text_view());

    let inspection = inspect(
        &registry,
        &SyntheticProcessor::valid_text(),
        &source,
        SYNTHETIC_MEDIA_TYPE,
    )
    .expect("synthetic bytes are inspected");

    let FileInspection::Validated(validated) = inspection else {
        panic!("strong synthetic bytes must validate");
    };
    assert_eq!(
        validated.detected_media_type(),
        &media_type(SYNTHETIC_MEDIA_TYPE)
    );
    assert_eq!(validated.validation(), ValidationEvidence::StrongSignature);
    assert_eq!(validated.source().digest(), source.digest());
}

/// INV-067: a caller declaration cannot override byte-derived detection.
#[test]
fn inv067_declared_type_disagreement_is_reported_without_fallback() {
    let source = MemorySource::synthetic();
    let registry = registry_with_view(text_view());

    let inspection = inspect(
        &registry,
        &SyntheticProcessor::valid_text(),
        &source,
        "text/plain",
    )
    .expect("synthetic bytes are inspected");

    assert_eq!(
        inspection,
        FileInspection::DeclaredMismatch {
            source: inspection_request(&source, "text/plain").source,
            declared: media_type("text/plain"),
            detected: media_type(SYNTHETIC_MEDIA_TYPE),
        }
    );
}

/// INV-068: oversized processor metadata never crosses the registry boundary.
#[test]
fn inv068_oversized_processor_metadata_is_sanitized_to_failure() {
    let source = MemorySource::synthetic();
    let registry = registry_with_view(text_view());
    let processor = SyntheticProcessor {
        validation: ValidationBehavior::OversizedMetadata,
        read: ReadBehavior::Text,
    };

    let outcome = inspect(&registry, &processor, &source, SYNTHETIC_MEDIA_TYPE);

    assert_eq!(outcome, Err(FileMediaFailure::ProcessorFailed));
}

/// INV-068: malformed injection-shaped processor metadata never propagates.
#[test]
fn inv068_malformed_injection_shaped_metadata_is_sanitized_to_failure() {
    let source = MemorySource::synthetic();
    let registry = registry_with_view(text_view());
    let processor = SyntheticProcessor {
        validation: ValidationBehavior::MalformedMetadata,
        read: ReadBehavior::Text,
    };

    let outcome = inspect(&registry, &processor, &source, SYNTHETIC_MEDIA_TYPE);

    assert_eq!(outcome, Err(FileMediaFailure::ProcessorFailed));
}

/// INV-068: an oversized processor text body never becomes a typed read result.
#[test]
fn inv068_oversized_processor_text_is_sanitized_to_failure() {
    let source = MemorySource::synthetic();
    let registry = registry_with_view(text_view());
    let processor = SyntheticProcessor {
        validation: ValidationBehavior::Valid,
        read: ReadBehavior::OversizedText,
    };
    let request = FileReadRequest {
        inspection: inspection_request(&source, SYNTHETIC_MEDIA_TYPE),
        view: ReadViewName::try_new(TEXT_VIEW_NAME).expect("fixture view name is valid"),
        options: serde_json::json!({}),
    };

    let outcome = block_on_ready(registry.read(&processor, request, &source, &NeverCancelled));

    assert_eq!(outcome, Err(FileMediaFailure::ProcessorFailed));
}

/// INV-068: malformed structured output carrying injection-shaped text is discarded.
#[test]
fn inv068_malformed_injection_shaped_structure_is_sanitized_to_failure() {
    let source = MemorySource::synthetic();
    let registry = registry_with_view(structured_view());
    let processor = SyntheticProcessor {
        validation: ValidationBehavior::Valid,
        read: ReadBehavior::MalformedStructured,
    };
    let request = FileReadRequest {
        inspection: inspection_request(&source, SYNTHETIC_MEDIA_TYPE),
        view: ReadViewName::try_new(STRUCTURED_VIEW_NAME).expect("fixture view name is valid"),
        options: serde_json::json!({}),
    };

    let outcome = block_on_ready(registry.read(&processor, request, &source, &NeverCancelled));

    assert_eq!(outcome, Err(FileMediaFailure::ProcessorFailed));
}

/// INV-068: duplicate structured members never cross the processor boundary.
#[test]
fn inv068_duplicate_structured_member_is_sanitized_to_failure() {
    let source = MemorySource::synthetic();
    let registry = registry_with_view(structured_view());
    let processor = SyntheticProcessor {
        validation: ValidationBehavior::Valid,
        read: ReadBehavior::DuplicateStructuredMember,
    };
    let request = FileReadRequest {
        inspection: inspection_request(&source, SYNTHETIC_MEDIA_TYPE),
        view: ReadViewName::try_new(STRUCTURED_VIEW_NAME).expect("fixture view name is valid"),
        options: serde_json::json!({}),
    };

    let outcome = block_on_ready(registry.read(&processor, request, &source, &NeverCancelled));

    assert_eq!(outcome, Err(FileMediaFailure::ProcessorFailed));
}

/// INV-068: contradictory continuation facts from a processor do not enter the
/// sanitized read-result type.
#[test]
fn inv068_contradictory_processor_continuation_is_sanitized_to_failure() {
    let source = MemorySource::synthetic();
    let registry = registry_with_view(text_view());
    let processor = SyntheticProcessor {
        validation: ValidationBehavior::Valid,
        read: ReadBehavior::ContradictoryContinuation,
    };
    let request = FileReadRequest {
        inspection: inspection_request(&source, SYNTHETIC_MEDIA_TYPE),
        view: ReadViewName::try_new(TEXT_VIEW_NAME).expect("fixture view name is valid"),
        options: serde_json::json!({}),
    };

    let outcome = block_on_ready(registry.read(&processor, request, &source, &NeverCancelled));

    assert_eq!(outcome, Err(FileMediaFailure::ProcessorFailed));
}

struct SourceFailureProcessor;

impl FileMediaProcessor for SourceFailureProcessor {
    fn probe<'a>(
        &'a self,
        _reader: &'a ReaderIdentity,
        _source: &'a dyn VerifiedBlobSource,
        _cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorProbeOutput> {
        Box::pin(async { Err(SourceReadError::Unavailable.into()) })
    }

    fn validate<'a>(
        &'a self,
        _reader: &'a ReaderIdentity,
        _request: FileMediaProviderValidationRequest,
        _source: &'a dyn VerifiedBlobSource,
        _cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorValidationOutput> {
        Box::pin(async { Err(ProcessorFailure::Failed.into()) })
    }

    fn read<'a>(
        &'a self,
        _reader: &'a ReaderIdentity,
        _request: FileMediaProviderReadRequest,
        _source: &'a dyn VerifiedBlobSource,
        _cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorReadOutput> {
        Box::pin(async { Err(ProcessorFailure::Failed.into()) })
    }
}

#[test]
fn processor_boundary_preserves_verified_source_unavailability() {
    let source = MemorySource::synthetic();
    let registry = registry_with_view(text_view());

    let outcome = block_on_ready(registry.inspect(
        &SourceFailureProcessor,
        inspection_request(&source, SYNTHETIC_MEDIA_TYPE),
        &source,
        &NeverCancelled,
    ));

    assert_eq!(outcome, Err(FileMediaFailure::BlobUnavailable));
}

#[test]
fn duplicate_static_media_claim_is_rejected() {
    let first_provider = provider_declaration("first", SYNTHETIC_MEDIA_TYPE);
    let second_provider = provider_declaration("second", SYNTHETIC_MEDIA_TYPE);

    let outcome = FileMediaRegistry::try_new(
        vec![first_provider, second_provider],
        FileMediaCeilings::version_one(),
        ProcessorIsolation::Available,
    );

    assert!(outcome.is_err());
}

fn provider_declaration(name: &str, owned_media_type: &str) -> FileMediaProviderDeclaration {
    let provider = FileReaderProviderName::try_new(name).expect("fixture provider name is valid");
    let reader = ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: provider.clone(),
        reader: FileReaderName::try_new("fixture").expect("fixture reader name is valid"),
        revision: FileReaderRevision::try_new("1").expect("fixture revision is valid"),
        media_types: vec![media_type(owned_media_type)],
        probe: ProbeDeclaration::new(4, 0, 0, 4),
        views: vec![text_view()],
        reason_codes: vec![ReasonCode::try_new(MALFORMED_REASON).expect("fixture reason is valid")],
        streaming_text_fallback: StreamingTextFallback::Disabled,
    })
    .expect("fixture reader declaration is nonempty");
    FileMediaProviderDeclaration::try_new(provider, vec![reader])
        .expect("fixture provider owns its reader")
}

#[test]
fn distinct_static_media_claims_are_admitted() {
    let first_provider = provider_declaration("first", SYNTHETIC_MEDIA_TYPE);
    let second_provider = provider_declaration("second", OTHER_SYNTHETIC_MEDIA_TYPE);

    let registry = FileMediaRegistry::try_new(
        vec![second_provider, first_provider],
        FileMediaCeilings::version_one(),
        ProcessorIsolation::Available,
    )
    .expect("distinct media claims are conflict-free");

    assert_eq!(registry.providers()[0].provider().as_str(), "first");
    assert_eq!(registry.providers()[1].provider().as_str(), "second");
}

#[test]
fn provider_reader_inventory_is_canonically_sorted() {
    let provider =
        FileReaderProviderName::try_new("synthetic").expect("fixture provider name is valid");
    let second = reader_declaration(&provider, "second", OTHER_SYNTHETIC_MEDIA_TYPE);
    let first = reader_declaration(&provider, "first", SYNTHETIC_MEDIA_TYPE);
    let declaration = FileMediaProviderDeclaration::try_new(provider, vec![second, first])
        .expect("fixture provider owns both readers");

    let registry = FileMediaRegistry::try_new(
        vec![declaration],
        FileMediaCeilings::version_one(),
        ProcessorIsolation::Available,
    )
    .expect("distinct reader claims are conflict-free");

    assert_eq!(
        registry.providers()[0].readers()[0]
            .identity()
            .reader()
            .as_str(),
        "first"
    );
    assert_eq!(
        registry.providers()[0].readers()[1]
            .identity()
            .reader()
            .as_str(),
        "second"
    );
}

#[test]
fn oversized_provider_reader_inventory_is_rejected() {
    let provider =
        FileReaderProviderName::try_new("synthetic").expect("fixture provider name is valid");
    let readers = (0..257)
        .map(|index| {
            let reader = format!("reader-{index:03}");
            let owned_media_type = format!("application/x-synthetic-{index:03}");
            reader_declaration(&provider, &reader, &owned_media_type)
        })
        .collect();
    let declaration = FileMediaProviderDeclaration::try_new(provider, readers)
        .expect("the provider constructor defers inventory limits to the registry");

    let outcome = FileMediaRegistry::try_new(
        vec![declaration],
        FileMediaCeilings::version_one(),
        ProcessorIsolation::Available,
    );

    assert!(matches!(
        outcome,
        Err(signalbox_file_media_runtime::FileMediaRegistryConstructionError::Inventory)
    ));
}

#[test]
fn read_view_source_work_above_the_compiled_ceiling_is_rejected() {
    let ceilings = FileMediaCeilings::version_one();
    let source_bytes = ceilings
        .read_source_bytes
        .checked_add(1)
        .expect("the fixture ceiling leaves room for one excessive byte");
    let view = bounded_text_view(ReadAccessPattern::Streaming, source_bytes);

    let outcome = registry_outcome_with_view(view, ceilings);

    assert!(matches!(
        outcome,
        Err(signalbox_file_media_runtime::FileMediaRegistryConstructionError::ViewBounds)
    ));
}

#[test]
fn random_access_range_fanout_above_the_compiled_ceiling_is_rejected() {
    let ceilings = FileMediaCeilings::version_one();
    let maximum_ranges = ceilings
        .read_ranges
        .checked_add(1)
        .expect("the fixture ceiling leaves room for one excessive range");
    let view = bounded_text_view(ReadAccessPattern::RandomAccess { maximum_ranges }, 1_024);

    let outcome = registry_outcome_with_view(view, ceilings);

    assert!(matches!(
        outcome,
        Err(signalbox_file_media_runtime::FileMediaRegistryConstructionError::ViewBounds)
    ));
}

fn bounded_text_view(access: ReadAccessPattern, source_bytes: u64) -> ReadViewDeclaration {
    ReadViewDeclaration::try_new(
        ReadViewName::try_new(TEXT_VIEW_NAME).expect("fixture view name is valid"),
        String::from("Reads the bounded synthetic body."),
        CanonicalJsonObjectSchema::try_new(EMPTY_OPTIONS_SCHEMA)
            .expect("fixture schema is object-rooted"),
        access,
        ReadViewBounds::Text {
            source_bytes,
            output_bytes: 64,
        },
    )
    .expect("the declaration constructor defers resource checks to the registry")
}

fn registry_outcome_with_view(
    view: ReadViewDeclaration,
    ceilings: FileMediaCeilings,
) -> Result<FileMediaRegistry, signalbox_file_media_runtime::FileMediaRegistryConstructionError> {
    let provider =
        FileReaderProviderName::try_new("synthetic").expect("fixture provider name is valid");
    let reader = reader_declaration_with_view(provider.clone(), view);
    let declaration = FileMediaProviderDeclaration::try_new(provider, vec![reader])
        .expect("fixture provider owns its reader");
    FileMediaRegistry::try_new(vec![declaration], ceilings, ProcessorIsolation::Available)
}

fn reader_declaration_with_view(
    provider: FileReaderProviderName,
    view: ReadViewDeclaration,
) -> ReaderDeclaration {
    ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider,
        reader: FileReaderName::try_new("fixture").expect("fixture reader name is valid"),
        revision: FileReaderRevision::try_new("1").expect("fixture revision is valid"),
        media_types: vec![media_type(SYNTHETIC_MEDIA_TYPE)],
        probe: ProbeDeclaration::new(4, 0, 0, 4),
        views: vec![view],
        reason_codes: vec![ReasonCode::try_new(MALFORMED_REASON).expect("fixture reason is valid")],
        streaming_text_fallback: StreamingTextFallback::Disabled,
    })
    .expect("fixture reader declaration is nonempty")
}

fn reader_declaration(
    provider: &FileReaderProviderName,
    reader: &str,
    owned_media_type: &str,
) -> ReaderDeclaration {
    ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: provider.clone(),
        reader: FileReaderName::try_new(reader).expect("fixture reader name is valid"),
        revision: FileReaderRevision::try_new("1").expect("fixture revision is valid"),
        media_types: vec![media_type(owned_media_type)],
        probe: ProbeDeclaration::new(4, 0, 0, 4),
        views: vec![text_view()],
        reason_codes: vec![ReasonCode::try_new(MALFORMED_REASON).expect("fixture reason is valid")],
        streaming_text_fallback: StreamingTextFallback::Disabled,
    })
    .expect("fixture reader declaration is nonempty")
}
