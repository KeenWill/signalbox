//! Bounded PDF interpretation inside the supervised file-media worker.

use std::{error::Error, num::NonZeroU64, str::FromStr};

use lopdf::{Document, Error as LopdfError};
use signalbox_file_media_runtime::{
    CancellationSignal, CanonicalJsonObjectSchema, CanonicalMediaType, FileMediaProvider,
    FileMediaProviderDeclaration, FileMediaProviderFuture, FileMediaProviderReadRequest,
    FileMediaProviderValidationRequest, FileReaderName, FileReaderProviderName, FileReaderRevision,
    ProbeDeclaration, ProbeStrength, ProcessorFailure, ProcessorProbeOutput, ProcessorReadOutput,
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
const READ_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const SOURCE_CHUNK_BYTES: u64 = 256 * 1024;
// Hard safety ceilings bound worker output and decompression-amplified memory.
const TEXT_OUTPUT_BYTES: usize = 768 * 1024;
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
    has_prev: bool,
    has_root: bool,
    has_size: bool,
    has_widths: bool,
    is_xref_stream: bool,
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
                return Err(ProcessorFailure::Protocol);
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
                return Err(ProcessorFailure::Protocol);
            }
            if !empty_options(&request.options) {
                return Ok(ProcessorReadOutput::InvalidViewArguments);
            }
            let source_length = source.byte_length().get();
            if source_length > READ_SOURCE_BYTES {
                return Ok(ProcessorReadOutput::SourceTooLarge {
                    maximum_bytes: READ_SOURCE_BYTES,
                });
            }
            let bytes = read_all(source, cancellation).await?;
            let document = Document::load_mem(&bytes).map_err(|_| ProcessorFailure::Failed)?;
            require_active(cancellation)?;
            if document.is_encrypted() {
                return Err(ProcessorFailure::Failed);
            }
            if let Err(limit_kind) = enforce_document_limits(&document) {
                return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
                    limit_kind: String::from(limit_kind),
                });
            }
            match request.view.as_str() {
                TEXT_VIEW => read_text(&document, cancellation),
                METADATA_VIEW => read_metadata(&document),
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
        ReadAccessPattern::Streaming,
        ReadViewBounds::Text {
            source_bytes: READ_SOURCE_BYTES,
            output_bytes: TEXT_OUTPUT_BYTES,
        },
    )?;
    let metadata_view = ReadViewDeclaration::try_new(
        ReadViewName::try_new(METADATA_VIEW)?,
        String::from("Returns bounded PDF version, page, and object counts."),
        CanonicalJsonObjectSchema::try_new(r#"{"additionalProperties":false,"type":"object"}"#)?,
        ReadAccessPattern::Streaming,
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
            1,
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
) -> Result<ProcessorValidationOutput, ProcessorFailure> {
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
    let xref_length = (source_length - xref_offset).min(VALIDATION_SOURCE_BYTES);
    let xref_bytes = read_range(source, xref_offset, xref_length).await?;
    require_active(cancellation)?;
    let Some(trailer) = parse_xref_structure(&xref_bytes) else {
        return Ok(malformed_validation());
    };
    if trailer.encrypted {
        return Ok(ProcessorValidationOutput::EncryptedOrLocked {
            media_type: String::from(MEDIA_TYPE),
        });
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
) -> Result<ProcessorValidationOutput, ProcessorFailure> {
    if !valid_header(bytes) || !has_pdf_trailer(bytes) {
        return Ok(malformed_validation());
    }
    let Some(xref_offset) = startxref_offset(bytes) else {
        return Ok(malformed_validation());
    };
    let Ok(xref_offset) = usize::try_from(xref_offset) else {
        return Ok(malformed_validation());
    };
    let Some(xref_bytes) = bytes.get(xref_offset..) else {
        return Ok(malformed_validation());
    };
    let Some(trailer) = parse_xref_structure(xref_bytes) else {
        return Ok(malformed_validation());
    };
    if trailer.encrypted {
        return Ok(ProcessorValidationOutput::EncryptedOrLocked {
            media_type: String::from(MEDIA_TYPE),
        });
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
    if document.objects.len() > MAX_OBJECTS || document.get_pages().len() > MAX_PAGES {
        return Ok(malformed_validation());
    }
    validated_output(
        evidence,
        effective_version(&document),
        Some(document.get_pages().len()),
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
) -> Result<ProcessorValidationOutput, ProcessorFailure> {
    let metadata_json = serde_json::to_string(&serde_json::json!({
        "bounded_validation": mode.is_bounded(),
        "objects": objects,
        "pages": pages,
        "version": version,
    }))
    .map_err(|_| ProcessorFailure::Failed)?;
    Ok(ProcessorValidationOutput::Validated {
        media_type: String::from(MEDIA_TYPE),
        evidence,
        metadata_json,
    })
}

fn read_text(
    document: &Document,
    cancellation: &dyn CancellationSignal,
) -> Result<ProcessorReadOutput, ProcessorFailure> {
    let pages = document.get_pages();
    let mut text = String::new();
    for (page_number, page_id) in pages {
        require_active(cancellation)?;
        validate_page_contents(document, page_id)?;
        let page_text =
            match document.extract_text_with_limit(&[page_number], MAX_DECOMPRESSED_PAGE_BYTES) {
                Ok(text) => text,
                Err(LopdfError::Decompress(_)) => {
                    return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
                        limit_kind: String::from(DECODED_CONTENT_LIMIT),
                    });
                }
                Err(_) => return Err(ProcessorFailure::Failed),
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

fn read_metadata(document: &Document) -> Result<ProcessorReadOutput, ProcessorFailure> {
    let body_json = serde_json::to_string(&serde_json::json!({
        "encrypted": false,
        "objects": document.objects.len(),
        "pages": document.get_pages().len(),
        "version": effective_version(document),
    }))
    .map_err(|_| ProcessorFailure::Failed)?;
    Ok(ProcessorReadOutput::Structured {
        body_json,
        truncated: false,
        cursor: None,
    })
}

fn enforce_document_limits(document: &Document) -> Result<(), &'static str> {
    if document.objects.len() > MAX_OBJECTS {
        return Err(OBJECT_COUNT_LIMIT);
    }
    if document.get_pages().len() > MAX_PAGES {
        return Err(PAGE_COUNT_LIMIT);
    }
    Ok(())
}

fn validate_page_contents(
    document: &Document,
    page_id: lopdf::ObjectId,
) -> Result<(), ProcessorFailure> {
    let page = document
        .get_dictionary(page_id)
        .map_err(|_| ProcessorFailure::Failed)?;
    let contents = match page.get(b"Contents") {
        Ok(contents) => contents,
        Err(LopdfError::DictKey(_)) => return Ok(()),
        Err(_) => return Err(ProcessorFailure::Failed),
    };
    match contents {
        lopdf::Object::Reference(object_id) => {
            document
                .get_object(*object_id)
                .map_err(|_| ProcessorFailure::Failed)?;
        }
        lopdf::Object::Array(objects) => {
            for object in objects {
                let object_id = object
                    .as_reference()
                    .map_err(|_| ProcessorFailure::Failed)?;
                document
                    .get_object(object_id)
                    .map_err(|_| ProcessorFailure::Failed)?;
            }
        }
        lopdf::Object::Stream(_) => {}
        _ => return Err(ProcessorFailure::Failed),
    }
    Ok(())
}

async fn read_all(
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
) -> Result<Vec<u8>, ProcessorFailure> {
    let source_length = source.byte_length().get();
    let capacity = usize::try_from(source_length).map_err(|_| ProcessorFailure::Failed)?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut offset = 0_u64;
    while offset < source_length {
        require_active(cancellation)?;
        let length = (source_length - offset).min(SOURCE_CHUNK_BYTES);
        bytes.extend(read_range(source, offset, length).await?);
        offset = offset.checked_add(length).ok_or(ProcessorFailure::Failed)?;
    }
    Ok(bytes)
}

async fn read_range(
    source: &dyn VerifiedBlobSource,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>, ProcessorFailure> {
    let length = NonZeroU64::new(length).ok_or(ProcessorFailure::Failed)?;
    source
        .read_range(offset, length)
        .await
        .map_err(|_| ProcessorFailure::Failed)
}

fn require_reader(reader: &ReaderIdentity) -> Result<(), ProcessorFailure> {
    if reader.provider().as_str() == PROVIDER_NAME
        && reader.reader().as_str() == READER_NAME
        && reader.revision().as_str() == READER_REVISION
    {
        Ok(())
    } else {
        Err(ProcessorFailure::Protocol)
    }
}

fn require_active(cancellation: &dyn CancellationSignal) -> Result<(), ProcessorFailure> {
    if cancellation.is_cancelled() {
        Err(ProcessorFailure::Cancelled)
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

fn parse_xref_structure(bytes: &[u8]) -> Option<TrailerFacts> {
    let mut cursor = 0;
    skip_pdf_space_and_comments(bytes, &mut cursor);
    if consume_keyword(bytes, &mut cursor, b"xref") {
        parse_classic_xref(bytes, cursor)
    } else {
        parse_xref_stream(bytes, cursor)
    }
}

fn parse_classic_xref(bytes: &[u8], mut cursor: usize) -> Option<TrailerFacts> {
    let mut entries = 0_u64;
    loop {
        skip_pdf_space_and_comments(bytes, &mut cursor);
        if consume_keyword(bytes, &mut cursor, b"trailer") {
            break;
        }
        parse_unsigned(bytes, &mut cursor)?;
        skip_pdf_whitespace(bytes, &mut cursor);
        let count = parse_unsigned(bytes, &mut cursor)?;
        entries = entries.checked_add(count)?;
        if entries > MAX_OBJECTS as u64 {
            return None;
        }
        for _ in 0..count {
            skip_pdf_space_and_comments(bytes, &mut cursor);
            parse_unsigned(bytes, &mut cursor)?;
            skip_pdf_whitespace(bytes, &mut cursor);
            parse_unsigned(bytes, &mut cursor)?;
            skip_pdf_whitespace(bytes, &mut cursor);
            let state = *bytes.get(cursor)?;
            if state != b'n' && state != b'f' {
                return None;
            }
            cursor += 1;
        }
    }
    let (facts, _) = parse_trailer_dictionary(bytes, cursor)?;
    valid_trailer_facts(facts, false)
}

fn parse_xref_stream(bytes: &[u8], mut cursor: usize) -> Option<TrailerFacts> {
    parse_unsigned(bytes, &mut cursor)?;
    skip_pdf_whitespace(bytes, &mut cursor);
    parse_unsigned(bytes, &mut cursor)?;
    skip_pdf_whitespace(bytes, &mut cursor);
    if !consume_keyword(bytes, &mut cursor, b"obj") {
        return None;
    }
    skip_pdf_space_and_comments(bytes, &mut cursor);
    let (facts, mut cursor) = parse_trailer_dictionary(bytes, cursor)?;
    skip_pdf_space_and_comments(bytes, &mut cursor);
    if !consume_keyword(bytes, &mut cursor, b"stream") {
        return None;
    }
    valid_trailer_facts(facts, true)
}

fn valid_trailer_facts(mut facts: TrailerFacts, stream: bool) -> Option<TrailerFacts> {
    if !facts.has_size || (!facts.has_root && !facts.has_prev) {
        return None;
    }
    if stream && (!facts.is_xref_stream || !facts.has_widths) {
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
        match key {
            b"Encrypt" => facts.encrypted = true,
            b"Prev" => facts.has_prev = true,
            b"Root" => facts.has_root = true,
            b"Size" => facts.has_size = true,
            b"W" => facts.has_widths = true,
            b"Type" => {
                let mut value_cursor = value_start;
                facts.is_xref_stream = parse_name(bytes, &mut value_cursor) == Some(b"XRef");
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
    skip_pdf_whitespace(bytes, cursor);
    let Some(second) = token_end(bytes, *cursor) else {
        *cursor = saved;
        return Some(());
    };
    *cursor = second;
    skip_pdf_whitespace(bytes, cursor);
    if bytes.get(*cursor) == Some(&b'R') {
        *cursor += 1;
    } else {
        *cursor = saved;
    }
    Some(())
}

fn parse_name<'a>(bytes: &'a [u8], cursor: &mut usize) -> Option<&'a [u8]> {
    if bytes.get(*cursor) != Some(&b'/') {
        return None;
    }
    *cursor += 1;
    let start = *cursor;
    while bytes
        .get(*cursor)
        .is_some_and(|byte| !is_pdf_delimiter(*byte))
    {
        *cursor += 1;
    }
    bytes.get(start..*cursor)
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
    while bytes.get(*cursor).is_some_and(u8::is_ascii_whitespace) {
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
    byte.is_ascii_whitespace()
        || matches!(
            byte,
            b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
        )
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
