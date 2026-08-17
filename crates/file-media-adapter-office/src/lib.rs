//! Bounded Open XML interpretation inside the supervised file-media worker.

use std::{
    collections::HashSet,
    error::Error,
    io::{Cursor, Read},
    num::NonZeroU64,
    path::Component,
    str::FromStr,
};

use flate2::read::DeflateDecoder;
use quick_xml::{Reader, events::Event};
use signalbox_file_media_runtime::{
    CancellationSignal, CanonicalJsonObjectSchema, CanonicalMediaType, FileMediaProvider,
    FileMediaProviderDeclaration, FileMediaProviderFuture, FileMediaProviderReadRequest,
    FileMediaProviderValidationRequest, FileReaderName, FileReaderProviderName, FileReaderRevision,
    ProbeDeclaration, ProbeStrength, ProcessorFailure, ProcessorProbeOutput, ProcessorReadOutput,
    ProcessorValidationOutput, ReadAccessPattern, ReadViewBounds, ReadViewDeclaration,
    ReadViewName, ReaderDeclaration, ReaderDeclarationInput, ReaderIdentity, ReasonCode,
    StreamingTextFallback, ValidationEvidence, VerifiedBlobSource,
};
use zip::ZipArchive;

const PROVIDER_NAME: &str = "office-open-xml";
const READER_REVISION: &str = "zip-8-quick-xml-0-41-v1";
const DOCX_READER: &str = "docx";
const XLSX_READER: &str = "xlsx";
const PPTX_READER: &str = "pptx";
const DOCX_MEDIA_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const XLSX_MEDIA_TYPE: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const PPTX_MEDIA_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.presentationml.presentation";
const TEXT_VIEW: &str = "text";
const METADATA_VIEW: &str = "metadata";
const MALFORMED_REASON: &str = "malformed_office_container";
const ENTRY_COUNT_LIMIT: &str = "entry_count_limit";
const DECOMPRESSED_SIZE_LIMIT: &str = "decompressed_size_limit";
const HOSTILE_ENTRY_NAME: &str = "hostile_entry_name";
const RECURSIVE_CONTAINER: &str = "recursive_container";
const SYMLINK_ENTRY: &str = "symlink_entry";
const XML_MALFORMED: &str = "xml_malformed";
const ZIP_PREFIX_BYTES: u64 = 4;
const ZIP_SUFFIX_BYTES: u64 = 65_536;
const VALIDATION_SOURCE_BYTES: u64 = 262_144;
const CONTENT_TYPES_COMPRESSED_BYTES: u64 = 64 * 1024;
const LOCAL_HEADER_BYTES: u64 = 30;
const READ_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const SOURCE_CHUNK_BYTES: u64 = 256 * 1024;
const MAX_ENTRIES: usize = 10_000;
const MAX_ENTRY_BYTES: u64 = 4 * 1024 * 1024;
const MAX_TOTAL_EXPANDED_BYTES: u64 = 16 * 1024 * 1024;
const TEXT_OUTPUT_BYTES: usize = 768 * 1024;
const METADATA_OUTPUT_BYTES: usize = 16 * 1024;
const CONTENT_TYPES: &str = "[Content_Types].xml";
const DOCX_MAIN_CONTENT_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml";
const XLSX_MAIN_CONTENT_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml";
const PPTX_MAIN_CONTENT_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml";

/// Macro-free Office Open XML adapter registered in its dedicated worker.
#[derive(Clone, Copy, Debug, Default)]
pub struct OfficeProvider;

impl OfficeProvider {
    /// Constructs the stateless Office provider.
    pub const fn new() -> Self {
        Self
    }
}

