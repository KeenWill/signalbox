//! Bounded PDF interpretation inside the supervised file-media worker.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    num::NonZeroU64,
    str::FromStr,
};

use lopdf::{Dictionary, Document, Error as LopdfError, Object, Stream};
use signalbox_file_media_runtime::{
    CancellationSignal, CanonicalJsonObjectSchema, CanonicalMediaType, FileMediaProvider,
    FileMediaProviderDeclaration, FileMediaProviderFailure, FileMediaProviderFuture,
    FileMediaProviderReadRequest, FileMediaProviderValidationRequest, FileReadInput,
    FileReaderName, FileReaderProviderName, FileReaderRevision, ProbeDeclaration,
    ProbeDeclarationInput, ProbeStrength, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, ReadAccessPattern, ReadViewBounds, ReadViewDeclaration,
    ReadViewName, ReaderDeclaration, ReaderDeclarationInput, ReaderIdentity, ReasonCode,
    StreamingTextFallback, ValidationEvidence, VerifiedBlobSource,
};

const MEDIA_TYPE: &str = "application/pdf";
const PROVIDER_NAME: &str = "pdf";
const READER_NAME: &str = "lopdf";
const READER_REVISION: &str = "lopdf-0-44-v1";
const TEXT_VIEW: &str = "text";
const METADATA_VIEW: &str = "metadata";
const MALFORMED_REASON: &str = "malformed_pdf";
const DECODED_CONTENT_LIMIT: &str = "decoded_content_limit";
const OBJECT_COUNT_LIMIT: &str = "object_count_limit";
const PAGE_COUNT_LIMIT: &str = "page_count_limit";
// Hard safety ceilings bound source reads and parser memory before untrusted PDF decoding.
const PDF_HEADER_BYTES: u64 = 8;
const PDF_TRAILER_BYTES: u64 = 65_536;
const VALIDATION_SOURCE_BYTES: u64 = 262_144;
const ROOT_VALIDATION_BYTES: u64 = 4_096;
// Hard safety ceiling bounds xref-stream decompression before entry parsing.
const MAX_XREF_STREAM_BYTES: usize = VALIDATION_SOURCE_BYTES as usize;
const MAX_TOTAL_XREF_STREAM_BYTES: usize = VALIDATION_SOURCE_BYTES as usize;
// Hard safety ceiling bounds decompressed object streams independently of source layout.
const MAX_OBJECT_STREAM_BYTES: usize = 1024 * 1024;
// Hard safety ceiling bounds aggregate object-stream retention before lopdf construction.
const MAX_TOTAL_OBJECT_STREAM_BYTES: usize = 16 * MAX_OBJECT_STREAM_BYTES;
const READ_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const SOURCE_CHUNK_BYTES: u64 = 256 * 1024;
// Hard safety ceilings bound worker output and decompression-amplified memory.
const TEXT_OUTPUT_BYTES: usize = signalbox_file_media_runtime::MAX_TEXT_BODY_BYTES;
const METADATA_OUTPUT_BYTES: usize = 16 * 1024;
const MAX_DECOMPRESSED_PAGE_BYTES: usize = 1024 * 1024;

const MAX_TOTAL_DECOMPRESSED_CONTENT_BYTES: usize = 256 * MAX_DECOMPRESSED_PAGE_BYTES;
// Hard safety ceilings bound object traversal and page-index construction latency.
const MAX_OBJECTS: usize = 10_000;
const MAX_PAGES: usize = 10_000;
const MAX_PAGE_CONTENT_STREAMS: usize = MAX_OBJECTS;
const MAX_GENERATION: u64 = 65_535;
const MAX_OBJECT_NUMBER: u64 = u32::MAX as u64;
const MAX_INDIRECT_LENGTH_DEPTH: usize = 32;

#[derive(Clone, Copy)]
enum ValidationMode {
    Complete,
    Bounded,
}

impl ValidationMode {
    const fn is_bounded(self) -> bool {
        matches!(self, Self::Bounded)
    }
}

#[derive(Default)]
struct TrailerFacts {
    encrypted: bool,
    root: Option<IndirectReference>,
    size: Option<u64>,
    widths: Option<[u64; 3]>,
    index: Option<Vec<(u64, u64)>>,
    length: Option<StreamLength>,
    prev: Option<u64>,
    xref_stream: Option<u64>,
    filters: Vec<Vec<u8>>,
    decode_parameters: Option<Vec<u8>>,
    is_xref_stream: bool,
    indirect_xref_lengths: Vec<(IndirectReference, u64)>,
    decoded_xref_bytes: usize,
    xref_stream_limit_exceeded: bool,
}

#[derive(Default)]
struct ObjectStreamFacts {
    count: Option<u64>,
    filters: Vec<Vec<u8>>,
    decode_parameters: Option<Vec<u8>>,
    first: Option<u64>,
    is_object_stream: bool,
    length: Option<StreamLength>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamLength {
    Direct(u64),
    Indirect(IndirectReference),
}

struct CatalogFacts {
    pages: IndirectReference,
    version: Option<String>,
    object_end: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct IndirectReference {
    object_number: u64,
    generation: u64,
}

#[derive(Clone, Copy)]
enum XrefLocation {
    Uncompressed(u64),
    Compressed { stream_object: u64, index: u64 },
}

#[derive(Clone, Copy)]
struct LiveXrefEntry {
    reference: IndirectReference,
    location: XrefLocation,
}

struct CachedObjectStream {
    object_number: u64,
    reference: IndirectReference,
    offset: u64,
    resolved_length: Option<(IndirectReference, u64)>,
}

struct ParsedXref {
    facts: TrailerFacts,
    live_entries: Vec<LiveXrefEntry>,
    declared_objects: BTreeSet<u64>,
    object_limit_exceeded: bool,
}

struct ValidationBudget {
    remaining_bytes: u64,
    remaining_ranges: u32,
}

impl ValidationBudget {
    fn new(maximum_source_bytes: u64, maximum_ranges: u32) -> Self {
        Self {
            remaining_bytes: maximum_source_bytes.min(VALIDATION_SOURCE_BYTES),
            remaining_ranges: maximum_ranges,
        }
    }

    fn available_after_reserving(&self, bytes: u64, ranges: u32) -> u64 {
        if self.remaining_ranges <= ranges {
            0
        } else {
            self.remaining_bytes.saturating_sub(bytes)
        }
    }

    fn can_read(&self, length: u64) -> bool {
        length > 0 && self.remaining_ranges > 0 && length <= self.remaining_bytes
    }
}

enum Preflight {
    Ready,
    Encrypted,
    ObjectLimit,
    DecodedLimit,
}

enum PageCollectionError {
    Limit,
    Malformed,
}

/// PDF adapter registered in the dedicated worker catalog.
#[derive(Clone, Copy, Debug, Default)]
pub struct PdfProvider;

impl PdfProvider {
    /// Constructs the stateless PDF provider.
    pub const fn new() -> Self {
        Self
    }
}

impl FileMediaProvider for PdfProvider {
    fn declaration(&self) -> FileMediaProviderDeclaration {
        declaration().unwrap_or_else(|error| {
            eprintln!("PDF declaration failed: {error}");
            std::process::exit(2);
        })
    }

    fn probe<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProviderFuture<'a, ProcessorProbeOutput> {
        Box::pin(async move {
            require_reader(reader)?;
            require_active(cancellation)?;
            let prefix_length = source.byte_length().get().min(PDF_HEADER_BYTES);
            let prefix = read_range(source, 0, prefix_length).await?;
            require_active(cancellation)?;
            if !prefix.starts_with(b"%PDF-") {
                return Ok(ProcessorProbeOutput::NoMatch);
            }
            if valid_header(&prefix) {
                Ok(ProcessorProbeOutput::Candidate {
                    media_type: String::from(MEDIA_TYPE),
                    strength: ProbeStrength::Strong,
                })
            } else {
                Ok(malformed_probe())
            }
        })
    }

    fn inspect<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: FileMediaProviderValidationRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProviderFuture<'a, ProcessorValidationOutput> {
        Box::pin(async move {
            require_reader(reader)?;
            require_active(cancellation)?;
            if request.media_type.as_str() != MEDIA_TYPE {
                return Err(FileMediaProviderFailure::Failed);
            }
            let source_length = source.byte_length().get();
            let mut budget =
                ValidationBudget::new(request.maximum_source_bytes, request.maximum_ranges);
            if source_length <= budget.remaining_bytes && budget.remaining_ranges > 0 {
                let bytes = read_validation_range(source, &mut budget, 0, source_length).await?;
                require_active(cancellation)?;
                return inspect_complete(&bytes, request.evidence);
            }
            inspect_bounded(source, request.evidence, cancellation, &mut budget).await
        })
    }

    fn read<'a>(
        &'a self,
        reader: &'a ReaderIdentity,
        request: FileMediaProviderReadRequest,
        source: &'a dyn VerifiedBlobSource,
        cancellation: &'a dyn CancellationSignal,
    ) -> FileMediaProviderFuture<'a, ProcessorReadOutput> {
        Box::pin(async move {
            require_reader(reader)?;
            require_active(cancellation)?;
            if request.detected_media_type.as_str() != MEDIA_TYPE {
                return Err(FileMediaProviderFailure::Failed);
            }
            let options = match &request.input {
                FileReadInput::Initial { options } => options,
                FileReadInput::Continuation { .. } => {
                    return Ok(ProcessorReadOutput::InvalidViewArguments);
                }
            };
            if !empty_options(options) {
                return Ok(ProcessorReadOutput::InvalidViewArguments);
            }
            let source_length = source.byte_length().get();
            if source_length > READ_SOURCE_BYTES {
                return Ok(ProcessorReadOutput::SourceTooLarge {
                    maximum_bytes: READ_SOURCE_BYTES,
                });
            }
            let bytes = read_all(source, cancellation).await?;
            match preflight_document(&bytes)? {
                Preflight::Ready => {}
                Preflight::Encrypted => return Err(FileMediaProviderFailure::Failed),
                Preflight::ObjectLimit => {
                    return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
                        limit_kind: String::from(OBJECT_COUNT_LIMIT),
                    });
                }
                Preflight::DecodedLimit => {
                    return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
                        limit_kind: String::from(DECODED_CONTENT_LIMIT),
                    });
                }
            }
            let document =
                Document::load_mem(&bytes).map_err(|_| FileMediaProviderFailure::Failed)?;
            require_active(cancellation)?;
            if document.is_encrypted() {
                return Err(FileMediaProviderFailure::Failed);
            }
            let pages = match collect_pages(&document) {
                Ok(pages) => pages,
                Err(PageCollectionError::Limit) => {
                    return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
                        limit_kind: String::from(PAGE_COUNT_LIMIT),
                    });
                }
                Err(PageCollectionError::Malformed) => {
                    return Err(FileMediaProviderFailure::Failed);
                }
            };
            match request.view.as_str() {
                TEXT_VIEW => read_text(&document, &pages, cancellation),
                METADATA_VIEW => read_metadata(&document, pages.len()),
                _ => Ok(ProcessorReadOutput::UnsupportedView),
            }
        })
    }
}

/// Returns the exact declaration shared by worker and daemon registration.
pub fn declaration() -> Result<FileMediaProviderDeclaration, Box<dyn Error>> {
    let provider = FileReaderProviderName::try_new(PROVIDER_NAME)?;
    let text_view = ReadViewDeclaration::try_new(
        ReadViewName::try_new(TEXT_VIEW)?,
        String::from("Extracts embedded PDF text without OCR."),
        CanonicalJsonObjectSchema::try_new(r#"{"additionalProperties":false,"type":"object"}"#)?,
        ReadAccessPattern::Streaming { maximum_ranges: 32 },
        ReadViewBounds::Text {
            source_bytes: READ_SOURCE_BYTES,
            output_bytes: TEXT_OUTPUT_BYTES,
        },
    )?;
    let metadata_view = ReadViewDeclaration::try_new(
        ReadViewName::try_new(METADATA_VIEW)?,
        String::from("Returns bounded PDF version, page, and object counts."),
        CanonicalJsonObjectSchema::try_new(r#"{"additionalProperties":false,"type":"object"}"#)?,
        ReadAccessPattern::Streaming { maximum_ranges: 32 },
        ReadViewBounds::Structured {
            source_bytes: READ_SOURCE_BYTES,
            output_bytes: METADATA_OUTPUT_BYTES,
            depth: 4,
            nodes: 32,
            string_bytes: 1024,
        },
    )?;
    let reader = ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: provider.clone(),
        reader: FileReaderName::try_new(READER_NAME)?,
        revision: FileReaderRevision::try_new(READER_REVISION)?,
        media_types: vec![CanonicalMediaType::from_str(MEDIA_TYPE)?],
        probe: ProbeDeclaration::new(ProbeDeclarationInput {
            prefix_bytes: PDF_HEADER_BYTES,
            suffix_bytes: PDF_TRAILER_BYTES,
            range_count: signalbox_file_media_runtime::MAX_PROBE_RANGES,
            cumulative_bytes: VALIDATION_SOURCE_BYTES,
        }),
        views: vec![text_view, metadata_view],
        reason_codes: vec![
            ReasonCode::try_new(MALFORMED_REASON)?,
            ReasonCode::try_new(DECODED_CONTENT_LIMIT)?,
            ReasonCode::try_new(OBJECT_COUNT_LIMIT)?,
            ReasonCode::try_new(PAGE_COUNT_LIMIT)?,
        ],
        streaming_text_fallback: StreamingTextFallback::Disabled,
    })?;
    Ok(FileMediaProviderDeclaration::try_new(
        provider,
        vec![reader],
    )?)
}

async fn inspect_bounded(
    source: &dyn VerifiedBlobSource,
    evidence: ValidationEvidence,
    cancellation: &dyn CancellationSignal,
    budget: &mut ValidationBudget,
) -> Result<ProcessorValidationOutput, FileMediaProviderFailure> {
    let source_length = source.byte_length().get();
    if budget.remaining_ranges < 2
        || budget.remaining_bytes <= PDF_HEADER_BYTES + 2 * ROOT_VALIDATION_BYTES
    {
        return Ok(malformed_validation());
    }
    let prefix = read_validation_range(source, budget, 0, PDF_HEADER_BYTES).await?;
    let suffix_length = source_length.min(PDF_TRAILER_BYTES).min(
        budget
            .remaining_bytes
            .saturating_sub(2 * ROOT_VALIDATION_BYTES),
    );
    if !budget.can_read(suffix_length) {
        return Ok(malformed_validation());
    }
    let suffix =
        read_validation_range(source, budget, source_length - suffix_length, suffix_length).await?;
    require_active(cancellation)?;
    if !valid_header(&prefix) || !has_pdf_trailer(&suffix) {
        return Ok(malformed_validation());
    }
    let Some(xref_offset) = startxref_offset(&suffix) else {
        return Ok(malformed_validation());
    };
    if xref_offset >= source_length {
        return Ok(malformed_validation());
    }
    let suffix_offset = source_length - suffix_length;
    let xref_bytes = if xref_offset >= suffix_offset {
        let relative_offset = usize::try_from(xref_offset - suffix_offset)
            .map_err(|_| FileMediaProviderFailure::Failed)?;
        suffix
            .get(relative_offset..)
            .ok_or(FileMediaProviderFailure::Failed)?
            .to_vec()
    } else {
        let missing_length = suffix_offset - xref_offset;
        let additional_length =
            missing_length.min(budget.available_after_reserving(2 * ROOT_VALIDATION_BYTES, 2));
        if !budget.can_read(additional_length) {
            return Ok(malformed_validation());
        }
        let mut bytes =
            read_validation_range(source, budget, xref_offset, additional_length).await?;
        if additional_length == missing_length {
            bytes.extend_from_slice(&suffix);
        }
        bytes
    };
    require_active(cancellation)?;
    let Some(mut parsed_xref) = parse_xref_structure(&xref_bytes, xref_offset) else {
        return Ok(malformed_validation());
    };
    let mut visited = BTreeSet::from([xref_offset]);
    if !merge_bounded_supplemental_xref(source, budget, &mut parsed_xref, xref_offset, &mut visited)
        .await?
    {
        return Ok(malformed_validation());
    }
    let mut newer_xref_offset = xref_offset;
    while let Some(prev) = parsed_xref.facts.prev {
        if !visited.insert(prev) || prev >= newer_xref_offset {
            return Ok(malformed_validation());
        }
        let length = (newer_xref_offset - prev)
            .min(budget.available_after_reserving(2 * ROOT_VALIDATION_BYTES, 2));
        if !budget.can_read(length) {
            return Ok(malformed_validation());
        }
        let previous_bytes = read_validation_range(source, budget, prev, length).await?;
        let Some(mut previous) = parse_xref_structure(&previous_bytes, prev) else {
            return Ok(malformed_validation());
        };
        if !merge_bounded_supplemental_xref(source, budget, &mut previous, prev, &mut visited)
            .await?
        {
            return Ok(malformed_validation());
        }
        merge_previous_xref(&mut parsed_xref, previous);
        newer_xref_offset = prev;
        require_active(cancellation)?;
    }
    let lengths = parsed_xref.facts.indirect_xref_lengths.clone();
    for (index, (reference, expected)) in lengths.iter().copied().enumerate() {
        let remaining = u64::try_from(lengths.len() - index - 1).unwrap_or(u64::MAX);
        let resolved = resolve_bounded_integer_object(
            source,
            budget,
            &parsed_xref,
            reference,
            (
                2 * ROOT_VALIDATION_BYTES + remaining * ROOT_VALIDATION_BYTES,
                2 + u32::try_from(remaining).unwrap_or(u32::MAX),
            ),
            &mut BTreeSet::new(),
        )
        .await?;
        if resolved != Some(expected) {
            return Ok(malformed_validation());
        }
    }
    if parsed_xref.facts.encrypted {
        return Ok(ProcessorValidationOutput::EncryptedOrLocked {
            media_type: String::from(MEDIA_TYPE),
        });
    }
    if parsed_xref.object_limit_exceeded
        || parsed_xref.facts.xref_stream_limit_exceeded
        || !effective_size_contains_declarations(&parsed_xref)
        || !valid_xref_targets(&parsed_xref, source_length)
    {
        return Ok(malformed_validation());
    }
    let Some((cached_object_streams, object_stream_ranges)) =
        bounded_object_stream_targets(source, budget, &parsed_xref).await?
    else {
        return Ok(malformed_validation());
    };
    let Some((root, root_location)) = root_location(&parsed_xref) else {
        return Ok(malformed_validation());
    };
    let mut cached_uncompressed_range = None;
    let catalog = match root_location {
        XrefLocation::Uncompressed(root_offset) => {
            let root_length = budget
                .remaining_bytes
                .saturating_sub(ROOT_VALIDATION_BYTES)
                .min(source_length - root_offset);
            if !budget.can_read(root_length) {
                return Ok(malformed_validation());
            }
            let root_bytes =
                read_validation_range(source, budget, root_offset, root_length).await?;
            let Some(catalog) = catalog_facts(&root_bytes, root) else {
                return Ok(malformed_validation());
            };
            let Some(end) = catalog.object_end else {
                return Ok(malformed_validation());
            };
            let end_offset = u64::try_from(end).map_err(|_| FileMediaProviderFailure::Failed)?;
            let Some(cached_offset) = root_offset.checked_add(end_offset) else {
                return Ok(malformed_validation());
            };
            let Some(cached_bytes) = root_bytes.get(end..) else {
                return Ok(malformed_validation());
            };
            cached_uncompressed_range = Some((cached_offset, cached_bytes.to_vec()));
            catalog
        }
        XrefLocation::Compressed {
            stream_object,
            index,
        } => {
            let Some(stream) = cached_object_streams.iter().find(|stream| {
                stream.object_number == stream_object
                    && stream.reference.object_number == stream_object
            }) else {
                return Ok(malformed_validation());
            };
            let Some(stream_bytes) = cached_range_at(&object_stream_ranges, stream.offset) else {
                return Ok(malformed_validation());
            };
            let Some(object) = object_stream_object(
                stream_bytes,
                stream.reference,
                root,
                index,
                MAX_OBJECT_STREAM_BYTES,
                stream.resolved_length,
            ) else {
                return Ok(malformed_validation());
            };
            let Some((catalog, mut cursor)) = parse_catalog_dictionary(&object, 0) else {
                return Ok(malformed_validation());
            };
            skip_pdf_space_and_comments(&object, &mut cursor);
            if cursor != object.len() {
                return Ok(malformed_validation());
            }
            catalog
        }
    };
    let pages = catalog.pages;
    let Some(pages_entry) = parsed_xref
        .live_entries
        .iter()
        .find(|entry| entry.reference == pages)
    else {
        return Ok(malformed_validation());
    };
    match pages_entry.location {
        XrefLocation::Uncompressed(pages_offset) => {
            let cached_pages =
                cached_uncompressed_range
                    .as_ref()
                    .and_then(|(cached_offset, bytes)| {
                        let relative = pages_offset.checked_sub(*cached_offset)?;
                        bytes.get(usize::try_from(relative).ok()?..)
                    });
            let owned_pages;
            let pages_bytes = match cached_pages {
                Some(bytes) => bytes,
                None => {
                    let pages_length = budget.remaining_bytes.min(source_length - pages_offset);
                    if !budget.can_read(pages_length) {
                        return Ok(malformed_validation());
                    }
                    owned_pages =
                        read_validation_range(source, budget, pages_offset, pages_length).await?;
                    owned_pages.as_slice()
                }
            };
            if !object_is_pages_for_xref(pages_bytes, pages, Some(&parsed_xref.live_entries)) {
                return Ok(malformed_validation());
            }
        }
        XrefLocation::Compressed {
            stream_object,
            index,
        } => {
            let Some(stream) = cached_object_streams
                .iter()
                .find(|stream| stream.object_number == stream_object)
            else {
                return Ok(malformed_validation());
            };
            let Some(stream_bytes) = cached_range_at(&object_stream_ranges, stream.offset) else {
                return Ok(malformed_validation());
            };
            let Some(object) = object_stream_object(
                stream_bytes,
                stream.reference,
                pages,
                index,
                MAX_OBJECT_STREAM_BYTES,
                stream.resolved_length,
            ) else {
                return Ok(malformed_validation());
            };
            if !pages_dictionary_is_valid_for_xref(&object, pages, &parsed_xref.live_entries) {
                return Ok(malformed_validation());
            }
        }
    }
    validated_output(
        evidence,
        catalog.version.unwrap_or_else(|| header_version(&prefix)),
        None,
        None,
        ValidationMode::Bounded,
    )
}

