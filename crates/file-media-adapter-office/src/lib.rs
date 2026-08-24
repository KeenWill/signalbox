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
use quick_xml::{Reader, XmlVersion, events::Event};
use signalbox_file_media_runtime::{
    CancellationSignal, CanonicalJsonObjectSchema, CanonicalMediaType, FileMediaProvider,
    FileMediaProviderDeclaration, FileMediaProviderFailure, FileMediaProviderFuture,
    FileMediaProviderReadRequest, FileMediaProviderValidationRequest, FileReadInput,
    FileReaderName, FileReaderProviderName, FileReaderRevision, MAX_OBSERVED_CONTAINER_ENTRIES,
    MAX_TEXT_BODY_BYTES, MAX_TEXT_OR_JSON_BYTES, ProbeDeclaration, ProbeStrength, ProcessorFailure,
    ProcessorProbeOutput, ProcessorReadOutput, ProcessorValidationOutput, ReadAccessPattern,
    ReadViewBounds, ReadViewDeclaration, ReadViewName, ReaderDeclaration, ReaderDeclarationInput,
    ReaderIdentity, ReasonCode, StreamingTextFallback, ValidationEvidence, VerifiedBlobSource,
};
use zip::{CompressionMethod, ZipArchive};

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
const EOCD_PRECEDING_BYTES: u64 = 21;
const VALIDATION_SOURCE_BYTES: u64 = 262_144;
const CONTENT_TYPES_COMPRESSED_BYTES: u64 = 64 * 1024;
const PACKAGE_RELS_COMPRESSED_BYTES: u64 = 8 * 1024;
const LOCAL_HEADER_BYTES: u64 = 30;
const CONTENT_TYPES_NAME_BYTES: u64 = 19;
const PACKAGE_RELS_NAME_BYTES: u64 = 11;
const READ_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const SOURCE_CHUNK_BYTES: u64 = 256 * 1024;
const MAX_ENTRY_BYTES: u64 = 4 * 1024 * 1024;
const MAX_TOTAL_EXPANDED_BYTES: u64 = 16 * 1024 * 1024;
const METADATA_OUTPUT_BYTES: usize = 16 * 1024;
const CONTENT_TYPES: &str = "[Content_Types].xml";
const PACKAGE_RELS: &str = "_rels/.rels";
const CONTENT_TYPES_NAMESPACE: &[u8] =
    b"http://schemas.openxmlformats.org/package/2006/content-types";
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
            let kind = require_reader(reader).map_err(|_| FileMediaProviderFailure::Failed)?;
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
                Err(CentralReadError::Processor(_)) => {
                    return Err(FileMediaProviderFailure::Failed);
                }
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
            let kind = require_reader(reader).map_err(|_| FileMediaProviderFailure::Failed)?;
            if request.media_type.as_str() != kind.media_type() {
                return Err(FileMediaProviderFailure::Failed);
            }
            let inventory = match read_central_inventory(source, cancellation).await {
                Ok(inventory) => inventory,
                Err(CentralReadError::Validation { issue, .. }) => {
                    return Ok(issue.validation(kind));
                }
                Err(CentralReadError::Processor(_)) => {
                    return Err(FileMediaProviderFailure::Failed);
                }
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
                .map_err(|_| FileMediaProviderFailure::Failed)
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
            let kind = require_reader(reader).map_err(|_| FileMediaProviderFailure::Failed)?;
            if request.detected_media_type.as_str() != kind.media_type() {
                return Err(FileMediaProviderFailure::Failed);
            }
            let FileReadInput::Initial { options } = &request.input else {
                return Ok(ProcessorReadOutput::InvalidViewArguments);
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
            let bytes = read_all(source, cancellation)
                .await
                .map_err(|_| FileMediaProviderFailure::Failed)?;
            let mut archive = ZipArchive::new(Cursor::new(bytes))
                .map_err(|_| FileMediaProviderFailure::Failed)?;
            match validate_archive(&mut archive, kind) {
                Ok(()) => {}
                Err(ReadIssue::Expansion(reason)) => {
                    return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
                        limit_kind: String::from(reason),
                    });
                }
                Err(ReadIssue::Failed) => return Err(FileMediaProviderFailure::Failed),
            }
            require_active(cancellation).map_err(|_| FileMediaProviderFailure::Failed)?;
            match request.view.as_str() {
                TEXT_VIEW => read_text(&mut archive, kind, cancellation)
                    .map_err(|_| FileMediaProviderFailure::Failed),
                METADATA_VIEW => {
                    read_metadata(&mut archive, kind).map_err(|_| FileMediaProviderFailure::Failed)
                }
                _ => Ok(ProcessorReadOutput::UnsupportedView),
            }
        })
    }
}

