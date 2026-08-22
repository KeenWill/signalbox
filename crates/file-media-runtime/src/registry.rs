use std::{collections::BTreeMap, error::Error, fmt, str::FromStr};

use crate::{
    BoundedMetadata, CanonicalMediaType, FileInspection, FileMediaCeilings, FileMediaFailure,
    FileMediaProcessor, FileMediaProviderDeclaration, FileMediaProviderReadRequest,
    FileMediaProviderValidationRequest, FileReadRequest, FileReadResult, InspectionRequest,
    MAX_READ_OPTIONS_BYTES, ProbeStrength, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, ReadAccessPattern, ReadContinuation, ReadContinuationCursor,
    ReadViewBounds, ReaderDeclaration, ReaderIdentity, ReasonCode, StreamingTextFallback,
    ValidatedFile, ValidationEvidence, VerifiedBlobSource,
};

// numeric-bound: ceiling - bounds process-lifetime provider inventory memory
const MAX_REGISTRY_PROVIDERS: usize = 256;
// numeric-bound: ceiling - bounds per-provider reader inventory memory and startup work
const MAX_READERS_PER_PROVIDER: usize = 256;
// numeric-bound: ceiling - bounds aggregate process-lifetime reader inventory memory
const MAX_REGISTRY_READERS: usize = 256;
// numeric-bound: ceiling - bounds per-reader media-claim memory and conflict checks
const MAX_MEDIA_TYPES_PER_READER: usize = 256;
// numeric-bound: ceiling - bounds aggregate process-lifetime media-claim memory
const MAX_REGISTRY_MEDIA_TYPES: usize = 4_096;
// numeric-bound: ceiling - bounds per-reader model-visible view inventory memory
const MAX_VIEWS_PER_READER: usize = 256;
// numeric-bound: ceiling - reserves tool-result space for fixed inspection facts and metadata
const MAX_INSPECTION_VIEW_INVENTORY_BYTES: usize = 512 * 1_024;
// numeric-bound: ceiling - bounds aggregate process-lifetime view inventory memory
const MAX_REGISTRY_VIEWS: usize = 4_096;
// numeric-bound: ceiling - bounds aggregate retained view-schema bytes
const MAX_REGISTRY_SCHEMA_BYTES: usize = 16 * 1_024 * 1_024;
// numeric-bound: ceiling - bounds per-reader sanitized reason inventory memory
const MAX_REASON_CODES_PER_READER: usize = 256;
// numeric-bound: ceiling - bounds aggregate process-lifetime reason inventory memory
const MAX_REGISTRY_REASON_CODES: usize = 4_096;
/// Immutable process-lifetime registry snapshot.
#[derive(Clone, Debug)]
pub struct FileMediaRegistry {
    providers: Vec<FileMediaProviderDeclaration>,
    readers: BTreeMap<ReaderIdentity, ReaderDeclaration>,
    media_readers: BTreeMap<CanonicalMediaType, ReaderIdentity>,
    streaming_text_reader: Option<ReaderIdentity>,
    ceilings: FileMediaCeilings,
}

/// Whether the daemon can launch the required processor isolation boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessorIsolation {
    /// The accepted isolation boundary is available.
    Available,
    /// No accepted isolation boundary is available.
    Unavailable,
}

