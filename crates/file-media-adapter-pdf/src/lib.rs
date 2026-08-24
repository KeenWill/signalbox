//! Bounded PDF interpretation inside the supervised file-media worker.

use std::{collections::BTreeSet, error::Error, num::NonZeroU64, str::FromStr};

use lopdf::{Dictionary, Document, Error as LopdfError, Object, Stream};
use signalbox_file_media_runtime::{
    CancellationSignal, CanonicalJsonObjectSchema, CanonicalMediaType, FileMediaProvider,
    FileMediaProviderDeclaration, FileMediaProviderFailure, FileMediaProviderFuture,
    FileMediaProviderReadRequest, FileMediaProviderValidationRequest, FileReadInput,
    FileReaderName, FileReaderProviderName, FileReaderRevision, ProbeDeclaration, ProbeStrength,
    ProcessorProbeOutput, ProcessorReadOutput, ProcessorValidationOutput, ReadAccessPattern,
    ReadViewBounds, ReadViewDeclaration, ReadViewName, ReaderDeclaration, ReaderDeclarationInput,
    ReaderIdentity, ReasonCode, StreamingTextFallback, ValidationEvidence, VerifiedBlobSource,
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
const READ_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const SOURCE_CHUNK_BYTES: u64 = 256 * 1024;
// Hard safety ceilings bound worker output and decompression-amplified memory.
const TEXT_OUTPUT_BYTES: usize = signalbox_file_media_runtime::MAX_TEXT_BODY_BYTES;
const METADATA_OUTPUT_BYTES: usize = 16 * 1024;
const MAX_DECOMPRESSED_PAGE_BYTES: usize = 1024 * 1024;
// Hard safety ceilings bound object traversal and page-index construction latency.
const MAX_OBJECTS: usize = 10_000;
const MAX_PAGES: usize = 10_000;

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
    length: Option<u64>,
    prev: Option<u64>,
    filters: Vec<Vec<u8>>,
    is_xref_stream: bool,
}