/// Returns the exact declaration shared by worker and daemon registration.
pub fn declaration() -> Result<FileMediaProviderDeclaration, Box<dyn Error>> {
    let provider = FileReaderProviderName::try_new(PROVIDER_NAME)?;
    let readers = [OfficeKind::Docx, OfficeKind::Pptx, OfficeKind::Xlsx]
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
        ReadAccessPattern::Streaming {
            maximum_ranges: u32::try_from(READ_SOURCE_BYTES.div_ceil(SOURCE_CHUNK_BYTES))?,
        },
        ReadViewBounds::Text {
            source_bytes: READ_SOURCE_BYTES,
            output_bytes: MAX_TEXT_BODY_BYTES,
        },
    )?;
    let metadata_view = ReadViewDeclaration::try_new(
        ReadViewName::try_new(METADATA_VIEW)?,
        String::from("Returns bounded Office Open XML container metadata."),
        CanonicalJsonObjectSchema::try_new(r#"{"additionalProperties":false,"type":"object"}"#)?,
        ReadAccessPattern::Streaming {
            maximum_ranges: u32::try_from(READ_SOURCE_BYTES.div_ceil(SOURCE_CHUNK_BYTES))?,
        },
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
            10,
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
    flags: u16,
    compression: u16,
    crc32: u32,
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
    let suffix_offset = source_length - suffix_length;
    let preceding_length = suffix_offset.min(EOCD_PRECEDING_BYTES);
    let suffix_start = suffix_offset - preceding_length;
    let mut suffix = if preceding_length == 0 {
        Vec::new()
    } else {
        read_range(source, suffix_start, preceding_length).await?
    };
    suffix.extend(read_range(source, suffix_offset, suffix_length).await?);
    let eocd_relative = find_eocd(&suffix).ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    let eocd = &suffix[eocd_relative..];
    let eocd_length = 22_usize
        .checked_add(usize::from(le_u16(eocd, 20)?))
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    if eocd.len() != eocd_length {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON).into());
    }
    let entries = usize::from(le_u16(eocd, 10)?);
    if entries > usize::try_from(MAX_OBSERVED_CONTAINER_ENTRIES).unwrap_or(usize::MAX) {
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
    let eocd_absolute = suffix_start
        .checked_add(
            u64::try_from(eocd_relative)
                .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?,
        )
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    if central_end > eocd_absolute
        || central_size
            > VALIDATION_SOURCE_BYTES
                - ZIP_SUFFIX_BYTES
                - EOCD_PRECEDING_BYTES
                - ZIP_PREFIX_BYTES
                - LOCAL_HEADER_BYTES
                - CONTENT_TYPES_NAME_BYTES
                - CONTENT_TYPES_COMPRESSED_BYTES
                - LOCAL_HEADER_BYTES
                - PACKAGE_RELS_NAME_BYTES
                - PACKAGE_RELS_COMPRESSED_BYTES
    {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON).into());
    }
    if entries == 0 || central_size == 0 {
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
    if inventory.encrypted {
        inventory.kinds = recognized;
        return Ok(inventory);
    }
    let content_types = match read_probe_entry(
        source,
        cancellation,
        &inventory,
        CONTENT_TYPES,
        CONTENT_TYPES_COMPRESSED_BYTES,
    )
    .await
    {
        Ok(content_types) => content_types,
        Err(CentralReadError::Validation { issue, .. }) => {
            return Err(CentralReadError::Validation {
                issue,
                kinds: recognized,
            });
        }
        Err(CentralReadError::Processor(error)) => {
            return Err(CentralReadError::Processor(error));
        }
    };
    let content_type_kinds = validate_content_types(&content_types, &inventory.entries_by_name)
        .map_err(|issue| CentralReadError::Validation {
            issue,
            kinds: recognized.clone(),
        })?;
    let package_relationships = read_probe_entry(
        source,
        cancellation,
        &inventory,
        PACKAGE_RELS,
        PACKAGE_RELS_COMPRESSED_BYTES,
    )
    .await?;
    inventory.kinds = validate_package_relationships(&package_relationships, &content_type_kinds)
        .map_err(|issue| CentralReadError::Validation {
        issue,
        kinds: recognized,
    })?;
    validate_selected_probe_entries(&inventory)?;
    Ok(inventory)
}

fn validate_selected_probe_entries(inventory: &CentralInventory) -> Result<(), CentralReadError> {
    for kind in &inventory.kinds {
        let entry = inventory
            .entries_by_name
            .iter()
            .find(|entry| entry.name == kind.marker())
            .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
        if !matches!(entry.compression, 0 | 8) {
            return Err(ValidationIssue::Malformed(MALFORMED_REASON).into());
        }
        if entry.expanded_bytes > MAX_ENTRY_BYTES {
            return Err(ValidationIssue::Malformed(DECOMPRESSED_SIZE_LIMIT).into());
        }
    }
    Ok(())
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
        let entry_encrypted = flags & 1 != 0;
        encrypted |= entry_encrypted;
        expanded_bytes =
            expanded_bytes
                .checked_add(entry.expanded_bytes)
                .ok_or(CentralParseError {
                    issue: ValidationIssue::Malformed(DECOMPRESSED_SIZE_LIMIT),
                    kinds: recognized.clone(),
                })?;
        if decoded_entry_name(&entry.name) && !entry_encrypted {
            if !matches!(entry.compression, 0 | 8) {
                return Err(CentralParseError {
                    issue: ValidationIssue::Malformed(MALFORMED_REASON),
                    kinds: recognized,
                });
            }
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
    if entries_by_name
        .iter()
        .any(|entry| macro_project_name(&entry.name))
    {
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
    let extra_start = 46_usize
        .checked_add(name_length)
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    let extra_end = extra_start
        .checked_add(extra_length)
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    let extra = record
        .get(extra_start..extra_end)
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    let compressed_32 = le_u32(fixed, 20)?;
    let expanded_32 = le_u32(fixed, 24)?;
    let local_offset_32 = le_u32(fixed, 42)?;
    let (expanded_bytes, compressed_bytes, local_offset) =
        decode_zip64_entry_fields(extra, expanded_32, compressed_32, local_offset_32)?;
    let next_offset = offset
        .checked_add(record_length)
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    Ok(ParsedCentralEntry {
        entry: CentralEntry {
            name: String::from(name),
            flags: le_u16(fixed, 8)?,
            compression: le_u16(fixed, 10)?,
            crc32: le_u32(fixed, 16)?,
            compressed_bytes,
            expanded_bytes,
            local_offset,
        },
        flags: le_u16(fixed, 8)?,
        external_attributes: le_u32(fixed, 38)?,
        next_offset,
    })
}

fn decode_zip64_entry_fields(
    extra: &[u8],
    expanded_32: u32,
    compressed_32: u32,
    local_offset_32: u32,
) -> Result<(u64, u64, u64), ValidationIssue> {
    let needs_zip64 =
        expanded_32 == u32::MAX || compressed_32 == u32::MAX || local_offset_32 == u32::MAX;
    if !needs_zip64 {
        return Ok((
            u64::from(expanded_32),
            u64::from(compressed_32),
            u64::from(local_offset_32),
        ));
    }

    let mut offset = 0_usize;
    while offset < extra.len() {
        let header = extra
            .get(offset..offset.saturating_add(4))
            .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
        let tag = u16::from_le_bytes([header[0], header[1]]);
        let size = usize::from(u16::from_le_bytes([header[2], header[3]]));
        let body_start = offset
            .checked_add(4)
            .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
        let body_end = body_start
            .checked_add(size)
            .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
        let body = extra
            .get(body_start..body_end)
            .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
        if tag == 1 {
            let mut zip64_offset = 0_usize;
            let expanded = zip64_or_32(body, &mut zip64_offset, expanded_32)?;
            let compressed = zip64_or_32(body, &mut zip64_offset, compressed_32)?;
            let local_offset = zip64_or_32(body, &mut zip64_offset, local_offset_32)?;
            return Ok((expanded, compressed, local_offset));
        }
        offset = body_end;
    }

    Err(ValidationIssue::Malformed(MALFORMED_REASON))
}

fn zip64_or_32(body: &[u8], offset: &mut usize, value: u32) -> Result<u64, ValidationIssue> {
    if value != u32::MAX {
        return Ok(u64::from(value));
    }
    let end = (*offset)
        .checked_add(8)
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    let bytes = body
        .get(*offset..end)
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    *offset = end;
    Ok(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

fn decoded_entry_name(name: &str) -> bool {
    matches!(name, CONTENT_TYPES | PACKAGE_RELS)
}

fn macro_project_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "word/vbaproject.bin" | "xl/vbaproject.bin" | "ppt/vbaproject.bin"
    )
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

async fn read_probe_entry(
    source: &dyn VerifiedBlobSource,
    cancellation: &dyn CancellationSignal,
    inventory: &CentralInventory,
    name: &str,
    maximum_compressed_bytes: u64,
) -> Result<Vec<u8>, CentralReadError> {
    let entry = inventory
        .entries_by_name
        .iter()
        .find(|entry| entry.name == name)
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    if entry.compressed_bytes == 0
        || entry.compressed_bytes > maximum_compressed_bytes
        || entry.expanded_bytes > MAX_ENTRY_BYTES
    {
        return Err(ValidationIssue::Malformed(DECOMPRESSED_SIZE_LIMIT).into());
    }
    require_active(cancellation)?;
    require_range(
        source.byte_length().get(),
        entry.local_offset,
        LOCAL_HEADER_BYTES,
    )?;
    let local = read_range(source, entry.local_offset, LOCAL_HEADER_BYTES).await?;
    if local.get(0..4) != Some(b"PK\x03\x04")
        || le_u16(&local, 6)? != entry.flags
        || le_u16(&local, 8)? != entry.compression
    {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON).into());
    }
    let name_length = u64::from(le_u16(&local, 26)?);
    let extra_length = u64::from(le_u16(&local, 28)?);
    let local_name_offset = entry
        .local_offset
        .checked_add(LOCAL_HEADER_BYTES)
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    require_range(source.byte_length().get(), local_name_offset, name_length)?;
    let local_name = read_range(source, local_name_offset, name_length).await?;
    if local_name.as_slice() != entry.name.as_bytes() {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON).into());
    }
    let data_offset = entry
        .local_offset
        .checked_add(LOCAL_HEADER_BYTES)
        .and_then(|value| value.checked_add(name_length))
        .and_then(|value| value.checked_add(extra_length))
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    require_range(
        source.byte_length().get(),
        data_offset,
        entry.compressed_bytes,
    )?;
    let compressed = read_range(source, data_offset, entry.compressed_bytes).await?;
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
        _ => return Err(ValidationIssue::Malformed(MALFORMED_REASON).into()),
    };
    if u64::try_from(bytes.len()).ok() != Some(entry.expanded_bytes)
        || u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_ENTRY_BYTES
    {
        return Err(ValidationIssue::Malformed(DECOMPRESSED_SIZE_LIMIT).into());
    }
    if crc32fast::hash(&bytes) != entry.crc32 {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON).into());
    }
    Ok(bytes)
}