impl FileMediaRegistry {
    /// Builds one deterministic registry and rejects every static conflict.
    pub fn try_new(
        mut providers: Vec<FileMediaProviderDeclaration>,
        ceilings: FileMediaCeilings,
        isolation: ProcessorIsolation,
    ) -> Result<Self, FileMediaRegistryConstructionError> {
        if !FileMediaCeilings::version_one().admits(ceilings) {
            return Err(FileMediaRegistryConstructionError::Ceilings);
        }
        if providers.len() > MAX_REGISTRY_PROVIDERS {
            return Err(FileMediaRegistryConstructionError::Inventory);
        }
        if !providers.is_empty() && isolation == ProcessorIsolation::Unavailable {
            return Err(FileMediaRegistryConstructionError::IsolationUnavailable);
        }
        if providers
            .iter()
            .any(|provider| provider.readers().len() > MAX_READERS_PER_PROVIDER)
        {
            return Err(FileMediaRegistryConstructionError::Inventory);
        }
        validate_aggregate_inventory(&providers)?;
        providers.sort_by(|left, right| left.provider().cmp(right.provider()));
        for provider in &mut providers {
            provider.sort_readers();
        }
        if providers
            .windows(2)
            .any(|pair| pair[0].provider() == pair[1].provider())
        {
            return Err(FileMediaRegistryConstructionError::DuplicateProvider);
        }

        let mut readers = BTreeMap::new();
        let mut media_readers = BTreeMap::new();
        let mut streaming_text_reader = None;
        for provider in &providers {
            for reader in provider.readers() {
                validate_reader(reader, ceilings)?;
                let identity = reader.identity().clone();
                if readers.insert(identity.clone(), reader.clone()).is_some() {
                    return Err(FileMediaRegistryConstructionError::DuplicateReader);
                }
                for media_type in reader.media_types() {
                    if media_readers
                        .insert(media_type.clone(), identity.clone())
                        .is_some()
                    {
                        return Err(FileMediaRegistryConstructionError::DuplicateMediaTypeClaim);
                    }
                }
                if reader.streaming_text_fallback() == StreamingTextFallback::Enabled {
                    let text_plain = CanonicalMediaType::from_str("text/plain")
                        .map_err(|_| FileMediaRegistryConstructionError::TextFallback)?;
                    if !reader.media_types().contains(&text_plain)
                        || streaming_text_reader.replace(identity).is_some()
                    {
                        return Err(FileMediaRegistryConstructionError::TextFallback);
                    }
                }
            }
        }
        Ok(Self {
            providers,
            readers,
            media_readers,
            streaming_text_reader,
            ceilings,
        })
    }

    /// Constructs the valid empty registry used before adapters are compiled.
    pub fn empty() -> Self {
        Self {
            providers: Vec::new(),
            readers: BTreeMap::new(),
            media_readers: BTreeMap::new(),
            streaming_text_reader: None,
            ceilings: FileMediaCeilings::version_one(),
        }
    }

    /// Borrows canonically ordered provider declarations.
    pub fn providers(&self) -> &[FileMediaProviderDeclaration] {
        &self.providers
    }

    /// Returns the effective lowerable-only ceiling set.
    pub const fn ceilings(&self) -> FileMediaCeilings {
        self.ceilings
    }

    /// Detects and validates exact bytes without consulting registration order.
    pub async fn inspect(
        &self,
        processor: &dyn FileMediaProcessor,
        request: InspectionRequest,
        source: &dyn VerifiedBlobSource,
        cancellation: &dyn crate::CancellationSignal,
    ) -> Result<FileInspection, FileMediaFailure> {
        if cancellation.is_cancelled() {
            return Err(FileMediaFailure::Cancelled);
        }
        if source.digest() != request.source.digest()
            || source.byte_length() != request.source.byte_length()
        {
            return Err(FileMediaFailure::ProcessorFailed);
        }
        if self.readers.is_empty() {
            return Ok(FileInspection::Unknown {
                source: request.source,
            });
        }

        let mut candidates = Vec::new();
        let mut malformed = Vec::new();
        for reader in self.readers.values() {
            let raw = processor
                .probe(reader.identity(), source, cancellation)
                .await?;
            match sanitize_probe(reader, raw)? {
                SanitizedProbe::NoMatch => {}
                SanitizedProbe::Candidate(candidate) => candidates.push(candidate),
                SanitizedProbe::Malformed {
                    media_type,
                    reason_code,
                } => {
                    malformed.push((media_type, reason_code));
                }
            }
        }
        if !malformed.is_empty() {
            malformed.sort();
            malformed.dedup();
            let distinct = distinct_media_types(
                malformed.iter().map(|(kind, _)| kind.clone()).chain(
                    candidates
                        .iter()
                        .filter(|candidate| recognized_probe_strength(candidate.strength))
                        .map(|candidate| candidate.media_type.clone()),
                ),
            );
            if distinct.len() > 1 {
                return Ok(FileInspection::Ambiguous {
                    source: request.source,
                    media_types: distinct,
                });
            }
            let Some((media_type, reason_code)) = malformed.into_iter().next() else {
                return Err(FileMediaFailure::ProcessorFailed);
            };
            return Ok(FileInspection::Malformed {
                source: request.source,
                media_type,
                reason_code,
            });
        }

        let strong = candidates
            .iter()
            .filter(|candidate| candidate.strength == ProbeStrength::Strong)
            .cloned()
            .collect::<Vec<_>>();
        if !strong.is_empty() {
            return self
                .resolve_candidates(
                    processor,
                    request,
                    source,
                    cancellation,
                    strong,
                    ValidationEvidence::StrongSignature,
                )
                .await;
        }

        let structural = candidates
            .iter()
            .filter(|candidate| candidate.strength == ProbeStrength::StructuralCandidate)
            .cloned()
            .collect::<Vec<_>>();
        if !structural.is_empty() {
            return self
                .resolve_candidates(
                    processor,
                    request,
                    source,
                    cancellation,
                    structural,
                    ValidationEvidence::StructuralValidation,
                )
                .await;
        }

        if let Ok(declared) = request.source.declared_media_type().canonical_essence()
            && let Some(reader) = self.media_readers.get(&declared)
        {
            return self
                .validate_candidate(
                    processor,
                    request,
                    source,
                    cancellation,
                    Candidate {
                        reader: reader.clone(),
                        media_type: declared,
                        strength: ProbeStrength::DeclaredCandidate,
                    },
                    ValidationEvidence::DeclaredCandidateStructurallyValidated,
                )
                .await;
        }

        if let Some(reader) = self.streaming_text_reader.as_ref() {
            let declaration = self
                .readers
                .get(reader)
                .ok_or(FileMediaFailure::ProcessorFailed)?;
            let text_plain = CanonicalMediaType::from_str("text/plain")
                .map_err(|_| FileMediaFailure::ProcessorFailed)?;
            return self
                .validate_candidate(
                    processor,
                    request,
                    source,
                    cancellation,
                    Candidate {
                        reader: declaration.identity().clone(),
                        media_type: text_plain,
                        strength: ProbeStrength::DeclaredCandidate,
                    },
                    ValidationEvidence::StreamingTextValidation,
                )
                .await;
        }

        Ok(FileInspection::Unknown {
            source: request.source,
        })
    }