#[derive(Default)]
struct ObjectStreamFacts {
    count: Option<u64>,
    filters: Vec<Vec<u8>>,
    first: Option<u64>,
    is_object_stream: bool,
    length: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

struct ParsedXref {
    facts: TrailerFacts,
    live_entries: Vec<LiveXrefEntry>,
    declared_objects: BTreeSet<u64>,
    object_limit_exceeded: bool,
}

enum Preflight {
    Ready,
    Encrypted,
    ObjectLimit,
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
            if source_length <= VALIDATION_SOURCE_BYTES {
                let bytes = read_range(source, 0, source_length).await?;
                require_active(cancellation)?;
                return inspect_complete(&bytes, request.evidence);
            }
            inspect_bounded(source, request.evidence, cancellation).await
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
        probe: ProbeDeclaration::new(
            PDF_HEADER_BYTES,
            PDF_TRAILER_BYTES,
            signalbox_file_media_runtime::MAX_PROBE_RANGES,
            VALIDATION_SOURCE_BYTES,
        ),
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
) -> Result<ProcessorValidationOutput, FileMediaProviderFailure> {
    let prefix = read_range(source, 0, PDF_HEADER_BYTES).await?;
    let source_length = source.byte_length().get();
    let suffix_length = source_length.min(PDF_TRAILER_BYTES);
    let suffix = read_range(source, source_length - suffix_length, suffix_length).await?;
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
        let remaining_budget = VALIDATION_SOURCE_BYTES
            .checked_sub(PDF_HEADER_BYTES)
            .and_then(|remaining| remaining.checked_sub(suffix_length))
            .ok_or(FileMediaProviderFailure::Failed)?;
        let missing_length = suffix_offset - xref_offset;
        let additional_length = missing_length.min(
            remaining_budget
                .checked_sub(ROOT_VALIDATION_BYTES)
                .ok_or(FileMediaProviderFailure::Failed)?,
        );
        let mut bytes = read_range(source, xref_offset, additional_length).await?;
        if additional_length == missing_length {
            bytes.extend_from_slice(&suffix);
        }
        bytes
    };
    require_active(cancellation)?;
    let Some(mut parsed_xref) = parse_xref_structure(&xref_bytes) else {
        return Ok(malformed_validation());
    };
    let mut remaining_budget = VALIDATION_SOURCE_BYTES
        .checked_sub(PDF_HEADER_BYTES + suffix_length)
        .ok_or(FileMediaProviderFailure::Failed)?;
    if xref_offset < suffix_offset {
        remaining_budget = remaining_budget
            .saturating_sub((xref_bytes.len() as u64).saturating_sub(suffix_length));
    }
    let mut visited = BTreeSet::from([xref_offset]);
    while let Some(prev) = parsed_xref.facts.prev {
        if !visited.insert(prev) || prev >= source_length || remaining_budget == 0 {
            return Ok(malformed_validation());
        }
        let length = remaining_budget
            .saturating_sub(ROOT_VALIDATION_BYTES)
            .min(source_length - prev);
        if length == 0 {
            return Ok(malformed_validation());
        }
        let previous_bytes = read_range(source, prev, length).await?;
        remaining_budget = remaining_budget.saturating_sub(length);
        let Some(previous) = parse_xref_structure(&previous_bytes) else {
            return Ok(malformed_validation());
        };
        merge_previous_xref(&mut parsed_xref, previous);
        require_active(cancellation)?;
    }
    if parsed_xref.facts.encrypted {
        return Ok(ProcessorValidationOutput::EncryptedOrLocked {
            media_type: String::from(MEDIA_TYPE),
        });
    }
    if parsed_xref.object_limit_exceeded || !valid_xref_targets(&parsed_xref, source_length) {
        return Ok(malformed_validation());
    }
    let Some((root, root_location)) = root_location(&parsed_xref) else {
        return Ok(malformed_validation());
    };
    match root_location {
        XrefLocation::Uncompressed(root_offset) => {
            let root_length = remaining_budget
                .min(ROOT_VALIDATION_BYTES)
                .min(source_length - root_offset);
            if root_length == 0 {
                return Ok(malformed_validation());
            }
            let root_bytes = read_range(source, root_offset, root_length).await?;
            if !object_is_catalog(&root_bytes, root) {
                return Ok(malformed_validation());
            }
        }
        XrefLocation::Compressed {
            stream_object,
            index,
        } => {
            let Some((stream_reference, stream_offset)) =
                object_stream_offset(&parsed_xref, stream_object)
            else {
                return Ok(malformed_validation());
            };
            let stream_length = remaining_budget.min(source_length - stream_offset);
            if stream_length == 0 {
                return Ok(malformed_validation());
            }
            let stream_bytes = read_range(source, stream_offset, stream_length).await?;
            let decoded_limit =
                usize::try_from(remaining_budget).map_err(|_| FileMediaProviderFailure::Failed)?;
            if !object_stream_contains_catalog(
                &stream_bytes,
                stream_reference,
                root,
                index,
                decoded_limit,
            ) {
                return Ok(malformed_validation());
            }
        }
    }
    validated_output(
        evidence,
        header_version(&prefix),
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
    let mut text = String::new();
    for (page_number, page_id) in pages {
        require_active(cancellation)?;
        validate_page_contents(document, *page_id)?;
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
    } else {
        Ok(Preflight::Ready)
    }
}

fn collect_pages(document: &Document) -> Result<Vec<(u32, lopdf::ObjectId)>, PageCollectionError> {
    let catalog = document
        .catalog()
        .map_err(|_| PageCollectionError::Malformed)?;
    let pages_root = catalog
        .get(b"Pages")
        .and_then(lopdf::Object::as_reference)
        .map_err(|_| PageCollectionError::Malformed)?;
    let mut pending = vec![pages_root];
    let mut visited = BTreeSet::new();
    let mut pages = Vec::new();
    while let Some(object_id) = pending.pop() {
        if !visited.insert(object_id) {
            return Err(PageCollectionError::Malformed);
        }
        let dictionary = document
            .get_dictionary(object_id)
            .map_err(|_| PageCollectionError::Malformed)?;
        match dictionary.get(b"Kids") {
            Ok(kids) => {
                let kids = kids
                    .as_array()
                    .map_err(|_| PageCollectionError::Malformed)?;
                if pending
                    .len()
                    .saturating_add(kids.len())
                    .saturating_add(pages.len())
                    > MAX_PAGES
                {
                    return Err(PageCollectionError::Limit);
                }
                for kid in kids.iter().rev() {
                    pending.push(
                        kid.as_reference()
                            .map_err(|_| PageCollectionError::Malformed)?,
                    );
                }
            }
            Err(LopdfError::DictKey(_)) => {
                if pages.len() == MAX_PAGES {
                    return Err(PageCollectionError::Limit);
                }
                let page_number =
                    u32::try_from(pages.len() + 1).map_err(|_| PageCollectionError::Limit)?;
                pages.push((page_number, object_id));
            }
            Err(_) => return Err(PageCollectionError::Malformed),
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
            document
                .get_object(*object_id)
                .map_err(|_| FileMediaProviderFailure::Failed)?;
        }
        lopdf::Object::Array(objects) => {
            for object in objects {
                let object_id = object
                    .as_reference()
                    .map_err(|_| FileMediaProviderFailure::Failed)?;
                document
                    .get_object(object_id)
                    .map_err(|_| FileMediaProviderFailure::Failed)?;
            }
        }
        lopdf::Object::Null | lopdf::Object::Stream(_) => {}
        _ => return Err(FileMediaProviderFailure::Failed),
    }
    Ok(())
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
    bytes
        .windows(b"%%EOF".len())
        .any(|window| window == b"%%EOF")
        && bytes
            .windows(b"startxref".len())
            .any(|window| window == b"startxref")
}

fn startxref_offset(bytes: &[u8]) -> Option<u64> {
    let marker = bytes
        .windows(b"startxref".len())
        .rposition(|window| window == b"startxref")?;
    let mut cursor = marker.checked_add(b"startxref".len())?;
    skip_pdf_whitespace(bytes, &mut cursor);
    parse_unsigned(bytes, &mut cursor)
}

fn parse_xref_structure(bytes: &[u8]) -> Option<ParsedXref> {
    let mut cursor = 0;
    skip_pdf_space_and_comments(bytes, &mut cursor);
    if consume_keyword(bytes, &mut cursor, b"xref") {
        parse_classic_xref(bytes, cursor)
    } else {
        parse_xref_stream(bytes, cursor)
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
            declared_objects.insert(object_number);
            skip_pdf_space_and_comments(bytes, &mut cursor);
            let offset = parse_unsigned(bytes, &mut cursor)?;
            skip_pdf_space_and_comments(bytes, &mut cursor);
            let generation = parse_unsigned(bytes, &mut cursor)?;
            skip_pdf_space_and_comments(bytes, &mut cursor);
            let state = *bytes.get(cursor)?;
            if state != b'n' && state != b'f' {
                return None;
            }
            cursor += 1;
            if state == b'n' {
                if live_entries.len() == MAX_OBJECTS {
                    object_limit_exceeded = true;
                } else if !object_limit_exceeded {
                    live_entries.push(LiveXrefEntry {
                        reference: IndirectReference {
                            object_number,
                            generation,
                        },
                        location: XrefLocation::Uncompressed(offset),
                    });
                }
            }
        }
    }
    let (facts, _) = parse_trailer_dictionary(bytes, cursor)?;
    let facts = valid_trailer_facts(facts, false)?;
    Some(ParsedXref {
        facts,
        live_entries,
        declared_objects,
        object_limit_exceeded,
    })
}