impl FileMediaProvider for OfficeProvider {
    fn declaration(&self) -> FileMediaProviderDeclaration {
        declaration().unwrap_or_else(|error| {
            eprintln!("Office declaration failed: {error}");
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
            let kind = require_reader(reader)?;
            let inventory = match read_central_inventory(source, cancellation).await {
                Ok(inventory) => inventory,
                Err(CentralReadError::Validation { issue, kinds }) => {
                    if kinds.contains(&kind) {
                        return Ok(ProcessorProbeOutput::RecognizedMalformed {
                            media_type: String::from(kind.media_type()),
                            reason_code: String::from(issue.reason()),
                        });
                    }
                    return Ok(ProcessorProbeOutput::NoMatch);
                }
                Err(CentralReadError::Processor(error)) => return Err(error),
            };
            if inventory.kinds.contains(&kind) {
                Ok(ProcessorProbeOutput::Candidate {
                    media_type: String::from(kind.media_type()),
                    strength: ProbeStrength::StructuralCandidate,
                })
            } else {
                Ok(ProcessorProbeOutput::NoMatch)
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
            let kind = require_reader(reader)?;
            if request.media_type.as_str() != kind.media_type() {
                return Err(ProcessorFailure::Protocol);
            }
            let inventory = match read_central_inventory(source, cancellation).await {
                Ok(inventory) => inventory,
                Err(CentralReadError::Validation { issue, .. }) => {
                    return Ok(issue.validation(kind));
                }
                Err(CentralReadError::Processor(error)) => return Err(error),
            };
            if inventory.encrypted {
                return Ok(ProcessorValidationOutput::EncryptedOrLocked {
                    media_type: String::from(kind.media_type()),
                });
            }
            if !inventory.kinds.contains(&kind) {
                return Ok(malformed_validation(kind, MALFORMED_REASON));
            }
            validated_output(kind, request.evidence, &inventory)
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
            let kind = require_reader(reader)?;
            if request.detected_media_type.as_str() != kind.media_type() {
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
            let mut archive =
                ZipArchive::new(Cursor::new(bytes)).map_err(|_| ProcessorFailure::Failed)?;
            match validate_archive(&mut archive, kind) {
                Ok(()) => {}
                Err(ReadIssue::Expansion(reason)) => {
                    return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
                        limit_kind: String::from(reason),
                    });
                }
                Err(ReadIssue::Failed) => return Err(ProcessorFailure::Failed),
            }
            require_active(cancellation)?;
            match request.view.as_str() {
                TEXT_VIEW => read_text(&mut archive, kind, cancellation),
                METADATA_VIEW => read_metadata(&mut archive, kind),
                _ => Ok(ProcessorReadOutput::UnsupportedView),
            }
        })
    }
}

/// Returns the exact declaration shared by worker and daemon registration.
pub fn declaration() -> Result<FileMediaProviderDeclaration, Box<dyn Error>> {
    let provider = FileReaderProviderName::try_new(PROVIDER_NAME)?;
    let readers = [OfficeKind::Docx, OfficeKind::Xlsx, OfficeKind::Pptx]
        .into_iter()
        .map(|kind| reader_declaration(&provider, kind))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FileMediaProviderDeclaration::try_new(provider, readers)?)
}

fn reader_declaration(
    provider: &FileReaderProviderName,
    kind: OfficeKind,
) -> Result<ReaderDeclaration, Box<dyn Error>> {
    let text_view = ReadViewDeclaration::try_new(
        ReadViewName::try_new(TEXT_VIEW)?,
        String::from("Extracts embedded Open XML text without macros, OCR, or rendering."),
        CanonicalJsonObjectSchema::try_new(r#"{"additionalProperties":false,"type":"object"}"#)?,
        ReadAccessPattern::Streaming,
        ReadViewBounds::Text {
            source_bytes: READ_SOURCE_BYTES,
            output_bytes: TEXT_OUTPUT_BYTES,
        },
    )?;
    let metadata_view = ReadViewDeclaration::try_new(
        ReadViewName::try_new(METADATA_VIEW)?,
        String::from("Returns bounded Office Open XML container metadata."),
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
    Ok(ReaderDeclaration::try_new(ReaderDeclarationInput {
        provider: provider.clone(),
        reader: FileReaderName::try_new(kind.reader())?,
        revision: FileReaderRevision::try_new(READER_REVISION)?,
        media_types: vec![CanonicalMediaType::from_str(kind.media_type())?],
        probe: ProbeDeclaration::new(
            ZIP_PREFIX_BYTES,
            ZIP_SUFFIX_BYTES,
            5,
            VALIDATION_SOURCE_BYTES,
        ),
        views: vec![text_view, metadata_view],
        reason_codes: vec![
            ReasonCode::try_new(MALFORMED_REASON)?,
            ReasonCode::try_new(ENTRY_COUNT_LIMIT)?,
            ReasonCode::try_new(DECOMPRESSED_SIZE_LIMIT)?,
            ReasonCode::try_new(HOSTILE_ENTRY_NAME)?,
            ReasonCode::try_new(RECURSIVE_CONTAINER)?,
            ReasonCode::try_new(SYMLINK_ENTRY)?,
            ReasonCode::try_new(XML_MALFORMED)?,
        ],
        streaming_text_fallback: StreamingTextFallback::Disabled,
    })?)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OfficeKind {
    Docx,
    Xlsx,
    Pptx,
}

impl OfficeKind {
    const fn reader(self) -> &'static str {
        match self {
            Self::Docx => DOCX_READER,
            Self::Xlsx => XLSX_READER,
            Self::Pptx => PPTX_READER,
        }
    }

    const fn media_type(self) -> &'static str {
        match self {
            Self::Docx => DOCX_MEDIA_TYPE,
            Self::Xlsx => XLSX_MEDIA_TYPE,
            Self::Pptx => PPTX_MEDIA_TYPE,
        }
    }

    const fn marker(self) -> &'static str {
        match self {
            Self::Docx => "word/document.xml",
            Self::Xlsx => "xl/workbook.xml",
            Self::Pptx => "ppt/presentation.xml",
        }
    }

    const fn main_content_type(self) -> &'static str {
        match self {
            Self::Docx => DOCX_MAIN_CONTENT_TYPE,
            Self::Xlsx => XLSX_MAIN_CONTENT_TYPE,
            Self::Pptx => PPTX_MAIN_CONTENT_TYPE,
        }
    }
}