    async fn resolve_candidates(
        &self,
        processor: &dyn FileMediaProcessor,
        request: InspectionRequest,
        source: &dyn VerifiedBlobSource,
        cancellation: &dyn crate::CancellationSignal,
        mut candidates: Vec<Candidate>,
        evidence: ValidationEvidence,
    ) -> Result<FileInspection, FileMediaFailure> {
        candidates.sort();
        candidates.dedup();
        let media_types = distinct_media_types(
            candidates
                .iter()
                .map(|candidate| candidate.media_type.clone()),
        );
        let readers = candidates
            .iter()
            .map(|candidate| candidate.reader.clone())
            .collect::<std::collections::BTreeSet<_>>();
        if media_types.len() != 1 || readers.len() != 1 {
            return Ok(FileInspection::Ambiguous {
                source: request.source,
                media_types,
            });
        }
        let Some(candidate) = candidates.into_iter().next() else {
            return Err(FileMediaFailure::ProcessorFailed);
        };
        self.validate_candidate(
            processor,
            request,
            source,
            cancellation,
            candidate,
            evidence,
        )
        .await
    }

    async fn validate_candidate(
        &self,
        processor: &dyn FileMediaProcessor,
        request: InspectionRequest,
        source: &dyn VerifiedBlobSource,
        cancellation: &dyn crate::CancellationSignal,
        candidate: Candidate,
        evidence: ValidationEvidence,
    ) -> Result<FileInspection, FileMediaFailure> {
        let reader = self
            .readers
            .get(&candidate.reader)
            .ok_or(FileMediaFailure::ProcessorFailed)?;
        let raw = processor
            .validate(
                reader.identity(),
                FileMediaProviderValidationRequest {
                    source: request.source.clone(),
                    media_type: candidate.media_type.clone(),
                    evidence,
                    maximum_source_bytes: self.ceilings.validation_source_bytes,
                    maximum_ranges: self.ceilings.validation_ranges,
                },
                source,
                cancellation,
            )
            .await?;
        match sanitize_validation(reader, &candidate.media_type, evidence, raw)? {
            SanitizedValidation::Validated { metadata } => {
                if let Ok(declared) = request.source.declared_media_type().canonical_essence()
                    && declared != candidate.media_type
                {
                    return Ok(FileInspection::DeclaredMismatch {
                        source: request.source,
                        declared,
                        detected: candidate.media_type,
                    });
                }
                Ok(FileInspection::Validated(ValidatedFile::new(
                    request.source,
                    candidate.media_type,
                    candidate.reader,
                    evidence,
                    metadata,
                    reader.views().to_vec(),
                )))
            }
            SanitizedValidation::Malformed { .. }
                if streaming_text_terminal_becomes_unknown(evidence) =>
            {
                Ok(FileInspection::Unknown {
                    source: request.source,
                })
            }
            SanitizedValidation::Malformed { reason_code } => Ok(FileInspection::Malformed {
                source: request.source,
                media_type: candidate.media_type,
                reason_code,
            }),
            SanitizedValidation::EncryptedOrLocked
                if streaming_text_terminal_becomes_unknown(evidence) =>
            {
                Ok(FileInspection::Unknown {
                    source: request.source,
                })
            }
            SanitizedValidation::EncryptedOrLocked => Ok(FileInspection::EncryptedOrLocked {
                source: request.source,
                media_type: candidate.media_type,
            }),
            SanitizedValidation::NoMatch
                if evidence == ValidationEvidence::DeclaredCandidateStructurallyValidated
                    || evidence == ValidationEvidence::StreamingTextValidation =>
            {
                Ok(FileInspection::Unknown {
                    source: request.source,
                })
            }
            SanitizedValidation::NoMatch => Err(FileMediaFailure::ProcessorFailed),
        }
    }