fn inspect_complete(
    bytes: &[u8],
    evidence: ValidationEvidence,
) -> Result<ProcessorValidationOutput, FileMediaProviderFailure> {
    if !valid_header(bytes) || !has_pdf_trailer(bytes) {
        return Ok(malformed_validation());
    }
    let Some(xref_offset) = startxref_offset(bytes) else {
        return Ok(malformed_validation());
    };
    let Ok(xref_offset) = usize::try_from(xref_offset) else {
        return Ok(malformed_validation());
    };
    let Some(parsed_xref) = parse_xref_chain(bytes, xref_offset) else {
        return Ok(malformed_validation());
    };
    if parsed_xref.facts.encrypted {
        return Ok(ProcessorValidationOutput::EncryptedOrLocked {
            media_type: String::from(MEDIA_TYPE),
        });
    }
    if parsed_xref.object_limit_exceeded || !valid_xref_targets(&parsed_xref, bytes.len() as u64) {
        return Ok(malformed_validation());
    }
    match object_streams_fit_expansion_limit(bytes, &parsed_xref) {
        Ok(true) => {}
        Ok(false) | Err(_) => return Ok(malformed_validation()),
    }
    let document = match Document::load_mem(bytes) {
        Ok(document) => document,
        Err(_) => return Ok(malformed_validation()),
    };
    if document.is_encrypted() {
        return Ok(ProcessorValidationOutput::EncryptedOrLocked {
            media_type: String::from(MEDIA_TYPE),
        });
    }
    if document.objects.len() > MAX_OBJECTS {
        return Ok(malformed_validation());
    }
    let pages = match collect_pages(&document) {
        Ok(pages) => pages,
        Err(_) => return Ok(malformed_validation()),
    };
    validated_output(
        evidence,
        effective_version(&document),
        Some(pages.len()),
        Some(document.objects.len()),
        ValidationMode::Complete,
    )
}

fn validated_output(
    evidence: ValidationEvidence,
    version: String,
    pages: Option<usize>,
    objects: Option<usize>,
    mode: ValidationMode,
) -> Result<ProcessorValidationOutput, FileMediaProviderFailure> {
    let metadata_json = serde_json::to_string(&serde_json::json!({
        "bounded_validation": mode.is_bounded(),
        "objects": objects,
        "pages": pages,
        "version": version,
    }))
    .map_err(|_| FileMediaProviderFailure::Failed)?;
    Ok(ProcessorValidationOutput::Validated {
        media_type: String::from(MEDIA_TYPE),
        evidence,
        metadata_json,
    })
}

fn read_text(
    document: &Document,
    pages: &[(u32, lopdf::ObjectId)],
    cancellation: &dyn CancellationSignal,
) -> Result<ProcessorReadOutput, FileMediaProviderFailure> {
    read_text_with_decoding_budget(
        document,
        pages,
        cancellation,
        MAX_TOTAL_DECOMPRESSED_CONTENT_BYTES,
    )
}

fn read_text_with_decoding_budget(
    document: &Document,
    pages: &[(u32, lopdf::ObjectId)],
    cancellation: &dyn CancellationSignal,
    decoding_budget: usize,
) -> Result<ProcessorReadOutput, FileMediaProviderFailure> {
    let mut text = String::new();
    let mut remaining_decoded_bytes = decoding_budget;
    let mut decoded_sizes = BTreeMap::new();
    for (page_number, page_id) in pages {
        require_active(cancellation)?;
        validate_page_contents(document, *page_id)?;
        if !charge_page_content_decoding(
            document,
            *page_id,
            &mut decoded_sizes,
            &mut remaining_decoded_bytes,
            cancellation,
        )? {
            return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
                limit_kind: String::from(DECODED_CONTENT_LIMIT),
            });
        }
        let page_text =
            match document.extract_text_with_limit(&[*page_number], MAX_DECOMPRESSED_PAGE_BYTES) {
                Ok(text) => text,
                Err(LopdfError::Decompress(_)) => {
                    return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
                        limit_kind: String::from(DECODED_CONTENT_LIMIT),
                    });
                }
                Err(_) => return Err(FileMediaProviderFailure::Failed),
            };
        let Some(total) = text.len().checked_add(page_text.len()) else {
            return Ok(ProcessorReadOutput::OutputUnitTooLarge);
        };
        if total > TEXT_OUTPUT_BYTES {
            return Ok(ProcessorReadOutput::OutputUnitTooLarge);
        }
        text.push_str(&page_text);
    }
    Ok(ProcessorReadOutput::Text {
        body: text,
        truncated: false,
        cursor: None,
    })
}

fn read_metadata(
    document: &Document,
    page_count: usize,
) -> Result<ProcessorReadOutput, FileMediaProviderFailure> {
    let body_json = serde_json::to_string(&serde_json::json!({
        "encrypted": false,
        "objects": document.objects.len(),
        "pages": page_count,
        "version": effective_version(document),
    }))
    .map_err(|_| FileMediaProviderFailure::Failed)?;
    Ok(ProcessorReadOutput::Structured {
        body_json,
        truncated: false,
        cursor: None,
    })
}

fn preflight_document(bytes: &[u8]) -> Result<Preflight, FileMediaProviderFailure> {
    let xref_offset = startxref_offset(bytes).ok_or(FileMediaProviderFailure::Failed)?;
    let xref_offset = usize::try_from(xref_offset).map_err(|_| FileMediaProviderFailure::Failed)?;
    let parsed = parse_xref_chain(bytes, xref_offset).ok_or(FileMediaProviderFailure::Failed)?;
    if parsed.facts.encrypted {
        Ok(Preflight::Encrypted)
    } else if parsed.object_limit_exceeded {
        Ok(Preflight::ObjectLimit)
    } else if parsed.facts.xref_stream_limit_exceeded
        || !object_streams_fit_expansion_limit(bytes, &parsed)?
    {
        Ok(Preflight::DecodedLimit)
    } else {
        Ok(Preflight::Ready)
    }
}

fn object_streams_fit_expansion_limit(
    bytes: &[u8],
    parsed: &ParsedXref,
) -> Result<bool, FileMediaProviderFailure> {
    let stream_objects = parsed
        .live_entries
        .iter()
        .filter_map(|entry| match entry.location {
            XrefLocation::Compressed { stream_object, .. } => Some(stream_object),
            XrefLocation::Uncompressed(_) => None,
        })
        .collect::<BTreeSet<_>>();
    let mut total_decoded_bytes = 0_usize;
    // Shared across every target so a /Length integer carried by one heavily
    // compressed object stream is decoded once even when thousands of object
    // streams reference it, instead of re-decoding the carrier per target.
    let mut resolved_lengths = BTreeMap::new();
    for stream_object in stream_objects {
        let (stream_reference, stream_offset) =
            object_stream_offset(parsed, stream_object).ok_or(FileMediaProviderFailure::Failed)?;
        let stream_offset =
            usize::try_from(stream_offset).map_err(|_| FileMediaProviderFailure::Failed)?;
        let stream_bytes = bytes
            .get(stream_offset..)
            .ok_or(FileMediaProviderFailure::Failed)?;
        let resolved_length = match object_stream_declared_length(stream_bytes, stream_reference)
            .ok_or(FileMediaProviderFailure::Failed)?
        {
            StreamLength::Direct(_) => None,
            StreamLength::Indirect(reference) => {
                let length = resolve_integer_object(
                    bytes,
                    parsed,
                    reference,
                    &mut BTreeSet::new(),
                    &mut resolved_lengths,
                )
                .ok_or(FileMediaProviderFailure::Failed)?;
                Some((reference, length))
            }
        };
        let (facts, encoded) = parse_object_stream(stream_bytes, stream_reference, resolved_length)
            .ok_or(FileMediaProviderFailure::Failed)?;
        if !facts.is_object_stream
            || facts.count.is_none_or(|count| count > MAX_OBJECTS as u64)
            || facts.first.is_none()
        {
            return Err(FileMediaProviderFailure::Failed);
        }
        let Some(decoded) = decode_pdf_stream(
            encoded,
            &facts.filters,
            facts.decode_parameters.as_deref(),
            MAX_OBJECT_STREAM_BYTES,
        ) else {
            return Ok(false);
        };
        let Some(total) = total_decoded_bytes.checked_add(decoded.len()) else {
            return Ok(false);
        };
        if total > MAX_TOTAL_OBJECT_STREAM_BYTES {
            return Ok(false);
        }
        total_decoded_bytes = total;
    }
    Ok(true)
}

fn resolve_integer_object(
    bytes: &[u8],
    parsed: &ParsedXref,
    reference: IndirectReference,
    resolving: &mut BTreeSet<IndirectReference>,
    cache: &mut BTreeMap<IndirectReference, u64>,
) -> Option<u64> {
    if let Some(cached) = cache.get(&reference) {
        return Some(*cached);
    }
    if resolving.len() >= MAX_INDIRECT_LENGTH_DEPTH || !resolving.insert(reference) {
        return None;
    }
    let resolved = (|| {
        let entry = parsed
            .live_entries
            .iter()
            .find(|entry| entry.reference == reference)?;
        match entry.location {
            XrefLocation::Uncompressed(offset) => {
                let offset = usize::try_from(offset).ok()?;
                indirect_integer_object(bytes.get(offset..)?, reference)
            }
            XrefLocation::Compressed {
                stream_object,
                index,
            } => {
                let (stream_reference, stream_offset) =
                    object_stream_offset(parsed, stream_object)?;
                let stream_offset = usize::try_from(stream_offset).ok()?;
                let stream_bytes = bytes.get(stream_offset..)?;
                let resolved_length =
                    match object_stream_declared_length(stream_bytes, stream_reference)? {
                        StreamLength::Direct(_) => None,
                        StreamLength::Indirect(length_reference) => Some((
                            length_reference,
                            resolve_integer_object(
                                bytes,
                                parsed,
                                length_reference,
                                resolving,
                                cache,
                            )?,
                        )),
                    };
                let object = object_stream_object(
                    stream_bytes,
                    stream_reference,
                    reference,
                    index,
                    MAX_OBJECT_STREAM_BYTES,
                    resolved_length,
                )?;
                parse_integer_object_value(&object)
            }
        }
    })();
    resolving.remove(&reference);
    // Cache only successful resolutions: a `None` here can reflect the
    // current call's remaining cycle-detection depth rather than the
    // reference itself being unresolvable, so caching it could poison a
    // fresh top-level resolution that would otherwise succeed.
    if let Some(value) = resolved {
        cache.insert(reference, value);
    }
    resolved
}

fn parse_integer_object_value(bytes: &[u8]) -> Option<u64> {
    let mut cursor = 0;
    skip_pdf_space_and_comments(bytes, &mut cursor);
    let value = parse_nonnegative_integer(bytes, &mut cursor)?;
    skip_pdf_space_and_comments(bytes, &mut cursor);
    (cursor == bytes.len()).then_some(value)
}

fn collect_pages(document: &Document) -> Result<Vec<(u32, lopdf::ObjectId)>, PageCollectionError> {
    let catalog = document
        .catalog()
        .map_err(|_| PageCollectionError::Malformed)?;
    let pages_root = catalog
        .get(b"Pages")
        .and_then(lopdf::Object::as_reference)
        .map_err(|_| PageCollectionError::Malformed)?;
    let mut pending = vec![(pages_root, false, None)];
    let mut visited = BTreeSet::new();
    let mut subtree_counts = std::collections::BTreeMap::new();
    let mut pages = Vec::new();
    while let Some((object_id, exiting, expected_parent)) = pending.pop() {
        if exiting {
            let dictionary = document
                .get_dictionary(object_id)
                .map_err(|_| PageCollectionError::Malformed)?;
            let declared_count = dictionary
                .get(b"Count")
                .and_then(lopdf::Object::as_i64)
                .map_err(|_| PageCollectionError::Malformed)?;
            let kids = dictionary
                .get(b"Kids")
                .and_then(lopdf::Object::as_array)
                .map_err(|_| PageCollectionError::Malformed)?;
            let descendant_count = kids.iter().try_fold(0_usize, |total, kid| {
                let kid = kid
                    .as_reference()
                    .map_err(|_| PageCollectionError::Malformed)?;
                total
                    .checked_add(
                        *subtree_counts
                            .get(&kid)
                            .ok_or(PageCollectionError::Malformed)?,
                    )
                    .ok_or(PageCollectionError::Limit)
            })?;
            if usize::try_from(declared_count).ok() != Some(descendant_count) {
                return Err(PageCollectionError::Malformed);
            }
            subtree_counts.insert(object_id, descendant_count);
            continue;
        }
        if !visited.insert(object_id) {
            return Err(PageCollectionError::Malformed);
        }
        let dictionary = document
            .get_dictionary(object_id)
            .map_err(|_| PageCollectionError::Malformed)?;
        let node_type = dictionary
            .get(b"Type")
            .and_then(lopdf::Object::as_name)
            .map_err(|_| PageCollectionError::Malformed)?;
        if expected_parent.is_none() {
            match dictionary.get(b"Parent") {
                Ok(Object::Null) | Err(LopdfError::DictKey(_)) => {}
                Ok(_) | Err(_) => return Err(PageCollectionError::Malformed),
            }
        }
        if let Some(expected_parent) = expected_parent {
            let parent = dictionary
                .get(b"Parent")
                .and_then(lopdf::Object::as_reference)
                .map_err(|_| PageCollectionError::Malformed)?;
            if parent != expected_parent {
                return Err(PageCollectionError::Malformed);
            }
        }
        match node_type {
            b"Pages" => {
                let count = dictionary
                    .get(b"Count")
                    .and_then(lopdf::Object::as_i64)
                    .map_err(|_| PageCollectionError::Malformed)?;
                if count < 0 {
                    return Err(PageCollectionError::Malformed);
                }
                if usize::try_from(count).map_or(true, |count| count > MAX_PAGES) {
                    return Err(PageCollectionError::Limit);
                }
                let kids = dictionary
                    .get(b"Kids")
                    .and_then(lopdf::Object::as_array)
                    .map_err(|_| PageCollectionError::Malformed)?;
                let count = usize::try_from(count).map_err(|_| PageCollectionError::Limit)?;
                if pages.len().saturating_add(count) > MAX_PAGES {
                    return Err(PageCollectionError::Limit);
                }
                if kids.len() > MAX_PAGES
                    || pending
                        .len()
                        .checked_add(kids.len() + 1)
                        .is_none_or(|entries| entries > 2 * MAX_PAGES)
                {
                    return Err(PageCollectionError::Limit);
                }
                pending.push((object_id, true, expected_parent));
                for kid in kids.iter().rev() {
                    pending.push((
                        kid.as_reference()
                            .map_err(|_| PageCollectionError::Malformed)?,
                        false,
                        Some(object_id),
                    ));
                }
            }
            b"Page" => {
                match dictionary.get(b"Kids") {
                    Ok(Object::Null) | Err(LopdfError::DictKey(_)) => {}
                    Ok(_) | Err(_) => return Err(PageCollectionError::Malformed),
                }
                if pages.len() == MAX_PAGES {
                    return Err(PageCollectionError::Limit);
                }
                let page_number =
                    u32::try_from(pages.len() + 1).map_err(|_| PageCollectionError::Limit)?;
                pages.push((page_number, object_id));
                subtree_counts.insert(object_id, 1);
            }
            _ => return Err(PageCollectionError::Malformed),
        }
    }
    Ok(pages)
}

fn validate_page_contents(
    document: &Document,
    page_id: lopdf::ObjectId,
) -> Result<(), FileMediaProviderFailure> {
    let page = document
        .get_dictionary(page_id)
        .map_err(|_| FileMediaProviderFailure::Failed)?;
    let contents = match page.get(b"Contents") {
        Ok(contents) => contents,
        Err(LopdfError::DictKey(_)) => return Ok(()),
        Err(_) => return Err(FileMediaProviderFailure::Failed),
    };
    match contents {
        lopdf::Object::Reference(object_id) => {
            let object = document
                .get_object(*object_id)
                .map_err(|_| FileMediaProviderFailure::Failed)?;
            if !matches!(object, Object::Stream(_)) {
                return Err(FileMediaProviderFailure::Failed);
            }
        }
        lopdf::Object::Array(objects) => {
            if objects.len() > MAX_PAGE_CONTENT_STREAMS {
                return Err(FileMediaProviderFailure::Failed);
            }
            for object in objects {
                let object_id = object
                    .as_reference()
                    .map_err(|_| FileMediaProviderFailure::Failed)?;
                let object = document
                    .get_object(object_id)
                    .map_err(|_| FileMediaProviderFailure::Failed)?;
                if !matches!(object, Object::Stream(_)) {
                    return Err(FileMediaProviderFailure::Failed);
                }
            }
        }
        lopdf::Object::Null | lopdf::Object::Stream(_) => {}
        _ => return Err(FileMediaProviderFailure::Failed),
    }
    Ok(())
}

/// Charges the decoded size of every content stream the page references
/// against the read-wide decompression budget, decoding each referenced
/// stream once and re-charging its recorded size per use so repeated
/// references to a shared stream cannot multiply decompression work beyond
/// the aggregate budget.
fn charge_page_content_decoding(
    document: &Document,
    page_id: lopdf::ObjectId,
    decoded_sizes: &mut BTreeMap<lopdf::ObjectId, usize>,
    remaining: &mut usize,
    cancellation: &dyn CancellationSignal,
) -> Result<bool, FileMediaProviderFailure> {
    let page = document
        .get_dictionary(page_id)
        .map_err(|_| FileMediaProviderFailure::Failed)?;
    let contents = match page.get(b"Contents") {
        Ok(contents) => contents,
        Err(LopdfError::DictKey(_)) => return Ok(true),
        Err(_) => return Err(FileMediaProviderFailure::Failed),
    };
    let mut stream_ids = Vec::new();
    match contents {
        lopdf::Object::Reference(object_id) => stream_ids.push(*object_id),
        lopdf::Object::Array(objects) => {
            for object in objects {
                stream_ids.push(
                    object
                        .as_reference()
                        .map_err(|_| FileMediaProviderFailure::Failed)?,
                );
            }
        }
        lopdf::Object::Stream(stream) => {
            let Some(size) = decoded_content_stream_size(stream, *remaining)? else {
                return Ok(false);
            };
            *remaining -= size;
            return Ok(true);
        }
        lopdf::Object::Null => return Ok(true),
        _ => return Err(FileMediaProviderFailure::Failed),
    }
    for object_id in stream_ids {
        // A page can reference many distinct compressed content streams; check
        // cancellation between decodes so an authoritative signal is observed
        // before this helper spends the full aggregate budget on one page.
        require_active(cancellation)?;
        let size = match decoded_sizes.get(&object_id) {
            Some(size) => *size,
            None => {
                let Ok(Object::Stream(stream)) = document.get_object(object_id) else {
                    return Err(FileMediaProviderFailure::Failed);
                };
                let Some(size) = decoded_content_stream_size(stream, *remaining)? else {
                    return Ok(false);
                };
                decoded_sizes.insert(object_id, size);
                size
            }
        };
        if size > *remaining {
            return Ok(false);
        }
        *remaining -= size;
    }
    Ok(true)
}