fn validate_package_relationships(
    bytes: &[u8],
    content_type_kinds: &[OfficeKind],
) -> Result<Vec<OfficeKind>, ValidationIssue> {
    let mut reader = Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut targets = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buffer)
            .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?
        {
            Event::Start(element) | Event::Empty(element)
                if local_name(element.name().as_ref()) == b"Relationship" =>
            {
                let mut relationship_type = None;
                let mut target = None;
                let mut external = false;
                for attribute in element.attributes() {
                    let attribute =
                        attribute.map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
                    let value = attribute
                        .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                        .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
                    match attribute.key.as_ref() {
                        b"Type" => relationship_type = Some(value.into_owned()),
                        b"Target" => target = Some(value.into_owned()),
                        b"TargetMode" => external = value.eq_ignore_ascii_case("external"),
                        _ => {}
                    }
                }
                if relationship_type
                    .as_deref()
                    .is_some_and(|value| value.ends_with("/officeDocument"))
                {
                    if external {
                        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
                    }
                    targets.push(target.ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?);
                }
            }
            Event::DocType(_) | Event::GeneralRef(_) => {
                return Err(ValidationIssue::Malformed(MALFORMED_REASON));
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    if targets.len() != 1 {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
    }
    let target = targets
        .pop()
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    let target = target.strip_prefix('/').unwrap_or(&target);
    let kinds = content_type_kinds
        .iter()
        .copied()
        .filter(|kind| target == kind.marker())
        .collect::<Vec<_>>();
    if kinds.len() != 1 {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
    }
    Ok(kinds)
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
    let mut content_types_prefix = None;
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
                    content_types_prefix = Some(content_types_namespace_prefix(&reader, &start)?);
                    saw_root = true;
                } else if depth == 1
                    && local_name(start.name().as_ref()) == b"Override"
                    && content_types_prefix.as_deref().is_some_and(|root_prefix| {
                        element_uses_content_types_namespace(&reader, &start, root_prefix)
                    })
                {
                    collect_content_type_kind(&reader, &start, entries, &mut kinds)?;
                }
                depth = depth
                    .checked_add(1)
                    .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
            }
            Event::Empty(empty)
                if depth == 1
                    && local_name(empty.name().as_ref()) == b"Override"
                    && content_types_prefix.as_deref().is_some_and(|root_prefix| {
                        element_uses_content_types_namespace(&reader, &empty, root_prefix)
                    }) =>
            {
                collect_content_type_kind(&reader, &empty, entries, &mut kinds)?;
            }
            Event::Empty(empty) if depth == 0 => {
                if saw_root || local_name(empty.name().as_ref()) != b"Types" {
                    return Err(ValidationIssue::Malformed(MALFORMED_REASON));
                }
                content_types_prefix = Some(content_types_namespace_prefix(&reader, &empty)?);
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

fn content_types_namespace_prefix(
    reader: &Reader<Cursor<&[u8]>>,
    element: &quick_xml::events::BytesStart<'_>,
) -> Result<Vec<u8>, ValidationIssue> {
    let qualified_name = element.name();
    let name = qualified_name.as_ref();
    let prefix = name
        .iter()
        .position(|byte| *byte == b':')
        .map_or(b"".as_slice(), |separator| &name[..separator]);
    if namespace_declaration(reader, element, prefix).as_deref() == Some(CONTENT_TYPES_NAMESPACE) {
        Ok(prefix.to_vec())
    } else {
        Err(ValidationIssue::Malformed(MALFORMED_REASON))
    }
}

fn element_uses_content_types_namespace(
    reader: &Reader<Cursor<&[u8]>>,
    element: &quick_xml::events::BytesStart<'_>,
    root_prefix: &[u8],
) -> bool {
    let qualified_name = element.name();
    let name = qualified_name.as_ref();
    let prefix = name
        .iter()
        .position(|byte| *byte == b':')
        .map_or(b"".as_slice(), |separator| &name[..separator]);
    namespace_declaration(reader, element, prefix).map_or(prefix == root_prefix, |namespace| {
        namespace.as_slice() == CONTENT_TYPES_NAMESPACE
    })
}

fn namespace_declaration(
    reader: &Reader<Cursor<&[u8]>>,
    element: &quick_xml::events::BytesStart<'_>,
    prefix: &[u8],
) -> Option<Vec<u8>> {
    element
        .attributes()
        .filter_map(Result::ok)
        .find_map(|attribute| {
            let key = attribute.key.as_ref();
            let declared_prefix = if key == b"xmlns" {
                b"".as_slice()
            } else {
                key.strip_prefix(b"xmlns:")?
            };
            if declared_prefix != prefix {
                return None;
            }
            attribute
                .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                .ok()
                .map(|value| value.as_bytes().to_vec())
        })
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
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
            .map_err(|_| ValidationIssue::Malformed(MALFORMED_REASON))?;
        match attribute.key.as_ref() {
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

fn require_range(source_length: u64, offset: u64, length: u64) -> Result<(), ValidationIssue> {
    let end = offset
        .checked_add(length)
        .ok_or(ValidationIssue::Malformed(MALFORMED_REASON))?;
    if length == 0 || end > source_length {
        return Err(ValidationIssue::Malformed(MALFORMED_REASON));
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
    if archive.len() > usize::try_from(MAX_OBSERVED_CONTAINER_ENTRIES).unwrap_or(usize::MAX) {
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
            if !matches!(
                file.compression(),
                CompressionMethod::Stored | CompressionMethod::Deflated
            ) {
                return Err(ReadIssue::Failed);
            }
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
        if macro_project_name(name) {
            return Err(ReadIssue::Failed);
        }
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
    let names = match kind {
        OfficeKind::Docx => vec![String::from(kind.marker())],
        OfficeKind::Xlsx => return read_spreadsheet_text(archive, cancellation),
        OfficeKind::Pptx => presentation_slide_names(archive)?,
    };
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
        let part = match extract_xml_text(&bytes, kind) {
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

fn read_spreadsheet_text<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    cancellation: &dyn CancellationSignal,
) -> Result<ProcessorReadOutput, ProcessorFailure> {
    let workbook = read_entry(archive, "xl/workbook.xml").map_err(|_| ProcessorFailure::Failed)?;
    let relationships =
        read_entry(archive, "xl/_rels/workbook.xml.rels").map_err(|_| ProcessorFailure::Failed)?;
    let relationship_ids =
        workbook_relationship_ids(&workbook).map_err(|_| ProcessorFailure::Failed)?;
    let targets =
        workbook_relationship_targets(&relationships).map_err(|_| ProcessorFailure::Failed)?;
    let shared_strings = match read_entry(archive, "xl/sharedStrings.xml") {
        Ok(bytes) => spreadsheet_shared_strings(&bytes).map_err(|_| ProcessorFailure::Failed)?,
        Err(ReadIssue::Failed) => Vec::new(),
        Err(ReadIssue::Expansion(reason)) => {
            return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
                limit_kind: String::from(reason),
            });
        }
    };
    let mut output = String::new();
    for relationship_id in relationship_ids {
        require_active(cancellation)?;
        let target = targets
            .iter()
            .find_map(|(candidate, target)| (candidate == &relationship_id).then_some(target))
            .ok_or(ProcessorFailure::Failed)?;
        let worksheet = match read_entry(archive, target) {
            Ok(bytes) => bytes,
            Err(ReadIssue::Expansion(reason)) => {
                return Ok(ProcessorReadOutput::ExpansionLimitExceeded {
                    limit_kind: String::from(reason),
                });
            }
            Err(ReadIssue::Failed) => return Err(ProcessorFailure::Failed),
        };
        let text = spreadsheet_worksheet_text(&worksheet, &shared_strings)
            .map_err(|_| ProcessorFailure::Failed)?;
        if append_bounded(&mut output, &text).is_err() {
            return Ok(ProcessorReadOutput::OutputUnitTooLarge);
        }
    }
    Ok(ProcessorReadOutput::Text {
        body: output,
        truncated: false,
        cursor: None,
    })
}

fn workbook_relationship_ids(bytes: &[u8]) -> Result<Vec<String>, XmlIssue> {
    ordered_relationship_ids(bytes, b"sheets", b"sheet")
}

fn ordered_relationship_ids(
    bytes: &[u8],
    list_name: &[u8],
    item_name: &[u8],
) -> Result<Vec<String>, XmlIssue> {
    let transcoded = transcode_xml(bytes)?;
    let mut reader = Reader::from_reader(Cursor::new(transcoded.as_ref()));
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut depth = 0_usize;
    let mut list_depth = None;
    let mut ids = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buffer)
            .map_err(|_| XmlIssue::Malformed)?
        {
            Event::Start(element) => {
                if local_name(element.name().as_ref()) == list_name {
                    if list_depth.is_some() {
                        return Err(XmlIssue::Malformed);
                    }
                    list_depth = Some(depth);
                } else if local_name(element.name().as_ref()) == item_name
                    && list_depth == depth.checked_sub(1)
                {
                    ids.push(required_prefixed_id(&reader, &element)?);
                }
                depth = depth.checked_add(1).ok_or(XmlIssue::Malformed)?;
            }
            Event::Empty(element)
                if local_name(element.name().as_ref()) == item_name
                    && list_depth == depth.checked_sub(1) =>
            {
                ids.push(required_prefixed_id(&reader, &element)?);
            }
            Event::End(element) => {
                depth = depth.checked_sub(1).ok_or(XmlIssue::Malformed)?;
                if local_name(element.name().as_ref()) == list_name {
                    list_depth = None;
                }
            }
            Event::DocType(_) | Event::GeneralRef(_) => return Err(XmlIssue::Malformed),
            Event::Eof if depth == 0 => break,
            Event::Eof => return Err(XmlIssue::Malformed),
            _ => {}
        }
        buffer.clear();
    }
    Ok(ids)
}

fn required_prefixed_id(
    reader: &Reader<Cursor<&[u8]>>,
    element: &quick_xml::events::BytesStart<'_>,
) -> Result<String, XmlIssue> {
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|_| XmlIssue::Malformed)?;
        let name = attribute.key.as_ref();
        if name.contains(&b':') && local_name(name) == b"id" {
            return Ok(attribute
                .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                .map_err(|_| XmlIssue::Malformed)?
                .into_owned());
        }
    }
    Err(XmlIssue::Malformed)
}