#[derive(Clone, Debug)]
struct CentralEntry {
    name: String,
    compression: u16,
    compressed_bytes: u64,
    expanded_bytes: u64,
    local_offset: u64,
}

#[derive(Debug)]
struct CentralInventory {
    entries: usize,
    expanded_bytes: u64,
    entries_by_name: Vec<CentralEntry>,
    kinds: Vec<OfficeKind>,
    encrypted: bool,
}

#[derive(Clone, Copy, Debug)]
enum ValidationIssue {
    Malformed(&'static str),
}

impl ValidationIssue {
    const fn reason(self) -> &'static str {
        match self {
            Self::Malformed(reason) => reason,
        }
    }

    fn validation(self, kind: OfficeKind) -> ProcessorValidationOutput {
        match self {
            Self::Malformed(reason) => malformed_validation(kind, reason),
        }
    }
}

#[derive(Debug)]
enum CentralReadError {
    Validation {
        issue: ValidationIssue,
        kinds: Vec<OfficeKind>,
    },
    Processor(ProcessorFailure),
}

impl From<ValidationIssue> for CentralReadError {
    fn from(issue: ValidationIssue) -> Self {
        Self::Validation {
            issue,
            kinds: Vec::new(),
        }
    }
}

impl From<ProcessorFailure> for CentralReadError {
    fn from(error: ProcessorFailure) -> Self {
        Self::Processor(error)
    }
}

#[derive(Clone, Copy, Debug)]
enum ReadIssue {
    Expansion(&'static str),
    Failed,
}

#[derive(Clone, Copy, Debug)]
enum XmlIssue {
    Malformed,
    OutputTooLarge,
}

async fn read_central_inventory(
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
) -> Result<CentralInventory, CentralReadError> {
    require_active(cancellation)?;
    let source_length = source.byte_length().get();
    let prefix_length = source_length.min(ZIP_PREFIX_BYTES);
    let prefix = read_range(source, 0, prefix_length).await?;
    if !prefix.starts_with(b"PK\x03\x04") {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON).into());
    }
    let suffix_length = source_length.min(ZIP_SUFFIX_BYTES);
    let suffix = read_range(source, source_length - suffix_length, suffix_length).await?;
    let eocd_relative = find_eocd(&suffix).ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    let eocd = &suffix[eocd_relative..];
    let eocd_length = 22_usize
        .checked_add(usize::from(le_u16(eocd, 20)?))
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    if eocd.len() != eocd_length {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON).into());
    }
    let entries = usize::from(le_u16(eocd, 10)?);
    if entries > MAX_ENTRIES {
        return Err(ValidationIssue::Malformed(ENTRY_COUNT_LIMIT).into());
    }
    if le_u16(eocd, 4)? != 0 || le_u16(eocd, 6)? != 0 || usize::from(le_u16(eocd, 8)?) != entries {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON).into());
    }
    let central_size = u64::from(le_u32(eocd, 12)?);
    let central_offset = u64::from(le_u32(eocd, 16)?);
    let central_end = central_offset
        .checked_add(central_size)
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    let eocd_absolute = (source_length - suffix_length)
        .checked_add(
            u64::try_from(eocd_relative)
                .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?,
        )
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    if central_end > eocd_absolute
        || central_size
            > VALIDATION_SOURCE_BYTES
                - ZIP_SUFFIX_BYTES
                - ZIP_PREFIX_BYTES
                - LOCAL_HEADER_BYTES
                - CONTENT_TYPES_COMPRESSED_BYTES
    {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON).into());
    }
    let central = read_range(source, central_offset, central_size).await?;
    require_active(cancellation)?;
    let mut inventory = parse_central_directory(&central, entries).map_err(|error| {
        CentralReadError::Validation {
            issue: error.issue,
            kinds: error.kinds,
        }
    })?;
    let recognized = marker_kinds(&inventory.entries_by_name);
    let content_types = read_content_types(source, cancellation, &inventory)
        .await
        .map_err(|issue| CentralReadError::Validation {
            issue,
            kinds: recognized.clone(),
        })?;
    inventory.kinds =
        validate_content_types(&content_types, &inventory.entries_by_name).map_err(|issue| {
            CentralReadError::Validation {
                issue,
                kinds: recognized,
            }
        })?;
    Ok(inventory)
}