fn decoded_content_stream_size(
    stream: &Stream,
    remaining: usize,
) -> Result<Option<usize>, FileMediaProviderFailure> {
    let limit = MAX_DECOMPRESSED_PAGE_BYTES.min(remaining);
    match stream.decompressed_content_with_limit(limit) {
        Ok(decoded) => Ok(Some(decoded.len())),
        Err(LopdfError::Decompress(_)) => Ok(None),
        Err(_) => Err(FileMediaProviderFailure::Failed),
    }
}

async fn read_all(
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
) -> Result<Vec<u8>, FileMediaProviderFailure> {
    let source_length = source.byte_length().get();
    let capacity = usize::try_from(source_length).map_err(|_| FileMediaProviderFailure::Failed)?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut offset = 0_u64;
    while offset < source_length {
        require_active(cancellation)?;
        let length = (source_length - offset).min(SOURCE_CHUNK_BYTES);
        bytes.extend(read_range(source, offset, length).await?);
        offset = offset
            .checked_add(length)
            .ok_or(FileMediaProviderFailure::Failed)?;
    }
    Ok(bytes)
}

async fn read_range(
    source: &dyn VerifiedBlobSource,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>, FileMediaProviderFailure> {
    let length = NonZeroU64::new(length).ok_or(FileMediaProviderFailure::Failed)?;
    source
        .read_range(offset, length)
        .await
        .map_err(|_| FileMediaProviderFailure::Failed)
}

async fn read_validation_range(
    source: &dyn VerifiedBlobSource,
    budget: &mut ValidationBudget,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>, FileMediaProviderFailure> {
    if !budget.can_read(length) {
        return Err(FileMediaProviderFailure::Failed);
    }
    budget.remaining_bytes -= length;
    budget.remaining_ranges -= 1;
    read_range(source, offset, length).await
}

fn require_reader(reader: &ReaderIdentity) -> Result<(), FileMediaProviderFailure> {
    if reader.provider().as_str() == PROVIDER_NAME
        && reader.reader().as_str() == READER_NAME
        && reader.revision().as_str() == READER_REVISION
    {
        Ok(())
    } else {
        Err(FileMediaProviderFailure::Failed)
    }
}

fn require_active(cancellation: &dyn CancellationSignal) -> Result<(), FileMediaProviderFailure> {
    if cancellation.is_cancelled() {
        Err(FileMediaProviderFailure::Failed)
    } else {
        Ok(())
    }
}

fn valid_header(bytes: &[u8]) -> bool {
    bytes.len() >= PDF_HEADER_BYTES as usize
        && bytes.starts_with(b"%PDF-")
        && bytes[5].is_ascii_digit()
        && bytes[6] == b'.'
        && bytes[7].is_ascii_digit()
}

fn has_pdf_trailer(bytes: &[u8]) -> bool {
    startxref_offset(bytes).is_some()
}

fn startxref_offset(bytes: &[u8]) -> Option<u64> {
    let marker = last_keyword_outside_comments(bytes, b"startxref")?;
    let mut cursor = marker.checked_add(b"startxref".len())?;
    skip_pdf_space_and_comments(bytes, &mut cursor);
    let offset = parse_unsigned(bytes, &mut cursor)?;
    loop {
        skip_pdf_whitespace(bytes, &mut cursor);
        if bytes
            .get(cursor..)
            .is_some_and(|remaining| remaining.starts_with(b"%%EOF"))
        {
            break;
        }
        if bytes.get(cursor) != Some(&b'%') {
            return None;
        }
        while bytes
            .get(cursor)
            .is_some_and(|byte| *byte != b'\n' && *byte != b'\r')
        {
            cursor += 1;
        }
    }
    if !consume_keyword(bytes, &mut cursor, b"%%EOF") {
        return None;
    }
    skip_pdf_whitespace(bytes, &mut cursor);
    (cursor == bytes.len()).then_some(offset)
}

fn last_keyword_outside_comments(bytes: &[u8], keyword: &[u8]) -> Option<usize> {
    let mut cursor = 0;
    let mut latest = None;
    let mut in_comment = false;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\n' | b'\r' => {
                in_comment = false;
                cursor += 1;
            }
            b'%' if !in_comment => {
                in_comment = true;
                cursor += 1;
            }
            _ if in_comment => cursor += 1,
            _ => {
                let starts_at_boundary = cursor == 0 || is_pdf_delimiter(bytes[cursor - 1]);
                let mut end = cursor;
                if starts_at_boundary && consume_keyword(bytes, &mut end, keyword) {
                    latest = Some(cursor);
                    cursor = end;
                } else {
                    cursor += 1;
                }
            }
        }
    }
    latest
}

fn parse_xref_structure(bytes: &[u8], section_offset: u64) -> Option<ParsedXref> {
    let mut cursor = 0;
    if consume_keyword(bytes, &mut cursor, b"xref") {
        parse_classic_xref(bytes, cursor)
    } else {
        parse_xref_stream(bytes, cursor, section_offset)
    }
}

fn parse_classic_xref(bytes: &[u8], mut cursor: usize) -> Option<ParsedXref> {
    let mut live_entries = Vec::new();
    let mut declared_objects = BTreeSet::new();
    let mut object_limit_exceeded = false;
    loop {
        skip_pdf_space_and_comments(bytes, &mut cursor);
        if consume_keyword(bytes, &mut cursor, b"trailer") {
            break;
        }
        let first_object = parse_unsigned(bytes, &mut cursor)?;
        skip_pdf_space_and_comments(bytes, &mut cursor);
        let count = parse_unsigned(bytes, &mut cursor)?;
        for index in 0..count {
            let object_number = first_object.checked_add(index)?;
            if object_number > MAX_OBJECT_NUMBER {
                return None;
            }
            if !declared_objects.insert(object_number) {
                return None;
            }
            skip_pdf_space_and_comments(bytes, &mut cursor);
            let offset = parse_unsigned(bytes, &mut cursor)?;
            skip_pdf_space_and_comments(bytes, &mut cursor);
            let generation = parse_unsigned(bytes, &mut cursor)?;
            if generation > MAX_GENERATION {
                return None;
            }
            skip_required_pdf_space_and_comments(bytes, &mut cursor)?;
            let state = *bytes.get(cursor)?;
            if state != b'n' && state != b'f' {
                return None;
            }
            cursor += 1;
            skip_required_pdf_space_and_comments(bytes, &mut cursor)?;
            if state == b'n' {
                if object_number == 0 {
                    return None;
                }
                live_entries.push(LiveXrefEntry {
                    reference: IndirectReference {
                        object_number,
                        generation,
                    },
                    location: XrefLocation::Uncompressed(offset),
                });
                object_limit_exceeded = live_entries.len() > MAX_OBJECTS;
            }
        }
    }
    let (facts, _) = parse_trailer_dictionary(bytes, cursor)?;
    let facts = valid_trailer_facts(facts, false)?;
    let size = facts.size?;
    if declared_objects.iter().any(|object| *object >= size) {
        return None;
    }
    Some(ParsedXref {
        facts,
        live_entries,
        declared_objects,
        object_limit_exceeded,
    })
}

fn parse_xref_stream(bytes: &[u8], mut cursor: usize, section_offset: u64) -> Option<ParsedXref> {
    let xref_object = parse_unsigned(bytes, &mut cursor)?;
    if xref_object == 0 || xref_object > MAX_OBJECT_NUMBER {
        return None;
    }
    skip_required_pdf_space_and_comments(bytes, &mut cursor)?;
    let xref_generation = parse_unsigned(bytes, &mut cursor)?;
    if xref_generation > MAX_GENERATION {
        return None;
    }
    skip_required_pdf_space_and_comments(bytes, &mut cursor)?;
    if !consume_keyword(bytes, &mut cursor, b"obj") {
        return None;
    }
    skip_pdf_space_and_comments(bytes, &mut cursor);
    let (facts, mut cursor) = parse_trailer_dictionary(bytes, cursor)?;
    skip_pdf_space_and_comments(bytes, &mut cursor);
    if !consume_keyword(bytes, &mut cursor, b"stream") {
        return None;
    }
    let mut facts = valid_trailer_facts(facts, true)?;
    consume_stream_line_end(bytes, &mut cursor)?;
    let stream_length = match facts.length? {
        StreamLength::Direct(length) => usize::try_from(length).ok()?,
        StreamLength::Indirect(reference) => {
            let inferred = inferred_xref_stream_length(bytes, cursor)?;
            facts
                .indirect_xref_lengths
                .push((reference, u64::try_from(inferred).ok()?));
            inferred
        }
    };
    let stream_end = cursor.checked_add(stream_length)?;
    let encoded_stream = bytes.get(cursor..stream_end)?;
    cursor = stream_end;
    skip_pdf_whitespace(bytes, &mut cursor);
    if !consume_keyword(bytes, &mut cursor, b"endstream") {
        return None;
    }
    skip_pdf_space_and_comments(bytes, &mut cursor);
    if !consume_keyword(bytes, &mut cursor, b"endobj") {
        return None;
    }
    let widths = facts.widths?;
    let indexes = facts
        .index
        .clone()
        .unwrap_or_else(|| vec![(0, facts.size.unwrap_or(0))]);
    let decoded_length = xref_decoded_length(widths, &indexes)?;
    if decoded_length > MAX_XREF_STREAM_BYTES {
        return None;
    }
    facts.decoded_xref_bytes = decoded_length;
    facts.xref_stream_limit_exceeded = decoded_length > MAX_TOTAL_XREF_STREAM_BYTES;
    let stream = decode_xref_stream(
        encoded_stream,
        &facts.filters,
        facts.decode_parameters.as_deref(),
        decoded_length,
    )?;
    let (mut live_entries, mut declared_objects, mut object_limit_exceeded) =
        parse_xref_stream_entries(&stream, widths, &indexes)?;
    let size = facts.size?;
    if declared_objects.iter().any(|object| *object >= size) || xref_object >= size {
        return None;
    }
    declared_objects.insert(xref_object);
    let xref_reference = IndirectReference {
        object_number: xref_object,
        generation: xref_generation,
    };
    if let Some(entry) = live_entries
        .iter()
        .find(|entry| entry.reference.object_number == xref_reference.object_number)
    {
        if entry.reference != xref_reference
            || !matches!(entry.location, XrefLocation::Uncompressed(offset) if offset == section_offset)
        {
            return None;
        }
    } else {
        live_entries.push(LiveXrefEntry {
            reference: xref_reference,
            location: XrefLocation::Uncompressed(section_offset),
        });
        object_limit_exceeded = live_entries.len() > MAX_OBJECTS;
    }
    Some(ParsedXref {
        facts,
        live_entries,
        declared_objects,
        object_limit_exceeded,
    })
}

fn valid_trailer_facts(mut facts: TrailerFacts, stream: bool) -> Option<TrailerFacts> {
    let size = facts.size?;
    if size == 0 || facts.root.is_some_and(|root| root.object_number >= size) {
        return None;
    }
    if stream
        && (!facts.is_xref_stream
            || facts
                .widths
                .is_none_or(|widths| widths.iter().all(|width| *width == 0)))
    {
        return None;
    }
    facts.is_xref_stream = stream;
    Some(facts)
}

fn parse_trailer_dictionary(bytes: &[u8], mut cursor: usize) -> Option<(TrailerFacts, usize)> {
    skip_pdf_space_and_comments(bytes, &mut cursor);
    if !bytes.get(cursor..)?.starts_with(b"<<") {
        return None;
    }
    cursor += 2;
    let mut facts = TrailerFacts::default();
    loop {
        skip_pdf_space_and_comments(bytes, &mut cursor);
        if bytes.get(cursor..)?.starts_with(b">>") {
            return Some((facts, cursor + 2));
        }
        let key = parse_name(bytes, &mut cursor)?;
        skip_pdf_space_and_comments(bytes, &mut cursor);
        let value_start = cursor;
        skip_pdf_object(bytes, &mut cursor, 0)?;
        match key.as_slice() {
            b"Encrypt" => {
                facts.encrypted = parse_encryption_value(bytes, value_start, cursor)?;
            }
            b"Root" => {
                facts.root = parse_optional_indirect_reference(bytes, value_start, cursor)?;
            }
            b"Size" => {
                facts.size = Some(parse_nonnegative_integer_value(bytes, value_start, cursor)?);
            }
            b"W" => {
                let mut value_cursor = value_start;
                facts.widths = Some(parse_widths(bytes, &mut value_cursor)?);
            }
            b"Index" => {
                facts.index = parse_optional_index(bytes, value_start, cursor)?;
            }
            b"Length" => {
                facts.length = Some(parse_stream_length(bytes, value_start, cursor)?);
            }
            b"Prev" => {
                facts.prev = parse_optional_nonnegative_integer(bytes, value_start, cursor)?;
            }
            b"XRefStm" => {
                facts.xref_stream = parse_optional_nonnegative_integer(bytes, value_start, cursor)?;
            }
            b"Filter" => {
                let mut value_cursor = value_start;
                facts.filters = parse_filter_names(bytes, &mut value_cursor)?;
            }
            b"DecodeParms" => {
                facts.decode_parameters = Some(bytes.get(value_start..cursor)?.to_vec());
            }
            b"Type" => {
                let mut value_cursor = value_start;
                facts.is_xref_stream =
                    parse_name(bytes, &mut value_cursor).as_deref() == Some(b"XRef");
            }
            _ => {}
        }
    }
}

fn parse_optional_nonnegative_integer(
    bytes: &[u8],
    start: usize,
    end: usize,
) -> Option<Option<u64>> {
    let mut cursor = start;
    if consume_keyword(bytes, &mut cursor, b"null") {
        return (cursor == end).then_some(None);
    }
    parse_nonnegative_integer_value(bytes, start, end).map(Some)
}

fn parse_optional_index(bytes: &[u8], start: usize, end: usize) -> Option<Option<Vec<(u64, u64)>>> {
    let mut cursor = start;
    if consume_keyword(bytes, &mut cursor, b"null") {
        return (cursor == end).then_some(None);
    }
    let index = parse_index(bytes, &mut cursor)?;
    (cursor == end).then_some(Some(index))
}

fn parse_encryption_value(bytes: &[u8], start: usize, end: usize) -> Option<bool> {
    let mut cursor = start;
    if consume_keyword(bytes, &mut cursor, b"null") {
        return (cursor == end).then_some(false);
    }
    if bytes.get(start..end)?.starts_with(b"<<") {
        return Some(true);
    }
    cursor = start;
    parse_indirect_reference(bytes, &mut cursor)?;
    (cursor == end).then_some(true)
}

fn parse_optional_indirect_reference(
    bytes: &[u8],
    start: usize,
    end: usize,
) -> Option<Option<IndirectReference>> {
    let mut cursor = start;
    if consume_keyword(bytes, &mut cursor, b"null") {
        return (cursor == end).then_some(None);
    }
    let reference = parse_indirect_reference(bytes, &mut cursor)?;
    (cursor == end).then_some(Some(reference))
}

fn skip_pdf_object(bytes: &[u8], cursor: &mut usize, depth: usize) -> Option<()> {
    if depth > 32 {
        return None;
    }
    skip_pdf_space_and_comments(bytes, cursor);
    if bytes.get(*cursor..)?.starts_with(b"<<") {
        *cursor += 2;
        loop {
            skip_pdf_space_and_comments(bytes, cursor);
            if bytes.get(*cursor..)?.starts_with(b">>") {
                *cursor += 2;
                return Some(());
            }
            parse_name(bytes, cursor)?;
            skip_pdf_object(bytes, cursor, depth + 1)?;
        }
    }
    match bytes.get(*cursor)? {
        b'[' => {
            *cursor += 1;
            loop {
                skip_pdf_space_and_comments(bytes, cursor);
                if bytes.get(*cursor) == Some(&b']') {
                    *cursor += 1;
                    return Some(());
                }
                skip_pdf_object(bytes, cursor, depth + 1)?;
            }
        }
        b'(' => skip_literal_string(bytes, cursor),
        b'<' => skip_hex_string(bytes, cursor),
        b'/' => parse_name(bytes, cursor).map(|_| ()),
        _ => skip_scalar_or_reference(bytes, cursor),
    }
}

fn skip_literal_string(bytes: &[u8], cursor: &mut usize) -> Option<()> {
    *cursor += 1;
    let mut depth = 1_u32;
    while depth > 0 {
        let byte = *bytes.get(*cursor)?;
        *cursor += 1;
        match byte {
            b'\\' => {
                bytes.get(*cursor)?;
                *cursor += 1;
            }
            b'(' => depth = depth.checked_add(1)?,
            b')' => depth -= 1,
            _ => {}
        }
    }
    Some(())
}

fn skip_hex_string(bytes: &[u8], cursor: &mut usize) -> Option<()> {
    *cursor += 1;
    while *bytes.get(*cursor)? != b'>' {
        let byte = *bytes.get(*cursor)?;
        if !byte.is_ascii_hexdigit() && !is_pdf_whitespace(byte) {
            return None;
        }
        *cursor += 1;
    }
    *cursor += 1;
    Some(())
}

fn skip_scalar_or_reference(bytes: &[u8], cursor: &mut usize) -> Option<()> {
    let start = *cursor;
    let mut reference_cursor = start;
    if parse_indirect_reference(bytes, &mut reference_cursor).is_some() {
        *cursor = reference_cursor;
        return Some(());
    }
    let end = token_end(bytes, start)?;
    if valid_pdf_keyword(bytes, start, end) || valid_pdf_number(bytes, start, end) {
        *cursor = end;
        Some(())
    } else {
        None
    }
}

fn valid_pdf_keyword(bytes: &[u8], start: usize, end: usize) -> bool {
    let length = end.saturating_sub(start);
    (length == 4
        && bytes.get(start) == Some(&b't')
        && bytes.get(start + 1) == Some(&b'r')
        && bytes.get(start + 2) == Some(&b'u')
        && bytes.get(start + 3) == Some(&b'e'))
        || (length == 5
            && bytes.get(start) == Some(&b'f')
            && bytes.get(start + 1) == Some(&b'a')
            && bytes.get(start + 2) == Some(&b'l')
            && bytes.get(start + 3) == Some(&b's')
            && bytes.get(start + 4) == Some(&b'e'))
        || (length == 4
            && bytes.get(start) == Some(&b'n')
            && bytes.get(start + 1) == Some(&b'u')
            && bytes.get(start + 2) == Some(&b'l')
            && bytes.get(start + 3) == Some(&b'l'))
}

fn valid_pdf_number(bytes: &[u8], start: usize, end: usize) -> bool {
    let mut cursor = start + usize::from(matches!(bytes.get(start), Some(b'+') | Some(b'-')));
    let mut digits = 0_usize;
    let mut decimal_points = 0_usize;
    while cursor < end {
        let Some(byte) = bytes.get(cursor) else {
            return false;
        };
        match byte {
            b'0'..=b'9' => digits += 1,
            b'.' if decimal_points == 0 => decimal_points += 1,
            _ => return false,
        }
        cursor += 1;
    }
    digits > 0 && cursor == end
}

fn parse_name(bytes: &[u8], cursor: &mut usize) -> Option<Vec<u8>> {
    if bytes.get(*cursor) != Some(&b'/') {
        return None;
    }
    *cursor += 1;
    let mut name = Vec::new();
    while bytes
        .get(*cursor)
        .is_some_and(|byte| !is_pdf_delimiter(*byte))
    {
        if bytes.get(*cursor) == Some(&b'#') {
            let high = hex_value(*bytes.get(*cursor + 1)?)?;
            let low = hex_value(*bytes.get(*cursor + 2)?)?;
            name.push((high << 4) | low);
            *cursor += 3;
        } else {
            name.push(*bytes.get(*cursor)?);
            *cursor += 1;
        }
    }
    Some(name)
}

fn parse_indirect_reference(bytes: &[u8], cursor: &mut usize) -> Option<IndirectReference> {
    let object_number = parse_nonnegative_integer(bytes, cursor)?;
    if object_number == 0 || object_number > MAX_OBJECT_NUMBER {
        return None;
    }
    skip_required_pdf_space_and_comments(bytes, cursor)?;
    let generation = parse_nonnegative_integer(bytes, cursor)?;
    if generation > MAX_GENERATION {
        return None;
    }
    skip_required_pdf_space_and_comments(bytes, cursor)?;
    if !consume_keyword(bytes, cursor, b"R") {
        return None;
    }
    Some(IndirectReference {
        object_number,
        generation,
    })
}

