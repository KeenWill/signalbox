#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "conformance fixtures use explicit construction and outcome expectations"
)]

use std::{future::Future, num::NonZeroU64, str::FromStr};

use signalbox_file_media_runtime::{
    AttachmentKind, CancellationSignal, CanonicalJsonObjectSchema, CanonicalMediaType,
    DeclaredMediaType, FileDigest, FileInspection, FileMediaCeilings, FileMediaFailure,
    FileMediaProcessor, FileMediaProcessorFuture, FileMediaProviderDeclaration,
    FileMediaProviderReadRequest, FileMediaProviderValidationRequest, FileMediaRegistry,
    FileReadInput, FileReadRequest, FileReadResult, FileReaderName, FileReaderProviderName,
    FileReaderRevision, FileUse, InspectionRequest, MAX_VALIDATION_RANGES,
    MAX_VALIDATION_SOURCE_BYTES, NeverCancelled, ProbeDeclaration, ProbeDeclarationInput,
    ProbeStrength, ProcessorFailure, ProcessorIsolation, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, ReadAccessPattern, ReadViewBounds, ReadViewDeclaration,
    ReadViewName, ReaderDeclaration, ReaderDeclarationInput, ReaderIdentity, ReasonCode,
    SourceReadError, SourceReadFuture, StreamingTextFallback, ValidationDeclaration,
    ValidationEvidence, VerifiedBlobSource,
};

const SYNTHETIC_MEDIA_TYPE: &str = "application/x-signalbox-synthetic";
const OTHER_SYNTHETIC_MEDIA_TYPE: &str = "application/x-signalbox-other";
const SYNTHETIC_SIGNATURE: &[u8] = b"SYN1";
const SYNTHETIC_BODY: &[u8] = b"SYN1 generated fixture bytes";
const TEXT_VIEW_NAME: &str = "body_text";
const STRUCTURED_VIEW_NAME: &str = "body_structure";
const MALFORMED_REASON: &str = "invalid_structure";
const EMPTY_OPTIONS_SCHEMA: &str = r#"{"additionalProperties":false,"type":"object"}"#;
/// Evidence every `SelectionProcessor` candidate reports, inside the fixture probe budget.
const SELECTION_PROBE_EVIDENCE_BYTES: u64 = 4;

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
    ValidWithEnvelope { source_bytes: u64, ranges: u32 },
    OversizedMetadata,
    MalformedMetadata,
}