fn parse_xref_stream(bytes: &[u8], mut cursor: usize) -> Option<ParsedXref> {
    let xref_object = parse_unsigned(bytes, &mut cursor)?;
    skip_pdf_space_and_comments(bytes, &mut cursor);
    let xref_generation = parse_unsigned(bytes, &mut cursor)?;
    skip_pdf_space_and_comments(bytes, &mut cursor);
    if !consume_keyword(bytes, &mut cursor, b"obj") {
        return None;
    }
    skip_pdf_space_and_comments(bytes, &mut cursor);
    let (facts, mut cursor) = parse_trailer_dictionary(bytes, cursor)?;
    skip_pdf_space_and_comments(bytes, &mut cursor);
    if !consume_keyword(bytes, &mut cursor, b"stream") {
        return None;
    }
    let facts = valid_trailer_facts(facts, true)?;
    consume_stream_line_end(bytes, &mut cursor)?;
    let stream_length = usize::try_from(facts.length?).ok()?;
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
    let stream = decode_xref_stream(encoded_stream, &facts.filters, decoded_length)?;
    let (mut live_entries, mut declared_objects, object_limit_exceeded) =
        parse_xref_stream_entries(&stream, widths, &indexes)?;
    declared_objects.insert(xref_object);
    let xref_reference = IndirectReference {
        object_number: xref_object,
        generation: xref_generation,
    };
    if !object_limit_exceeded
        && live_entries
            .iter()
            .all(|entry| entry.reference != xref_reference)
    {
        live_entries.push(LiveXrefEntry {
            reference: xref_reference,
            location: XrefLocation::Uncompressed(0),
        });
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
            b"Encrypt" => facts.encrypted = true,
            b"Root" => {
                let mut value_cursor = value_start;
                facts.root = Some(parse_indirect_reference(bytes, &mut value_cursor)?);
            }
            b"Size" => {
                let mut value_cursor = value_start;
                facts.size = Some(parse_unsigned(bytes, &mut value_cursor)?);
            }
            b"W" => {
                let mut value_cursor = value_start;
                facts.widths = Some(parse_widths(bytes, &mut value_cursor)?);
            }
            b"Index" => {
                let mut value_cursor = value_start;
                facts.index = Some(parse_index(bytes, &mut value_cursor)?);
            }
            b"Length" => {
                let mut value_cursor = value_start;
                facts.length = Some(parse_unsigned(bytes, &mut value_cursor)?);
            }
            b"Prev" => {
                let mut value_cursor = value_start;
                facts.prev = Some(parse_unsigned(bytes, &mut value_cursor)?);
            }
            b"Filter" => {
                let mut value_cursor = value_start;
                facts.filters = parse_filter_names(bytes, &mut value_cursor)?;
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
        *cursor += 1;
    }
    *cursor += 1;
    Some(())
}

fn skip_scalar_or_reference(bytes: &[u8], cursor: &mut usize) -> Option<()> {
    *cursor = token_end(bytes, *cursor)?;
    let saved = *cursor;
    skip_pdf_space_and_comments(bytes, cursor);
    let Some(second) = token_end(bytes, *cursor) else {
        *cursor = saved;
        return Some(());
    };
    *cursor = second;
    skip_pdf_space_and_comments(bytes, cursor);
    if bytes.get(*cursor) == Some(&b'R') {
        *cursor += 1;
    } else {
        *cursor = saved;
    }
    Some(())
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
    let object_number = parse_unsigned(bytes, cursor)?;
    skip_pdf_space_and_comments(bytes, cursor);
    let generation = parse_unsigned(bytes, cursor)?;
    skip_pdf_space_and_comments(bytes, cursor);
    if bytes.get(*cursor) != Some(&b'R') {
        return None;
    }
    *cursor += 1;
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
    let first = parse_unsigned(bytes, cursor)?;
    skip_pdf_space_and_comments(bytes, cursor);
    let second = parse_unsigned(bytes, cursor)?;
    skip_pdf_space_and_comments(bytes, cursor);
    let third = parse_unsigned(bytes, cursor)?;
    skip_pdf_space_and_comments(bytes, cursor);
    if bytes.get(*cursor) != Some(&b']') {
        return None;
    }
    *cursor += 1;
    Some([first, second, third])
}

fn parse_filter_names(bytes: &[u8], cursor: &mut usize) -> Option<Vec<Vec<u8>>> {
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
            return (!filters.is_empty()).then_some(filters);
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

fn decode_xref_stream(encoded: &[u8], filters: &[Vec<u8>], limit: usize) -> Option<Vec<u8>> {
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
    let Some(root) = parsed.facts.root else {
        return false;
    };
    parsed
        .live_entries
        .iter()
        .any(|entry| entry.reference == root)
}

fn parse_xref_chain(bytes: &[u8], start: usize) -> Option<ParsedXref> {
    let mut visited = BTreeSet::new();
    let mut offset = start;
    let mut parsed = parse_xref_structure(bytes.get(offset..)?)?;
    visited.insert(offset);
    while let Some(previous_offset) = parsed.facts.prev {
        offset = usize::try_from(previous_offset).ok()?;
        if !visited.insert(offset) {
            return None;
        }
        let previous = parse_xref_structure(bytes.get(offset..)?)?;
        merge_previous_xref(&mut parsed, previous);
    }
    Some(parsed)
}

fn merge_previous_xref(current: &mut ParsedXref, previous: ParsedXref) {
    current.facts.encrypted |= previous.facts.encrypted;
    if current.facts.root.is_none() {
        current.facts.root = previous.facts.root;
    }
    current.facts.prev = previous.facts.prev;
    current.object_limit_exceeded |= previous.object_limit_exceeded;
    for entry in previous.live_entries {
        if !current
            .declared_objects
            .contains(&entry.reference.object_number)
        {
            if current.live_entries.len() == MAX_OBJECTS {
                current.object_limit_exceeded = true;
                break;
            }
            current.live_entries.push(entry);
        }
    }
    current.declared_objects.extend(previous.declared_objects);
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

fn object_stream_contains_catalog(
    bytes: &[u8],
    stream_reference: IndirectReference,
    catalog_reference: IndirectReference,
    catalog_index: u64,
    decoded_limit: usize,
) -> bool {
    let Some((facts, encoded)) = parse_object_stream(bytes, stream_reference) else {
        return false;
    };
    let Some(count) = facts.count.and_then(|value| usize::try_from(value).ok()) else {
        return false;
    };
    let Some(first) = facts.first.and_then(|value| usize::try_from(value).ok()) else {
        return false;
    };
    let Some(index) = usize::try_from(catalog_index).ok() else {
        return false;
    };
    if !facts.is_object_stream || index >= count {
        return false;
    }
    let Some(decoded) = decode_pdf_stream(encoded, &facts.filters, decoded_limit) else {
        return false;
    };
    let Some(header) = decoded.get(..first) else {
        return false;
    };
    let mut cursor = 0_usize;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        skip_pdf_space_and_comments(header, &mut cursor);
        let Some(object_number) = parse_unsigned(header, &mut cursor) else {
            return false;
        };
        skip_pdf_space_and_comments(header, &mut cursor);
        let Some(offset) = parse_unsigned(header, &mut cursor) else {
            return false;
        };
        entries.push((object_number, offset));
    }
    skip_pdf_space_and_comments(header, &mut cursor);
    if cursor != header.len() || entries[index].0 != catalog_reference.object_number {
        return false;
    }
    let Some(start) = usize::try_from(entries[index].1)
        .ok()
        .and_then(|offset| first.checked_add(offset))
    else {
        return false;
    };
    let end = if index + 1 < entries.len() {
        let Some(end) = usize::try_from(entries[index + 1].1)
            .ok()
            .and_then(|offset| first.checked_add(offset))
        else {
            return false;
        };
        end
    } else {
        decoded.len()
    };
    let Some(object) = decoded.get(start..end) else {
        return false;
    };
    let Some((catalog, mut cursor)) = parse_catalog_dictionary(object, 0) else {
        return false;
    };
    skip_pdf_space_and_comments(object, &mut cursor);
    catalog && cursor == object.len()
}

fn parse_object_stream(
    bytes: &[u8],
    expected: IndirectReference,
) -> Option<(ObjectStreamFacts, &[u8])> {
    let mut cursor = 0_usize;
    skip_pdf_space_and_comments(bytes, &mut cursor);
    if parse_unsigned(bytes, &mut cursor) != Some(expected.object_number) {
        return None;
    }
    skip_pdf_space_and_comments(bytes, &mut cursor);
    if parse_unsigned(bytes, &mut cursor) != Some(expected.generation) {
        return None;
    }
    skip_pdf_space_and_comments(bytes, &mut cursor);
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
            b"N" => facts.count = Some(parse_unsigned(bytes, &mut value_cursor)?),
            b"First" => facts.first = Some(parse_unsigned(bytes, &mut value_cursor)?),
            b"Length" => facts.length = Some(parse_unsigned(bytes, &mut value_cursor)?),
            b"Filter" => facts.filters = parse_filter_names(bytes, &mut value_cursor)?,
            _ => {}
        }
    }
    skip_pdf_space_and_comments(bytes, &mut cursor);
    if !consume_keyword(bytes, &mut cursor, b"stream") {
        return None;
    }
    consume_stream_line_end(bytes, &mut cursor)?;
    let length = usize::try_from(facts.length?).ok()?;
    let end = cursor.checked_add(length)?;
    Some((facts, bytes.get(cursor..end)?))
}