fn parse_widths(bytes: &[u8], cursor: &mut usize) -> Option<[u64; 3]> {
    if bytes.get(*cursor) != Some(&b'[') {
        return None;
    }
    *cursor += 1;
    skip_pdf_space_and_comments(bytes, cursor);
    let first = parse_nonnegative_integer(bytes, cursor)?;
    skip_pdf_space_and_comments(bytes, cursor);
    let second = parse_nonnegative_integer(bytes, cursor)?;
    skip_pdf_space_and_comments(bytes, cursor);
    let third = parse_nonnegative_integer(bytes, cursor)?;
    skip_pdf_space_and_comments(bytes, cursor);
    if bytes.get(*cursor) != Some(&b']') {
        return None;
    }
    *cursor += 1;
    Some([first, second, third])
}

fn parse_filter_names(bytes: &[u8], cursor: &mut usize) -> Option<Vec<Vec<u8>>> {
    if consume_keyword(bytes, cursor, b"null") {
        return Some(Vec::new());
    }
    if bytes.get(*cursor) == Some(&b'/') {
        return Some(vec![parse_name(bytes, cursor)?]);
    }
    if bytes.get(*cursor) != Some(&b'[') {
        return None;
    }
    *cursor += 1;
    let mut filters = Vec::new();
    loop {
        skip_pdf_space_and_comments(bytes, cursor);
        if bytes.get(*cursor) == Some(&b']') {
            *cursor += 1;
            return Some(filters);
        }
        filters.push(parse_name(bytes, cursor)?);
    }
}

fn xref_decoded_length(widths: [u64; 3], indexes: &[(u64, u64)]) -> Option<usize> {
    let entry_width = widths
        .iter()
        .try_fold(0_u64, |total, width| total.checked_add(*width))?;
    let entries = indexes
        .iter()
        .try_fold(0_u64, |total, (_, count)| total.checked_add(*count))?;
    usize::try_from(entry_width.checked_mul(entries)?).ok()
}

fn decode_xref_stream(
    encoded: &[u8],
    filters: &[Vec<u8>],
    decode_parameters: Option<&[u8]>,
    limit: usize,
) -> Option<Vec<u8>> {
    if filters.is_empty() {
        return (encoded.len() == limit).then(|| encoded.to_vec());
    }
    let filter = if filters.len() == 1 {
        Object::Name(filters[0].clone())
    } else {
        Object::Array(filters.iter().cloned().map(Object::Name).collect())
    };
    let mut dictionary = Dictionary::new();
    dictionary.set("Filter", filter);
    if let Some(parameter_bytes) = decode_parameters {
        let mut cursor = 0;
        let parameters = parse_lopdf_object(parameter_bytes, &mut cursor, 0)?;
        skip_pdf_space_and_comments(parameter_bytes, &mut cursor);
        if cursor != parameter_bytes.len() {
            return None;
        }
        dictionary.set("DecodeParms", parameters);
    }
    let stream = Stream::new(dictionary, encoded.to_vec());
    let decoded = stream.decompressed_content_with_limit(limit).ok()?;
    (decoded.len() == limit).then_some(decoded)
}

fn valid_xref_targets(parsed: &ParsedXref, source_length: u64) -> bool {
    if parsed.live_entries.iter().any(|entry| {
        matches!(
            entry.location,
            XrefLocation::Uncompressed(offset) if offset >= source_length
        )
    }) {
        return false;
    }
    if parsed.live_entries.iter().any(|entry| {
        let XrefLocation::Compressed { stream_object, .. } = entry.location else {
            return false;
        };
        !parsed.live_entries.iter().any(|stream| {
            stream.reference.object_number == stream_object
                && matches!(stream.location, XrefLocation::Uncompressed(_))
        })
    }) {
        return false;
    }
    let Some(root) = parsed.facts.root else {
        return false;
    };
    parsed
        .live_entries
        .iter()
        .any(|entry| entry.reference == root)
}

async fn bounded_object_stream_targets(
    source: &dyn VerifiedBlobSource,
    budget: &mut ValidationBudget,
    parsed: &ParsedXref,
) -> Result<Option<(Vec<CachedObjectStream>, BTreeMap<u64, Vec<u8>>)>, FileMediaProviderFailure> {
    let mut targets = BTreeMap::<u64, Vec<(IndirectReference, u64)>>::new();
    for entry in &parsed.live_entries {
        if let XrefLocation::Compressed {
            stream_object,
            index,
        } = entry.location
        {
            targets
                .entry(stream_object)
                .or_default()
                .push((entry.reference, index));
        }
    }
    let mut ranges = BTreeMap::<u64, Vec<u8>>::new();
    let mut streams = Vec::with_capacity(targets.len());
    let mut ordered_targets = Vec::with_capacity(targets.len());
    for (stream_object, compressed_entries) in targets {
        let Some((stream_reference, stream_offset)) = object_stream_offset(parsed, stream_object)
        else {
            return Ok(None);
        };
        ordered_targets.push((
            stream_offset,
            stream_object,
            stream_reference,
            compressed_entries,
        ));
    }
    ordered_targets.sort_by_key(|(stream_offset, ..)| *stream_offset);
    let target_count = ordered_targets.len();
    let mut remaining_decoded_bytes = MAX_TOTAL_OBJECT_STREAM_BYTES;
    for (index, (stream_offset, stream_object, stream_reference, compressed_entries)) in
        ordered_targets.into_iter().enumerate()
    {
        // Every target still to come after this one may need its own fresh
        // range read plus an out-of-range length probe (2 ranges, worst
        // case), so the reservation scales with the remaining uncached-
        // stream count instead of a flat constant; otherwise the first range
        // greedily starves later probes. This target's own probe needs only
        // 1 more range beyond that, matching the prior flat constants when
        // exactly one target remains.
        let future_targets = u32::try_from(target_count - index - 1)
            .map_err(|_| FileMediaProviderFailure::Failed)?;
        let probe_reservation = (
            u64::from(2 * future_targets) * ROOT_VALIDATION_BYTES,
            2 * future_targets,
        );
        if cached_range_at(&ranges, stream_offset).is_none() {
            let Some(available) = source.byte_length().get().checked_sub(stream_offset) else {
                return Ok(None);
            };
            let length = available.min(budget.available_after_reserving(
                probe_reservation.0 + ROOT_VALIDATION_BYTES,
                probe_reservation.1 + 1,
            ));
            if !budget.can_read(length) {
                return Ok(None);
            }
            let bytes = read_validation_range(source, budget, stream_offset, length).await?;
            ranges.insert(stream_offset, bytes);
        }
        let Some(bytes) = cached_range_at(&ranges, stream_offset) else {
            return Ok(None);
        };
        let resolved_length = resolve_object_stream_length(
            source,
            budget,
            parsed,
            bytes,
            stream_offset,
            stream_reference,
            probe_reservation,
        )
        .await?;
        if !object_stream_coordinates_are_valid(
            bytes,
            stream_reference,
            resolved_length,
            &compressed_entries,
            &mut remaining_decoded_bytes,
        ) {
            return Ok(None);
        }
        streams.push(CachedObjectStream {
            object_number: stream_object,
            reference: stream_reference,
            offset: stream_offset,
            resolved_length,
        });
    }
    Ok(Some((streams, ranges)))
}

fn cached_range_at(ranges: &BTreeMap<u64, Vec<u8>>, offset: u64) -> Option<&[u8]> {
    let (start, bytes) = ranges.range(..=offset).next_back()?;
    let relative = usize::try_from(offset.checked_sub(*start)?).ok()?;
    bytes.get(relative..)
}

fn object_stream_coordinates_are_valid(
    bytes: &[u8],
    stream_reference: IndirectReference,
    resolved_length: Option<(IndirectReference, u64)>,
    targets: &[(IndirectReference, u64)],
    remaining_decoded_bytes: &mut usize,
) -> bool {
    let Some((facts, encoded)) = parse_object_stream(bytes, stream_reference, resolved_length)
    else {
        return false;
    };
    let Some(count) = facts.count.and_then(|value| usize::try_from(value).ok()) else {
        return false;
    };
    let Some(first) = facts.first.and_then(|value| usize::try_from(value).ok()) else {
        return false;
    };
    if !facts.is_object_stream || count > MAX_OBJECTS {
        return false;
    }
    let Some(decoded) = decode_pdf_stream(
        encoded,
        &facts.filters,
        facts.decode_parameters.as_deref(),
        MAX_OBJECT_STREAM_BYTES.min(*remaining_decoded_bytes),
    ) else {
        return false;
    };
    *remaining_decoded_bytes -= decoded.len();
    let Some(header) = decoded.get(..first) else {
        return false;
    };
    let mut cursor = 0_usize;
    let mut object_numbers = Vec::with_capacity(count);
    for index in 0..count {
        if index > 0 && skip_required_pdf_space_and_comments(header, &mut cursor).is_none() {
            return false;
        }
        let Some(object_number) = parse_nonnegative_integer(header, &mut cursor) else {
            return false;
        };
        if skip_required_pdf_space_and_comments(header, &mut cursor).is_none()
            || parse_nonnegative_integer(header, &mut cursor).is_none()
        {
            return false;
        }
        object_numbers.push(object_number);
    }
    skip_pdf_space_and_comments(header, &mut cursor);
    cursor == header.len()
        && targets.iter().all(|(reference, index)| {
            usize::try_from(*index)
                .ok()
                .and_then(|index| object_numbers.get(index))
                == Some(&reference.object_number)
        })
}

fn parse_xref_chain(bytes: &[u8], start: usize) -> Option<ParsedXref> {
    let mut visited = BTreeSet::new();
    let mut offset = start;
    let mut parsed = parse_xref_structure(bytes.get(offset..)?, u64::try_from(offset).ok()?)?;
    visited.insert(offset);
    merge_complete_supplemental_xref(bytes, &mut parsed, offset, &mut visited)?;
    while let Some(previous_offset) = parsed.facts.prev {
        let previous_offset = usize::try_from(previous_offset).ok()?;
        if previous_offset >= offset {
            return None;
        }
        offset = previous_offset;
        if !visited.insert(offset) {
            return None;
        }
        let mut previous = parse_xref_structure(bytes.get(offset..)?, u64::try_from(offset).ok()?)?;
        merge_complete_supplemental_xref(bytes, &mut previous, offset, &mut visited)?;
        merge_previous_xref(&mut parsed, previous);
    }
    let mut resolved_lengths = BTreeMap::new();
    let lengths_match = parsed
        .facts
        .indirect_xref_lengths
        .iter()
        .all(|(reference, expected)| {
            resolve_integer_object(
                bytes,
                &parsed,
                *reference,
                &mut BTreeSet::new(),
                &mut resolved_lengths,
            ) == Some(*expected)
        });
    (lengths_match && effective_size_contains_declarations(&parsed)).then_some(parsed)
}

fn effective_size_contains_declarations(parsed: &ParsedXref) -> bool {
    parsed.facts.size.is_some_and(|size| {
        parsed
            .declared_objects
            .iter()
            .all(|object_number| *object_number < size)
    })
}

fn merge_complete_supplemental_xref(
    bytes: &[u8],
    parsed: &mut ParsedXref,
    mut section_offset: usize,
    visited: &mut BTreeSet<usize>,
) -> Option<()> {
    while let Some(offset) = parsed.facts.xref_stream.take() {
        let offset = usize::try_from(offset).ok()?;
        if offset >= section_offset || !visited.insert(offset) {
            return None;
        }
        let supplemental = parse_xref_structure(bytes.get(offset..)?, u64::try_from(offset).ok()?)?;
        merge_supplemental_xref(parsed, supplemental);
        section_offset = offset;
    }
    Some(())
}

fn inferred_xref_stream_length(bytes: &[u8], stream_start: usize) -> Option<usize> {
    let tail = bytes.get(stream_start..)?;
    tail.windows(b"endstream".len())
        .enumerate()
        .find_map(|(relative, window)| {
            if window != b"endstream" {
                return None;
            }
            let mut keyword_cursor = stream_start.checked_add(relative)?;
            if !consume_keyword(bytes, &mut keyword_cursor, b"endstream") {
                return None;
            }
            skip_pdf_space_and_comments(bytes, &mut keyword_cursor);
            if !consume_keyword(bytes, &mut keyword_cursor, b"endobj") {
                return None;
            }
            let mut stream_end = stream_start.checked_add(relative)?;
            if stream_end > stream_start && bytes.get(stream_end - 1) == Some(&b'\n') {
                stream_end -= 1;
                if stream_end > stream_start && bytes.get(stream_end - 1) == Some(&b'\r') {
                    stream_end -= 1;
                }
            } else if stream_end > stream_start && bytes.get(stream_end - 1) == Some(&b'\r') {
                stream_end -= 1;
            }
            stream_end.checked_sub(stream_start)
        })
}

async fn merge_bounded_supplemental_xref(
    source: &dyn VerifiedBlobSource,
    budget: &mut ValidationBudget,
    parsed: &mut ParsedXref,
    mut section_offset: u64,
    visited: &mut BTreeSet<u64>,
) -> Result<bool, FileMediaProviderFailure> {
    while let Some(offset) = parsed.facts.xref_stream.take() {
        if offset >= section_offset || !visited.insert(offset) {
            return Ok(false);
        }
        let length = (section_offset - offset)
            .min(budget.available_after_reserving(2 * ROOT_VALIDATION_BYTES, 2));
        if !budget.can_read(length) {
            return Ok(false);
        }
        let bytes = read_validation_range(source, budget, offset, length).await?;
        let Some(supplemental) = parse_xref_structure(&bytes, offset) else {
            return Ok(false);
        };
        merge_supplemental_xref(parsed, supplemental);
        section_offset = offset;
    }
    Ok(true)
}

fn merge_supplemental_xref(current: &mut ParsedXref, mut supplemental: ParsedXref) {
    current.facts.encrypted |= supplemental.facts.encrypted;
    if current.facts.root.is_none() {
        current.facts.root = supplemental.facts.root;
    }
    current.facts.xref_stream = supplemental.facts.xref_stream;
    current
        .facts
        .indirect_xref_lengths
        .append(&mut supplemental.facts.indirect_xref_lengths);
    current.facts.decoded_xref_bytes = current
        .facts
        .decoded_xref_bytes
        .saturating_add(supplemental.facts.decoded_xref_bytes);
    current.facts.xref_stream_limit_exceeded |= supplemental.facts.xref_stream_limit_exceeded
        || current.facts.decoded_xref_bytes > MAX_TOTAL_XREF_STREAM_BYTES;
    current.live_entries.retain(|entry| {
        !supplemental
            .declared_objects
            .contains(&entry.reference.object_number)
    });
    current.live_entries.extend(supplemental.live_entries);
    current
        .declared_objects
        .extend(supplemental.declared_objects);
    current.object_limit_exceeded = current.live_entries.len() > MAX_OBJECTS;
}

fn merge_previous_xref(current: &mut ParsedXref, mut previous: ParsedXref) {
    current.facts.encrypted |= previous.facts.encrypted;
    if current.facts.root.is_none() {
        current.facts.root = previous.facts.root;
    }
    current.facts.prev = previous.facts.prev;
    current
        .facts
        .indirect_xref_lengths
        .append(&mut previous.facts.indirect_xref_lengths);
    current.facts.decoded_xref_bytes = current
        .facts
        .decoded_xref_bytes
        .saturating_add(previous.facts.decoded_xref_bytes);
    current.facts.xref_stream_limit_exceeded |= previous.facts.xref_stream_limit_exceeded
        || current.facts.decoded_xref_bytes > MAX_TOTAL_XREF_STREAM_BYTES;
    for entry in previous.live_entries {
        if !current
            .declared_objects
            .contains(&entry.reference.object_number)
        {
            current.live_entries.push(entry);
        }
    }
    current.declared_objects.extend(previous.declared_objects);
    current.object_limit_exceeded = current.live_entries.len() > MAX_OBJECTS;
}

fn root_location(parsed: &ParsedXref) -> Option<(IndirectReference, XrefLocation)> {
    let root = parsed.facts.root?;
    parsed
        .live_entries
        .iter()
        .find(|entry| entry.reference == root)
        .map(|entry| (root, entry.location))
}

#[cfg(test)]
fn root_offset(parsed: &ParsedXref) -> Option<(IndirectReference, u64)> {
    let (root, location) = root_location(parsed)?;
    match location {
        XrefLocation::Uncompressed(offset) => Some((root, offset)),
        XrefLocation::Compressed { .. } => None,
    }
}

fn object_stream_offset(
    parsed: &ParsedXref,
    object_number: u64,
) -> Option<(IndirectReference, u64)> {
    parsed.live_entries.iter().find_map(|entry| {
        if entry.reference.object_number != object_number {
            return None;
        }
        match entry.location {
            XrefLocation::Uncompressed(offset) => Some((entry.reference, offset)),
            XrefLocation::Compressed { .. } => None,
        }
    })
}

fn object_stream_object(
    bytes: &[u8],
    stream_reference: IndirectReference,
    expected_reference: IndirectReference,
    expected_index: u64,
    decoded_limit: usize,
    resolved_length: Option<(IndirectReference, u64)>,
) -> Option<Vec<u8>> {
    let (facts, encoded) = parse_object_stream(bytes, stream_reference, resolved_length)?;
    let count = facts.count.and_then(|value| usize::try_from(value).ok())?;
    let first = facts.first.and_then(|value| usize::try_from(value).ok())?;
    let index = usize::try_from(expected_index).ok()?;
    if !facts.is_object_stream || count > MAX_OBJECTS || index >= count {
        return None;
    }
    let decoded = decode_pdf_stream(
        encoded,
        &facts.filters,
        facts.decode_parameters.as_deref(),
        decoded_limit,
    )?;
    let header = decoded.get(..first)?;
    let mut cursor = 0_usize;
    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        if index > 0 {
            skip_required_pdf_space_and_comments(header, &mut cursor)?;
        }
        let object_number = parse_nonnegative_integer(header, &mut cursor)?;
        skip_required_pdf_space_and_comments(header, &mut cursor)?;
        let offset = parse_nonnegative_integer(header, &mut cursor)?;
        entries.push((object_number, offset));
    }
    skip_pdf_space_and_comments(header, &mut cursor);
    if cursor != header.len() || entries[index].0 != expected_reference.object_number {
        return None;
    }
    let start = usize::try_from(entries[index].1)
        .ok()
        .and_then(|offset| first.checked_add(offset))?;
    let end = if index + 1 < entries.len() {
        usize::try_from(entries[index + 1].1)
            .ok()
            .and_then(|offset| first.checked_add(offset))?
    } else {
        decoded.len()
    };
    Some(decoded.get(start..end)?.to_vec())
}

#[cfg(test)]
fn pages_dictionary_is_valid(bytes: &[u8]) -> bool {
    pages_dictionary_is_valid_for_root(bytes, None, None)
}

fn pages_dictionary_is_valid_for_root(
    bytes: &[u8],
    expected_root: Option<IndirectReference>,
    live_entries: Option<&[LiveXrefEntry]>,
) -> bool {
    let mut cursor = 0;
    skip_pdf_space_and_comments(bytes, &mut cursor);
    if !bytes
        .get(cursor..)
        .is_some_and(|tail| tail.starts_with(b"<<"))
    {
        return false;
    }
    cursor += 2;
    let mut pages = false;
    let mut kids = None;
    let mut count = None;
    let mut parent_absent = true;
    loop {
        skip_pdf_space_and_comments(bytes, &mut cursor);
        if bytes
            .get(cursor..)
            .is_some_and(|tail| tail.starts_with(b">>"))
        {
            cursor += 2;
            skip_pdf_space_and_comments(bytes, &mut cursor);
            return pages
                && kids.is_some()
                && count.is_some_and(|count| {
                    count <= MAX_PAGES as u64 && (count == 0) == (kids == Some(0))
                })
                && parent_absent
                && cursor == bytes.len();
        }
        let Some(key) = parse_name(bytes, &mut cursor) else {
            return false;
        };
        skip_pdf_space_and_comments(bytes, &mut cursor);
        let value_start = cursor;
        if skip_pdf_object(bytes, &mut cursor, 0).is_none() {
            return false;
        }
        let mut value_cursor = value_start;
        match key.as_slice() {
            b"Type" => {
                pages = parse_name(bytes, &mut value_cursor).as_deref() == Some(b"Pages");
            }
            b"Kids" => {
                kids = kids_array_reference_count(
                    bytes,
                    value_start,
                    cursor,
                    expected_root,
                    live_entries,
                )
            }
            b"Count" => {
                count = parse_nonnegative_integer(bytes, &mut value_cursor)
                    .filter(|_| value_cursor == cursor);
            }
            b"Parent" => {
                parent_absent =
                    consume_keyword(bytes, &mut value_cursor, b"null") && value_cursor == cursor;
            }
            _ => {}
        }
    }
}