#[derive(Debug)]
struct CentralParseError {
    issue: ValidationIssue,
    kinds: Vec<OfficeKind>,
}

fn parse_central_directory(
    bytes: &[u8],
    expected_entries: usize,
) -> Result<CentralInventory, CentralParseError> {
    let mut offset = 0_usize;
    let mut entries_by_name = Vec::with_capacity(expected_entries);
    let mut names = HashSet::with_capacity(expected_entries);
    let mut expanded_bytes = 0_u64;
    let mut decoded_bytes = 0_u64;
    let mut encrypted = false;
    while offset < bytes.len() {
        let parsed = parse_central_entry(bytes, offset).map_err(|issue| CentralParseError {
            issue,
            kinds: marker_kinds(&entries_by_name),
        })?;
        let entry = parsed.entry;
        let mut recognized = marker_kinds(&entries_by_name);
        if let Some(kind) = kind_for_marker(&entry.name)
            && !recognized.contains(&kind)
        {
            recognized.push(kind);
        }
        if !names.insert(entry.name.clone()) {
            return Err(CentralParseError {
                issue: ValidationIssue::Malformed(MALFORMED_REASON),
                kinds: recognized,
            });
        }
        validate_entry_name(&entry.name).map_err(|issue| CentralParseError {
            issue,
            kinds: recognized.clone(),
        })?;
        if ((parsed.external_attributes >> 16) & 0o170_000) == 0o120_000 {
            return Err(CentralParseError {
                issue: ValidationIssue::Malformed(SYMLINK_ENTRY),
                kinds: recognized,
            });
        }
        if recursive_container_name(&entry.name) {
            return Err(CentralParseError {
                issue: ValidationIssue::Malformed(RECURSIVE_CONTAINER),
                kinds: recognized,
            });
        }
        let flags = parsed.flags;
        encrypted |= flags & 1 != 0;
        expanded_bytes =
            expanded_bytes
                .checked_add(entry.expanded_bytes)
                .ok_or(CentralParseError {
                    issue: ValidationIssue::Malformed(DECOMPRESSED_SIZE_LIMIT),
                    kinds: recognized.clone(),
                })?;
        if decoded_entry_name(&entry.name) {
            if entry.expanded_bytes > MAX_ENTRY_BYTES {
                return Err(CentralParseError {
                    issue: ValidationIssue::Malformed(DECOMPRESSED_SIZE_LIMIT),
                    kinds: recognized,
                });
            }
            decoded_bytes =
                decoded_bytes
                    .checked_add(entry.expanded_bytes)
                    .ok_or(CentralParseError {
                        issue: ValidationIssue::Malformed(DECOMPRESSED_SIZE_LIMIT),
                        kinds: recognized.clone(),
                    })?;
            if decoded_bytes > MAX_TOTAL_EXPANDED_BYTES {
                return Err(CentralParseError {
                    issue: ValidationIssue::Malformed(DECOMPRESSED_SIZE_LIMIT),
                    kinds: recognized,
                });
            }
        }
        entries_by_name.push(entry);
        offset = parsed.next_offset;
    }
    if entries_by_name.len() != expected_entries {
        return Err(CentralParseError {
            issue: ValidationIssue::Malformed(MALFORMED_REASON),
            kinds: marker_kinds(&entries_by_name),
        });
    }
    Ok(CentralInventory {
        entries: entries_by_name.len(),
        expanded_bytes,
        entries_by_name,
        kinds: Vec::new(),
        encrypted,
    })
}

struct ParsedCentralEntry {
    entry: CentralEntry,
    flags: u16,
    external_attributes: u32,
    next_offset: usize,
}