fn decode_pdf_stream(encoded: &[u8], filters: &[Vec<u8>], limit: usize) -> Option<Vec<u8>> {
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
    Stream::new(dictionary, encoded.to_vec())
        .decompressed_content_with_limit(limit)
        .ok()
}

fn object_is_catalog(bytes: &[u8], expected: IndirectReference) -> bool {
    let mut cursor = 0;
    skip_pdf_space_and_comments(bytes, &mut cursor);
    if parse_unsigned(bytes, &mut cursor) != Some(expected.object_number) {
        return false;
    }
    skip_pdf_space_and_comments(bytes, &mut cursor);
    if parse_unsigned(bytes, &mut cursor) != Some(expected.generation) {
        return false;
    }
    skip_pdf_space_and_comments(bytes, &mut cursor);
    if !consume_keyword(bytes, &mut cursor, b"obj") {
        return false;
    }
    skip_pdf_space_and_comments(bytes, &mut cursor);
    let Some((catalog, mut cursor)) = parse_catalog_dictionary(bytes, cursor) else {
        return false;
    };
    skip_pdf_space_and_comments(bytes, &mut cursor);
    catalog && consume_keyword(bytes, &mut cursor, b"endobj")
}

fn parse_catalog_dictionary(bytes: &[u8], mut cursor: usize) -> Option<(bool, usize)> {
    if !bytes.get(cursor..)?.starts_with(b"<<") {
        return None;
    }
    cursor += 2;
    let mut catalog = false;
    let mut pages = false;
    loop {
        skip_pdf_space_and_comments(bytes, &mut cursor);
        if bytes.get(cursor..)?.starts_with(b">>") {
            return Some((catalog && pages, cursor + 2));
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
            pages = parse_indirect_reference(bytes, &mut value_cursor).is_some();
        }
    }
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
        let first = parse_unsigned(bytes, cursor)?;
        skip_pdf_space_and_comments(bytes, cursor);
        let count = parse_unsigned(bytes, cursor)?;
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
            if entry_type == 1 || entry_type == 2 {
                if live_entries.len() == MAX_OBJECTS {
                    object_limit_exceeded = true;
                    continue;
                }
                let field_two = parse_big_endian(entry.get(type_end..field_two_end)?)?;
                let generation = parse_big_endian(entry.get(field_two_end..)?)?;
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
    if bytes
        .get(*cursor..)
        .is_some_and(|tail| tail.starts_with(keyword))
    {
        *cursor += keyword.len();
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
    fn declaration_fits_probe_and_text_ceilings() {
        let declaration = declaration().expect("valid declaration");
        let reader = &declaration.readers()[0];
        let text = reader
            .views()
            .iter()
            .find(|view| view.name().as_str() == TEXT_VIEW)
            .expect("text view");

        assert!(reader.probe().range_count() >= 2);
        assert!(reader.probe().range_count() <= FileMediaCeilings::version_one().probe_ranges);
        assert!(matches!(
            text.bounds(),
            ReadViewBounds::Text { output_bytes, .. }
                if output_bytes <= signalbox_file_media_runtime::MAX_TEXT_BODY_BYTES
        ));
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
    fn xref_stream_requires_live_entries_and_a_resolvable_root() {
        let stream = [
            0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 17, 1, 0, 0, 0, 0, 0, 0, 42,
        ];
        let mut bytes =
            b"2 0 obj\n<< /Type /XRef /Size 3 /Root 1 0 R /W [1 7 0] /Length 24 >>\nstream\n"
                .to_vec();
        bytes.extend_from_slice(&stream);
        bytes.extend_from_slice(b"\nendstream\nendobj");

        let parsed = parse_xref_structure(&bytes).expect("valid xref stream");

        assert!(valid_xref_targets(&parsed, 128));
        assert_eq!(root_offset(&parsed).map(|(_, offset)| offset), Some(17));
    }

    #[test]
    fn xref_stream_without_its_declared_body_is_rejected() {
        let bytes = b"2 0 obj\n<< /Type /XRef /Size 3 /Root 1 0 R /W [1 7 0] /Length 24 >>\nstream\nendstream\nendobj";

        assert!(parse_xref_structure(bytes).is_none());
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

        let parsed = parse_xref_structure(&bytes).expect("filtered xref stream");
        assert_eq!(parsed.declared_objects.len(), 16);
    }

    #[test]
    fn type_two_xref_entry_retains_object_stream_coordinates() {
        let parsed =
            parse_xref_stream_entries(&[2, 7, 3], [1, 1, 1], &[(11, 1)]).expect("type two entry");

        assert!(matches!(
            parsed.0[0].location,
            XrefLocation::Compressed {
                stream_object: 7,
                index: 3
            }
        ));
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
    fn previous_xref_supplies_the_root_and_encryption_state() {
        let mut bytes = b"%PDF-1.5\n1 0 obj\n<< /Type /Catalog >>\nendobj\n".to_vec();
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
        assert_eq!(parsed.facts.root.map(|root| root.object_number), Some(1));
        assert!(valid_xref_targets(&parsed, bytes.len() as u64));
    }

    #[test]
    fn sparse_object_numbers_count_only_live_entries() {
        let bytes = b"xref\n0 1\n0000000000 65535 f\n20000 1\n0000000017 00000 n\ntrailer\n<< /Size 20001 /Root 20000 0 R >>";

        let parsed = parse_xref_structure(bytes).expect("sparse xref");

        assert!(!parsed.object_limit_exceeded);
        assert_eq!(parsed.live_entries.len(), 1);
    }

    #[test]
    fn root_probe_requires_the_referenced_catalog_object() {
        let root = IndirectReference {
            object_number: 7,
            generation: 0,
        };

        assert!(object_is_catalog(
            b"7 0 obj\n<< /Type /Catalog /Pages 8 0 R >>\nendobj",
            root
        ));
        assert!(!object_is_catalog(
            b"7 0 obj\n<< /Type /Pages /Count 0 >>\nendobj",
            root
        ));
        assert!(!object_is_catalog(
            b"7 0 obj\n<< /Type /Catalog >>\nendobj",
            root
        ));
        assert!(!object_is_catalog(
            b"7 0 obj\n<< /Type /Catalog /Pages 8 0 R >>",
            root
        ));
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

        let stream = IndirectReference {
            object_number: 5,
            generation: 0,
        };
        let catalog = IndirectReference {
            object_number: 7,
            generation: 0,
        };
        assert!(object_stream_contains_catalog(
            &bytes, stream, catalog, 1, 4_096
        ));
        assert!(!object_stream_contains_catalog(
            &bytes, stream, catalog, 0, 4_096
        ));
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
}