fn workbook_relationship_targets(bytes: &[u8]) -> Result<Vec<(String, String)>, XmlIssue> {
    relationship_targets(bytes, "/worksheet", spreadsheet_target_name)
}

fn relationship_targets(
    bytes: &[u8],
    relationship_suffix: &str,
    normalize: fn(&str) -> Result<String, XmlIssue>,
) -> Result<Vec<(String, String)>, XmlIssue> {
    let transcoded = transcode_xml(bytes)?;
    let mut reader = Reader::from_reader(Cursor::new(transcoded.as_ref()));
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut targets = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buffer)
            .map_err(|_| XmlIssue::Malformed)?
        {
            Event::Start(element) | Event::Empty(element)
                if local_name(element.name().as_ref()) == b"Relationship" =>
            {
                let mut id = None;
                let mut target = None;
                let mut relationship_type = None;
                let mut external = false;
                for attribute in element.attributes() {
                    let attribute = attribute.map_err(|_| XmlIssue::Malformed)?;
                    let value = attribute
                        .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                        .map_err(|_| XmlIssue::Malformed)?;
                    match attribute.key.as_ref() {
                        b"Id" => id = Some(value.into_owned()),
                        b"Target" => target = Some(value.into_owned()),
                        b"Type" => relationship_type = Some(value.into_owned()),
                        b"TargetMode" => external = value.eq_ignore_ascii_case("external"),
                        _ => {}
                    }
                }
                if relationship_type
                    .as_deref()
                    .is_some_and(|value| value.ends_with(relationship_suffix))
                {
                    if external {
                        return Err(XmlIssue::Malformed);
                    }
                    let id = id.ok_or(XmlIssue::Malformed)?;
                    if targets.iter().any(|(candidate, _)| candidate == &id) {
                        return Err(XmlIssue::Malformed);
                    }
                    targets.push((id, normalize(&target.ok_or(XmlIssue::Malformed)?)?));
                }
            }
            Event::DocType(_) | Event::GeneralRef(_) => return Err(XmlIssue::Malformed),
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    Ok(targets)
}