fn parse_central_entry(bytes: &[u8], offset: usize) -> Result<ParsedCentralEntry, ValidationIssue> {
    let fixed = bytes
        .get(offset..offset.saturating_add(46))
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    if fixed.get(0..4) != Some(b"PK\x01\x02") {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
    }
    let name_length = usize::from(le_u16(fixed, 28)?);
    let extra_length = usize::from(le_u16(fixed, 30)?);
    let comment_length = usize::from(le_u16(fixed, 32)?);
    let record_length = 46_usize
        .checked_add(name_length)
        .and_then(|value| value.checked_add(extra_length))
        .and_then(|value| value.checked_add(comment_length))
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    let record = bytes
        .get(offset..offset.saturating_add(record_length))
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    let name = std::str::from_utf8(&record[46..46 + name_length])
        .map_err(|_| ValidationIssue::Malformed(HOSTILE_ENTRY_NAME))?;
    let next_offset = offset
        .checked_add(record_length)
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    Ok(ParsedCentralEntry {
        entry: CentralEntry {
            name: String::from(name),
            compression: le_u16(fixed, 10)?,
            compressed_bytes: u64::from(le_u32(fixed, 20)?),
            expanded_bytes: u64::from(le_u32(fixed, 24)?),
            local_offset: u64::from(le_u32(fixed, 42)?),
        },
        flags: le_u16(fixed, 8)?,
        external_attributes: le_u32(fixed, 38)?,
        next_offset,
    })
}

fn decoded_entry_name(name: &str) -> bool {
    name == CONTENT_TYPES || name.ends_with(".xml")
}

fn kind_for_marker(name: &str) -> Option<OfficeKind> {
    [OfficeKind::Docx, OfficeKind::Xlsx, OfficeKind::Pptx]
        .into_iter()
        .find(|kind| name == kind.marker())
}

fn marker_kinds(entries: &[CentralEntry]) -> Vec<OfficeKind> {
    [OfficeKind::Docx, OfficeKind::Xlsx, OfficeKind::Pptx]
        .into_iter()
        .filter(|kind| entries.iter().any(|entry| entry.name == kind.marker()))
        .collect()
}

async fn read_content_types(
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
    inventory: &CentralInventory,
) -> Result<Vec<u8>, ValidationIssue> {
    let entry = inventory
        .entries_by_name
        .iter()
        .find(|entry| entry.name == CONTENT_TYPES)
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    if entry.compressed_bytes == 0
        || entry.compressed_bytes > CONTENT_TYPES_COMPRESSED_BYTES
        || entry.expanded_bytes > MAX_ENTRY_BYTES
    {
        return Err(ValidationIssue::Malformed(DECOMPRESSED_SIZE_LIMIT));
    }
    require_active(cancellation).map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
    let local = read_range(source, entry.local_offset, LOCAL_HEADER_BYTES)
        .await
        .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
    if local.get(0..4) != Some(b"PK\x03\x04") || le_u16(&local, 8)? != entry.compression {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
    }
    let name_length = u64::from(le_u16(&local, 26)?);
    let extra_length = u64::from(le_u16(&local, 28)?);
    let data_offset = entry
        .local_offset
        .checked_add(LOCAL_HEADER_BYTES)
        .and_then(|value| value.checked_add(name_length))
        .and_then(|value| value.checked_add(extra_length))
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    let compressed = read_range(source, data_offset, entry.compressed_bytes)
        .await
        .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
    let bytes = match entry.compression {
        0 => compressed,
        8 => {
            let mut decoded = Vec::new();
            DeflateDecoder::new(Cursor::new(compressed))
                .take(MAX_ENTRY_BYTES + 1)
                .read_to_end(&mut decoded)
                .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
            decoded
        }
        _ => return Err(ValidationIssue::Malformed(MALFORMED_REASON)),
    };
    if u64::try_from(bytes.len()).ok() != Some(entry.expanded_bytes)
        || u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_ENTRY_BYTES
    {
        return Err(ValidationIssue::Malformed(DECOMPRESSED_SIZE_LIMIT));
    }
    Ok(bytes)
}