#[derive(Clone, Copy)]
enum ReadBehavior {
    Text,
    TextRequiringSourceBytes(u64),
    InvalidViewArguments,
    SourceTooLarge,
    OversizedText,
    MalformedStructured,
    DuplicateStructuredMember,
    CanonicalizedStructuredOverflow,
    ExcessiveContainerEntries,
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
                    evidence_bytes: SYNTHETIC_SIGNATURE.len() as u64,
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
            let expected_envelope = match self.validation {
                ValidationBehavior::ValidWithEnvelope {
                    source_bytes,
                    ranges,
                } => (source_bytes, ranges),
                ValidationBehavior::Valid
                | ValidationBehavior::OversizedMetadata
                | ValidationBehavior::MalformedMetadata => {
                    (MAX_VALIDATION_SOURCE_BYTES, MAX_VALIDATION_RANGES)
                }
            };
            if (request.maximum_source_bytes, request.maximum_ranges) != expected_envelope {
                return Err(ProcessorFailure::Failed.into());
            }
            let metadata_json = match self.validation {
                ValidationBehavior::Valid | ValidationBehavior::ValidWithEnvelope { .. } => {
                    String::from(r#"{"synthetic":true}"#)
                }
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
        request: FileMediaProviderReadRequest,
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
                ReadBehavior::TextRequiringSourceBytes(source_bytes) => {
                    if request.maximum_source_bytes != source_bytes {
                        return Err(ProcessorFailure::Failed.into());
                    }
                    ProcessorReadOutput::Text {
                        body: String::from("synthetic admitted text"),
                        truncated: false,
                        cursor: None,
                    }
                }
                ReadBehavior::InvalidViewArguments => ProcessorReadOutput::InvalidViewArguments,
                ReadBehavior::SourceTooLarge => ProcessorReadOutput::SourceTooLarge {
                    maximum_bytes: 1_024,
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
                ReadBehavior::CanonicalizedStructuredOverflow => ProcessorReadOutput::Structured {
                    body_json: String::from("1e400"),
                    truncated: false,
                    cursor: None,
                },
                ReadBehavior::ExcessiveContainerEntries => ProcessorReadOutput::Structured {
                    body_json: String::from(r#"{"entries":[{},{}]}"#),
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
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("the conformance runtime is constructed")
        .block_on(future)
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
        ReadAccessPattern::Streaming { maximum_ranges: 16 },
        ReadViewBounds::Text {
            source_bytes: 1_024,
            output_bytes: 64,
        },
    )
    .expect("fixture view declaration is valid")
}

fn structured_view() -> ReadViewDeclaration {
    structured_view_with_output_bytes(256)
}

fn structured_view_with_output_bytes(output_bytes: usize) -> ReadViewDeclaration {
    ReadViewDeclaration::try_new(
        ReadViewName::try_new(STRUCTURED_VIEW_NAME).expect("fixture view name is valid"),
        String::from("Reads bounded synthetic structure."),
        CanonicalJsonObjectSchema::try_new(EMPTY_OPTIONS_SCHEMA)
            .expect("fixture schema is object-rooted"),
        ReadAccessPattern::Streaming { maximum_ranges: 16 },
        ReadViewBounds::Structured {
            source_bytes: 1_024,
            output_bytes,
            depth: 8,
            nodes: 32,
            string_bytes: output_bytes.min(128),
        },
    )
    .expect("fixture view declaration is valid")
}

fn registry_with_view(view: ReadViewDeclaration) -> FileMediaRegistry {
    registry_with_view_result(view).expect("fixture registry is conflict-free")
}

fn registry_with_view_result(
    view: ReadViewDeclaration,
) -> Result<FileMediaRegistry, signalbox_file_media_runtime::FileMediaRegistryConstructionError> {
    registry_with_view_validation_and_ceilings(
        view,
        ValidationDeclaration::new(MAX_VALIDATION_SOURCE_BYTES, MAX_VALIDATION_RANGES),
        FileMediaCeilings::version_one(),
    )
}

fn registry_with_view_and_container_entries(
    view: ReadViewDeclaration,
    observed_container_entries: Option<u64>,
) -> Result<FileMediaRegistry, signalbox_file_media_runtime::FileMediaRegistryConstructionError> {
    registry_with_view_parts(
        view,
        ValidationDeclaration::new(MAX_VALIDATION_SOURCE_BYTES, MAX_VALIDATION_RANGES),
        FileMediaCeilings::version_one(),
        observed_container_entries,
    )
}

fn registry_with_view_validation_and_ceilings(
    view: ReadViewDeclaration,
    validation: ValidationDeclaration,
    ceilings: FileMediaCeilings,
) -> Result<FileMediaRegistry, signalbox_file_media_runtime::FileMediaRegistryConstructionError> {
    registry_with_view_parts(view, validation, ceilings, None)
}

fn registry_with_view_parts(
    view: ReadViewDeclaration,
    validation: ValidationDeclaration,
    ceilings: FileMediaCeilings,
    observed_container_entries: Option<u64>,
) -> Result<FileMediaRegistry, signalbox_file_media_runtime::FileMediaRegistryConstructionError> {
    let provider =
        FileReaderProviderName::try_new("synthetic").expect("fixture provider name is valid");
    let reader = ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: provider.clone(),
        reader: FileReaderName::try_new("fixture").expect("fixture reader name is valid"),
        revision: FileReaderRevision::try_new("1").expect("fixture revision is valid"),
        media_types: vec![media_type(SYNTHETIC_MEDIA_TYPE)],
        probe: ProbeDeclaration::new(ProbeDeclarationInput {
            prefix_bytes: 4,
            suffix_bytes: 0,
            range_count: 0,
            cumulative_bytes: 4,
        }),
        validation,
        views: vec![view],
        reason_codes: vec![ReasonCode::try_new(MALFORMED_REASON).expect("fixture reason is valid")],
        streaming_text_fallback: StreamingTextFallback::Disabled,
    })
    .expect("fixture reader declaration is nonempty");
    let declaration = FileMediaProviderDeclaration::try_new_with_container_entries(
        provider,
        vec![reader],
        observed_container_entries,
    )
    .expect("fixture provider owns its reader");
    FileMediaRegistry::try_new(vec![declaration], ceilings, ProcessorIsolation::Available)
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
    processor: &dyn FileMediaProcessor,
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

#[derive(Clone, Copy)]
enum SelectionProbe {
    Strong,
    ProvisionalStructural,
    Structural,
    Malformed,
    NoMatch,
}

#[derive(Clone, Copy)]
enum SelectionValidation {
    Validated,
    Malformed,
    EncryptedOrLocked,
    NoMatch,
    DeclaredMissStreamingValidated,
}

struct SelectionProcessor {
    probe: SelectionProbe,
    validation: SelectionValidation,
}

impl SelectionProcessor {
    fn media_type(&self) -> CanonicalMediaType {
        media_type(SYNTHETIC_MEDIA_TYPE)
    }

    fn malformed_reason(&self) -> ReasonCode {
        ReasonCode::try_new(MALFORMED_REASON).expect("fixture reason is valid")
    }
}

impl FileMediaProcessor for SelectionProcessor {
    fn probe<'a>(
        &'a self,
        _reader: &'a ReaderIdentity,
        _source: &'a dyn VerifiedBlobSource,
        _cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorProbeOutput> {
        Box::pin(async move {
            Ok(match self.probe {
                SelectionProbe::Strong => ProcessorProbeOutput::Candidate {
                    media_type: String::from(SYNTHETIC_MEDIA_TYPE),
                    strength: ProbeStrength::Strong,
                    evidence_bytes: SELECTION_PROBE_EVIDENCE_BYTES,
                },
                SelectionProbe::ProvisionalStructural => ProcessorProbeOutput::Candidate {
                    media_type: String::from(SYNTHETIC_MEDIA_TYPE),
                    strength: ProbeStrength::ProvisionalStructuralCandidate,
                    evidence_bytes: SELECTION_PROBE_EVIDENCE_BYTES,
                },
                SelectionProbe::Structural => ProcessorProbeOutput::Candidate {
                    media_type: String::from(SYNTHETIC_MEDIA_TYPE),
                    strength: ProbeStrength::StructuralCandidate,
                    evidence_bytes: SELECTION_PROBE_EVIDENCE_BYTES,
                },
                SelectionProbe::Malformed => ProcessorProbeOutput::RecognizedMalformed {
                    media_type: String::from(SYNTHETIC_MEDIA_TYPE),
                    reason_code: String::from(MALFORMED_REASON),
                },
                SelectionProbe::NoMatch => ProcessorProbeOutput::NoMatch,
            })
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
            Ok(match self.validation {
                SelectionValidation::Validated => ProcessorValidationOutput::Validated {
                    media_type: request.media_type.to_string(),
                    evidence: request.evidence,
                    metadata_json: String::from(r#"{"synthetic":true}"#),
                },
                SelectionValidation::Malformed => ProcessorValidationOutput::Malformed {
                    media_type: request.media_type.to_string(),
                    reason_code: String::from(MALFORMED_REASON),
                },
                SelectionValidation::EncryptedOrLocked => {
                    ProcessorValidationOutput::EncryptedOrLocked {
                        media_type: request.media_type.to_string(),
                    }
                }
                SelectionValidation::NoMatch => ProcessorValidationOutput::NoMatch,
                SelectionValidation::DeclaredMissStreamingValidated => match request.evidence {
                    ValidationEvidence::DeclaredCandidateStructurallyValidated => {
                        ProcessorValidationOutput::NoMatch
                    }
                    ValidationEvidence::StreamingTextValidation => {
                        ProcessorValidationOutput::Validated {
                            media_type: request.media_type.to_string(),
                            evidence: request.evidence,
                            metadata_json: String::from(r#"{"synthetic":true}"#),
                        }
                    }
                    ValidationEvidence::StrongSignature
                    | ValidationEvidence::StructuralValidation => {
                        return Err(ProcessorFailure::Failed.into());
                    }
                },
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
        Box::pin(async { Err(ProcessorFailure::Failed.into()) })
    }
}

struct ProvisionalCollisionProcessor;

impl FileMediaProcessor for ProvisionalCollisionProcessor {
    fn probe<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        _source: &'a dyn VerifiedBlobSource,
        _cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorProbeOutput> {
        Box::pin(async move {
            let media_type = match reader.provider().as_str() {
                "first" => SYNTHETIC_MEDIA_TYPE,
                "second" => OTHER_SYNTHETIC_MEDIA_TYPE,
                _ => return Err(ProcessorFailure::Failed.into()),
            };
            Ok(ProcessorProbeOutput::Candidate {
                media_type: String::from(media_type),
                strength: ProbeStrength::ProvisionalStructuralCandidate,
                evidence_bytes: 4,
            })
        })
    }

    fn validate<'a>(
        &'a self,
        _reader: &'a ReaderIdentity,
        _request: FileMediaProviderValidationRequest,
        _source: &'a dyn VerifiedBlobSource,
        _cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProcessorFuture<'a, ProcessorValidationOutput> {
        Box::pin(async { Ok(ProcessorValidationOutput::NoMatch) })
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

fn selection_registry(
    owned_media_type: &str,
    streaming_text_fallback: StreamingTextFallback,
) -> FileMediaRegistry {
    selection_registry_with_ceilings(
        owned_media_type,
        streaming_text_fallback,
        FileMediaCeilings::version_one(),
    )
}

fn selection_registry_with_ceilings(
    owned_media_type: &str,
    streaming_text_fallback: StreamingTextFallback,
    ceilings: FileMediaCeilings,
) -> FileMediaRegistry {
    selection_registry_with_parts(
        owned_media_type,
        streaming_text_fallback,
        ceilings,
        MAX_VALIDATION_SOURCE_BYTES,
    )
}

fn selection_registry_with_parts(
    owned_media_type: &str,
    streaming_text_fallback: StreamingTextFallback,
    ceilings: FileMediaCeilings,
    validation_source_bytes: u64,
) -> FileMediaRegistry {
    let provider =
        FileReaderProviderName::try_new("selection").expect("fixture provider name is valid");
    let reader = ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: provider.clone(),
        reader: FileReaderName::try_new("fixture").expect("fixture reader name is valid"),
        revision: FileReaderRevision::try_new("1").expect("fixture revision is valid"),
        media_types: vec![media_type(owned_media_type)],
        probe: ProbeDeclaration::new(ProbeDeclarationInput {
            prefix_bytes: 4,
            suffix_bytes: 0,
            range_count: 0,
            cumulative_bytes: 8,
        }),
        validation: signalbox_file_media_runtime::ValidationDeclaration::new(
            validation_source_bytes,
            MAX_VALIDATION_RANGES,
        ),
        views: vec![text_view()],
        reason_codes: vec![ReasonCode::try_new(MALFORMED_REASON).expect("fixture reason is valid")],
        streaming_text_fallback,
    })
    .expect("fixture reader declaration is valid");
    let declaration = FileMediaProviderDeclaration::try_new(provider, vec![reader])
        .expect("fixture provider owns its reader");
    FileMediaRegistry::try_new(vec![declaration], ceilings, ProcessorIsolation::Available)
        .expect("selection fixture registry is valid")
}

fn selection_inspection(
    probe: SelectionProbe,
    validation: SelectionValidation,
    registry_media_type: &str,
    declared_media_type: &str,
    streaming_text_fallback: StreamingTextFallback,
) -> Result<FileInspection, FileMediaFailure> {
    let source = MemorySource::synthetic();
    let registry = selection_registry(registry_media_type, streaming_text_fallback);
    let processor = SelectionProcessor { probe, validation };
    inspect(&registry, &processor, &source, declared_media_type)
}

fn selection_inspection_with_processor(
    processor: &SelectionProcessor,
    registry_media_type: &str,
    declared_media_type: &str,
    streaming_text_fallback: StreamingTextFallback,
) -> Result<FileInspection, FileMediaFailure> {
    let source = MemorySource::synthetic();
    let registry = selection_registry(registry_media_type, streaming_text_fallback);
    inspect(&registry, processor, &source, declared_media_type)
}

fn validated_evidence(inspection: FileInspection) -> ValidationEvidence {
    let FileInspection::Validated(validated) = inspection else {
        panic!("fixture must produce a validated inspection");
    };
    validated.validation()
}

#[test]
fn structural_candidate_precedes_declared_candidate() {
    let inspection = selection_inspection(
        SelectionProbe::Structural,
        SelectionValidation::Validated,
        SYNTHETIC_MEDIA_TYPE,
        SYNTHETIC_MEDIA_TYPE,
        StreamingTextFallback::Disabled,
    )
    .expect("structural candidate validates");

    assert_eq!(
        validated_evidence(inspection),
        ValidationEvidence::StructuralValidation
    );
}

#[test]
fn structural_candidate_with_actual_probe_inside_validation_ceiling_is_retained() {
    let source = MemorySource::synthetic();
    let mut ceilings = FileMediaCeilings::version_one();
    ceilings.validation_source_bytes = 4;
    let registry = selection_registry_with_ceilings(
        SYNTHETIC_MEDIA_TYPE,
        StreamingTextFallback::Disabled,
        ceilings,
    );
    let processor = SelectionProcessor {
        probe: SelectionProbe::Structural,
        validation: SelectionValidation::Validated,
    };

    let inspection = inspect(&registry, &processor, &source, SYNTHETIC_MEDIA_TYPE)
        .expect("actual probe evidence within the validation ceiling is retained");

    assert_eq!(
        validated_evidence(inspection),
        ValidationEvidence::StructuralValidation
    );
}

/// The retained-candidate filter names the envelope validation will actually grant, so a
/// reader whose declared validation envelope sits below the deployment ceiling cannot keep
/// evidence that envelope never covers.
#[test]
fn probe_evidence_outside_the_reader_validation_envelope_is_dropped() {
    let source = MemorySource::synthetic();
    let ceilings = FileMediaCeilings::version_one();
    // Only the reader's own envelope excludes this evidence; the deployment ceiling admits it.
    assert!(ceilings.validation_source_bytes > SELECTION_PROBE_EVIDENCE_BYTES);
    let registry = selection_registry_with_parts(
        SYNTHETIC_MEDIA_TYPE,
        StreamingTextFallback::Disabled,
        ceilings,
        SELECTION_PROBE_EVIDENCE_BYTES - 1,
    );
    let processor = SelectionProcessor {
        probe: SelectionProbe::Strong,
        validation: SelectionValidation::Validated,
    };

    let outcome = inspect(&registry, &processor, &source, "unknown")
        .expect("a dropped candidate leaves an ordinary inspection outcome");

    let FileInspection::Unknown { .. } = outcome else {
        panic!("evidence outside the reader validation envelope must not be retained");
    };
}

#[test]
fn declared_candidate_follows_probe_miss() {
    let inspection = selection_inspection(
        SelectionProbe::NoMatch,
        SelectionValidation::Validated,
        SYNTHETIC_MEDIA_TYPE,
        SYNTHETIC_MEDIA_TYPE,
        StreamingTextFallback::Disabled,
    )
    .expect("declared candidate validates");

    assert_eq!(
        validated_evidence(inspection),
        ValidationEvidence::DeclaredCandidateStructurallyValidated
    );
}

#[test]
fn streaming_text_fallback_follows_probe_and_declaration_miss() {
    let inspection = selection_inspection(
        SelectionProbe::NoMatch,
        SelectionValidation::Validated,
        "text/plain",
        "unknown",
        StreamingTextFallback::Enabled,
    )
    .expect("streaming text fallback validates");

    assert_eq!(
        validated_evidence(inspection),
        ValidationEvidence::StreamingTextValidation
    );
}

#[test]
fn oversized_streaming_text_fallback_becomes_unknown_before_validation() {
    let source = MemorySource::synthetic();
    let mut ceilings = FileMediaCeilings::version_one();
    ceilings.validation_source_bytes = 1;
    let registry =
        selection_registry_with_ceilings("text/plain", StreamingTextFallback::Enabled, ceilings);
    let processor = SelectionProcessor {
        probe: SelectionProbe::NoMatch,
        validation: SelectionValidation::Validated,
    };

    let outcome = inspect(&registry, &processor, &source, "unknown")
        .expect("oversized streaming fallback becomes unknown");

    let FileInspection::Unknown { .. } = outcome else {
        panic!("oversized streaming fallback must produce unknown inspection");
    };
}

#[test]
fn probe_recognized_malformed_is_terminal() {
    let processor = SelectionProcessor {
        probe: SelectionProbe::Malformed,
        validation: SelectionValidation::Validated,
    };
    let expected_media_type = processor.media_type();
    let expected_reason = processor.malformed_reason();
    let outcome = selection_inspection_with_processor(
        &processor,
        SYNTHETIC_MEDIA_TYPE,
        SYNTHETIC_MEDIA_TYPE,
        StreamingTextFallback::Disabled,
    )
    .expect("recognized malformed probe is terminal");

    let FileInspection::Malformed {
        media_type: detected_media_type,
        reason_code,
        ..
    } = outcome
    else {
        panic!("recognized malformed probe must produce malformed inspection");
    };
    assert_eq!(detected_media_type, expected_media_type);
    assert_eq!(reason_code, expected_reason);
}

#[test]
fn validation_malformed_is_terminal_for_strong_evidence() {
    let processor = SelectionProcessor {
        probe: SelectionProbe::Strong,
        validation: SelectionValidation::Malformed,
    };
    let expected_media_type = processor.media_type();
    let expected_reason = processor.malformed_reason();
    let outcome = selection_inspection_with_processor(
        &processor,
        SYNTHETIC_MEDIA_TYPE,
        SYNTHETIC_MEDIA_TYPE,
        StreamingTextFallback::Disabled,
    )
    .expect("malformed strong validation is terminal");

    let FileInspection::Malformed {
        media_type: detected_media_type,
        reason_code,
        ..
    } = outcome
    else {
        panic!("malformed strong validation must produce malformed inspection");
    };
    assert_eq!(detected_media_type, expected_media_type);
    assert_eq!(reason_code, expected_reason);
}

#[test]
fn validation_encrypted_is_terminal_for_strong_evidence() {
    let processor = SelectionProcessor {
        probe: SelectionProbe::Strong,
        validation: SelectionValidation::EncryptedOrLocked,
    };
    let expected_media_type = processor.media_type();
    let outcome = selection_inspection_with_processor(
        &processor,
        SYNTHETIC_MEDIA_TYPE,
        SYNTHETIC_MEDIA_TYPE,
        StreamingTextFallback::Disabled,
    )
    .expect("encrypted strong validation is terminal");

    let FileInspection::EncryptedOrLocked {
        media_type: detected_media_type,
        ..
    } = outcome
    else {
        panic!("encrypted strong validation must produce encrypted inspection");
    };
    assert_eq!(detected_media_type, expected_media_type);
}

#[test]
fn strong_validation_no_match_is_processor_failure() {
    let outcome = selection_inspection(
        SelectionProbe::Strong,
        SelectionValidation::NoMatch,
        SYNTHETIC_MEDIA_TYPE,
        SYNTHETIC_MEDIA_TYPE,
        StreamingTextFallback::Disabled,
    );

    assert_eq!(outcome, Err(FileMediaFailure::ProcessorFailed));
}

#[test]
fn structural_validation_no_match_is_processor_failure() {
    let outcome = selection_inspection(
        SelectionProbe::Structural,
        SelectionValidation::NoMatch,
        SYNTHETIC_MEDIA_TYPE,
        SYNTHETIC_MEDIA_TYPE,
        StreamingTextFallback::Disabled,
    );

    assert_eq!(outcome, Err(FileMediaFailure::ProcessorFailed));
}

#[test]
fn provisional_structural_validation_no_match_resumes_fallback() {
    let outcome = selection_inspection(
        SelectionProbe::ProvisionalStructural,
        SelectionValidation::NoMatch,
        SYNTHETIC_MEDIA_TYPE,
        SYNTHETIC_MEDIA_TYPE,
        StreamingTextFallback::Disabled,
    )
    .expect("provisional structural validation miss resumes fallback");

    let FileInspection::Unknown { .. } = outcome else {
        panic!("provisional structural validation miss must become unknown");
    };
}

#[test]
fn all_provisional_collision_misses_resume_fallback() {
    let source = MemorySource::synthetic();
    let registry = FileMediaRegistry::try_new(
        vec![
            provider_declaration("first", SYNTHETIC_MEDIA_TYPE),
            provider_declaration("second", OTHER_SYNTHETIC_MEDIA_TYPE),
        ],
        FileMediaCeilings::version_one(),
        ProcessorIsolation::Available,
    )
    .expect("distinct provisional claims are registrable");

    let outcome = inspect(
        &registry,
        &ProvisionalCollisionProcessor,
        &source,
        "unknown",
    )
    .expect("all provisional collision misses resume fallback");

    let FileInspection::Unknown { .. } = outcome else {
        panic!("all provisional collision misses must become unknown");
    };
}

#[test]
fn declared_validation_no_match_becomes_unknown() {
    let outcome = selection_inspection(
        SelectionProbe::NoMatch,
        SelectionValidation::NoMatch,
        SYNTHETIC_MEDIA_TYPE,
        SYNTHETIC_MEDIA_TYPE,
        StreamingTextFallback::Disabled,
    )
    .expect("declared validation miss becomes unknown");

    let FileInspection::Unknown { .. } = outcome else {
        panic!("declared validation miss must produce unknown inspection");
    };
}

#[test]
fn declared_validation_miss_resumes_streaming_text_fallback() {
    let inspection = selection_inspection(
        SelectionProbe::NoMatch,
        SelectionValidation::DeclaredMissStreamingValidated,
        "text/plain",
        "text/plain",
        StreamingTextFallback::Enabled,
    )
    .expect("declared validation miss resumes streaming text fallback");

    assert_eq!(
        validated_evidence(inspection),
        ValidationEvidence::StreamingTextValidation
    );
}

#[test]
fn streaming_text_validation_no_match_becomes_unknown() {
    let outcome = selection_inspection(
        SelectionProbe::NoMatch,
        SelectionValidation::NoMatch,
        "text/plain",
        "unknown",
        StreamingTextFallback::Enabled,
    )
    .expect("streaming text validation miss becomes unknown");

    let FileInspection::Unknown { .. } = outcome else {
        panic!("streaming text validation miss must produce unknown inspection");
    };
}

/// byte signatures, not caller metadata, select one reader.
#[test]
fn synthetic_signature_produces_validated_detection() {
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

/// a caller declaration cannot override byte-derived detection.
#[test]
fn declared_type_disagreement_is_reported_without_fallback() {
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

/// oversized processor metadata never crosses the registry boundary.
#[test]
fn oversized_processor_metadata_is_sanitized_to_failure() {
    let source = MemorySource::synthetic();
    let registry = registry_with_view(text_view());
    let processor = SyntheticProcessor {
        validation: ValidationBehavior::OversizedMetadata,
        read: ReadBehavior::Text,
    };

    let outcome = inspect(&registry, &processor, &source, SYNTHETIC_MEDIA_TYPE);

    assert_eq!(outcome, Err(FileMediaFailure::ProcessorFailed));
}

/// malformed injection-shaped processor metadata never propagates.
#[test]
fn malformed_injection_shaped_metadata_is_sanitized_to_failure() {
    let source = MemorySource::synthetic();
    let registry = registry_with_view(text_view());
    let processor = SyntheticProcessor {
        validation: ValidationBehavior::MalformedMetadata,
        read: ReadBehavior::Text,
    };

    let outcome = inspect(&registry, &processor, &source, SYNTHETIC_MEDIA_TYPE);

    assert_eq!(outcome, Err(FileMediaFailure::ProcessorFailed));
}

/// an oversized processor text body never becomes a typed read result.
#[test]
fn oversized_processor_text_is_sanitized_to_failure() {
    let source = MemorySource::synthetic();
    let registry = registry_with_view(text_view());
    let processor = SyntheticProcessor {
        validation: ValidationBehavior::Valid,
        read: ReadBehavior::OversizedText,
    };
    let request = FileReadRequest {
        inspection: inspection_request(&source, SYNTHETIC_MEDIA_TYPE),
        view: ReadViewName::try_new(TEXT_VIEW_NAME).expect("fixture view name is valid"),
        input: FileReadInput::Initial {
            options: serde_json::json!({}),
        },
    };

    let outcome = block_on_ready(registry.read(&processor, request, &source, &NeverCancelled));

    assert_eq!(outcome, Err(FileMediaFailure::ProcessorFailed));
}

/// malformed structured output carrying injection-shaped text is discarded.
#[test]
fn malformed_injection_shaped_structure_is_sanitized_to_failure() {
    let source = MemorySource::synthetic();
    let registry = registry_with_view(structured_view());
    let processor = SyntheticProcessor {
        validation: ValidationBehavior::Valid,
        read: ReadBehavior::MalformedStructured,
    };
    let request = FileReadRequest {
        inspection: inspection_request(&source, SYNTHETIC_MEDIA_TYPE),
        view: ReadViewName::try_new(STRUCTURED_VIEW_NAME).expect("fixture view name is valid"),
        input: FileReadInput::Initial {
            options: serde_json::json!({}),
        },
    };

    let outcome = block_on_ready(registry.read(&processor, request, &source, &NeverCancelled));

    assert_eq!(outcome, Err(FileMediaFailure::ProcessorFailed));
}

/// duplicate structured members never cross the processor boundary.
#[test]
fn duplicate_structured_member_is_sanitized_to_failure() {
    let source = MemorySource::synthetic();
    let registry = registry_with_view(structured_view());
    let processor = SyntheticProcessor {
        validation: ValidationBehavior::Valid,
        read: ReadBehavior::DuplicateStructuredMember,
    };
    let request = FileReadRequest {
        inspection: inspection_request(&source, SYNTHETIC_MEDIA_TYPE),
        view: ReadViewName::try_new(STRUCTURED_VIEW_NAME).expect("fixture view name is valid"),
        input: FileReadInput::Initial {
            options: serde_json::json!({}),
        },
    };

    let outcome = block_on_ready(registry.read(&processor, request, &source, &NeverCancelled));

    assert_eq!(outcome, Err(FileMediaFailure::ProcessorFailed));
}

/// canonicalization cannot expand structured output past its declared bound.
#[test]
fn canonicalized_structured_bytes_are_rechecked() {
    let source = MemorySource::synthetic();
    let registry = registry_with_view(structured_view_with_output_bytes(5));
    let processor = SyntheticProcessor {
        validation: ValidationBehavior::Valid,
        read: ReadBehavior::CanonicalizedStructuredOverflow,
    };
    let request = FileReadRequest {
        inspection: inspection_request(&source, SYNTHETIC_MEDIA_TYPE),
        view: ReadViewName::try_new(STRUCTURED_VIEW_NAME).expect("fixture view name is valid"),
        input: FileReadInput::Initial {
            options: serde_json::json!({}),
        },
    };

    let outcome = block_on_ready(registry.read(&processor, request, &source, &NeverCancelled));

    assert_eq!(outcome, Err(FileMediaFailure::ProcessorFailed));
}

/// structured output cannot exceed its provider's declared container inventory.
#[test]
fn provider_container_entry_bound_is_enforced_on_read() {
    let source = MemorySource::synthetic();
    let registry = registry_with_view_and_container_entries(structured_view(), Some(1))
        .expect("fixture registry is conflict-free");
    let processor = SyntheticProcessor {
        validation: ValidationBehavior::Valid,
        read: ReadBehavior::ExcessiveContainerEntries,
    };
    let request = FileReadRequest {
        inspection: inspection_request(&source, SYNTHETIC_MEDIA_TYPE),
        view: ReadViewName::try_new(STRUCTURED_VIEW_NAME).expect("fixture view name is valid"),
        input: FileReadInput::Initial {
            options: serde_json::json!({}),
        },
    };

    let outcome = block_on_ready(registry.read(&processor, request, &source, &NeverCancelled));

    assert_eq!(outcome, Err(FileMediaFailure::ProcessorFailed));
}

#[test]
fn structured_view_reserves_processor_frame_escaping_space() {
    let view = structured_view_with_output_bytes(500 * 1_024 + 1);

    let outcome = registry_with_view_result(view);

    let Err(error) = outcome else {
        panic!("structured view above the processor-frame allowance must be rejected");
    };
    assert_eq!(
        error,
        signalbox_file_media_runtime::FileMediaRegistryConstructionError::ViewBounds
    );
}

/// contradictory continuation facts from a processor do not enter the
/// sanitized read-result type.
#[test]
fn contradictory_processor_continuation_is_sanitized_to_failure() {
    let source = MemorySource::synthetic();
    let registry = registry_with_view(text_view());
    let processor = SyntheticProcessor {
        validation: ValidationBehavior::Valid,
        read: ReadBehavior::ContradictoryContinuation,
    };
    let request = FileReadRequest {
        inspection: inspection_request(&source, SYNTHETIC_MEDIA_TYPE),
        view: ReadViewName::try_new(TEXT_VIEW_NAME).expect("fixture view name is valid"),
        input: FileReadInput::Initial {
            options: serde_json::json!({}),
        },
    };

    let outcome = block_on_ready(registry.read(&processor, request, &source, &NeverCancelled));

    assert_eq!(outcome, Err(FileMediaFailure::ProcessorFailed));
}

/// a processor cannot relabel a continuation cursor as invalid
/// model-supplied initial options.
#[test]
fn continuation_invalid_arguments_is_sanitized_to_failure() {
    let source = MemorySource::synthetic();
    let registry = registry_with_view(text_view());
    let processor = SyntheticProcessor {
        validation: ValidationBehavior::Valid,
        read: ReadBehavior::InvalidViewArguments,
    };
    let request = FileReadRequest {
        inspection: inspection_request(&source, SYNTHETIC_MEDIA_TYPE),
        view: ReadViewName::try_new(TEXT_VIEW_NAME).expect("fixture view name is valid"),
        input: FileReadInput::Continuation {
            cursor: signalbox_file_media_runtime::ReadContinuationCursor::try_new("next-page")
                .expect("fixture continuation cursor is valid"),
        },
    };

    let outcome = block_on_ready(registry.read(&processor, request, &source, &NeverCancelled));

    assert_eq!(outcome, Err(FileMediaFailure::ProcessorFailed));
}

/// a processor cannot report an authenticated in-bounds source as too large.
#[test]
fn false_source_too_large_is_sanitized_to_failure() {
    let source = MemorySource::synthetic();
    let registry = registry_with_view(text_view());
    let processor = SyntheticProcessor {
        validation: ValidationBehavior::Valid,
        read: ReadBehavior::SourceTooLarge,
    };
    let request = FileReadRequest {
        inspection: inspection_request(&source, SYNTHETIC_MEDIA_TYPE),
        view: ReadViewName::try_new(TEXT_VIEW_NAME).expect("fixture view name is valid"),
        input: FileReadInput::Initial {
            options: serde_json::json!({}),
        },
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
        probe: ProbeDeclaration::new(ProbeDeclarationInput {
            prefix_bytes: 4,
            suffix_bytes: 0,
            range_count: 0,
            cumulative_bytes: 4,
        }),
        validation: signalbox_file_media_runtime::ValidationDeclaration::new(
            MAX_VALIDATION_SOURCE_BYTES,
            MAX_VALIDATION_RANGES,
        ),
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
    let first_name = first_provider.provider().clone();
    let second_name = second_provider.provider().clone();

    let registry = FileMediaRegistry::try_new(
        vec![second_provider, first_provider],
        FileMediaCeilings::version_one(),
        ProcessorIsolation::Available,
    )
    .expect("distinct media claims are conflict-free");

    assert_eq!(registry.providers()[0].provider(), &first_name);
    assert_eq!(registry.providers()[1].provider(), &second_name);
}

#[test]
fn provider_reader_inventory_is_canonically_sorted() {
    let provider =
        FileReaderProviderName::try_new("synthetic").expect("fixture provider name is valid");
    let second = reader_declaration(&provider, "second", OTHER_SYNTHETIC_MEDIA_TYPE);
    let first = reader_declaration(&provider, "first", SYNTHETIC_MEDIA_TYPE);
    let first_name = first.identity().reader().clone();
    let second_name = second.identity().reader().clone();
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
        first_name.as_str()
    );
    assert_eq!(
        registry.providers()[0].readers()[1]
            .identity()
            .reader()
            .as_str(),
        second_name.as_str()
    );
}

fn oversized_reader_inventory(provider: &FileReaderProviderName) -> Vec<ReaderDeclaration> {
    (0..257)
        .map(|index| {
            let reader = format!("reader-{index:03}");
            let owned_media_type = format!("application/x-synthetic-{index:03}");
            reader_declaration(provider, &reader, &owned_media_type)
        })
        .collect()
}

fn inspection_probe_inventory(
    provider: &FileReaderProviderName,
    count: usize,
    probe: ProbeDeclaration,
) -> Vec<ReaderDeclaration> {
    (0..count)
        .map(|index| {
            let reader = format!("probe-reader-{index:03}");
            let owned_media_type = format!("application/x-probe-synthetic-{index:03}");
            reader_declaration_with_probe(provider, &reader, &owned_media_type, probe)
        })
        .collect()
}

fn oversized_inspection_view_inventory() -> Vec<ReadViewDeclaration> {
    (0..17)
        .map(|index| {
            let name = format!("view-{index:02}");
            let schema = format!(
                r#"{{"description":"{}","type":"object"}}"#,
                "x".repeat(65_000)
            );
            ReadViewDeclaration::try_new(
                ReadViewName::try_new(name).expect("fixture view name is valid"),
                String::from("Reads one bounded synthetic projection."),
                CanonicalJsonObjectSchema::try_new(&schema)
                    .expect("fixture schema is object-rooted and individually bounded"),
                ReadAccessPattern::Streaming { maximum_ranges: 16 },
                ReadViewBounds::Text {
                    source_bytes: 1_024,
                    output_bytes: 64,
                },
            )
            .expect("fixture view declaration is individually valid")
        })
        .collect()
}

#[test]
fn oversized_provider_reader_inventory_is_rejected() {
    let provider =
        FileReaderProviderName::try_new("synthetic").expect("fixture provider name is valid");
    let readers = oversized_reader_inventory(&provider);
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
fn oversized_inspection_probe_byte_budget_is_rejected() {
    let provider =
        FileReaderProviderName::try_new("synthetic").expect("fixture provider name is valid");
    let probe = ProbeDeclaration::new(ProbeDeclarationInput {
        prefix_bytes: 1,
        suffix_bytes: 0,
        range_count: 0,
        cumulative_bytes: 262_144,
    });
    let readers = inspection_probe_inventory(&provider, 65, probe);
    let declaration = FileMediaProviderDeclaration::try_new(provider, readers)
        .expect("the provider constructor defers probe budgets to the registry");

    let outcome = FileMediaRegistry::try_new(
        vec![declaration],
        FileMediaCeilings::version_one(),
        ProcessorIsolation::Available,
    );

    assert!(matches!(
        outcome,
        Err(signalbox_file_media_runtime::FileMediaRegistryConstructionError::ProbeBounds)
    ));
}

#[test]
fn oversized_inspection_probe_request_budget_is_rejected() {
    let provider =
        FileReaderProviderName::try_new("synthetic").expect("fixture provider name is valid");
    let probe = ProbeDeclaration::new(ProbeDeclarationInput {
        prefix_bytes: 1,
        suffix_bytes: 1,
        range_count: 16,
        cumulative_bytes: 2,
    });
    let readers = inspection_probe_inventory(&provider, 57, probe);
    let declaration = FileMediaProviderDeclaration::try_new(provider, readers)
        .expect("the provider constructor defers probe budgets to the registry");

    let outcome = FileMediaRegistry::try_new(
        vec![declaration],
        FileMediaCeilings::version_one(),
        ProcessorIsolation::Available,
    );

    assert!(matches!(
        outcome,
        Err(signalbox_file_media_runtime::FileMediaRegistryConstructionError::ProbeBounds)
    ));
}

#[test]
fn oversized_inspection_view_inventory_is_rejected() {
    let provider =
        FileReaderProviderName::try_new("synthetic").expect("fixture provider name is valid");
    let views = oversized_inspection_view_inventory();
    let reader = ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: provider.clone(),
        reader: FileReaderName::try_new("fixture").expect("fixture reader name is valid"),
        revision: FileReaderRevision::try_new("1").expect("fixture revision is valid"),
        media_types: vec![media_type(SYNTHETIC_MEDIA_TYPE)],
        probe: ProbeDeclaration::new(ProbeDeclarationInput {
            prefix_bytes: 4,
            suffix_bytes: 0,
            range_count: 0,
            cumulative_bytes: 4,
        }),
        validation: signalbox_file_media_runtime::ValidationDeclaration::new(
            MAX_VALIDATION_SOURCE_BYTES,
            MAX_VALIDATION_RANGES,
        ),
        views,
        reason_codes: vec![ReasonCode::try_new(MALFORMED_REASON).expect("fixture reason is valid")],
        streaming_text_fallback: StreamingTextFallback::Disabled,
    })
    .expect("fixture reader declaration is nonempty");
    let declaration = FileMediaProviderDeclaration::try_new(provider, vec![reader])
        .expect("fixture provider owns its reader");

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
fn lowered_result_ceiling_applies_to_inspection_view_inventory() {
    let mut ceilings = FileMediaCeilings::version_one();
    ceilings.text_or_json_bytes = 64 * 1_024;
    let view = text_view();

    let outcome = registry_outcome_with_view(view, ceilings);

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
    let view = bounded_text_view(
        ReadAccessPattern::Streaming { maximum_ranges: 16 },
        source_bytes,
    );

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

#[test]
fn streaming_range_fanout_above_the_compiled_ceiling_is_rejected() {
    let ceilings = FileMediaCeilings::version_one();
    let maximum_ranges = ceilings
        .read_ranges
        .checked_add(1)
        .expect("the fixture ceiling leaves room for one excessive range");
    let view = bounded_text_view(ReadAccessPattern::Streaming { maximum_ranges }, 1_024);

    let outcome = registry_outcome_with_view(view, ceilings);

    assert!(matches!(
        outcome,
        Err(signalbox_file_media_runtime::FileMediaRegistryConstructionError::ViewBounds)
    ));
}

#[test]
fn validation_source_work_ceiling_can_only_be_lowered() {
    let compiled = FileMediaCeilings::version_one();
    let mut lowered = compiled;
    lowered.validation_source_bytes = compiled.validation_source_bytes - 1;
    lowered.validation_ranges = compiled.validation_ranges - 1;
    let mut raised_bytes = compiled;
    raised_bytes.validation_source_bytes = compiled.validation_source_bytes + 1;
    let mut raised_ranges = compiled;
    raised_ranges.validation_ranges = compiled.validation_ranges + 1;

    assert!(compiled.admits(lowered));
    assert!(!compiled.admits(raised_bytes));
    assert!(!compiled.admits(raised_ranges));
}

#[test]
fn reader_validation_envelope_clamps_registry_request() {
    let source = MemorySource::synthetic();
    let registry = registry_with_view_validation_and_ceilings(
        text_view(),
        ValidationDeclaration::new(32, 2),
        FileMediaCeilings::version_one(),
    )
    .expect("reader validation envelope is within compiled ceilings");
    let processor = SyntheticProcessor {
        validation: ValidationBehavior::ValidWithEnvelope {
            source_bytes: 32,
            ranges: 2,
        },
        read: ReadBehavior::Text,
    };

    let outcome = inspect(&registry, &processor, &source, SYNTHETIC_MEDIA_TYPE);

    assert!(matches!(outcome, Ok(FileInspection::Validated { .. })));
}

#[test]
fn lowered_global_validation_envelope_clamps_reader_request() {
    let source = MemorySource::synthetic();
    let mut ceilings = FileMediaCeilings::version_one();
    ceilings.validation_source_bytes = 16;
    ceilings.validation_ranges = 1;
    let registry = registry_with_view_validation_and_ceilings(
        text_view(),
        ValidationDeclaration::new(32, 2),
        ceilings,
    )
    .expect("lowered global ceilings admit the reader declaration");
    let processor = SyntheticProcessor {
        validation: ValidationBehavior::ValidWithEnvelope {
            source_bytes: 16,
            ranges: 1,
        },
        read: ReadBehavior::Text,
    };

    let outcome = inspect(&registry, &processor, &source, SYNTHETIC_MEDIA_TYPE);

    assert!(matches!(outcome, Ok(FileInspection::Validated { .. })));
}

/// The read request names the prefix validation covered, so a reader whose declared
/// validation envelope is below the deployment ceiling is told the smaller number.
#[test]
fn reader_validation_envelope_clamps_registry_read_request() {
    let source = MemorySource::synthetic();
    let ceilings = FileMediaCeilings::version_one();
    let registry = registry_with_view_validation_and_ceilings(
        text_view(),
        ValidationDeclaration::new(32, 2),
        ceilings,
    )
    .expect("reader validation envelope is within compiled ceilings");
    assert!(ceilings.validation_source_bytes > 32);
    let processor = SyntheticProcessor {
        validation: ValidationBehavior::ValidWithEnvelope {
            source_bytes: 32,
            ranges: 2,
        },
        read: ReadBehavior::TextRequiringSourceBytes(32),
    };
    let request = FileReadRequest {
        inspection: inspection_request(&source, SYNTHETIC_MEDIA_TYPE),
        view: ReadViewName::try_new(TEXT_VIEW_NAME).expect("fixture view name is valid"),
        input: FileReadInput::Initial {
            options: serde_json::json!({}),
        },
    };

    let outcome = block_on_ready(registry.read(&processor, request, &source, &NeverCancelled));

    assert!(matches!(outcome, Ok(FileReadResult::Text { .. })));
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
        probe: ProbeDeclaration::new(ProbeDeclarationInput {
            prefix_bytes: 4,
            suffix_bytes: 0,
            range_count: 0,
            cumulative_bytes: 4,
        }),
        validation: signalbox_file_media_runtime::ValidationDeclaration::new(
            MAX_VALIDATION_SOURCE_BYTES,
            MAX_VALIDATION_RANGES,
        ),
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
    reader_declaration_with_probe(
        provider,
        reader,
        owned_media_type,
        ProbeDeclaration::new(ProbeDeclarationInput {
            prefix_bytes: 4,
            suffix_bytes: 0,
            range_count: 0,
            cumulative_bytes: 4,
        }),
    )
}

fn reader_declaration_with_probe(
    provider: &FileReaderProviderName,
    reader: &str,
    owned_media_type: &str,
    probe: ProbeDeclaration,
) -> ReaderDeclaration {
    ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: provider.clone(),
        reader: FileReaderName::try_new(reader).expect("fixture reader name is valid"),
        revision: FileReaderRevision::try_new("1").expect("fixture revision is valid"),
        media_types: vec![media_type(owned_media_type)],
        probe,
        validation: signalbox_file_media_runtime::ValidationDeclaration::new(
            MAX_VALIDATION_SOURCE_BYTES,
            MAX_VALIDATION_RANGES,
        ),
        views: vec![text_view()],
        reason_codes: vec![ReasonCode::try_new(MALFORMED_REASON).expect("fixture reason is valid")],
        streaming_text_fallback: StreamingTextFallback::Disabled,
    })
    .expect("fixture reader declaration is nonempty")
}