fn spreadsheet_target_name(target: &str) -> Result<String, XmlIssue> {
    normalized_part_target(target, "xl/", "xl/worksheets/")
}

fn normalized_part_target(
    target: &str,
    base: &str,
    required_prefix: &str,
) -> Result<String, XmlIssue> {
    if target.contains('\\') {
        return Err(XmlIssue::Malformed);
    }
    let name = target
        .strip_prefix('/')
        .map_or_else(|| format!("{base}{target}"), String::from);
    if !name.starts_with(required_prefix)
        || !name.ends_with(".xml")
        || std::path::Path::new(&name)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(XmlIssue::Malformed);
    }
    Ok(name)
}

fn spreadsheet_shared_strings(bytes: &[u8]) -> Result<Vec<String>, XmlIssue> {
    let transcoded = transcode_xml(bytes)?;
    let mut reader = Reader::from_reader(Cursor::new(transcoded.as_ref()));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut strings = Vec::new();
    let mut current = None::<String>;
    let mut text_depth = 0_usize;
    loop {
        match reader
            .read_event_into(&mut buffer)
            .map_err(|_| XmlIssue::Malformed)?
        {
            Event::Start(element) if local_name(element.name().as_ref()) == b"si" => {
                if current.replace(String::new()).is_some() {
                    return Err(XmlIssue::Malformed);
                }
            }
            Event::Start(element)
                if current.is_some() && local_name(element.name().as_ref()) == b"t" =>
            {
                text_depth = text_depth.checked_add(1).ok_or(XmlIssue::Malformed)?;
            }
            Event::End(element)
                if local_name(element.name().as_ref()) == b"t" && text_depth > 0 =>
            {
                text_depth -= 1;
            }
            Event::End(element) if local_name(element.name().as_ref()) == b"si" => {
                strings.push(current.take().ok_or(XmlIssue::Malformed)?);
            }
            Event::Text(text) if text_depth > 0 => {
                let decoded = text.xml10_content().map_err(|_| XmlIssue::Malformed)?;
                append_xml_text(current.as_mut().ok_or(XmlIssue::Malformed)?, &decoded)?;
            }
            Event::CData(text) if text_depth > 0 => {
                let decoded = text.decode().map_err(|_| XmlIssue::Malformed)?;
                append_xml_text(current.as_mut().ok_or(XmlIssue::Malformed)?, &decoded)?;
            }
            Event::DocType(_) | Event::GeneralRef(_) => return Err(XmlIssue::Malformed),
            Event::Eof if current.is_none() && text_depth == 0 => break,
            Event::Eof => return Err(XmlIssue::Malformed),
            _ => {}
        }
        buffer.clear();
    }
    Ok(strings)
}