fn validate_content_types(
    bytes: &[u8],
    entries: &[CentralEntry],
) -> Result<Vec<OfficeKind>, ValidationIssue> {
    let mut reader = Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut depth = 0_usize;
    let mut saw_root = false;
    let mut kinds = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buffer)
            .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?
        {
            Event::Start(start) => {
                if depth == 0 {
                    if saw_root || local_name(start.name().as_ref()) != b"Types" {
                        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
                    }
                    saw_root = true;
                } else if depth == 1 && local_name(start.name().as_ref()) == b"Override" {
                    collect_content_type_kind(&reader, &start, entries, &mut kinds)?;
                }
                depth = depth
                    .checked_add(1)
                    .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
            }
            Event::Empty(empty)
                if depth == 1 && local_name(empty.name().as_ref()) == b"Override" =>
            {
                collect_content_type_kind(&reader, &empty, entries, &mut kinds)?;
            }
            Event::Empty(empty) if depth == 0 => {
                if saw_root || local_name(empty.name().as_ref()) != b"Types" {
                    return Err(ValidationIssue::Malformed(MALFORMED_REASON));
                }
                saw_root = true;
            }
            Event::End(_) => {
                depth = depth
                    .checked_sub(1)
                    .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
            }
            Event::DocType(_) | Event::GeneralRef(_) => {
                return Err(ValidationIssue::Malformed(MALFORMED_REASON));
            }
            Event::Eof if saw_root && depth == 0 => break,
            Event::Eof => return Err(ValidationIssue::Malformed(MALFORMED_REASON)),
            _ => {}
        }
        buffer.clear();
    }
    kinds.sort_by_key(|kind| kind.reader());
    kinds.dedup();
    if kinds.is_empty() {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
    }
    Ok(kinds)
}

fn collect_content_type_kind(
    reader: &Reader<Cursor<&[u8]>>,
    element: &quick_xml::events::BytesStart<'_>,
    entries: &[CentralEntry],
    kinds: &mut Vec<OfficeKind>,
) -> Result<(), ValidationIssue> {
    let mut part_name = None;
    let mut content_type = None;
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
        let value = attribute
            .decode_and_unescape_value(reader.decoder())
            .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
        match local_name(attribute.key.as_ref()) {
            b"PartName" => part_name = Some(value.into_owned()),
            b"ContentType" => content_type = Some(value.into_owned()),
            _ => {}
        }
    }
    let (Some(part_name), Some(content_type)) = (part_name, content_type) else {
        return Ok(());
    };
    for kind in [OfficeKind::Docx, OfficeKind::Xlsx, OfficeKind::Pptx] {
        let expected_part = format!("/{}", kind.marker());
        if part_name == expected_part
            && content_type == kind.main_content_type()
            && entries.iter().any(|entry| entry.name == kind.marker())
        {
            kinds.push(kind);
        }
    }
    Ok(())
}

fn validate_entry_name(name: &str) -> Result<(), ValidationIssue> {
    if name.is_empty() || name.contains('\\') || name.contains('\0') {
        return Err(ValidationIssue::Malformed(HOSTILE_ENTRY_NAME));
    }
    if std::path::Path::new(name).components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(ValidationIssue::Malformed(HOSTILE_ENTRY_NAME));
    }
    Ok(())
}

fn recursive_container_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".docx", ".xlsx", ".pptx", ".zip"]
        .into_iter()
        .any(|suffix| lower.ends_with(suffix))
}

fn validate_archive<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    kind: OfficeKind,
) -> Result<(), ReadIssue> {
    if archive.len() > MAX_ENTRIES {
        return Err(ReadIssue::Expansion(ENTRY_COUNT_LIMIT));
    }
    let mut decoded_total = 0_u64;
    let mut names = HashSet::with_capacity(archive.len());
    let mut has_content_types = false;
    let mut has_marker = false;
    for index in 0..archive.len() {
        let file = archive.by_index_raw(index).map_err(|_| ReadIssue::Failed)?;
        let name = file.name();
        validate_entry_name(name).map_err(|_| ReadIssue::Failed)?;
        if !names.insert(String::from(name)) {
            return Err(ReadIssue::Failed);
        }
        if recursive_container_name(name) || is_symlink(file.unix_mode()) || file.encrypted() {
            return Err(ReadIssue::Failed);
        }
        if decoded_entry_name(name) {
            if file.size() > MAX_ENTRY_BYTES {
                return Err(ReadIssue::Expansion(DECOMPRESSED_SIZE_LIMIT));
            }
            decoded_total = decoded_total
                .checked_add(file.size())
                .ok_or(ReadIssue::Expansion(DECOMPRESSED_SIZE_LIMIT))?;
            if decoded_total > MAX_TOTAL_EXPANDED_BYTES {
                return Err(ReadIssue::Expansion(DECOMPRESSED_SIZE_LIMIT));
            }
        }
        has_content_types |= name == CONTENT_TYPES;
        has_marker |= name == kind.marker();
    }
    if !has_content_types || !has_marker {
        return Err(ReadIssue::Failed);
    }
    Ok(())
}