    /// Repeats inspection, selects one declared view, and sanitizes all output.
    pub async fn read(
        &self,
        processor: &dyn FileMediaProcessor,
        request: FileReadRequest,
        source: &dyn VerifiedBlobSource,
        cancellation: &dyn crate::CancellationSignal,
    ) -> Result<FileReadResult, FileMediaFailure> {
        let initial_request = match &request.input {
            crate::FileReadInput::Initial { options }
                if options.is_object()
                    && serde_json::to_vec(options)
                        .is_ok_and(|encoded| encoded.len() <= MAX_READ_OPTIONS_BYTES) =>
            {
                true
            }
            crate::FileReadInput::Initial { .. } => {
                return Err(FileMediaFailure::InvalidViewArguments);
            }
            crate::FileReadInput::Continuation { .. } => false,
        };
        let inspection = self
            .inspect(processor, request.inspection.clone(), source, cancellation)
            .await?;
        let validated = match inspection {
            FileInspection::Validated(validated) => validated,
            FileInspection::Unknown { .. } => return Err(FileMediaFailure::UnknownType),
            FileInspection::Malformed {
                media_type,
                reason_code,
                ..
            } => {
                return Err(FileMediaFailure::Malformed {
                    media_type,
                    reason_code,
                });
            }
            FileInspection::Ambiguous { .. } => return Err(FileMediaFailure::AmbiguousType),
            FileInspection::DeclaredMismatch {
                declared, detected, ..
            } => {
                return Err(FileMediaFailure::DeclaredTypeMismatch { declared, detected });
            }
            FileInspection::EncryptedOrLocked { media_type, .. } => {
                return Err(FileMediaFailure::EncryptedOrLocked { media_type });
            }
        };
        let view = validated
            .views()
            .iter()
            .find(|view| view.name() == &request.view)
            .ok_or(FileMediaFailure::UnsupportedView)?;
        let reader = self
            .readers
            .get(validated.reader())
            .ok_or(FileMediaFailure::ProcessorFailed)?;
        let raw = processor
            .read(
                validated.reader(),
                FileMediaProviderReadRequest {
                    source: validated.source().clone(),
                    detected_media_type: validated.detected_media_type().clone(),
                    validation: validated.validation(),
                    metadata: validated.metadata().clone(),
                    view: request.view,
                    input: request.input,
                },
                source,
                cancellation,
            )
            .await?;
        sanitize_read(reader, view, self.ceilings, initial_request, raw)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Candidate {
    reader: ReaderIdentity,
    media_type: CanonicalMediaType,
    strength: ProbeStrength,
}

fn recognized_probe_strength(strength: ProbeStrength) -> bool {
    matches!(
        strength,
        ProbeStrength::Strong | ProbeStrength::StructuralCandidate
    )
}

fn streaming_text_terminal_becomes_unknown(evidence: ValidationEvidence) -> bool {
    evidence == ValidationEvidence::StreamingTextValidation
}

enum SanitizedProbe {
    NoMatch,
    Candidate(Candidate),
    Malformed {
        media_type: CanonicalMediaType,
        reason_code: ReasonCode,
    },
}

fn sanitize_probe(
    reader: &ReaderDeclaration,
    raw: ProcessorProbeOutput,
) -> Result<SanitizedProbe, FileMediaFailure> {
    match raw {
        ProcessorProbeOutput::NoMatch => Ok(SanitizedProbe::NoMatch),
        ProcessorProbeOutput::Candidate {
            media_type,
            strength,
        } => {
            let media_type = CanonicalMediaType::from_str(&media_type)
                .map_err(|_| FileMediaFailure::ProcessorFailed)?;
            if !reader.media_types().contains(&media_type)
                || strength == ProbeStrength::DeclaredCandidate
            {
                return Err(FileMediaFailure::ProcessorFailed);
            }
            Ok(SanitizedProbe::Candidate(Candidate {
                reader: reader.identity().clone(),
                media_type,
                strength,
            }))
        }
        ProcessorProbeOutput::RecognizedMalformed {
            media_type,
            reason_code,
        } => {
            let media_type = CanonicalMediaType::from_str(&media_type)
                .map_err(|_| FileMediaFailure::ProcessorFailed)?;
            let reason_code = registered_reason(reader, &reason_code)?;
            if !reader.media_types().contains(&media_type) {
                return Err(FileMediaFailure::ProcessorFailed);
            }
            Ok(SanitizedProbe::Malformed {
                media_type,
                reason_code,
            })
        }
    }
}

enum SanitizedValidation {
    Validated { metadata: BoundedMetadata },
    Malformed { reason_code: ReasonCode },
    EncryptedOrLocked,
    NoMatch,
}

fn sanitize_validation(
    reader: &ReaderDeclaration,
    selected_media_type: &CanonicalMediaType,
    selected_evidence: ValidationEvidence,
    raw: ProcessorValidationOutput,
) -> Result<SanitizedValidation, FileMediaFailure> {
    match raw {
        ProcessorValidationOutput::Validated {
            media_type,
            evidence,
            metadata_json,
        } => {
            let media_type = CanonicalMediaType::from_str(&media_type)
                .map_err(|_| FileMediaFailure::ProcessorFailed)?;
            if &media_type != selected_media_type || evidence != selected_evidence {
                return Err(FileMediaFailure::ProcessorFailed);
            }
            let metadata = BoundedMetadata::try_new(&metadata_json)
                .map_err(|_| FileMediaFailure::ProcessorFailed)?;
            Ok(SanitizedValidation::Validated { metadata })
        }
        ProcessorValidationOutput::Malformed {
            media_type,
            reason_code,
        } => {
            let media_type = CanonicalMediaType::from_str(&media_type)
                .map_err(|_| FileMediaFailure::ProcessorFailed)?;
            if &media_type != selected_media_type {
                return Err(FileMediaFailure::ProcessorFailed);
            }
            Ok(SanitizedValidation::Malformed {
                reason_code: registered_reason(reader, &reason_code)?,
            })
        }
        ProcessorValidationOutput::EncryptedOrLocked { media_type } => {
            let media_type = CanonicalMediaType::from_str(&media_type)
                .map_err(|_| FileMediaFailure::ProcessorFailed)?;
            if &media_type != selected_media_type {
                return Err(FileMediaFailure::ProcessorFailed);
            }
            Ok(SanitizedValidation::EncryptedOrLocked)
        }
        ProcessorValidationOutput::NoMatch => Ok(SanitizedValidation::NoMatch),
    }
}

fn sanitize_read(
    reader: &ReaderDeclaration,
    view: &crate::ReadViewDeclaration,
    ceilings: FileMediaCeilings,
    initial_request: bool,
    raw: ProcessorReadOutput,
) -> Result<FileReadResult, FileMediaFailure> {
    match raw {
        ProcessorReadOutput::Text {
            body,
            truncated,
            cursor,
        } => {
            let ReadViewBounds::Text { output_bytes, .. } = view.bounds() else {
                return Err(FileMediaFailure::ProcessorFailed);
            };
            if body.len() > output_bytes
                || body.len() > crate::MAX_TEXT_BODY_BYTES
                || body.len() > ceilings.text_or_json_bytes
                || body.contains('\0')
            {
                return Err(FileMediaFailure::ProcessorFailed);
            }
            let continuation = sanitize_continuation(truncated, cursor)?;
            Ok(FileReadResult::Text { body, continuation })
        }
        ProcessorReadOutput::Structured {
            body_json,
            truncated,
            cursor,
        } => {
            let ReadViewBounds::Structured {
                output_bytes,
                depth,
                nodes,
                string_bytes,
                ..
            } = view.bounds()
            else {
                return Err(FileMediaFailure::ProcessorFailed);
            };
            if body_json.len() > output_bytes
                || body_json.len() > ceilings.text_or_json_bytes
                || body_json.contains('\0')
            {
                return Err(FileMediaFailure::ProcessorFailed);
            }
            let continuation = sanitize_continuation(truncated, cursor)?;
            let body = crate::value::parse_json_without_duplicate_members(&body_json)
                .map_err(|_| FileMediaFailure::ProcessorFailed)?;
            let canonical_bytes = serde_json::to_string(&body)
                .map_err(|_| FileMediaFailure::ProcessorFailed)?
                .len();
            if canonical_bytes > output_bytes || canonical_bytes > ceilings.text_or_json_bytes {
                return Err(FileMediaFailure::ProcessorFailed);
            }
            let mut observed = ObservedJson::default();
            observe_json(&body, 1, &mut observed)?;
            if observed.depth > depth
                || observed.depth > ceilings.structured_depth
                || observed.nodes > nodes
                || observed.nodes > ceilings.structured_nodes
                || observed.max_container_entries > ceilings.observed_container_entries
                || observed.string_bytes > string_bytes
            {
                return Err(FileMediaFailure::ProcessorFailed);
            }
            Ok(FileReadResult::Structured { body, continuation })
        }
        ProcessorReadOutput::InvalidViewArguments if initial_request => {
            Err(FileMediaFailure::InvalidViewArguments)
        }
        ProcessorReadOutput::InvalidViewArguments => Err(FileMediaFailure::ProcessorFailed),
        ProcessorReadOutput::UnsupportedView => Err(FileMediaFailure::ProcessorFailed),
        ProcessorReadOutput::SourceTooLarge { maximum_bytes } => {
            if maximum_bytes != view.bounds().source_bytes() {
                return Err(FileMediaFailure::ProcessorFailed);
            }
            Err(FileMediaFailure::SourceTooLarge { maximum_bytes })
        }
        ProcessorReadOutput::ExpansionLimitExceeded { limit_kind } => {
            Err(FileMediaFailure::ExpansionLimitExceeded {
                limit_kind: registered_reason(reader, &limit_kind)?,
            })
        }
        ProcessorReadOutput::OutputUnitTooLarge => Err(FileMediaFailure::OutputUnitTooLarge),
    }
}

#[derive(Default)]
struct ObservedJson {
    depth: u32,
    nodes: u64,
    string_bytes: usize,
    max_container_entries: u64,
}

fn observe_json(
    value: &serde_json::Value,
    depth: u32,
    observed: &mut ObservedJson,
) -> Result<(), FileMediaFailure> {
    observed.depth = observed.depth.max(depth);
    observed.nodes = observed
        .nodes
        .checked_add(1)
        .ok_or(FileMediaFailure::ProcessorFailed)?;
    match value {
        serde_json::Value::String(value) => {
            observed.string_bytes = observed
                .string_bytes
                .checked_add(value.len())
                .ok_or(FileMediaFailure::ProcessorFailed)?;
        }
        serde_json::Value::Array(values) => {
            let entries =
                u64::try_from(values.len()).map_err(|_| FileMediaFailure::ProcessorFailed)?;
            observed.max_container_entries = observed.max_container_entries.max(entries);
            let next = depth
                .checked_add(1)
                .ok_or(FileMediaFailure::ProcessorFailed)?;
            for value in values {
                observe_json(value, next, observed)?;
            }
        }
        serde_json::Value::Object(values) => {
            let entries =
                u64::try_from(values.len()).map_err(|_| FileMediaFailure::ProcessorFailed)?;
            observed.max_container_entries = observed.max_container_entries.max(entries);
            let next = depth
                .checked_add(1)
                .ok_or(FileMediaFailure::ProcessorFailed)?;
            for (name, value) in values {
                observed.string_bytes = observed
                    .string_bytes
                    .checked_add(name.len())
                    .ok_or(FileMediaFailure::ProcessorFailed)?;
                observe_json(value, next, observed)?;
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
    Ok(())
}

fn sanitize_continuation(
    truncated: bool,
    cursor: Option<String>,
) -> Result<ReadContinuation, FileMediaFailure> {
    match (truncated, cursor) {
        (false, None) => Ok(ReadContinuation::Complete),
        (true, Some(cursor)) => {
            let cursor = ReadContinuationCursor::try_new(cursor)
                .map_err(|_| FileMediaFailure::ProcessorFailed)?;
            Ok(ReadContinuation::More { cursor })
        }
        (false, Some(_)) | (true, None) => Err(FileMediaFailure::ProcessorFailed),
    }
}

fn registered_reason(
    reader: &ReaderDeclaration,
    raw: &str,
) -> Result<ReasonCode, FileMediaFailure> {
    let reason = ReasonCode::try_new(raw).map_err(|_| FileMediaFailure::ProcessorFailed)?;
    if reader.reason_codes().contains(&reason) {
        Ok(reason)
    } else {
        Err(FileMediaFailure::ProcessorFailed)
    }
}

fn distinct_media_types(
    values: impl IntoIterator<Item = CanonicalMediaType>,
) -> Vec<CanonicalMediaType> {
    values
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn validate_reader(
    reader: &ReaderDeclaration,
    ceilings: FileMediaCeilings,
) -> Result<(), FileMediaRegistryConstructionError> {
    if reader.media_types().len() > MAX_MEDIA_TYPES_PER_READER
        || reader.views().len() > MAX_VIEWS_PER_READER
        || reader.reason_codes().len() > MAX_REASON_CODES_PER_READER
    {
        return Err(FileMediaRegistryConstructionError::Inventory);
    }
    if has_duplicates(reader.media_types())
        || has_duplicates(reader.reason_codes())
        || has_duplicate_view_names(reader)
    {
        return Err(FileMediaRegistryConstructionError::DuplicateReaderMember);
    }
    validate_inspection_view_inventory(reader)?;
    let probe = reader.probe();
    if probe.prefix_bytes() > ceilings.probe_prefix_bytes
        || probe.suffix_bytes() > ceilings.probe_suffix_bytes
        || probe.range_count() > ceilings.probe_ranges
        || (probe.prefix_bytes() == 0 && probe.suffix_bytes() == 0 && probe.range_count() == 0)
        || probe.cumulative_bytes() == 0
        || probe.cumulative_bytes() > ceilings.probe_cumulative_bytes
        || probe
            .prefix_bytes()
            .checked_add(probe.suffix_bytes())
            .is_none_or(|minimum| minimum > probe.cumulative_bytes())
    {
        return Err(FileMediaRegistryConstructionError::ProbeBounds);
    }
    for view in reader.views() {
        validate_view(view.access(), view.bounds(), ceilings)?;
    }
    Ok(())
}

fn validate_inspection_view_inventory(
    reader: &ReaderDeclaration,
) -> Result<(), FileMediaRegistryConstructionError> {
    let mut projected_bytes = 2_usize;
    for (index, view) in reader.views().iter().enumerate() {
        let encoded = serde_json::to_vec(&serde_json::json!({
            "name": view.name().as_str(),
            "description": view.description(),
            "arguments_schema": view.arguments_schema().value(),
            "output": inspection_output_kind(view.output_kind()),
        }))
        .map_err(|_| FileMediaRegistryConstructionError::Inventory)?;
        projected_bytes = projected_bytes
            .checked_add(encoded.len())
            .and_then(|total| total.checked_add(usize::from(index > 0)))
            .ok_or(FileMediaRegistryConstructionError::Inventory)?;
        if projected_bytes > MAX_INSPECTION_VIEW_INVENTORY_BYTES {
            return Err(FileMediaRegistryConstructionError::Inventory);
        }
    }
    Ok(())
}

const fn inspection_output_kind(kind: crate::ReadOutputKind) -> &'static str {
    match kind {
        crate::ReadOutputKind::Text => "text",
        crate::ReadOutputKind::Structured => "structured",
        crate::ReadOutputKind::Image => "image",
        crate::ReadOutputKind::Audio => "audio",
        crate::ReadOutputKind::File => "file",
    }
}

fn validate_aggregate_inventory(
    providers: &[FileMediaProviderDeclaration],
) -> Result<(), FileMediaRegistryConstructionError> {
    let mut readers = 0_usize;
    let mut media_types = 0_usize;
    let mut views = 0_usize;
    let mut schema_bytes = 0_usize;
    let mut reason_codes = 0_usize;
    for provider in providers {
        readers = readers
            .checked_add(provider.readers().len())
            .ok_or(FileMediaRegistryConstructionError::Inventory)?;
        if readers > MAX_REGISTRY_READERS {
            return Err(FileMediaRegistryConstructionError::Inventory);
        }
        for reader in provider.readers() {
            media_types = media_types
                .checked_add(reader.media_types().len())
                .ok_or(FileMediaRegistryConstructionError::Inventory)?;
            views = views
                .checked_add(reader.views().len())
                .ok_or(FileMediaRegistryConstructionError::Inventory)?;
            reason_codes = reason_codes
                .checked_add(reader.reason_codes().len())
                .ok_or(FileMediaRegistryConstructionError::Inventory)?;
            if media_types > MAX_REGISTRY_MEDIA_TYPES
                || views > MAX_REGISTRY_VIEWS
                || reason_codes > MAX_REGISTRY_REASON_CODES
            {
                return Err(FileMediaRegistryConstructionError::Inventory);
            }
            for view in reader.views() {
                schema_bytes = schema_bytes
                    .checked_add(view.arguments_schema().as_str().len())
                    .ok_or(FileMediaRegistryConstructionError::Inventory)?;
                if schema_bytes > MAX_REGISTRY_SCHEMA_BYTES {
                    return Err(FileMediaRegistryConstructionError::Inventory);
                }
            }
        }
    }
    Ok(())
}

fn has_duplicates<Value: Ord + Clone>(values: &[Value]) -> bool {
    values
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        != values.len()
}

fn has_duplicate_view_names(reader: &ReaderDeclaration) -> bool {
    reader
        .views()
        .iter()
        .map(|view| view.name().clone())
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        != reader.views().len()
}

fn validate_view(
    access: ReadAccessPattern,
    bounds: ReadViewBounds,
    ceilings: FileMediaCeilings,
) -> Result<(), FileMediaRegistryConstructionError> {
    if matches!(
        access,
        ReadAccessPattern::Streaming { maximum_ranges }
            | ReadAccessPattern::RandomAccess { maximum_ranges }
            if maximum_ranges == 0 || maximum_ranges > ceilings.read_ranges
    ) || bounds.source_bytes() == 0
        || bounds.source_bytes() > ceilings.read_source_bytes
    {
        return Err(FileMediaRegistryConstructionError::ViewBounds);
    }
    let valid = match bounds {
        ReadViewBounds::Text { output_bytes, .. } => {
            output_bytes > 0
                && output_bytes <= crate::MAX_TEXT_BODY_BYTES
                && output_bytes <= ceilings.text_or_json_bytes
        }
        ReadViewBounds::Structured {
            output_bytes,
            depth,
            nodes,
            string_bytes,
            ..
        } => {
            output_bytes > 0
                && output_bytes <= ceilings.text_or_json_bytes
                && depth > 0
                && depth <= ceilings.structured_depth
                && nodes > 0
                && nodes <= ceilings.structured_nodes
                && string_bytes > 0
                && string_bytes <= output_bytes
        }
        ReadViewBounds::Image { .. }
        | ReadViewBounds::Audio { .. }
        | ReadViewBounds::File { .. } => false,
    };
    if valid {
        Ok(())
    } else {
        Err(FileMediaRegistryConstructionError::ViewBounds)
    }
}

/// Closed static registry construction failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileMediaRegistryConstructionError {
    /// Configured ceilings did not lower the compiled set.
    Ceilings,
    /// A finite inventory exceeded its compiled count.
    Inventory,
    /// Providers were declared without available strong isolation.
    IsolationUnavailable,
    /// Provider identity was duplicated.
    DuplicateProvider,
    /// Reader identity was duplicated.
    DuplicateReader,
    /// An exact media type was claimed by several readers.
    DuplicateMediaTypeClaim,
    /// One reader repeated a media type, view name, or reason code.
    DuplicateReaderMember,
    /// Probe bounds were zero, contradictory, or excessive.
    ProbeBounds,
    /// View bounds were absent, contradictory, or excessive.
    ViewBounds,
    /// Text fallback registration was absent or ambiguous.
    TextFallback,
}

impl fmt::Display for FileMediaRegistryConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Ceilings => "file media ceilings may only lower compiled limits",
            Self::Inventory => "file media registry inventory exceeds a compiled bound",
            Self::IsolationUnavailable => "file media isolation is unavailable",
            Self::DuplicateProvider => "file media provider identity is duplicated",
            Self::DuplicateReader => "file media reader identity is duplicated",
            Self::DuplicateMediaTypeClaim => "file media type has several registered readers",
            Self::DuplicateReaderMember => "file media reader member is duplicated",
            Self::ProbeBounds => "file media probe bounds are invalid",
            Self::ViewBounds => "file media view bounds are invalid",
            Self::TextFallback => "file media text fallback is invalid",
        })
    }
}

impl Error for FileMediaRegistryConstructionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_ambiguity_includes_structural_and_strong_claims() {
        assert!(recognized_probe_strength(
            ProbeStrength::StructuralCandidate
        ));
        assert!(recognized_probe_strength(ProbeStrength::Strong));
        assert!(!recognized_probe_strength(ProbeStrength::DeclaredCandidate));
    }

    #[test]
    fn streaming_text_terminal_validation_becomes_unknown() {
        assert!(streaming_text_terminal_becomes_unknown(
            ValidationEvidence::StreamingTextValidation
        ));
        assert!(!streaming_text_terminal_becomes_unknown(
            ValidationEvidence::StructuralValidation
        ));
    }
}