fn spreadsheet_worksheet_text(bytes: &[u8], shared: &[String]) -> Result<String, XmlIssue> {
    let transcoded = transcode_xml(bytes)?;
    let mut reader = Reader::from_reader(Cursor::new(transcoded.as_ref()));
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut output = String::new();
    let mut shared_cell = false;
    let mut value_depth = 0_usize;
    let mut value = String::new();
    loop {
        match reader
            .read_event_into(&mut buffer)
            .map_err(|_| XmlIssue::Malformed)?
        {
            Event::Start(element) if local_name(element.name().as_ref()) == b"c" => {
                shared_cell = false;
                for attribute in element.attributes() {
                    let attribute = attribute.map_err(|_| XmlIssue::Malformed)?;
                    if attribute.key.as_ref() == b"t" {
                        shared_cell = attribute
                            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                            .map_err(|_| XmlIssue::Malformed)?
                            == "s";
                    }
                }
            }
            Event::Start(element) if shared_cell && local_name(element.name().as_ref()) == b"v" => {
                value.clear();
                value_depth = value_depth.checked_add(1).ok_or(XmlIssue::Malformed)?;
            }
            Event::Text(text) if value_depth > 0 => {
                value.push_str(&text.xml10_content().map_err(|_| XmlIssue::Malformed)?);
            }
            Event::End(element)
                if local_name(element.name().as_ref()) == b"v" && value_depth > 0 =>
            {
                value_depth -= 1;
            }
            Event::End(element) if local_name(element.name().as_ref()) == b"c" => {
                if shared_cell {
                    let index = value.parse::<usize>().map_err(|_| XmlIssue::Malformed)?;
                    append_xml_text(&mut output, shared.get(index).ok_or(XmlIssue::Malformed)?)?;
                    append_xml_text(&mut output, "\n")?;
                }
                shared_cell = false;
                value.clear();
            }
            Event::DocType(_) | Event::GeneralRef(_) => return Err(XmlIssue::Malformed),
            Event::Eof if value_depth == 0 => break,
            Event::Eof => return Err(XmlIssue::Malformed),
            _ => {}
        }
        buffer.clear();
    }
    Ok(output)
}

fn presentation_slide_names<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
) -> Result<Vec<String>, ProcessorFailure> {
    let presentation =
        read_entry(archive, "ppt/presentation.xml").map_err(|_| ProcessorFailure::Failed)?;
    let relationships = read_entry(archive, "ppt/_rels/presentation.xml.rels")
        .map_err(|_| ProcessorFailure::Failed)?;
    let relationship_ids =
        presentation_relationship_ids(&presentation).map_err(|_| ProcessorFailure::Failed)?;
    let targets =
        presentation_relationship_targets(&relationships).map_err(|_| ProcessorFailure::Failed)?;
    let mut names = Vec::with_capacity(relationship_ids.len());
    for relationship_id in relationship_ids {
        let target = targets
            .iter()
            .find_map(|(candidate, target)| (candidate == &relationship_id).then_some(target))
            .ok_or(ProcessorFailure::Failed)?;
        archive
            .by_name(target)
            .map_err(|_| ProcessorFailure::Failed)?;
        names.push(target.clone());
    }
    Ok(names)
}

fn presentation_relationship_ids(bytes: &[u8]) -> Result<Vec<String>, XmlIssue> {
    ordered_relationship_ids(bytes, b"sldIdLst", b"sldId")
}

fn presentation_relationship_targets(bytes: &[u8]) -> Result<Vec<(String, String)>, XmlIssue> {
    relationship_targets(bytes, "/slide", presentation_target_name)
}

fn presentation_target_name(target: &str) -> Result<String, XmlIssue> {
    normalized_part_target(target, "ppt/", "ppt/slides/")
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

fn extract_xml_text(bytes: &[u8], kind: OfficeKind) -> Result<String, XmlIssue> {
    let transcoded = transcode_xml(bytes)?;
    let mut reader = Reader::from_reader(Cursor::new(transcoded.as_ref()));
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut output = String::new();
    let mut element_depth = 0_usize;
    let mut text_depth = 0_usize;
    let mut saw_root = false;
    let mut alternate_depth = None;
    let mut fallback_depth = None;
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
                let qualified_name = start.name();
                let name = local_name(qualified_name.as_ref());
                if name == b"AlternateContent" {
                    if alternate_depth.replace(element_depth).is_some() {
                        return Err(XmlIssue::Malformed);
                    }
                } else if name == b"Fallback"
                    && alternate_depth.is_some_and(|depth| element_depth == depth + 1)
                {
                    if fallback_depth.replace(element_depth).is_some() {
                        return Err(XmlIssue::Malformed);
                    }
                } else if name == b"t" && (alternate_depth.is_none() || fallback_depth.is_some()) {
                    text_depth = text_depth.checked_add(1).ok_or(XmlIssue::Malformed)?;
                }
            }
            Event::End(end) => {
                element_depth = element_depth.checked_sub(1).ok_or(XmlIssue::Malformed)?;
                let qualified_name = end.name();
                let name = local_name(qualified_name.as_ref());
                if name == b"t" {
                    if alternate_depth.is_none() || fallback_depth.is_some() {
                        text_depth = text_depth.checked_sub(1).ok_or(XmlIssue::Malformed)?;
                    }
                } else if name == b"Fallback"
                    && fallback_depth.is_some_and(|depth| element_depth + 1 == depth)
                {
                    fallback_depth = None;
                } else if name == b"AlternateContent"
                    && alternate_depth.is_some_and(|depth| element_depth + 1 == depth)
                {
                    if fallback_depth.is_some() {
                        return Err(XmlIssue::Malformed);
                    }
                    alternate_depth = None;
                } else if (name == b"p"
                    || (kind == OfficeKind::Xlsx && (name == b"si" || name == b"c")))
                    && (alternate_depth.is_none() || fallback_depth.is_some())
                    && !output.is_empty()
                    && !output.ends_with('\n')
                {
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
                let selected = alternate_depth.is_none() || fallback_depth.is_some();
                if name == b"tab" && selected {
                    append_xml_text(&mut output, "\t")?;
                } else if name == b"br" && selected {
                    append_xml_text(&mut output, "\n")?;
                }
            }
            Event::Text(text) if text_depth > 0 => {
                let decoded = text.xml10_content().map_err(|_| XmlIssue::Malformed)?;
                append_xml_text(&mut output, &decoded)?;
            }
            Event::CData(text) if text_depth > 0 => {
                let decoded = text.decode().map_err(|_| XmlIssue::Malformed)?;
                append_xml_text(&mut output, &decoded)?;
            }
            Event::GeneralRef(reference) if text_depth > 0 => {
                let decoded = reference.decode().map_err(|_| XmlIssue::Malformed)?;
                let value = decode_xml_reference(&decoded)?;
                append_xml_text(&mut output, &value)?;
            }
            Event::DocType(_) | Event::GeneralRef(_) => return Err(XmlIssue::Malformed),
            Event::Eof
                if saw_root
                    && element_depth == 0
                    && text_depth == 0
                    && alternate_depth.is_none()
                    && fallback_depth.is_none() =>
            {
                break;
            }
            Event::Eof => return Err(XmlIssue::Malformed),
            _ => {}
        }
        buffer.clear();
    }
    Ok(output)
}