fn read_text<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    kind: OfficeKind,
    cancellation: &dyn CancellationSignal,
) -> Result<ProcessorReadOutput, ProcessorFailure> {
    let mut names = match kind {
        OfficeKind::Docx => vec![String::from(kind.marker())],
        OfficeKind::Xlsx => selected_names(archive, "xl/worksheets/", "xl/sharedStrings.xml")?,
        OfficeKind::Pptx => selected_names(archive, "ppt/slides/slide", "")?,
    };
    names.sort();
    let mut output = String::new();
    for name in names {
        require_active(cancellation)?;
        let bytes = match read_entry(archive, &name) {
            Ok(bytes) => bytes,
            Err(ReadIssue::Expansion(reason)) => {
                return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
                    limit_kind: String::from(reason),
                });
            }
            Err(ReadIssue::Failed) => return Err(ProcessorFailure::Failed),
        };
        let part = match extract_xml_text(&bytes) {
            Ok(part) => part,
            Err(XmlIssue::OutputTooLarge) => return Ok(ProcessorReadOutput::OutputUnitTooLarge),
            Err(XmlIssue::Malformed) => return Err(ProcessorFailure::Failed),
        };
        if let Err(XmlIssue::OutputTooLarge) = append_bounded(&mut output, &part) {
            return Ok(ProcessorReadOutput::OutputUnitTooLarge);
        }
    }
    Ok(ProcessorReadOutput::Text {
        body: output,
        truncated: false,
        cursor: None,
    })
}

fn selected_names<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    prefix: &str,
    exact: &str,
) -> Result<Vec<String>, ProcessorFailure> {
    let mut names = Vec::new();
    for index in 0..archive.len() {
        let file = archive
            .by_index_raw(index)
            .map_err(|_| ProcessorFailure::Failed)?;
        let name = file.name();
        if (!exact.is_empty() && name == exact)
            || (name.starts_with(prefix) && name.ends_with(".xml"))
        {
            names.push(String::from(name));
        }
    }
    Ok(names)
}

fn read_entry<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>, ReadIssue> {
    let file = archive.by_name(name).map_err(|_| ReadIssue::Failed)?;
    let mut bytes = Vec::new();
    file.take(MAX_ENTRY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ReadIssue::Failed)?;
    if u64::try_from(bytes.len()).map_err(|_| ReadIssue::Failed)? > MAX_ENTRY_BYTES {
        return Err(ReadIssue::Expansion(DECOMPRESSED_SIZE_LIMIT));
    }
    Ok(bytes)
}

fn extract_xml_text(bytes: &[u8]) -> Result<String, XmlIssue> {
    let mut reader = Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut output = String::new();
    let mut element_depth = 0_usize;
    let mut text_depth = 0_usize;
    let mut saw_root = false;
    loop {
        match reader
            .read_event_into(&mut buffer)
            .map_err(|_| XmlIssue::Malformed)?
        {
            Event::Start(start) => {
                if element_depth == 0 {
                    if saw_root {
                        return Err(XmlIssue::Malformed);
                    }
                    saw_root = true;
                }
                element_depth = element_depth.checked_add(1).ok_or(XmlIssue::Malformed)?;
                if local_name(start.name().as_ref()) == b"t" {
                    text_depth = text_depth.checked_add(1).ok_or(XmlIssue::Malformed)?;
                }
            }
            Event::End(end) => {
                element_depth = element_depth.checked_sub(1).ok_or(XmlIssue::Malformed)?;
                let qualified_name = end.name();
                let name = local_name(qualified_name.as_ref());
                if name == b"t" {
                    text_depth = text_depth.checked_sub(1).ok_or(XmlIssue::Malformed)?;
                } else if name == b"p" && !output.ends_with('\n') {
                    append_xml_text(&mut output, "\n")?;
                }
            }
            Event::Empty(empty) => {
                if element_depth == 0 {
                    if saw_root {
                        return Err(XmlIssue::Malformed);
                    }
                    saw_root = true;
                }
                let qualified_name = empty.name();
                let name = local_name(qualified_name.as_ref());
                if name == b"tab" {
                    append_xml_text(&mut output, "\t")?;
                } else if name == b"br" {
                    append_xml_text(&mut output, "\n")?;
                }
            }
            Event::Text(text) if text_depth > 0 => {
                let decoded = text.xml10_content().map_err(|_| XmlIssue::Malformed)?;
                append_xml_text(&mut output, &decoded)?;
            }
            Event::GeneralRef(reference) if text_depth > 0 => {
                let decoded = reference.decode().map_err(|_| XmlIssue::Malformed)?;
                let value = match decoded.as_ref() {
                    "amp" => "&",
                    "lt" => "<",
                    "gt" => ">",
                    "apos" => "'",
                    "quot" => "\"",
                    _ => return Err(XmlIssue::Malformed),
                };
                append_xml_text(&mut output, value)?;
            }
            Event::DocType(_) | Event::GeneralRef(_) => return Err(XmlIssue::Malformed),
            Event::Eof if saw_root && element_depth == 0 && text_depth == 0 => break,
            Event::Eof => return Err(XmlIssue::Malformed),
            _ => {}
        }
        buffer.clear();
    }
    Ok(output)
}