fn pages_dictionary_is_valid_for_xref(
    bytes: &[u8],
    expected_root: IndirectReference,
    live_entries: &[LiveXrefEntry],
) -> bool {
    pages_dictionary_is_valid_for_root(bytes, Some(expected_root), Some(live_entries))
}

fn kids_array_reference_count(
    bytes: &[u8],
    start: usize,
    end: usize,
    expected_root: Option<IndirectReference>,
    live_entries: Option<&[LiveXrefEntry]>,
) -> Option<usize> {
    let mut cursor = start;
    if bytes.get(cursor) != Some(&b'[') {
        return None;
    }
    cursor += 1;
    let mut references = 0_usize;
    let mut unique = BTreeSet::new();
    loop {
        skip_pdf_space_and_comments(bytes, &mut cursor);
        if bytes.get(cursor) == Some(&b']') {
            cursor += 1;
            return (cursor == end).then_some(references);
        }
        let reference = parse_indirect_reference(bytes, &mut cursor)?;
        if Some(reference) == expected_root
            || !unique.insert(reference)
            || live_entries
                .is_some_and(|entries| !entries.iter().any(|entry| entry.reference == reference))
        {
            return None;
        }
        references += 1;
        if references > MAX_PAGES {
            return None;
        }
    }
}

fn parse_object_stream(
    bytes: &[u8],
    expected: IndirectReference,
    resolved_length: Option<(IndirectReference, u64)>,
) -> Option<(ObjectStreamFacts, &[u8])> {
    let mut cursor = 0_usize;
    if parse_unsigned(bytes, &mut cursor) != Some(expected.object_number) {
        return None;
    }
    skip_required_pdf_space_and_comments(bytes, &mut cursor)?;
    if parse_unsigned(bytes, &mut cursor) != Some(expected.generation) {
        return None;
    }
    skip_required_pdf_space_and_comments(bytes, &mut cursor)?;
    if !consume_keyword(bytes, &mut cursor, b"obj") {
        return None;
    }
    skip_pdf_space_and_comments(bytes, &mut cursor);
    if !bytes.get(cursor..)?.starts_with(b"<<") {
        return None;
    }
    cursor += 2;
    let mut facts = ObjectStreamFacts::default();
    loop {
        skip_pdf_space_and_comments(bytes, &mut cursor);
        if bytes.get(cursor..)?.starts_with(b">>") {
            cursor += 2;
            break;
        }
        let key = parse_name(bytes, &mut cursor)?;
        skip_pdf_space_and_comments(bytes, &mut cursor);
        let value_start = cursor;
        skip_pdf_object(bytes, &mut cursor, 0)?;
        let mut value_cursor = value_start;
        match key.as_slice() {
            b"Type" => {
                facts.is_object_stream =
                    parse_name(bytes, &mut value_cursor).as_deref() == Some(b"ObjStm");
            }
            b"N" => {
                facts.count = Some(parse_nonnegative_integer_value(bytes, value_start, cursor)?);
            }
            b"First" => {
                facts.first = Some(parse_nonnegative_integer_value(bytes, value_start, cursor)?);
            }
            b"Length" => {
                facts.length = Some(parse_stream_length(bytes, value_start, cursor)?);
            }
            b"Filter" => facts.filters = parse_filter_names(bytes, &mut value_cursor)?,
            b"DecodeParms" => {
                facts.decode_parameters = Some(bytes.get(value_start..cursor)?.to_vec());
            }
            _ => {}
        }
    }
    skip_pdf_space_and_comments(bytes, &mut cursor);
    if !consume_keyword(bytes, &mut cursor, b"stream") {
        return None;
    }
    consume_stream_line_end(bytes, &mut cursor)?;
    let length = match facts.length? {
        StreamLength::Direct(length) => length,
        StreamLength::Indirect(reference) => {
            let (resolved_reference, length) = resolved_length?;
            (reference == resolved_reference).then_some(length)?
        }
    };
    let length = usize::try_from(length).ok()?;
    let end = cursor.checked_add(length)?;
    let encoded = bytes.get(cursor..end)?;
    cursor = end;
    skip_pdf_whitespace(bytes, &mut cursor);
    if !consume_keyword(bytes, &mut cursor, b"endstream") {
        return None;
    }
    skip_pdf_space_and_comments(bytes, &mut cursor);
    if !consume_keyword(bytes, &mut cursor, b"endobj") {
        return None;
    }
    Some((facts, encoded))
}

fn parse_stream_length(bytes: &[u8], start: usize, end: usize) -> Option<StreamLength> {
    let mut cursor = start;
    if let Some(reference) = parse_indirect_reference(bytes, &mut cursor)
        && cursor == end
    {
        return Some(StreamLength::Indirect(reference));
    }
    cursor = start;
    let length = parse_nonnegative_integer(bytes, &mut cursor)?;
    (cursor == end).then_some(StreamLength::Direct(length))
}

fn indirect_integer_object(bytes: &[u8], expected: IndirectReference) -> Option<u64> {
    let mut cursor = 0;
    if parse_unsigned(bytes, &mut cursor) != Some(expected.object_number) {
        return None;
    }
    skip_required_pdf_space_and_comments(bytes, &mut cursor)?;
    if parse_unsigned(bytes, &mut cursor) != Some(expected.generation) {
        return None;
    }
    skip_required_pdf_space_and_comments(bytes, &mut cursor)?;
    if !consume_keyword(bytes, &mut cursor, b"obj") {
        return None;
    }
    skip_pdf_space_and_comments(bytes, &mut cursor);
    let value = parse_nonnegative_integer(bytes, &mut cursor)?;
    skip_required_pdf_space_and_comments(bytes, &mut cursor)?;
    consume_keyword(bytes, &mut cursor, b"endobj").then_some(value)
}

fn object_stream_declared_length(
    bytes: &[u8],
    expected: IndirectReference,
) -> Option<StreamLength> {
    let mut cursor = 0;
    if parse_unsigned(bytes, &mut cursor) != Some(expected.object_number) {
        return None;
    }
    skip_required_pdf_space_and_comments(bytes, &mut cursor)?;
    if parse_unsigned(bytes, &mut cursor) != Some(expected.generation) {
        return None;
    }
    skip_required_pdf_space_and_comments(bytes, &mut cursor)?;
    if !consume_keyword(bytes, &mut cursor, b"obj") {
        return None;
    }
    skip_pdf_space_and_comments(bytes, &mut cursor);
    if !bytes.get(cursor..)?.starts_with(b"<<") {
        return None;
    }
    cursor += 2;
    loop {
        skip_pdf_space_and_comments(bytes, &mut cursor);
        if bytes.get(cursor..)?.starts_with(b">>") {
            return None;
        }
        let key = parse_name(bytes, &mut cursor)?;
        skip_pdf_space_and_comments(bytes, &mut cursor);
        let value_start = cursor;
        skip_pdf_object(bytes, &mut cursor, 0)?;
        if key == b"Length" {
            return parse_stream_length(bytes, value_start, cursor);
        }
    }
}

async fn resolve_object_stream_length(
    source: &dyn VerifiedBlobSource,
    budget: &mut ValidationBudget,
    parsed: &ParsedXref,
    stream_bytes: &[u8],
    stream_offset: u64,
    stream_reference: IndirectReference,
    reservation: (u64, u32),
) -> Result<Option<(IndirectReference, u64)>, FileMediaProviderFailure> {
    let Some(StreamLength::Indirect(reference)) =
        object_stream_declared_length(stream_bytes, stream_reference)
    else {
        return Ok(None);
    };
    if let Some(length) =
        cached_uncompressed_integer(parsed, stream_bytes, stream_offset, reference)
    {
        return Ok(Some((reference, length)));
    }
    let length = resolve_bounded_integer_object(
        source,
        budget,
        parsed,
        reference,
        reservation,
        &mut BTreeSet::new(),
    )
    .await?;
    Ok(length.map(|length| (reference, length)))
}

/// Resolves an indirect integer object from the already-cached range that
/// starts at `stream_offset`, so a length probe does not require budget the
/// preceding range read has already consumed.
fn cached_uncompressed_integer(
    parsed: &ParsedXref,
    stream_bytes: &[u8],
    stream_offset: u64,
    reference: IndirectReference,
) -> Option<u64> {
    let entry = parsed
        .live_entries
        .iter()
        .find(|entry| entry.reference == reference)?;
    let XrefLocation::Uncompressed(offset) = entry.location else {
        return None;
    };
    let relative = usize::try_from(offset.checked_sub(stream_offset)?).ok()?;
    indirect_integer_object(stream_bytes.get(relative..)?, reference)
}

fn resolve_bounded_integer_object<'a>(
    source: &'a dyn VerifiedBlobSource,
    budget: &'a mut ValidationBudget,
    parsed: &'a ParsedXref,
    reference: IndirectReference,
    reservation: (u64, u32),
    resolving: &'a mut BTreeSet<IndirectReference>,
) -> FileMediaProviderFuture<'a, Option<u64>> {
    Box::pin(async move {
        if resolving.len() >= MAX_INDIRECT_LENGTH_DEPTH || !resolving.insert(reference) {
            return Ok(None);
        }
        let result: Result<Option<u64>, FileMediaProviderFailure> = async {
            let Some(entry) = parsed
                .live_entries
                .iter()
                .find(|entry| entry.reference == reference)
            else {
                return Ok(None);
            };
            match entry.location {
                XrefLocation::Uncompressed(offset) => {
                    let Some(available) = source.byte_length().get().checked_sub(offset) else {
                        return Ok(None);
                    };
                    let length = budget
                        .available_after_reserving(reservation.0, reservation.1)
                        .min(available);
                    if !budget.can_read(length) {
                        return Ok(None);
                    }
                    let bytes = read_validation_range(source, budget, offset, length).await?;
                    Ok(indirect_integer_object(&bytes, reference))
                }
                XrefLocation::Compressed {
                    stream_object,
                    index,
                } => {
                    let Some((stream_reference, stream_offset)) =
                        object_stream_offset(parsed, stream_object)
                    else {
                        return Ok(None);
                    };
                    let Some(available) = source.byte_length().get().checked_sub(stream_offset)
                    else {
                        return Ok(None);
                    };
                    let length = budget
                        .available_after_reserving(reservation.0, reservation.1)
                        .min(available);
                    if !budget.can_read(length) {
                        return Ok(None);
                    }
                    let bytes =
                        read_validation_range(source, budget, stream_offset, length).await?;
                    let Some(declared_length) =
                        object_stream_declared_length(&bytes, stream_reference)
                    else {
                        return Ok(None);
                    };
                    let resolved_length = match declared_length {
                        StreamLength::Direct(_) => None,
                        StreamLength::Indirect(length_reference) => {
                            let Some(length) = resolve_bounded_integer_object(
                                source,
                                budget,
                                parsed,
                                length_reference,
                                reservation,
                                resolving,
                            )
                            .await?
                            else {
                                return Ok(None);
                            };
                            Some((length_reference, length))
                        }
                    };
                    Ok(object_stream_object(
                        &bytes,
                        stream_reference,
                        reference,
                        index,
                        MAX_OBJECT_STREAM_BYTES,
                        resolved_length,
                    )
                    .and_then(|object| parse_integer_object_value(&object)))
                }
            }
        }
        .await;
        resolving.remove(&reference);
        result
    })
}

fn decode_pdf_stream(
    encoded: &[u8],
    filters: &[Vec<u8>],
    decode_parameters: Option<&[u8]>,
    limit: usize,
) -> Option<Vec<u8>> {
    if filters.is_empty() {
        return (encoded.len() <= limit).then(|| encoded.to_vec());
    }
    let filter = if filters.len() == 1 {
        Object::Name(filters[0].clone())
    } else {
        Object::Array(filters.iter().cloned().map(Object::Name).collect())
    };
    let mut dictionary = Dictionary::new();
    dictionary.set("Filter", filter);
    if let Some(parameter_bytes) = decode_parameters {
        let mut cursor = 0;
        let parameters = parse_lopdf_object(parameter_bytes, &mut cursor, 0)?;
        skip_pdf_space_and_comments(parameter_bytes, &mut cursor);
        if cursor != parameter_bytes.len() {
            return None;
        }
        dictionary.set("DecodeParms", parameters);
    }
    Stream::new(dictionary, encoded.to_vec())
        .decompressed_content_with_limit(limit)
        .ok()
}

fn parse_lopdf_object(bytes: &[u8], cursor: &mut usize, depth: usize) -> Option<Object> {
    if depth > 16 {
        return None;
    }
    skip_pdf_space_and_comments(bytes, cursor);
    if bytes.get(*cursor..)?.starts_with(b"<<") {
        *cursor += 2;
        let mut dictionary = Dictionary::new();
        loop {
            skip_pdf_space_and_comments(bytes, cursor);
            if bytes.get(*cursor..)?.starts_with(b">>") {
                *cursor += 2;
                return Some(Object::Dictionary(dictionary));
            }
            let key = parse_name(bytes, cursor)?;
            let value = parse_lopdf_object(bytes, cursor, depth + 1)?;
            dictionary.set(key, value);
        }
    }
    if bytes.get(*cursor) == Some(&b'[') {
        *cursor += 1;
        let mut values = Vec::new();
        loop {
            skip_pdf_space_and_comments(bytes, cursor);
            if bytes.get(*cursor) == Some(&b']') {
                *cursor += 1;
                return Some(Object::Array(values));
            }
            values.push(parse_lopdf_object(bytes, cursor, depth + 1)?);
        }
    }
    if bytes.get(*cursor) == Some(&b'/') {
        return Some(Object::Name(parse_name(bytes, cursor)?));
    }
    if bytes.get(*cursor..)?.starts_with(b"null") {
        *cursor += 4;
        return Some(Object::Null);
    }
    let value = parse_nonnegative_integer(bytes, cursor)?;
    Some(Object::Integer(i64::try_from(value).ok()?))
}

fn catalog_facts(bytes: &[u8], expected: IndirectReference) -> Option<CatalogFacts> {
    let mut cursor = 0;
    if parse_unsigned(bytes, &mut cursor) != Some(expected.object_number) {
        return None;
    }
    skip_required_pdf_space_and_comments(bytes, &mut cursor)?;
    if parse_unsigned(bytes, &mut cursor) != Some(expected.generation) {
        return None;
    }
    skip_required_pdf_space_and_comments(bytes, &mut cursor)?;
    if !consume_keyword(bytes, &mut cursor, b"obj") {
        return None;
    }
    skip_pdf_space_and_comments(bytes, &mut cursor);
    let (mut catalog, mut cursor) = parse_catalog_dictionary(bytes, cursor)?;
    skip_pdf_space_and_comments(bytes, &mut cursor);
    if !consume_keyword(bytes, &mut cursor, b"endobj") {
        return None;
    }
    catalog.object_end = Some(cursor);
    Some(catalog)
}

fn parse_catalog_dictionary(bytes: &[u8], mut cursor: usize) -> Option<(CatalogFacts, usize)> {
    if !bytes.get(cursor..)?.starts_with(b"<<") {
        return None;
    }
    cursor += 2;
    let mut catalog = false;
    let mut pages = None;
    let mut version = None;
    loop {
        skip_pdf_space_and_comments(bytes, &mut cursor);
        if bytes.get(cursor..)?.starts_with(b">>") {
            return catalog.then_some((
                CatalogFacts {
                    pages: pages?,
                    version,
                    object_end: None,
                },
                cursor + 2,
            ));
        }
        let key = parse_name(bytes, &mut cursor)?;
        skip_pdf_space_and_comments(bytes, &mut cursor);
        let value_start = cursor;
        skip_pdf_object(bytes, &mut cursor, 0)?;
        if key == b"Type" {
            let mut value_cursor = value_start;
            catalog = parse_name(bytes, &mut value_cursor).as_deref() == Some(b"Catalog");
        } else if key == b"Pages" {
            let mut value_cursor = value_start;
            pages = Some(parse_indirect_reference(bytes, &mut value_cursor)?);
        } else if key == b"Version" {
            let mut value_cursor = value_start;
            if consume_keyword(bytes, &mut value_cursor, b"null") {
                if value_cursor != cursor {
                    return None;
                }
                version = None;
            } else {
                let value = parse_name(bytes, &mut value_cursor)?;
                if value_cursor != cursor {
                    return None;
                }
                version = std::str::from_utf8(&value).ok().map(sanitized_version);
            }
        }
    }
}

#[cfg(test)]
fn object_is_pages(bytes: &[u8], expected: IndirectReference) -> bool {
    object_is_pages_for_xref(bytes, expected, None)
}

fn object_is_pages_for_xref(
    bytes: &[u8],
    expected: IndirectReference,
    live_entries: Option<&[LiveXrefEntry]>,
) -> bool {
    let mut cursor = 0;
    if parse_unsigned(bytes, &mut cursor) != Some(expected.object_number) {
        return false;
    }
    if skip_required_pdf_space_and_comments(bytes, &mut cursor).is_none() {
        return false;
    }
    if parse_unsigned(bytes, &mut cursor) != Some(expected.generation) {
        return false;
    }
    if skip_required_pdf_space_and_comments(bytes, &mut cursor).is_none() {
        return false;
    }
    if !consume_keyword(bytes, &mut cursor, b"obj") {
        return false;
    }
    skip_pdf_space_and_comments(bytes, &mut cursor);
    if !bytes
        .get(cursor..)
        .is_some_and(|bytes| bytes.starts_with(b"<<"))
    {
        return false;
    }
    cursor += 2;
    let mut pages = false;
    let mut kids = None;
    let mut count = None;
    let mut parent_absent = true;
    loop {
        skip_pdf_space_and_comments(bytes, &mut cursor);
        if bytes
            .get(cursor..)
            .is_some_and(|bytes| bytes.starts_with(b">>"))
        {
            cursor += 2;
            break;
        }
        let Some(key) = parse_name(bytes, &mut cursor) else {
            return false;
        };
        skip_pdf_space_and_comments(bytes, &mut cursor);
        let value_start = cursor;
        if skip_pdf_object(bytes, &mut cursor, 0).is_none() {
            return false;
        }
        if key == b"Type" {
            let mut value_cursor = value_start;
            pages = parse_name(bytes, &mut value_cursor).as_deref() == Some(b"Pages");
        } else if key == b"Kids" {
            kids = kids_array_reference_count(
                bytes,
                value_start,
                cursor,
                Some(expected),
                live_entries,
            );
        } else if key == b"Count" {
            let mut value_cursor = value_start;
            count = parse_nonnegative_integer(bytes, &mut value_cursor)
                .filter(|_| value_cursor == cursor);
        } else if key == b"Parent" {
            let mut value_cursor = value_start;
            parent_absent =
                consume_keyword(bytes, &mut value_cursor, b"null") && value_cursor == cursor;
        }
    }
    skip_pdf_space_and_comments(bytes, &mut cursor);
    pages
        && kids.is_some()
        && count.is_some_and(|count| count <= MAX_PAGES as u64 && (count == 0) == (kids == Some(0)))
        && parent_absent
        && consume_keyword(bytes, &mut cursor, b"endobj")
}

fn consume_stream_line_end(bytes: &[u8], cursor: &mut usize) -> Option<()> {
    match bytes.get(*cursor)? {
        b'\r' => {
            *cursor += 1;
            if bytes.get(*cursor) == Some(&b'\n') {
                *cursor += 1;
            }
        }
        b'\n' => *cursor += 1,
        _ => return None,
    }
    Some(())
}

fn parse_index(bytes: &[u8], cursor: &mut usize) -> Option<Vec<(u64, u64)>> {
    if bytes.get(*cursor) != Some(&b'[') {
        return None;
    }
    *cursor += 1;
    let mut indexes = Vec::new();
    loop {
        skip_pdf_space_and_comments(bytes, cursor);
        if bytes.get(*cursor) == Some(&b']') {
            *cursor += 1;
            return (!indexes.is_empty()).then_some(indexes);
        }
        let first = parse_nonnegative_integer(bytes, cursor)?;
        skip_pdf_space_and_comments(bytes, cursor);
        let count = parse_nonnegative_integer(bytes, cursor)?;
        if indexes
            .last()
            .is_some_and(|(previous_first, previous_count)| {
                previous_first
                    .checked_add(*previous_count)
                    .is_none_or(|previous_end| first < previous_end)
            })
        {
            return None;
        }
        first.checked_add(count)?;
        indexes.push((first, count));
    }
}