fn transcode_xml(bytes: &[u8]) -> Result<std::borrow::Cow<'_, [u8]>, XmlIssue> {
    let (little_endian, body) = if let Some(body) = bytes.strip_prefix(&[0xff, 0xfe]) {
        (Some(true), body)
    } else if let Some(body) = bytes.strip_prefix(&[0xfe, 0xff]) {
        (Some(false), body)
    } else if bytes.starts_with(&[b'<', 0, b'?', 0]) {
        (Some(true), bytes)
    } else if bytes.starts_with(&[0, b'<', 0, b'?']) {
        (Some(false), bytes)
    } else {
        (None, bytes)
    };
    let Some(little_endian) = little_endian else {
        return Ok(std::borrow::Cow::Borrowed(bytes));
    };
    if body.len() % 2 != 0 {
        return Err(XmlIssue::Malformed);
    }
    let units = body.chunks_exact(2).map(|pair| {
        if little_endian {
            u16::from_le_bytes([pair[0], pair[1]])
        } else {
            u16::from_be_bytes([pair[0], pair[1]])
        }
    });
    let mut decoded = char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .map_err(|_| XmlIssue::Malformed)?;
    if decoded.starts_with("<?xml") {
        let declaration_end = decoded.find("?>").ok_or(XmlIssue::Malformed)?;
        decoded.drain(..declaration_end + 2);
    }
    Ok(std::borrow::Cow::Owned(decoded.into_bytes()))
}

fn decode_xml_reference(reference: &str) -> Result<String, XmlIssue> {
    let numeric = if let Some(decimal) = reference.strip_prefix('#') {
        let value = if let Some(hexadecimal) = decimal
            .strip_prefix('x')
            .or_else(|| decimal.strip_prefix('X'))
        {
            u32::from_str_radix(hexadecimal, 16).map_err(|_| XmlIssue::Malformed)?
        } else {
            decimal.parse::<u32>().map_err(|_| XmlIssue::Malformed)?
        };
        if !is_xml_character(value) {
            return Err(XmlIssue::Malformed);
        }
        Some(char::from_u32(value).ok_or(XmlIssue::Malformed)?)
    } else {
        None
    };
    Ok(match reference {
        "amp" => String::from("&"),
        "lt" => String::from("<"),
        "gt" => String::from(">"),
        "apos" => String::from("'"),
        "quot" => String::from("\""),
        _ => numeric.map(String::from).ok_or(XmlIssue::Malformed)?,
    })
}

fn is_xml_character(value: u32) -> bool {
    matches!(
        value,
        0x9 | 0xa | 0xd | 0x20..=0xd7ff | 0xe000..=0xfffd | 0x10000..=0x10ffff
    )
}