fn append_xml_text(output: &mut String, value: &str) -> Result<(), XmlIssue> {
    let total = output
        .len()
        .checked_add(value.len())
        .ok_or(XmlIssue::OutputTooLarge)?;
    if total > TEXT_OUTPUT_BYTES {
        return Err(XmlIssue::OutputTooLarge);
    }
    output.push_str(value);
    Ok(())
}

fn append_bounded(output: &mut String, value: &str) -> Result<(), XmlIssue> {
    let total = output
        .len()
        .checked_add(value.len())
        .ok_or(XmlIssue::OutputTooLarge)?;
    if total > TEXT_OUTPUT_BYTES {
        return Err(XmlIssue::OutputTooLarge);
    }
    output.push_str(value);
    Ok(())
}

fn read_metadata<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    kind: OfficeKind,
) -> Result<ProcessorReadOutput, ProcessorFailure> {
    let mut expanded_bytes = 0_u64;
    for index in 0..archive.len() {
        let file = archive
            .by_index_raw(index)
            .map_err(|_| ProcessorFailure::Failed)?;
        expanded_bytes = expanded_bytes
            .checked_add(file.size())
            .ok_or(ProcessorFailure::Failed)?;
    }
    let body_json = serde_json::to_string(&serde_json::json!({
        "entries": archive.len(),
        "expanded_bytes": expanded_bytes,
        "format": kind.reader(),
    }))
    .map_err(|_| ProcessorFailure::Failed)?;
    Ok(ProcessorReadOutput::Structured {
        body_json,
        truncated: false,
        cursor: None,
    })
}

fn validated_output(
    kind: OfficeKind,
    evidence: ValidationEvidence,
    inventory: &CentralInventory,
) -> Result<ProcessorValidationOutput, ProcessorFailure> {
    let metadata_json = serde_json::to_string(&serde_json::json!({
        "entries": inventory.entries,
        "expanded_bytes": inventory.expanded_bytes,
        "format": kind.reader(),
    }))
    .map_err(|_| ProcessorFailure::Failed)?;
    Ok(ProcessorValidationOutput::Validated {
        media_type: String::from(kind.media_type()),
        evidence,
        metadata_json,
    })
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

fn require_reader(reader: &ReaderIdentity) -> Result<OfficeKind, ProcessorFailure> {
    if reader.provider().as_str() != PROVIDER_NAME || reader.revision().as_str() != READER_REVISION
    {
        return Err(ProcessorFailure::Protocol);
    }
    match reader.reader().as_str() {
        DOCX_READER => Ok(OfficeKind::Docx),
        XLSX_READER => Ok(OfficeKind::Xlsx),
        PPTX_READER => Ok(OfficeKind::Pptx),
        _ => Err(ProcessorFailure::Protocol),
    }
}

fn require_active(cancellation: &dyn CancellationSignal) -> Result<(), ProcessorFailure> {
    if cancellation.is_cancelled() {
        Err(ProcessorFailure::Cancelled)
    } else {
        Ok(())
    }
}

fn find_eocd(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).rposition(|window| window == b"PK\x05\x06")
}

fn le_u16(bytes: &[u8], offset: usize) -> Result<u16, ValidationIssue> {
    let value = bytes
        .get(offset..offset.saturating_add(2))
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn le_u32(bytes: &[u8], offset: usize) -> Result<u32, ValidationIssue> {
    let value = bytes
        .get(offset..offset.saturating_add(4))
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn is_symlink(mode: Option<u32>) -> bool {
    mode.is_some_and(|mode| mode & 0o170_000 == 0o120_000)
}

fn local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|byte| *byte == b':').next().unwrap_or(name)
}

fn empty_options(options: &serde_json::Value) -> bool {
    options.as_object().is_some_and(serde_json::Map::is_empty)
}

fn malformed_validation(kind: OfficeKind, reason: &str) -> ProcessorValidationOutput {
    ProcessorValidationOutput::Malformed {
        media_type: String::from(kind.media_type()),
        reason_code: String::from(reason),
    }
}