fn parse_xref_stream_entries(
    bytes: &[u8],
    widths: [u64; 3],
    indexes: &[(u64, u64)],
) -> Option<(Vec<LiveXrefEntry>, BTreeSet<u64>, bool)> {
    let entry_width = widths.iter().try_fold(0_usize, |total, width| {
        total.checked_add(usize::try_from(*width).ok()?)
    })?;
    if entry_width == 0 {
        return None;
    }
    let mut cursor = 0_usize;
    let mut live_entries = Vec::new();
    let mut declared_objects = BTreeSet::new();
    let mut object_limit_exceeded = false;
    for (first, count) in indexes {
        for index in 0..*count {
            let object_number = first.checked_add(index)?;
            if object_number > MAX_OBJECT_NUMBER {
                return None;
            }
            declared_objects.insert(object_number);
            let end = cursor.checked_add(entry_width)?;
            let entry = bytes.get(cursor..end)?;
            cursor = end;
            let type_end = usize::try_from(widths[0]).ok()?;
            let field_two_end = type_end.checked_add(usize::try_from(widths[1]).ok()?)?;
            let entry_type = if widths[0] == 0 {
                1
            } else {
                parse_big_endian(entry.get(..type_end)?)?
            };
            let generation = parse_big_endian(entry.get(field_two_end..)?)?;
            if entry_type == 0 && generation > MAX_GENERATION {
                return None;
            }
            if entry_type == 1 || entry_type == 2 {
                if object_number == 0 {
                    return None;
                }
                let field_two = parse_big_endian(entry.get(type_end..field_two_end)?)?;
                if entry_type == 1 && generation > MAX_GENERATION {
                    return None;
                }
                let location = if entry_type == 1 {
                    XrefLocation::Uncompressed(field_two)
                } else {
                    XrefLocation::Compressed {
                        stream_object: field_two,
                        index: generation,
                    }
                };
                live_entries.push(LiveXrefEntry {
                    reference: IndirectReference {
                        object_number,
                        generation: if entry_type == 1 { generation } else { 0 },
                    },
                    location,
                });
                object_limit_exceeded = live_entries.len() > MAX_OBJECTS;
            }
        }
    }
    if cursor != bytes.len() {
        return None;
    }
    Some((live_entries, declared_objects, object_limit_exceeded))
}

fn parse_big_endian(bytes: &[u8]) -> Option<u64> {
    bytes.iter().try_fold(0_u64, |value, byte| {
        value.checked_mul(256)?.checked_add(u64::from(*byte))
    })
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_unsigned(bytes: &[u8], cursor: &mut usize) -> Option<u64> {
    let start = *cursor;
    while bytes.get(*cursor).is_some_and(u8::is_ascii_digit) {
        *cursor += 1;
    }
    if start == *cursor {
        return None;
    }
    std::str::from_utf8(bytes.get(start..*cursor)?)
        .ok()?
        .parse()
        .ok()
}

fn parse_nonnegative_integer(bytes: &[u8], cursor: &mut usize) -> Option<u64> {
    if bytes.get(*cursor) == Some(&b'+') {
        *cursor += 1;
    }
    parse_unsigned(bytes, cursor)
}

fn parse_nonnegative_integer_value(bytes: &[u8], start: usize, end: usize) -> Option<u64> {
    let mut cursor = start;
    let value = parse_nonnegative_integer(bytes, &mut cursor)?;
    (cursor == end).then_some(value)
}

fn token_end(bytes: &[u8], mut cursor: usize) -> Option<usize> {
    let start = cursor;
    while bytes
        .get(cursor)
        .is_some_and(|byte| !is_pdf_delimiter(*byte))
    {
        cursor += 1;
    }
    (cursor > start).then_some(cursor)
}

fn consume_keyword(bytes: &[u8], cursor: &mut usize, keyword: &[u8]) -> bool {
    let Some(end) = cursor.checked_add(keyword.len()) else {
        return false;
    };
    if bytes.get(*cursor..end) == Some(keyword)
        && bytes.get(end).is_none_or(|byte| is_pdf_delimiter(*byte))
    {
        *cursor = end;
        true
    } else {
        false
    }
}

fn skip_pdf_whitespace(bytes: &[u8], cursor: &mut usize) {
    while bytes
        .get(*cursor)
        .is_some_and(|byte| is_pdf_whitespace(*byte))
    {
        *cursor += 1;
    }
}

fn skip_pdf_space_and_comments(bytes: &[u8], cursor: &mut usize) {
    loop {
        skip_pdf_whitespace(bytes, cursor);
        if bytes.get(*cursor) != Some(&b'%') {
            return;
        }
        while bytes
            .get(*cursor)
            .is_some_and(|byte| *byte != b'\n' && *byte != b'\r')
        {
            *cursor += 1;
        }
    }
}

fn skip_required_pdf_space_and_comments(bytes: &[u8], cursor: &mut usize) -> Option<()> {
    let start = *cursor;
    skip_pdf_space_and_comments(bytes, cursor);
    (*cursor > start).then_some(())
}

fn is_pdf_delimiter(byte: u8) -> bool {
    is_pdf_whitespace(byte)
        || matches!(
            byte,
            b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
        )
}

fn is_pdf_whitespace(byte: u8) -> bool {
    matches!(byte, 0x00 | b'\t' | b'\n' | 0x0c | b'\r' | b' ')
}

fn header_version(bytes: &[u8]) -> String {
    bytes
        .get(5..8)
        .and_then(|value| std::str::from_utf8(value).ok())
        .map_or_else(|| String::from("unknown"), String::from)
}

fn effective_version(document: &Document) -> String {
    document
        .catalog()
        .ok()
        .and_then(|catalog| catalog.get(b"Version").ok())
        .and_then(|version| version.as_name().ok())
        .and_then(|version| std::str::from_utf8(version).ok())
        .map_or_else(|| sanitized_version(&document.version), sanitized_version)
}

fn sanitized_version(version: &str) -> String {
    if version.len() <= 16
        && !version.is_empty()
        && version
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        String::from(version)
    } else {
        String::from("unknown")
    }
}

fn empty_options(options: &serde_json::Value) -> bool {
    options.as_object().is_some_and(serde_json::Map::is_empty)
}

fn malformed_probe() -> ProcessorProbeOutput {
    ProcessorProbeOutput::RecognizedMalformed {
        media_type: String::from(MEDIA_TYPE),
        reason_code: String::from(MALFORMED_REASON),
    }
}