fn append_xml_text(output: &mut String, value: &str) -> Result<(), XmlIssue> {
    let total = output
        .len()
        .checked_add(value.len())
        .ok_or(XmlIssue::OutputTooLarge)?;
    if total > MAX_TEXT_OR_JSON_BYTES {
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
    if total > MAX_TEXT_OR_JSON_BYTES {
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
    bytes
        .windows(4)
        .enumerate()
        .rev()
        .find_map(|(offset, signature)| {
            if signature != b"PK\x05\x06" {
                return None;
            }
            let record = bytes.get(offset..)?;
            let comment_length = usize::from(le_u16(record, 20).ok()?);
            let expected_length = 22_usize.checked_add(comment_length)?;
            (record.len() == expected_length).then_some(offset)
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use signalbox_file_media_runtime::{
        FileDigest, NeverCancelled, SourceReadError, SourceReadFuture,
    };

    struct UnavailableSource;

    impl VerifiedBlobSource for UnavailableSource {
        fn digest(&self) -> FileDigest {
            FileDigest::from_bytes([0x4f; 32])
        }

        fn byte_length(&self) -> NonZeroU64 {
            NonZeroU64::new(64).expect("fixture length is nonzero")
        }

        fn read_range(&self, _offset: u64, _length: NonZeroU64) -> SourceReadFuture<'_> {
            Box::pin(async { Err(SourceReadError::Unavailable) })
        }
    }

    #[test]
    fn content_types_requires_the_opc_namespace() {
        let xml = concat!(
            "<Types>",
            "<Override PartName=\"/word/document.xml\" ",
            "ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>",
            "</Types>"
        );

        let result = validate_content_types(xml.as_bytes(), &[docx_main_entry()]);

        assert!(matches!(
            result,
            Err(ValidationIssue::Malformed(MALFORMED_REASON))
        ));
    }

    #[test]
    fn content_types_accepts_the_prefixed_opc_namespace() {
        let xml = concat!(
            "<ct:Types xmlns:ct=\"http://schemas.openxmlformats.org/package/2006/content-types\">",
            "<ct:Override PartName=\"/word/document.xml\" ",
            "ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>",
            "</ct:Types>"
        );

        let result = validate_content_types(xml.as_bytes(), &[docx_main_entry()]);

        assert!(matches!(result, Ok(kinds) if kinds == vec![OfficeKind::Docx]));
    }

    #[test]
    fn decoded_xml_rejects_unsupported_compression_during_inventory_parsing() {
        let name = CONTENT_TYPES.as_bytes();
        let mut central = vec![0_u8; 46 + name.len()];
        central[0..4].copy_from_slice(b"PK\x01\x02");
        central[10..12].copy_from_slice(&99_u16.to_le_bytes());
        central[28..30].copy_from_slice(
            &u16::try_from(name.len())
                .expect("fixture entry name fits in a ZIP length")
                .to_le_bytes(),
        );
        central[46..].copy_from_slice(name);

        let result = parse_central_directory(&central, 1);

        assert!(matches!(
            result,
            Err(CentralParseError {
                issue: ValidationIssue::Malformed(MALFORMED_REASON),
                ..
            })
        ));
    }

    #[test]
    fn probe_budget_reserves_the_content_types_local_filename() {
        let admitted_central_bytes = VALIDATION_SOURCE_BYTES
            - ZIP_SUFFIX_BYTES
            - ZIP_PREFIX_BYTES
            - LOCAL_HEADER_BYTES
            - CONTENT_TYPES_NAME_BYTES
            - CONTENT_TYPES_COMPRESSED_BYTES
            - LOCAL_HEADER_BYTES
            - PACKAGE_RELS_NAME_BYTES
            - PACKAGE_RELS_COMPRESSED_BYTES;

        assert_eq!(
            ZIP_PREFIX_BYTES
                + ZIP_SUFFIX_BYTES
                + admitted_central_bytes
                + LOCAL_HEADER_BYTES
                + CONTENT_TYPES_NAME_BYTES
                + CONTENT_TYPES_COMPRESSED_BYTES
                + LOCAL_HEADER_BYTES
                + PACKAGE_RELS_NAME_BYTES
                + PACKAGE_RELS_COMPRESSED_BYTES,
            VALIDATION_SOURCE_BYTES
        );
    }

    #[tokio::test]
    async fn content_type_source_failure_remains_a_processor_failure() {
        let inventory = CentralInventory {
            entries: 1,
            expanded_bytes: 1,
            entries_by_name: vec![CentralEntry {
                name: String::from(CONTENT_TYPES),
                flags: 0,
                compression: 0,
                crc32: 0,
                compressed_bytes: 1,
                expanded_bytes: 1,
                local_offset: 0,
            }],
            kinds: vec![OfficeKind::Docx],
            encrypted: false,
        };

        let result = read_probe_entry(
            &UnavailableSource,
            &NeverCancelled,
            &inventory,
            CONTENT_TYPES,
            CONTENT_TYPES_COMPRESSED_BYTES,
        )
        .await;

        assert!(matches!(
            result,
            Err(CentralReadError::Processor(ProcessorFailure::Failed))
        ));
    }

    #[test]
    fn text_extraction_decodes_numeric_references() {
        let xml = b"<w:document><w:t>&#65;&#x42;</w:t></w:document>";

        let result = extract_xml_text(xml, OfficeKind::Docx);

        assert!(matches!(result, Ok(text) if text == "AB"));
    }

    #[test]
    fn text_extraction_preserves_cdata() {
        let xml = b"<w:document><w:t><![CDATA[A&B]]></w:t></w:document>";

        let result = extract_xml_text(xml, OfficeKind::Docx);

        assert!(matches!(result, Ok(text) if text == "A&B"));
    }

    #[test]
    fn text_extraction_selects_markup_compatibility_fallback() {
        let xml = br#"<w:document xmlns:w="urn:w" xmlns:mc="urn:mc"><mc:AlternateContent><mc:Choice Requires="w14"><w:p><w:t>choice</w:t></w:p></mc:Choice><mc:Fallback><w:p><w:t>fallback</w:t></w:p></mc:Fallback></mc:AlternateContent></w:document>"#;

        let result = extract_xml_text(xml, OfficeKind::Docx);

        assert!(matches!(result, Ok(text) if text == "fallback\n"));
    }

    #[test]
    fn presentation_relationship_ids_ignore_extension_slide_ids() {
        let xml = br#"<p:presentation xmlns:p="urn:p" xmlns:r="urn:r" xmlns:p14="urn:p14"><p:sldIdLst><p:sldId r:id="rId1"/></p:sldIdLst><p:extLst><p14:sldId id="99"/></p:extLst></p:presentation>"#;

        let result = presentation_relationship_ids(xml);

        assert!(matches!(result, Ok(ids) if ids == vec![String::from("rId1")]));
    }

    #[test]
    fn workbook_relationship_ids_preserve_sheet_order() {
        let xml = br#"<workbook xmlns:r="urn:r"><sheets><sheet r:id="rId2"/><sheet r:id="rId1"/></sheets></workbook>"#;

        let result = workbook_relationship_ids(xml);

        assert!(
            matches!(result, Ok(ids) if ids == vec![String::from("rId2"), String::from("rId1")])
        );
    }

    #[test]
    fn worksheet_shared_strings_follow_cell_occurrence_order() {
        let xml = br#"<worksheet><sheetData><row><c t="s"><v>1</v></c><c t="s"><v>1</v></c></row></sheetData></worksheet>"#;
        let shared = vec![String::from("unused"), String::from("used")];

        let result = spreadsheet_worksheet_text(xml, &shared);

        assert!(matches!(result, Ok(text) if text == "used\nused\n"));
    }

    #[test]
    fn text_extraction_transcodes_utf16_little_endian() {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-16\"?><w:document><w:t>wide text</w:t></w:document>";
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));

        let result = extract_xml_text(&bytes, OfficeKind::Docx);

        assert!(matches!(result, Ok(text) if text == "wide text"));
    }

    #[test]
    fn eocd_scan_ignores_signature_bytes_inside_the_comment() {
        let mut bytes = vec![0_u8; 22];
        bytes[0..4].copy_from_slice(b"PK\x05\x06");
        bytes[20..22].copy_from_slice(&8_u16.to_le_bytes());
        bytes.extend_from_slice(b"PK\x05\x06tail");

        assert_eq!(find_eocd(&bytes), Some(0));
    }

    fn docx_main_entry() -> CentralEntry {
        CentralEntry {
            name: String::from("word/document.xml"),
            flags: 0,
            compression: 0,
            crc32: 0,
            compressed_bytes: 0,
            expanded_bytes: 0,
            local_offset: 0,
        }
    }
}