fn malformed_validation() -> ProcessorValidationOutput {
    ProcessorValidationOutput::Malformed {
        media_type: String::from(MEDIA_TYPE),
        reason_code: String::from(MALFORMED_REASON),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{Object, dictionary};
    use signalbox_file_media_runtime::{FileMediaCeilings, ReadViewBounds};

    #[test]
    fn declaration_probe_fits_runtime_ceiling() {
        let declaration = declaration().expect("valid declaration");
        let reader = &declaration.readers()[0];

        assert!(reader.probe().range_count() >= 2);
        assert!(reader.probe().range_count() <= FileMediaCeilings::version_one().probe_ranges);
    }

    #[test]
    fn declaration_text_view_fits_runtime_ceiling() {
        let declaration = declaration().expect("valid declaration");
        let text = declaration.readers()[0]
            .views()
            .iter()
            .find(|view| view.name().as_str() == TEXT_VIEW)
            .expect("text view");

        let ReadViewBounds::Text { output_bytes, .. } = text.bounds() else {
            panic!("expected text view bounds");
        };

        assert!(output_bytes <= signalbox_file_media_runtime::MAX_TEXT_BODY_BYTES);
    }

    #[test]
    fn xref_preflight_rejects_object_count_before_document_construction() {
        let entries =
            std::iter::repeat_n("0000000009 00000 n\n", MAX_OBJECTS + 1).collect::<String>();
        let bytes = format!(
            "%PDF-1.5\nxref\n1 {}\n{entries}trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n9\n%%EOF\n",
            MAX_OBJECTS + 1,
            MAX_OBJECTS + 2
        );

        assert!(matches!(
            preflight_document(bytes.as_bytes()),
            Ok(Preflight::ObjectLimit)
        ));
    }

    #[test]
    fn xref_stream_has_valid_live_targets() {
        let stream = [
            0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 17, 1, 0, 0, 0, 0, 0, 0, 42,
        ];
        let mut bytes =
            b"2 0 obj\n<< /Type /XRef /Size 3 /Root 1 0 R /W [1 7 0] /Length 24 >>\nstream\n"
                .to_vec();
        bytes.extend_from_slice(&stream);
        bytes.extend_from_slice(b"\nendstream\nendobj");

        let parsed = parse_xref_structure(&bytes, 42).expect("valid xref stream");

        assert!(valid_xref_targets(&parsed, 128));
    }

    #[test]
    fn xref_stream_resolves_the_root_offset() {
        let (parsed, expected_offset) = parsed_xref_stream_fixture();

        assert_eq!(
            root_offset(&parsed).map(|(_, offset)| offset),
            Some(expected_offset)
        );
    }

    #[test]
    fn compressed_xref_requires_a_live_object_stream() {
        let parsed = ParsedXref {
            facts: TrailerFacts {
                root: Some(IndirectReference {
                    object_number: 7,
                    generation: 0,
                }),
                ..TrailerFacts::default()
            },
            live_entries: vec![LiveXrefEntry {
                reference: IndirectReference {
                    object_number: 7,
                    generation: 0,
                },
                location: XrefLocation::Compressed {
                    stream_object: 5,
                    index: 0,
                },
            }],
            declared_objects: BTreeSet::from([7]),
            object_limit_exceeded: false,
        };

        assert!(!valid_xref_targets(&parsed, 128));
    }

    fn parsed_xref_stream_fixture() -> (ParsedXref, u64) {
        let stream = [
            0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 17, 1, 0, 0, 0, 0, 0, 0, 42,
        ];
        let mut bytes =
            b"2 0 obj\n<< /Type /XRef /Size 3 /Root 1 0 R /W [1 7 0] /Length 24 >>\nstream\n"
                .to_vec();
        bytes.extend_from_slice(&stream);
        bytes.extend_from_slice(b"\nendstream\nendobj");
        let expected_offset = u64::from(stream[15]);
        (
            parse_xref_structure(&bytes, 42).expect("valid xref stream"),
            expected_offset,
        )
    }

    #[test]
    fn xref_stream_rejects_a_conflicting_self_generation() {
        let stream = [0, 0, 0, 1, 17, 0, 1, 42, 1];
        let mut bytes =
            b"2 0 obj\n<< /Type /XRef /Size 3 /Root 1 0 R /W [1 1 1] /Index [0 3] /Length 9 >>\nstream\n"
                .to_vec();
        bytes.extend_from_slice(&stream);
        bytes.extend_from_slice(b"\nendstream\nendobj");

        assert!(parse_xref_structure(&bytes, 42).is_none());
    }

    #[test]
    fn xref_stream_accepts_an_indirect_length() {
        let stream = [
            0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 17, 1, 0, 0, 0, 0, 0, 0, 42,
        ];
        let mut bytes =
            b"2 0 obj\n<< /Type /XRef /Size 4 /Root 1 0 R /W [1 7 0] /Index [0 3] /Length 3 0 R >>\nstream\n"
                .to_vec();
        bytes.extend_from_slice(&stream);
        bytes.extend_from_slice(b"\nendstream\nendobj");

        assert!(parse_xref_structure(&bytes, 42).is_some());
    }

    #[test]
    fn indirect_length_xref_stream_preserves_trailing_nul_payload() {
        let stream = [0, 0, 0, 1, 17, 0, 1, 42, 0];
        let mut bytes =
            b"2 0 obj\n<< /Type /XRef /Size 3 /Root 1 0 R /W [1 1 1] /Index [0 3] /Length 3 0 R >>\nstream\n"
                .to_vec();
        bytes.extend_from_slice(&stream);
        bytes.extend_from_slice(b"\nendstream\nendobj");

        let parsed = parse_xref_structure(&bytes, 42).expect("xref stream with trailing NUL");

        assert!(valid_xref_targets(&parsed, 128));
    }

    #[test]
    fn classic_xref_rejects_overlapping_subsections() {
        let bytes = b"xref\n1 2\n0000000017 00000 n\n0000000042 00000 n\n2 1\n0000000064 00000 n\ntrailer\n<< /Size 3 /Root 1 0 R >>";

        assert!(parse_xref_structure(bytes, 0).is_none());
    }

    #[test]
    fn xref_stream_without_its_declared_body_is_rejected() {
        let bytes = b"2 0 obj\n<< /Type /XRef /Size 3 /Root 1 0 R /W [1 7 0] /Length 24 >>\nstream\nendstream\nendobj";

        assert!(parse_xref_structure(bytes, 0).is_none());
    }

    #[test]
    fn xref_stream_rejects_expansion_above_parser_ceiling() {
        let count = MAX_XREF_STREAM_BYTES as u64 + 1;
        let bytes = format!(
            "2 0 obj\n<< /Type /XRef /Size {count} /Root 1 0 R /W [1 0 0] /Index [0 {count}] /Length 1 /Filter /FlateDecode >>\nstream\nx\nendstream\nendobj"
        );

        assert!(parse_xref_structure(bytes.as_bytes(), 0).is_none());
    }

    #[test]
    fn startxref_accepts_a_comment_before_the_offset() {
        assert_eq!(
            startxref_offset(b"startxref % offset comment\n42\n%%EOF"),
            Some(42)
        );
    }

    #[test]
    fn startxref_accepts_a_comment_after_the_offset() {
        assert_eq!(
            startxref_offset(b"startxref\n42 % offset comment\n%%EOF"),
            Some(42)
        );
    }

    #[test]
    fn startxref_requires_a_following_terminal_eof() {
        assert_eq!(startxref_offset(b"%%EOF\nstartxref\n42"), None);
        assert_eq!(startxref_offset(b"startxref\n42\n%%EOF\n"), Some(42));
    }

    #[test]
    fn keyword_consumption_requires_a_token_boundary() {
        let mut cursor = 0;
        assert!(!consume_keyword(b"endobjjunk", &mut cursor, b"endobj"));
        assert_eq!(cursor, 0);
        assert!(consume_keyword(b"endobj\n", &mut cursor, b"endobj"));
    }

    #[test]
    fn null_encrypt_value_is_treated_as_absent() {
        let (facts, _) =
            parse_trailer_dictionary(b"<< /Encrypt null >>", 0).expect("trailer dictionary");
        assert!(!facts.encrypted);
    }

    #[test]
    fn null_root_value_is_treated_as_absent() {
        let (facts, _) = parse_trailer_dictionary(b"<< /Size 3 /Root null /Prev 42 >>", 0)
            .expect("trailer dictionary");

        assert_eq!(facts.root, None);
        assert_eq!(facts.prev, Some(42));
    }

    #[test]
    fn nonnegative_pdf_integers_accept_a_leading_plus() {
        let (facts, _) = parse_trailer_dictionary(b"<< /Size +3 /Root +1 +0 R /Prev +42 >>", 0)
            .expect("trailer dictionary");

        assert_eq!(facts.size, Some(3));
        assert_eq!(facts.root.map(|root| root.object_number), Some(1));
        assert_eq!(facts.prev, Some(42));
        assert!(pages_dictionary_is_valid(
            b"<< /Type /Pages /Kids [] /Count +0 >>"
        ));
    }

    #[test]
    fn trailer_integers_reject_token_suffixes() {
        assert!(parse_trailer_dictionary(b"<< /Size 3junk >>", 0).is_none());
        assert!(parse_trailer_dictionary(b"<< /Prev +42junk >>", 0).is_none());
    }

    #[test]
    fn null_stream_filter_is_treated_as_absent() {
        let mut cursor = 0;

        assert_eq!(parse_filter_names(b"null", &mut cursor), Some(Vec::new()));
        assert_eq!(cursor, 4);
    }

    #[test]
    fn validation_budget_honors_effective_bytes_and_ranges() {
        let mut budget = ValidationBudget::new(64, 2);

        assert!(budget.can_read(32));
        budget.remaining_bytes -= 32;
        budget.remaining_ranges -= 1;
        assert!(budget.can_read(32));
        assert!(!budget.can_read(33));
        assert_eq!(budget.available_after_reserving(1, 1), 0);
    }

    #[test]
    fn flate_xref_stream_is_decoded_within_expected_entry_size() {
        let decoded = [0_u8; 128];
        let mut compressed = Stream::new(Dictionary::new(), decoded.to_vec());
        compressed.compress().expect("compress xref fixture");
        assert!(compressed.dict.has(b"Filter"));
        let mut bytes = format!(
            "2 0 obj\n<< /Type /XRef /Size 16 /Root 1 0 R /W [1 7 0] /Length {} /Filter /FlateDecode >>\nstream\n",
            compressed.content.len()
        )
        .into_bytes();
        bytes.extend_from_slice(&compressed.content);
        bytes.extend_from_slice(b"\nendstream\nendobj");

        let parsed = parse_xref_structure(&bytes, 0).expect("filtered xref stream");
        assert_eq!(parsed.declared_objects.len(), 16);
    }

    #[test]
    fn type_two_xref_entry_retains_object_stream_coordinates() {
        let fixture = type_two_xref_entry_fixture();
        let parsed = parse_xref_stream_entries(&fixture.bytes, [1, 1, 1], &[(11, 1)])
            .expect("type two entry");

        assert_eq!(
            compressed_coordinates(parsed.0[0].location),
            Some((fixture.stream_object, fixture.index))
        );
    }

    struct TypeTwoXrefEntryFixture {
        bytes: [u8; 3],
        stream_object: u64,
        index: u64,
    }

    fn type_two_xref_entry_fixture() -> TypeTwoXrefEntryFixture {
        let stream_object = 7_u8;
        let index = 3_u8;
        TypeTwoXrefEntryFixture {
            bytes: [2, stream_object, index],
            stream_object: u64::from(stream_object),
            index: u64::from(index),
        }
    }

    fn compressed_coordinates(location: XrefLocation) -> Option<(u64, u64)> {
        match location {
            XrefLocation::Compressed {
                stream_object,
                index,
            } => Some((stream_object, index)),
            XrefLocation::Uncompressed(_) => None,
        }
    }

    #[test]
    fn indirect_reference_accepts_pdf_comments_between_tokens() {
        let bytes = b"1 % object\r\n0 % generation\nR";
        let mut cursor = 0;

        assert_eq!(
            parse_indirect_reference(bytes, &mut cursor),
            Some(IndirectReference {
                object_number: 1,
                generation: 0,
            })
        );
    }

    #[test]
    fn indirect_reference_requires_separators() {
        let mut cursor = 0;
        assert!(parse_indirect_reference(b"1 0R", &mut cursor).is_none());
        cursor = 0;
        assert!(parse_indirect_reference(b"10 R", &mut cursor).is_none());
    }

    #[test]
    fn indirect_reference_rejects_large_generation() {
        let mut cursor = 0;
        assert!(parse_indirect_reference(b"1 65536 R", &mut cursor).is_none());
    }

    #[test]
    fn classic_xref_rejects_large_generation() {
        assert!(
            parse_xref_structure(
                b"xref\n1 1\n0000000017 65536 n\ntrailer\n<< /Size 2 /Root 1 0 R >>",
                0,
            )
            .is_none()
        );
    }

    #[test]
    fn indirect_reference_rejects_unrepresentable_object_number() {
        let mut cursor = 0;
        assert!(parse_indirect_reference(b"4294967296 0 R", &mut cursor).is_none());
    }

    #[test]
    fn optional_xref_fields_accept_null() {
        let (facts, _) = parse_trailer_dictionary(b"<< /Prev null /XRefStm null /Index null >>", 0)
            .expect("trailer dictionary");
        assert_eq!(facts.prev, None);
        assert_eq!(facts.xref_stream, None);
        assert_eq!(facts.index, None);
    }

    #[test]
    fn empty_filter_array_is_absent() {
        let mut cursor = 0;
        assert_eq!(parse_filter_names(b"[]", &mut cursor), Some(Vec::new()));
        assert_eq!(cursor, 2);
    }

    #[test]
    fn merged_xrefs_count_only_effective_live_objects() {
        let mut c = ParsedXref {
            facts: TrailerFacts::default(),
            live_entries: Vec::new(),
            declared_objects: (1..=MAX_OBJECTS as u64).collect(),
            object_limit_exceeded: false,
        };
        let p = ParsedXref {
            facts: TrailerFacts::default(),
            live_entries: (1..=MAX_OBJECTS as u64 + 1)
                .map(|n| LiveXrefEntry {
                    reference: IndirectReference {
                        object_number: n,
                        generation: 0,
                    },
                    location: XrefLocation::Uncompressed(n),
                })
                .collect(),
            declared_objects: (1..=MAX_OBJECTS as u64 + 1).collect(),
            object_limit_exceeded: true,
        };
        merge_previous_xref(&mut c, p);
        assert_eq!(c.live_entries.len(), 1);
        assert!(!c.object_limit_exceeded);
    }

    #[test]
    fn previous_xref_supplies_the_root() {
        let mut bytes = b"%PDF-1.5\n1 0 obj\n<< /Type /Catalog >>\nendobj\n".to_vec();
        let previous_offset = bytes.len();
        bytes.extend_from_slice(
            b"xref\n1 1\n0000000009 00000 n\ntrailer\n<< /Size 3 /Root 1 0 R >>\n",
        );
        let latest_offset = bytes.len();
        bytes.extend_from_slice(
            format!(
                "xref\n2 1\n0000000042 00000 n\ntrailer\n<< /Size 3 /Prev {previous_offset} >>\n"
            )
            .as_bytes(),
        );

        let parsed = parse_xref_chain(&bytes, latest_offset).expect("chained xref");

        assert_eq!(parsed.facts.root.map(|root| root.object_number), Some(1));
        assert!(valid_xref_targets(&parsed, bytes.len() as u64));
    }

    #[test]
    fn previous_xref_propagates_encryption_state() {
        let mut bytes = b"%PDF-1.5\n".to_vec();
        let previous_offset = bytes.len();
        bytes.extend_from_slice(
            b"xref\n1 1\n0000000009 00000 n\ntrailer\n<< /Size 3 /Root 1 0 R /Encrypt 2 0 R >>\n",
        );
        let latest_offset = bytes.len();
        bytes.extend_from_slice(
            format!(
                "xref\n2 1\n0000000042 00000 n\ntrailer\n<< /Size 3 /Prev {previous_offset} >>\n"
            )
            .as_bytes(),
        );

        let parsed = parse_xref_chain(&bytes, latest_offset).expect("chained xref");

        assert!(parsed.facts.encrypted);
    }

    #[test]
    fn startxref_ignores_markers_inside_post_offset_comments() {
        assert_eq!(
            startxref_offset(b"startxref\n42 % copied startxref marker\n%%EOF"),
            Some(42)
        );
    }

    #[test]
    fn xref_stream_rejects_overlapping_index_ranges() {
        let mut cursor = 0;

        assert!(parse_index(b"[1 2 2 2]", &mut cursor).is_none());
    }

    #[test]
    fn supplemental_xref_entry_replaces_classic_placeholder() {
        let mut current = ParsedXref {
            facts: TrailerFacts::default(),
            live_entries: Vec::new(),
            declared_objects: BTreeSet::from([7]),
            object_limit_exceeded: false,
        };
        let supplemental = ParsedXref {
            facts: TrailerFacts::default(),
            live_entries: vec![LiveXrefEntry {
                reference: IndirectReference {
                    object_number: 7,
                    generation: 0,
                },
                location: XrefLocation::Compressed {
                    stream_object: 5,
                    index: 0,
                },
            }],
            declared_objects: BTreeSet::from([7]),
            object_limit_exceeded: false,
        };

        merge_supplemental_xref(&mut current, supplemental);

        assert!(matches!(
            current.live_entries[0].location,
            XrefLocation::Compressed {
                stream_object: 5,
                index: 0
            }
        ));
    }

    #[test]
    fn supplemental_xref_free_entries_remove_classic_live_entries() {
        let mut current = ParsedXref {
            facts: TrailerFacts::default(),
            live_entries: vec![LiveXrefEntry {
                reference: IndirectReference {
                    object_number: 7,
                    generation: 0,
                },
                location: XrefLocation::Uncompressed(42),
            }],
            declared_objects: BTreeSet::from([7]),
            object_limit_exceeded: false,
        };
        let supplemental = ParsedXref {
            facts: TrailerFacts::default(),
            live_entries: Vec::new(),
            declared_objects: BTreeSet::from([7]),
            object_limit_exceeded: false,
        };

        merge_supplemental_xref(&mut current, supplemental);

        assert!(current.live_entries.is_empty());
    }

    #[test]
    fn scalar_object_skipping_rejects_unknown_keywords() {
        let mut cursor = 0;

        assert!(skip_pdf_object(b"garbage", &mut cursor, 0).is_none());
    }

    #[test]
    fn scalar_object_skipping_accepts_pdf_number() {
        let mut cursor = 0;

        assert!(skip_pdf_object(b"-12.5", &mut cursor, 0).is_some());
        assert_eq!(cursor, 5);
    }

    #[test]
    fn hexadecimal_string_skipping_rejects_non_hexadecimal_bytes() {
        let mut cursor = 0;

        assert!(skip_pdf_object(b"<GG>", &mut cursor, 0).is_none());
    }

    #[test]
    fn hexadecimal_string_skipping_accepts_whitespace_and_odd_digits() {
        let mut cursor = 0;

        assert!(skip_pdf_object(b"<A 0f>", &mut cursor, 0).is_some());
        assert_eq!(cursor, 6);
    }

    #[test]
    fn hybrid_xref_stream_entries_are_merged_before_preflight() {
        let supplemental_offset = b"%PDF-1.5\n".len();
        let entry_count = MAX_OBJECTS + 2;
        let stream = hybrid_xref_stream_entries(entry_count);
        let mut bytes = b"%PDF-1.5\n".to_vec();
        bytes.extend_from_slice(
            format!(
                "2 0 obj\n<< /Type /XRef /Size {entry_count} /Root 1 0 R /W [1 1 1] /Length {} >>\nstream\n",
                stream.len()
            )
            .as_bytes(),
        );
        bytes.extend_from_slice(&stream);
        bytes.extend_from_slice(b"\nendstream\nendobj\n");
        let classic_offset = bytes.len();
        bytes.extend_from_slice(
            format!(
                "xref\n1 1\n0000000009 00000 n\ntrailer\n<< /Size {entry_count} /Root 1 0 R /XRefStm {supplemental_offset} >>\n"
            )
            .as_bytes(),
        );

        let parsed = parse_xref_chain(&bytes, classic_offset).expect("hybrid xref chain");

        assert!(parsed.object_limit_exceeded);
    }

    fn hybrid_xref_stream_entries(entry_count: usize) -> Vec<u8> {
        let mut entries = vec![0, 0, 0, 2, 1, 1, 1, 9, 0];
        entries.extend(
            (3..entry_count).flat_map(|index| [2, 1, u8::try_from(index % 2).unwrap_or(0)]),
        );
        entries
    }

    #[test]
    fn sparse_object_numbers_count_only_live_entries() {
        let bytes = b"xref\n0 1\n0000000000 65535 f\n20000 1\n0000000017 00000 n\ntrailer\n<< /Size 20001 /Root 20000 0 R >>";

        let parsed = parse_xref_structure(bytes, 0).expect("sparse xref");

        assert!(!parsed.object_limit_exceeded);
        assert_eq!(parsed.live_entries.len(), 1);
    }

    #[test]
    fn root_probe_requires_the_referenced_catalog_object() {
        let root = IndirectReference {
            object_number: 7,
            generation: 0,
        };

        assert_eq!(
            catalog_facts(b"7 0 obj\n<< /Type /Catalog /Pages 8 0 R >>\nendobj", root,)
                .map(|catalog| catalog.pages),
            Some(IndirectReference {
                object_number: 8,
                generation: 0,
            })
        );
        assert!(catalog_facts(b"7 0 obj\n<< /Type /Pages /Count 0 >>\nendobj", root).is_none());
        assert!(catalog_facts(b"7 0 obj\n<< /Type /Catalog >>\nendobj", root).is_none());
        assert!(catalog_facts(b"7 0 obj\n<< /Type /Catalog /Pages 8 0 R >>", root).is_none());
    }

    #[test]
    fn root_probe_requires_xref_offset_at_object_header() {
        let root = IndirectReference {
            object_number: 7,
            generation: 0,
        };

        assert!(
            catalog_facts(
                b"% misplaced xref offset\n7 0 obj\n<< /Type /Catalog /Pages 8 0 R >>\nendobj",
                root,
            )
            .is_none()
        );
    }

    #[test]
    fn root_probe_reports_the_catalog_version_override() {
        let root = IndirectReference {
            object_number: 7,
            generation: 0,
        };

        let catalog = catalog_facts(
            b"7 0 obj\n<< /Type /Catalog /Pages 8 0 R /Version /1.7 >>\nendobj",
            root,
        )
        .expect("catalog facts");

        assert_eq!(catalog.version.as_deref(), Some("1.7"));
    }

    #[test]
    fn compressed_catalog_is_resolved_at_the_xref_index() {
        let content = b"9 0 7 4 null<< /Type /Catalog /Pages 8 0 R >>";
        let mut bytes = format!(
            "5 0 obj\n<< /Type /ObjStm /N 2 /First 8 /Length {} >>\nstream\n",
            content.len()
        )
        .into_bytes();
        bytes.extend_from_slice(content);
        bytes.extend_from_slice(b"\nendstream\nendobj");

        let stream = IndirectReference {
            object_number: 5,
            generation: 0,
        };
        let catalog = IndirectReference {
            object_number: 7,
            generation: 0,
        };
        let object =
            object_stream_object(&bytes, stream, catalog, 1, 4_096, None).expect("catalog object");
        assert_eq!(
            parse_catalog_dictionary(&object, 0).map(|(catalog, _)| catalog.pages),
            Some(IndirectReference {
                object_number: 8,
                generation: 0,
            })
        );
        assert!(object_stream_object(&bytes, stream, catalog, 0, 4_096, None).is_none());
    }

    #[test]
    fn object_stream_accepts_a_resolved_indirect_length() {
        let content = b"7 0 << /Type /Catalog /Pages 8 0 R >>";
        let mut bytes =
            b"5 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Length 6 0 R >>\nstream\n".to_vec();
        bytes.extend_from_slice(content);
        bytes.extend_from_slice(b"\nendstream\nendobj");
        let stream = IndirectReference {
            object_number: 5,
            generation: 0,
        };
        let catalog = IndirectReference {
            object_number: 7,
            generation: 0,
        };
        let length = IndirectReference {
            object_number: 6,
            generation: 0,
        };

        let object = object_stream_object(
            &bytes,
            stream,
            catalog,
            0,
            4_096,
            Some((length, content.len() as u64)),
        )
        .expect("catalog object");

        assert_eq!(
            parse_catalog_dictionary(&object, 0).map(|(catalog, _)| catalog.pages),
            Some(IndirectReference {
                object_number: 8,
                generation: 0,
            })
        );
    }

    #[test]
    fn compressed_integer_resolves_an_object_stream_length() {
        let length_object = b"6 0 42";
        let mut bytes = format!(
            "5 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Length {} >>\nstream\n",
            length_object.len()
        )
        .into_bytes();
        bytes.extend_from_slice(length_object);
        bytes.extend_from_slice(b"\nendstream\nendobj");
        let parsed = ParsedXref {
            facts: TrailerFacts::default(),
            live_entries: vec![
                LiveXrefEntry {
                    reference: IndirectReference {
                        object_number: 5,
                        generation: 0,
                    },
                    location: XrefLocation::Uncompressed(0),
                },
                LiveXrefEntry {
                    reference: IndirectReference {
                        object_number: 6,
                        generation: 0,
                    },
                    location: XrefLocation::Compressed {
                        stream_object: 5,
                        index: 0,
                    },
                },
            ],
            declared_objects: BTreeSet::from([5, 6]),
            object_limit_exceeded: false,
        };

        assert_eq!(
            resolve_integer_object(
                &bytes,
                &parsed,
                IndirectReference {
                    object_number: 6,
                    generation: 0,
                },
                &mut BTreeSet::new(),
                &mut BTreeMap::new(),
            ),
            Some(42)
        );
    }

    #[test]
    fn compressed_catalog_requires_object_stream_terminators() {
        let content = b"7 0 << /Type /Catalog /Pages 8 0 R >>";
        let mut bytes = format!(
            "5 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Length {} >>\nstream\n",
            content.len()
        )
        .into_bytes();
        bytes.extend_from_slice(content);

        assert!(
            object_stream_object(
                &bytes,
                IndirectReference {
                    object_number: 5,
                    generation: 0,
                },
                IndirectReference {
                    object_number: 7,
                    generation: 0,
                },
                0,
                4_096,
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn object_stream_count_is_capped_before_header_allocation() {
        let content = b"7 0 << /Type /Catalog /Pages 8 0 R >>";
        let mut bytes = format!(
            "5 0 obj\n<< /Type /ObjStm /N {} /First 4 /Length {} >>\nstream\n",
            MAX_OBJECTS + 1,
            content.len()
        )
        .into_bytes();
        bytes.extend_from_slice(content);
        bytes.extend_from_slice(b"\nendstream\nendobj");

        assert!(
            object_stream_object(
                &bytes,
                IndirectReference {
                    object_number: 5,
                    generation: 0,
                },
                IndirectReference {
                    object_number: 7,
                    generation: 0,
                },
                0,
                4_096,
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn object_stream_decoding_uses_the_explicit_expansion_ceiling() {
        let decoded = vec![b'a'; 8_192];
        let mut compressed = Stream::new(Dictionary::new(), decoded.clone());
        compressed
            .compress()
            .expect("compress object stream fixture");

        let expanded = decode_pdf_stream(
            &compressed.content,
            &[b"FlateDecode".to_vec()],
            None,
            MAX_OBJECT_STREAM_BYTES,
        )
        .expect("decode object stream fixture");

        assert!(compressed.content.len() < decoded.len());
        assert_eq!(expanded, decoded);
    }

    #[test]
    fn shared_object_stream_resolves_catalog_and_page_tree() {
        let (bytes, catalog_index, pages_index) = shared_object_stream_fixture();
        let stream = IndirectReference {
            object_number: 5,
            generation: 0,
        };
        let catalog = IndirectReference {
            object_number: 7,
            generation: 0,
        };
        let pages = IndirectReference {
            object_number: 8,
            generation: 0,
        };

        let catalog_object =
            object_stream_object(&bytes, stream, catalog, catalog_index, 4_096, None)
                .expect("catalog object");
        let pages_object = object_stream_object(&bytes, stream, pages, pages_index, 4_096, None)
            .expect("page-tree object");

        assert!(parse_catalog_dictionary(&catalog_object, 0).is_some());
        assert!(pages_dictionary_is_valid(&pages_object));
    }

    fn shared_object_stream_fixture() -> (Vec<u8>, u64, u64) {
        let catalog = b"<< /Type /Catalog /Pages 8 0 R >>";
        let pages = b"<< /Type /Pages /Kids [] /Count 0 >>";
        let header = format!("7 0 8 {} ", catalog.len());
        let first = header.len();
        let mut content = header.into_bytes();
        content.extend_from_slice(catalog);
        content.extend_from_slice(pages);
        let mut bytes = format!(
            "5 0 obj\n<< /Type /ObjStm /N 2 /First {first} /Length {} >>\nstream\n",
            content.len()
        )
        .into_bytes();
        bytes.extend_from_slice(&content);
        bytes.extend_from_slice(b"\nendstream\nendobj");
        (bytes, 0, 1)
    }

    #[test]
    fn compressed_page_tree_dictionary_requires_mandatory_fields() {
        assert!(pages_dictionary_is_valid(
            b"<< /Type /Pages /Kids [] /Count 0 >>"
        ));
        assert!(!pages_dictionary_is_valid(b"<< /Type /Pages /Count 0 >>"));
        assert!(!pages_dictionary_is_valid(b"<< /Type /Pages /Kids [] >>"));
        assert!(!pages_dictionary_is_valid(
            b"<< /Type /Pages /Kids [] /Count 9 0 R >>"
        ));
    }

    #[test]
    fn bounded_page_tree_root_rejects_a_parent() {
        let root = IndirectReference {
            object_number: 8,
            generation: 0,
        };

        assert!(!pages_dictionary_is_valid_for_root(
            b"<< /Type /Pages /Parent 9 0 R /Kids [] /Count 0 >>",
            Some(root),
            None,
        ));
    }

    #[test]
    fn bounded_page_tree_root_rejects_duplicate_kids() {
        let root = IndirectReference {
            object_number: 8,
            generation: 0,
        };

        assert!(!pages_dictionary_is_valid_for_root(
            b"<< /Type /Pages /Kids [9 0 R 9 0 R] /Count 2 >>",
            Some(root),
            None,
        ));
    }

    #[test]
    fn bounded_page_tree_root_rejects_itself_as_a_kid() {
        let root = IndirectReference {
            object_number: 8,
            generation: 0,
        };

        assert!(!pages_dictionary_is_valid_for_root(
            b"<< /Type /Pages /Kids [8 0 R] /Count 1 >>",
            Some(root),
            None,
        ));
    }

    #[test]
    fn bounded_page_tree_root_rejects_a_dangling_kid() {
        let root = IndirectReference {
            object_number: 8,
            generation: 0,
        };
        let entries = [LiveXrefEntry {
            reference: root,
            location: XrefLocation::Uncompressed(0),
        }];

        assert!(!pages_dictionary_is_valid_for_xref(
            b"<< /Type /Pages /Kids [9 0 R] /Count 1 >>",
            root,
            &entries,
        ));
    }

    #[test]
    fn compressed_page_tree_object_is_resolved_at_its_xref_index() {
        let content = b"8 0 << /Type /Pages /Kids [] /Count 0 >>";
        let mut bytes = format!(
            "5 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Length {} >>\nstream\n",
            content.len()
        )
        .into_bytes();
        bytes.extend_from_slice(content);
        bytes.extend_from_slice(b"\nendstream\nendobj");

        let object = object_stream_object(
            &bytes,
            IndirectReference {
                object_number: 5,
                generation: 0,
            },
            IndirectReference {
                object_number: 8,
                generation: 0,
            },
            0,
            4_096,
            None,
        )
        .expect("compressed page-tree object");

        assert!(pages_dictionary_is_valid(&object));
    }

    #[test]
    fn stream_decode_parameters_are_preserved_as_lopdf_objects() {
        let bytes = b"<< /Predictor 12 /Columns 3 /Colors 1 /BitsPerComponent 8 >>";
        let mut cursor = 0;
        let parsed = parse_lopdf_object(bytes, &mut cursor, 0).expect("decode parameters");
        skip_pdf_space_and_comments(bytes, &mut cursor);

        assert_eq!(cursor, bytes.len());
        let Object::Dictionary(parameters) = parsed else {
            panic!("expected DecodeParms dictionary");
        };
        assert_eq!(
            parameters
                .get(b"Predictor")
                .and_then(Object::as_i64)
                .expect("integer Predictor"),
            12
        );
        assert_eq!(
            parameters
                .get(b"Columns")
                .and_then(Object::as_i64)
                .expect("integer Columns"),
            3
        );
    }

    #[test]
    fn page_tree_probe_requires_the_referenced_pages_object() {
        let pages = IndirectReference {
            object_number: 8,
            generation: 0,
        };

        assert!(object_is_pages(
            b"8 0 obj\n<< /Type /Pages /Count 0 /Kids [] >>\nendobj",
            pages
        ));
        assert!(!object_is_pages(
            b"8 0 obj\n<< /Type /Page >>\nendobj",
            pages
        ));
        assert!(!object_is_pages(
            b"9 0 obj\n<< /Type /Pages /Count 0 /Kids [] >>\nendobj",
            pages
        ));
    }

    #[test]
    fn page_tree_xref_offset_requires_object_header() {
        let pages = IndirectReference {
            object_number: 8,
            generation: 0,
        };
        assert!(!object_is_pages(
            b"% offset comment
8 0 obj
<< /Type /Pages /Count 0 /Kids [] >>
endobj",
            pages
        ));
    }

    #[test]
    fn uncompressed_page_tree_requires_reference_kids() {
        let p = IndirectReference {
            object_number: 8,
            generation: 0,
        };
        assert!(!object_is_pages(
            b"8 0 obj
<< /Type /Pages /Count 2 /Kids [1 0 R 2] >>
endobj",
            p
        ));
    }

    #[test]
    fn page_stream_read_reserves_length_probe_budget() {
        let b = ValidationBudget::new(16_384, 2);
        assert_eq!(
            b.available_after_reserving(ROOT_VALIDATION_BYTES, 1),
            12_288
        );
    }

    #[test]
    fn bounded_page_tree_rejects_count_above_ceiling() {
        let pages = IndirectReference {
            object_number: 8,
            generation: 0,
        };
        let dictionary = format!("<< /Type /Pages /Kids [] /Count {} >>", MAX_PAGES + 1);
        let object = format!(
            "8 0 obj\n<< /Type /Pages /Count {} /Kids [] >>\nendobj",
            MAX_PAGES + 1
        );

        assert!(!pages_dictionary_is_valid(dictionary.as_bytes()));
        assert!(!object_is_pages(object.as_bytes(), pages));
    }

    #[test]
    fn bounded_page_tree_rejects_empty_kids_with_positive_count() {
        let pages = IndirectReference {
            object_number: 8,
            generation: 0,
        };
        assert!(!pages_dictionary_is_valid(
            b"<< /Type /Pages /Kids [] /Count 1 >>"
        ));
        assert!(!object_is_pages(
            b"8 0 obj\n<< /Type /Pages /Kids [] /Count 1 >>\nendobj",
            pages
        ));
    }

    #[test]
    fn page_collection_rejects_inconsistent_declared_count() {
        let mut document = Document::with_version("1.5");
        let pages_id = document.add_object(dictionary! {
            "Type" => "Pages",
            "Kids" => Vec::<Object>::new(),
            "Count" => 1,
        });
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);

        assert!(matches!(
            collect_pages(&document),
            Err(PageCollectionError::Malformed)
        ));
    }

    #[test]
    fn page_collection_requires_typed_page_tree_nodes() {
        let mut document = Document::with_version("1.5");
        let pages_id = document.add_object(Dictionary::new());
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);

        assert!(matches!(
            collect_pages(&document),
            Err(PageCollectionError::Malformed)
        ));
    }

    #[test]
    fn page_collection_rejects_a_parent_on_the_root() {
        let mut document = Document::with_version("1.5");
        let parent_id = document.new_object_id();
        let pages_id = document.add_object(dictionary! {
            "Type" => "Pages",
            "Parent" => parent_id,
            "Kids" => Vec::<Object>::new(),
            "Count" => 0,
        });
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);

        assert!(matches!(
            collect_pages(&document),
            Err(PageCollectionError::Malformed)
        ));
    }

    #[test]
    fn referenced_page_contents_must_be_streams() {
        let mut document = Document::with_version("1.5");
        let contents_id = document.add_object(Object::Integer(42));
        let page_id =
            document.add_object(dictionary! { "Type" => "Page", "Contents" => contents_id });
        assert!(validate_page_contents(&document, page_id).is_err());
    }

    #[test]
    fn null_page_contents_are_treated_as_absent() {
        let mut document = Document::with_version("1.5");
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Contents" => Object::Null,
        });

        assert!(validate_page_contents(&document, page_id).is_ok());
    }

    #[test]
    fn page_content_stream_arrays_are_bounded_before_lookup() {
        let mut document = Document::with_version("1.5");
        let stream_id = document.add_object(Stream::new(Dictionary::new(), Vec::new()));
        let contents = vec![Object::Reference(stream_id); MAX_PAGE_CONTENT_STREAMS + 1];
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Contents" => contents,
        });

        assert!(validate_page_contents(&document, page_id).is_err());
    }

    #[test]
    fn null_page_kids_are_treated_as_absent() {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Kids" => Object::Null,
        });
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);

        let Ok(pages) = collect_pages(&document) else {
            panic!("expected valid page collection");
        };

        assert_eq!(pages.len(), 1);
    }

    #[test]
    fn nested_page_tree_accepts_exact_page_ceiling() {
        let document = nested_page_tree_fixture(MAX_PAGES);

        let Ok(pages) = collect_pages(&document) else {
            panic!("expected exact-ceiling page tree to remain valid");
        };

        assert_eq!(pages.len(), MAX_PAGES);
    }

    fn nested_page_tree_fixture(page_count: usize) -> Document {
        let mut document = Document::with_version("1.5");
        let root_id = document.new_object_id();
        let nested_id = document.new_object_id();
        let page_ids = (0..page_count)
            .map(|_| document.new_object_id())
            .collect::<Vec<_>>();
        for page_id in &page_ids {
            document.objects.insert(
                *page_id,
                Object::Dictionary(dictionary! {
                    "Type" => "Page",
                    "Parent" => nested_id,
                }),
            );
        }
        let declared_count = i64::try_from(page_count).expect("page count fits i64");
        document.objects.insert(
            nested_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Parent" => root_id,
                "Kids" => page_ids.iter().copied().map(Object::Reference).collect::<Vec<_>>(),
                "Count" => declared_count,
            }),
        );
        document.objects.insert(
            root_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(nested_id)],
                "Count" => declared_count,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => root_id,
        });
        document.trailer.set("Root", catalog_id);
        document
    }

    #[test]
    fn complete_xref_chain_rejects_forward_supplemental_stream() {
        let forward_offset = 256_usize;
        let mut bytes = format!(
            "xref\n1 1\n0000000009 00000 n\ntrailer\n<< /Size 3 /Root 1 0 R /XRefStm {forward_offset} >>\n"
        )
        .into_bytes();
        bytes.resize(forward_offset, b' ');
        bytes.extend_from_slice(
            b"2 0 obj\n<< /Type /XRef /Size 3 /Root 1 0 R /W [1 1 1] /Length 9 >>\nstream\n\0\0\0\x01\x09\0\x01\x2a\0\nendstream\nendobj",
        );

        assert!(parse_xref_chain(&bytes, 0).is_none());
    }

    #[test]
    fn page_collection_bounds_kids_before_stack_expansion() {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let kids = (0..=MAX_PAGES)
            .map(|_| Object::Reference(document.new_object_id()))
            .collect::<Vec<_>>();
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => 0,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);

        assert!(matches!(
            collect_pages(&document),
            Err(PageCollectionError::Limit)
        ));
    }

    #[test]
    fn complete_xref_chain_rejects_forward_prev_link() {
        let forward_offset = 256_usize;
        let mut bytes = format!(
            "xref\n1 1\n0000000009 00000 n\ntrailer\n<< /Size 3 /Root 1 0 R /Prev {forward_offset} >>\n"
        )
        .into_bytes();
        bytes.resize(forward_offset, b' ');
        bytes.extend_from_slice(
            b"xref\n2 1\n0000000042 00000 n\ntrailer\n<< /Size 3 /Root 1 0 R >>\n",
        );

        assert!(parse_xref_chain(&bytes, 0).is_none());
    }

    #[test]
    fn complete_preflight_bounds_object_stream_expansion() {
        let decoded = vec![b'a'; MAX_OBJECT_STREAM_BYTES + 1];
        let mut compressed = Stream::new(Dictionary::new(), decoded);
        compressed
            .compress()
            .expect("compress object stream fixture");
        let mut bytes = format!(
            "5 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Length {} /Filter /FlateDecode >>\nstream\n",
            compressed.content.len()
        )
        .into_bytes();
        bytes.extend_from_slice(&compressed.content);
        bytes.extend_from_slice(b"\nendstream\nendobj");
        let parsed = ParsedXref {
            facts: TrailerFacts::default(),
            live_entries: vec![
                LiveXrefEntry {
                    reference: IndirectReference {
                        object_number: 5,
                        generation: 0,
                    },
                    location: XrefLocation::Uncompressed(0),
                },
                LiveXrefEntry {
                    reference: IndirectReference {
                        object_number: 7,
                        generation: 0,
                    },
                    location: XrefLocation::Compressed {
                        stream_object: 5,
                        index: 0,
                    },
                },
            ],
            declared_objects: BTreeSet::from([5, 7]),
            object_limit_exceeded: false,
        };

        let fits = object_streams_fit_expansion_limit(&bytes, &parsed)
            .expect("well-formed object stream preflight");

        assert!(!fits);
    }

    #[test]
    fn page_collection_stops_before_exceeding_the_page_ceiling() {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let kids = (0..=MAX_PAGES)
            .map(|_| Object::Reference(document.new_object_id()))
            .collect::<Vec<_>>();
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => i64::try_from(MAX_PAGES + 1).unwrap_or(i64::MAX),
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);

        assert!(matches!(
            collect_pages(&document),
            Err(PageCollectionError::Limit)
        ));
    }
    #[test]
    fn newest_size_bounds_merged_xrefs() {
        let mut b = b"%PDF-1.5\n".to_vec();
        let prev = b.len();
        b.extend_from_slice(b"xref\n7 1\n0000000009 00000 n\ntrailer\n<< /Size 8 /Root 1 0 R >>\n");
        let latest = b.len();
        b.extend_from_slice(
            format!(
                "xref\n2 1\n0000000042 00000 n\ntrailer\n<< /Size 3 /Root 1 0 R /Prev {prev} >>\n"
            )
            .as_bytes(),
        );
        assert!(parse_xref_chain(&b, latest).is_none());
    }

    #[test]
    fn compressed_pages_require_reference_kids() {
        assert!(!pages_dictionary_is_valid(
            b"<< /Type /Pages /Kids [1 0 R 2] /Count 2 >>"
        ));
        assert!(pages_dictionary_is_valid(
            b"<< /Type /Pages /Kids [1 0 R 2 0 R] /Count 2 >>"
        ));
    }

    #[test]
    fn object_stream_n_requires_integer_token() {
        let b =
            b"5 0 obj\n<< /Type /ObjStm /N 1.0 /First 0 /Length 0 >>\nstream\n\nendstream\nendobj";
        let r = IndirectReference {
            object_number: 5,
            generation: 0,
        };
        assert!(parse_object_stream(b, r, None).is_none());
    }

    #[test]
    fn object_stream_first_requires_integer_token() {
        let b =
            b"5 0 obj\n<< /Type /ObjStm /N 1 /First 4.0 /Length 0 >>\nstream\n\nendstream\nendobj";
        let r = IndirectReference {
            object_number: 5,
            generation: 0,
        };
        assert!(parse_object_stream(b, r, None).is_none());
    }

    fn aggregate_stream_fixture() -> (Vec<u8>, ParsedXref) {
        let mut z = Stream::new(Dictionary::new(), vec![b'a'; MAX_OBJECT_STREAM_BYTES]);
        z.compress().expect("compress");
        let count = MAX_TOTAL_OBJECT_STREAM_BYTES / MAX_OBJECT_STREAM_BYTES + 1;
        let mut b = Vec::new();
        let mut entries = Vec::new();
        let mut declared = BTreeSet::new();
        for i in 0..count {
            let stream = u64::try_from(i * 2 + 1).expect("stream");
            let target = stream + 1;
            let offset = u64::try_from(b.len()).expect("offset");
            b.extend_from_slice(format!("{stream} 0 obj\n<< /Type /ObjStm /N 1 /First 0 /Length {} /Filter /FlateDecode >>\nstream\n",z.content.len()).as_bytes());
            b.extend_from_slice(&z.content);
            b.extend_from_slice(b"\nendstream\nendobj\n");
            entries.push(LiveXrefEntry {
                reference: IndirectReference {
                    object_number: stream,
                    generation: 0,
                },
                location: XrefLocation::Uncompressed(offset),
            });
            entries.push(LiveXrefEntry {
                reference: IndirectReference {
                    object_number: target,
                    generation: 0,
                },
                location: XrefLocation::Compressed {
                    stream_object: stream,
                    index: 0,
                },
            });
            declared.insert(stream);
            declared.insert(target);
        }
        (
            b,
            ParsedXref {
                facts: TrailerFacts::default(),
                live_entries: entries,
                declared_objects: declared,
                object_limit_exceeded: false,
            },
        )
    }

    #[test]
    fn compressed_xref_coordinates_require_the_indexed_object() {
        let stream = IndirectReference {
            object_number: 5,
            generation: 0,
        };
        let target = IndirectReference {
            object_number: 7,
            generation: 0,
        };
        let content = b"7 0 null";
        let mut bytes = format!(
            "5 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Length {} >>\nstream\n",
            content.len()
        )
        .into_bytes();
        bytes.extend_from_slice(content);
        bytes.extend_from_slice(b"\nendstream\nendobj");

        let mut remaining = MAX_TOTAL_OBJECT_STREAM_BYTES;

        assert!(object_stream_coordinates_are_valid(
            &bytes,
            stream,
            None,
            &[(target, 0)],
            &mut remaining,
        ));
        assert!(!object_stream_coordinates_are_valid(
            &bytes,
            stream,
            None,
            &[(target, 1)],
            &mut remaining,
        ));
        assert!(!object_stream_coordinates_are_valid(
            &bytes,
            stream,
            None,
            &[(
                IndirectReference {
                    object_number: 8,
                    generation: 0,
                },
                0,
            )],
            &mut remaining,
        ));
    }

    #[test]
    fn complete_preflight_bounds_aggregate_stream_expansion() {
        let (b, p) = aggregate_stream_fixture();
        assert!(!object_streams_fit_expansion_limit(&b, &p).expect("preflight"));
    }

    struct UnitSource {
        bytes: Vec<u8>,
        length: NonZeroU64,
    }
    impl UnitSource {
        fn new(bytes: Vec<u8>) -> Self {
            let length =
                NonZeroU64::new(u64::try_from(bytes.len()).expect("length")).expect("nonempty");
            Self { bytes, length }
        }
    }
    impl VerifiedBlobSource for UnitSource {
        fn digest(&self) -> signalbox_file_media_runtime::FileDigest {
            signalbox_file_media_runtime::FileDigest::from_bytes([0x50; 32])
        }
        fn byte_length(&self) -> NonZeroU64 {
            self.length
        }
        fn read_range(
            &self,
            o: u64,
            l: NonZeroU64,
        ) -> signalbox_file_media_runtime::SourceReadFuture<'_> {
            let value = usize::try_from(o)
                .ok()
                .and_then(|s| {
                    usize::try_from(l.get())
                        .ok()
                        .and_then(|n| s.checked_add(n).map(|e| (s, e)))
                })
                .and_then(|(s, e)| self.bytes.get(s..e).map(<[u8]>::to_vec))
                .ok_or(signalbox_file_media_runtime::SourceReadError::RangeOutOfBounds);
            Box::pin(async move { value })
        }
    }

    #[tokio::test]
    async fn bounded_object_stream_targets_reuse_covering_ranges() {
        let first = b"5 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Length 8 >>\nstream\n7 0 null\nendstream\nendobj\n";
        let second_offset = u64::try_from(first.len()).expect("second offset");
        let second = b"6 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Length 8 >>\nstream\n8 0 null\nendstream\nendobj\n";
        let mut bytes = first.to_vec();
        bytes.extend_from_slice(second);
        bytes.resize(16_384, b' ');
        let source = UnitSource::new(bytes);
        let parsed = ParsedXref {
            facts: TrailerFacts::default(),
            live_entries: vec![
                LiveXrefEntry {
                    reference: IndirectReference {
                        object_number: 5,
                        generation: 0,
                    },
                    location: XrefLocation::Uncompressed(0),
                },
                LiveXrefEntry {
                    reference: IndirectReference {
                        object_number: 6,
                        generation: 0,
                    },
                    location: XrefLocation::Uncompressed(second_offset),
                },
                LiveXrefEntry {
                    reference: IndirectReference {
                        object_number: 7,
                        generation: 0,
                    },
                    location: XrefLocation::Compressed {
                        stream_object: 5,
                        index: 0,
                    },
                },
                LiveXrefEntry {
                    reference: IndirectReference {
                        object_number: 8,
                        generation: 0,
                    },
                    location: XrefLocation::Compressed {
                        stream_object: 6,
                        index: 0,
                    },
                },
            ],
            declared_objects: BTreeSet::from([5, 6, 7, 8]),
            object_limit_exceeded: false,
        };
        let mut budget = ValidationBudget::new(source.byte_length().get(), 4);

        let validated = bounded_object_stream_targets(&source, &mut budget, &parsed)
            .await
            .expect("bounded validation");

        assert!(validated.is_some());
        assert_eq!(budget.remaining_ranges, 3);
    }

    #[tokio::test]
    async fn bounded_resolves_compressed_stream_length() {
        let holder=b"9 0 obj\n<< /Type /ObjStm /N 1 /First 5 /Length 6 >>\nstream\n20 0 6\nendstream\nendobj\n";
        let length_reference = IndirectReference {
            object_number: 20,
            generation: 0,
        };
        let holder_reference = IndirectReference {
            object_number: 9,
            generation: 0,
        };
        let expected_length = object_stream_object(
            holder,
            holder_reference,
            length_reference,
            0,
            MAX_OBJECT_STREAM_BYTES,
            None,
        )
        .and_then(|object| parse_integer_object_value(&object))
        .expect("fixture length");
        let offset = u64::try_from(holder.len()).expect("offset");
        let target=b"5 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Length 20 0 R >>\nstream\n7 0 x!\nendstream\nendobj";
        let mut bytes = holder.to_vec();
        bytes.extend_from_slice(target);
        let source = UnitSource::new(bytes);
        let entries = vec![
            LiveXrefEntry {
                reference: IndirectReference {
                    object_number: 9,
                    generation: 0,
                },
                location: XrefLocation::Uncompressed(0),
            },
            LiveXrefEntry {
                reference: IndirectReference {
                    object_number: 20,
                    generation: 0,
                },
                location: XrefLocation::Compressed {
                    stream_object: 9,
                    index: 0,
                },
            },
            LiveXrefEntry {
                reference: IndirectReference {
                    object_number: 5,
                    generation: 0,
                },
                location: XrefLocation::Uncompressed(offset),
            },
        ];
        let parsed = ParsedXref {
            facts: TrailerFacts::default(),
            live_entries: entries,
            declared_objects: BTreeSet::from([5, 9, 20]),
            object_limit_exceeded: false,
        };
        let mut budget = ValidationBudget::new(source.byte_length().get(), 4);
        let stream = IndirectReference {
            object_number: 5,
            generation: 0,
        };
        let got = resolve_object_stream_length(
            &source,
            &mut budget,
            &parsed,
            target,
            offset,
            stream,
            (0, 0),
        )
        .await
        .expect("resolve");
        assert_eq!(got, Some((length_reference, expected_length)));
    }

    #[test]
    fn object_stream_coordinates_charge_the_aggregate_decode_budget() {
        let stream = IndirectReference {
            object_number: 5,
            generation: 0,
        };
        let target = IndirectReference {
            object_number: 7,
            generation: 0,
        };
        let content = b"7 0 null";
        let mut bytes = format!(
            "5 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Length {} >>\nstream\n",
            content.len()
        )
        .into_bytes();
        bytes.extend_from_slice(content);
        bytes.extend_from_slice(b"\nendstream\nendobj");
        let mut remaining = content.len() * 2 + 1;

        assert!(object_stream_coordinates_are_valid(
            &bytes,
            stream,
            None,
            &[(target, 0)],
            &mut remaining,
        ));
        assert_eq!(remaining, content.len() + 1);
        assert!(object_stream_coordinates_are_valid(
            &bytes,
            stream,
            None,
            &[(target, 0)],
            &mut remaining,
        ));
        assert_eq!(remaining, 1);
        assert!(!object_stream_coordinates_are_valid(
            &bytes,
            stream,
            None,
            &[(target, 0)],
            &mut remaining,
        ));
    }

    #[tokio::test]
    async fn bounded_length_probes_resolve_from_the_cached_range() {
        let target = b"5 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Length 20 0 R >>\nstream\n7 0 x!\nendstream\nendobj\n";
        let mut bytes = target.to_vec();
        let integer_offset = u64::try_from(bytes.len()).expect("offset");
        bytes.extend_from_slice(b"20 0 obj 6 endobj\n");
        let source = UnitSource::new(bytes.clone());
        let parsed = ParsedXref {
            facts: TrailerFacts::default(),
            live_entries: vec![
                LiveXrefEntry {
                    reference: IndirectReference {
                        object_number: 5,
                        generation: 0,
                    },
                    location: XrefLocation::Uncompressed(0),
                },
                LiveXrefEntry {
                    reference: IndirectReference {
                        object_number: 20,
                        generation: 0,
                    },
                    location: XrefLocation::Uncompressed(integer_offset),
                },
            ],
            declared_objects: BTreeSet::from([5, 20]),
            object_limit_exceeded: false,
        };
        let mut budget = ValidationBudget::new(0, 0);
        let stream = IndirectReference {
            object_number: 5,
            generation: 0,
        };

        let got =
            resolve_object_stream_length(&source, &mut budget, &parsed, &bytes, 0, stream, (0, 0))
                .await
                .expect("resolve");

        assert_eq!(
            got,
            Some((
                IndirectReference {
                    object_number: 20,
                    generation: 0,
                },
                6,
            ))
        );
    }

    #[test]
    fn text_reads_charge_shared_content_streams_per_use() {
        struct ActiveSignal;
        impl CancellationSignal for ActiveSignal {
            fn is_cancelled(&self) -> bool {
                false
            }
        }
        let mut document = Document::with_version("1.5");
        let content_id = document.add_object(Object::Stream(Stream::new(
            Dictionary::new(),
            vec![b' '; 64],
        )));
        let pages_id = document.new_object_id();
        let mut kids = Vec::new();
        let mut pages = Vec::new();
        for index in 0..4_u32 {
            let page_id = document.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "Contents" => content_id,
            });
            kids.push(Object::Reference(page_id));
            pages.push((index + 1, page_id));
        }
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => 4,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);

        let allowed =
            read_text_with_decoding_budget(&document, &pages, &ActiveSignal, 4 * 64).expect("read");
        assert!(matches!(allowed, ProcessorReadOutput::Text { .. }));

        let capped = read_text_with_decoding_budget(&document, &pages, &ActiveSignal, 4 * 64 - 1)
            .expect("read");
        assert!(matches!(
            capped,
            ProcessorReadOutput::ExpansionLimitExceeded { .. }
        ));
    }
}
